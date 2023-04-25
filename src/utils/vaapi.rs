// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::rc::Rc;

use anyhow::anyhow;
use libva::Config;
use libva::Context;
use libva::Display;
use libva::Image;
use libva::PictureEnd;
use libva::PictureNew;
use libva::PictureSync;
use libva::Surface;
use libva::VAConfigAttrib;
use libva::VAConfigAttribType;

use crate::decoders::DecodedHandle as DecodedHandleTrait;
use crate::decoders::DynHandle;
use crate::decoders::Error as VideoDecoderError;
use crate::decoders::MappableHandle;
use crate::decoders::Result as VideoDecoderResult;
use crate::decoders::StatelessBackendError;
use crate::decoders::StatelessBackendResult;
use crate::decoders::VideoDecoderBackend;
use crate::i420_copy;
use crate::nv12_copy;
use crate::DecodedFormat;
use crate::Resolution;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct FormatMap {
    pub rt_format: u32,
    pub va_fourcc: u32,
    pub decoded_format: DecodedFormat,
}

/// Maps a given VA_RT_FORMAT to a compatible decoded format in an arbitrary
/// preferred order.
const FORMAT_MAP: [FormatMap; 2] = [
    FormatMap {
        rt_format: libva::constants::VA_RT_FORMAT_YUV420,
        va_fourcc: libva::constants::VA_FOURCC_NV12,
        decoded_format: DecodedFormat::NV12,
    },
    FormatMap {
        rt_format: libva::constants::VA_RT_FORMAT_YUV420,
        va_fourcc: libva::constants::VA_FOURCC_I420,
        decoded_format: DecodedFormat::I420,
    },
];

/// Returns a set of supported decoded formats given `rt_format`
fn supported_formats_for_rt_format(
    display: &Display,
    rt_format: u32,
    profile: i32,
    entrypoint: u32,
    image_formats: &[libva::VAImageFormat],
) -> anyhow::Result<HashSet<FormatMap>> {
    let mut attrs = vec![VAConfigAttrib {
        type_: VAConfigAttribType::VAConfigAttribRTFormat,
        value: 0,
    }];

    display.get_config_attributes(profile, entrypoint, &mut attrs)?;

    // See whether this RT_FORMAT is supported by the given VAProfile and
    // VAEntrypoint pair.
    if attrs[0].value == libva::constants::VA_ATTRIB_NOT_SUPPORTED
        || attrs[0].value & rt_format == 0
    {
        return Err(anyhow!(
            "rt_format {:?} not supported for profile {:?} and entrypoint {:?}",
            rt_format,
            profile,
            entrypoint
        ));
    }

    let mut supported_formats = HashSet::new();

    for format in FORMAT_MAP {
        if format.rt_format == rt_format {
            supported_formats.insert(format);
        }
    }

    // Only retain those that the hardware can actually map into.
    supported_formats.retain(|&entry| {
        image_formats
            .iter()
            .any(|fmt| fmt.fourcc == entry.va_fourcc)
    });

    Ok(supported_formats)
}

impl TryInto<Surface> for PictureState {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<Surface, Self::Error> {
        match self {
            PictureState::Ready(picture) => picture.take_surface(),
            PictureState::Pending(picture) => picture.sync().map_err(|(e, _)| e)?.take_surface(),
            PictureState::Invalid => unreachable!(),
        }
    }
}

/// A decoded frame handle.
pub(crate) type DecodedHandle = Rc<RefCell<GenericBackendHandle>>;

impl DecodedHandleTrait for DecodedHandle {
    fn display_resolution(&self) -> Resolution {
        self.borrow().display_resolution
    }

    fn timestamp(&self) -> u64 {
        self.borrow().timestamp()
    }

    fn dyn_picture_mut(&self) -> RefMut<dyn DynHandle> {
        self.borrow_mut()
    }

    fn is_ready(&self) -> bool {
        self.borrow().is_va_ready().unwrap_or(true)
    }

    fn sync(&self) -> StatelessBackendResult<()> {
        self.borrow_mut().sync().map_err(|e| e.into())
    }
}

/// A surface pool handle to reduce the number of costly Surface allocations.
#[derive(Clone)]
pub(crate) struct SurfacePoolHandle {
    surfaces: Rc<RefCell<VecDeque<Surface>>>,
    coded_resolution: Resolution,
}

impl SurfacePoolHandle {
    /// Creates a new pool
    fn new(surfaces: Vec<Surface>, resolution: Resolution) -> Self {
        Self {
            surfaces: Rc::new(RefCell::new(VecDeque::from(surfaces))),
            coded_resolution: resolution,
        }
    }

    /// Retrieve the current coded resolution of the pool
    pub(crate) fn coded_resolution(&self) -> Resolution {
        self.coded_resolution
    }

    /// Adds a new surface to the pool
    fn add_surface(&mut self, surface: Surface) {
        self.surfaces.borrow_mut().push_back(surface)
    }

    /// Gets a free surface from the pool
    pub(crate) fn get_surface(&mut self) -> Option<Surface> {
        let mut vec = self.surfaces.borrow_mut();
        vec.pop_front()
    }

    /// Returns new number of surfaces left.
    fn num_surfaces_left(&self) -> usize {
        self.surfaces.borrow().len()
    }
}

/// A trait for providing the basic information needed to setup libva for decoding.
pub(crate) trait StreamInfo {
    /// Returns the VA profile of the stream.
    fn va_profile(&self) -> anyhow::Result<i32>;
    /// Returns the RT format of the stream.
    fn rt_format(&self) -> anyhow::Result<u32>;
    /// Returns the minimum number of surfaces required to decode the stream.
    fn min_num_surfaces(&self) -> usize;
    /// Returns the coded size of the surfaces required to decode the stream.
    fn coded_size(&self) -> (u32, u32);
    /// Returns the visible rectangle within the coded size for the stream.
    fn visible_rect(&self) -> ((u32, u32), (u32, u32));
}

pub(crate) struct ParsedStreamMetadata {
    /// A VAContext from which we can decode from.
    pub(crate) context: Rc<Context>,
    /// The VAConfig that created the context. It must kept here so that
    /// it does not get dropped while it is in use.
    #[allow(dead_code)]
    config: Config,
    /// A pool of surfaces. We reuse surfaces as they are expensive to allocate.
    pub(crate) surface_pool: SurfacePoolHandle,
    /// The number of surfaces required to parse the stream.
    min_num_surfaces: usize,
    /// The decoder current display resolution.
    display_resolution: Resolution,
    /// The image format we will use to map the surfaces. This is usually the
    /// same as the surface's internal format, but occasionally we can try
    /// mapping in a different format if requested and if the VA-API driver can
    /// do it.
    map_format: Rc<libva::VAImageFormat>,
    /// The rt_format parsed from the stream.
    rt_format: u32,
    /// The profile parsed from the stream.
    profile: i32,
}

/// State of the input stream, which can be either unparsed (we don't know the stream properties
/// yet) or parsed (we know the stream properties and are ready to decode).
pub(crate) enum StreamMetadataState {
    /// The metadata for the current stream has not yet been parsed.
    Unparsed,
    /// The metadata for the current stream has been parsed and a suitable
    /// VAContext has been created to accomodate it.
    Parsed(ParsedStreamMetadata),
}

impl StreamMetadataState {
    /// Returns a reference to the parsed metadata state or an error if we haven't reached that
    /// state yet.
    pub(crate) fn get_parsed(&self) -> anyhow::Result<&ParsedStreamMetadata> {
        match self {
            StreamMetadataState::Unparsed { .. } => Err(anyhow!("Stream metadata not parsed yet")),
            StreamMetadataState::Parsed(parsed_metadata) => Ok(parsed_metadata),
        }
    }

    /// Returns a mutable reference to the parsed metadata state or an error if we haven't reached
    /// that state yet.
    pub(crate) fn get_parsed_mut(&mut self) -> anyhow::Result<&mut ParsedStreamMetadata> {
        match self {
            StreamMetadataState::Unparsed { .. } => Err(anyhow!("Stream metadata not parsed yet")),
            StreamMetadataState::Parsed(parsed_metadata) => Ok(parsed_metadata),
        }
    }

    /// Initializes or reinitializes the codec state.
    fn open<S: StreamInfo>(
        display: &Rc<Display>,
        hdr: S,
        format_map: Option<&FormatMap>,
    ) -> anyhow::Result<StreamMetadataState> {
        let va_profile = hdr.va_profile()?;
        let rt_format = hdr.rt_format()?;
        let (frame_w, frame_h) = hdr.coded_size();

        let attrs = vec![libva::VAConfigAttrib {
            type_: libva::VAConfigAttribType::VAConfigAttribRTFormat,
            value: rt_format,
        }];

        let config =
            display.create_config(attrs, va_profile, libva::VAEntrypoint::VAEntrypointVLD)?;

        let format_map = if let Some(format_map) = format_map {
            format_map
        } else {
            // Pick the first one that fits
            FORMAT_MAP
                .iter()
                .find(|&map| map.rt_format == rt_format)
                .ok_or(anyhow!("Unsupported format {}", rt_format))?
        };

        let map_format = display
            .query_image_formats()?
            .iter()
            .find(|f| f.fourcc == format_map.va_fourcc)
            .cloned()
            .unwrap();

        let min_num_surfaces = hdr.min_num_surfaces();

        let surfaces = display.create_surfaces(
            rt_format,
            Some(map_format.fourcc),
            frame_w,
            frame_h,
            Some(libva::UsageHint::USAGE_HINT_DECODER),
            min_num_surfaces as u32,
        )?;

        let context = display.create_context(
            &config,
            i32::try_from(frame_w)?,
            i32::try_from(frame_h)?,
            Some(&surfaces),
            true,
        )?;

        let coded_resolution = Resolution {
            width: frame_w,
            height: frame_h,
        };

        let visible_rect = hdr.visible_rect();

        let display_resolution = Resolution {
            width: visible_rect.1 .0 - visible_rect.0 .0,
            height: visible_rect.1 .1 - visible_rect.0 .1,
        };

        let surface_pool = SurfacePoolHandle::new(surfaces, coded_resolution);

        Ok(StreamMetadataState::Parsed(ParsedStreamMetadata {
            context,
            config,
            surface_pool,
            min_num_surfaces,
            display_resolution,
            map_format: Rc::new(map_format),
            rt_format,
            profile: va_profile,
        }))
    }
}

/// VA-API backend handle.
///
/// This includes the VA picture which can be pending rendering or complete, as well as useful
/// meta-information.
pub struct GenericBackendHandle {
    state: PictureState,
    /// The decoder resolution when this frame was processed. Not all codecs
    /// send resolution data in every frame header.
    coded_resolution: Resolution,
    /// Actual resolution of the visible rectangle in the decoded buffer.
    display_resolution: Resolution,
    /// Image format for this surface, taken from the pool it originates from.
    map_format: Rc<libva::VAImageFormat>,
    /// A handle to the surface pool from which the backing surface originates.
    surface_pool: SurfacePoolHandle,
}

impl Drop for GenericBackendHandle {
    fn drop(&mut self) {
        // Take ownership of the internal state.
        let state = std::mem::replace(&mut self.state, PictureState::Invalid);
        if self.surface_pool.coded_resolution() == self.coded_resolution {
            if let Ok(surface) = state.try_into() {
                self.surface_pool.add_surface(surface);
            }
        }
    }
}

impl GenericBackendHandle {
    /// Creates a new pending handle on `surface_id`.
    fn new(
        picture: libva::Picture<PictureNew>,
        metadata: &ParsedStreamMetadata,
    ) -> anyhow::Result<Self> {
        let picture = picture.begin()?.render()?.end()?;
        Ok(Self {
            state: PictureState::Pending(picture),
            coded_resolution: metadata.surface_pool.coded_resolution(),
            display_resolution: metadata.display_resolution,
            map_format: Rc::clone(&metadata.map_format),
            surface_pool: metadata.surface_pool.clone(),
        })
    }

    pub fn sync(&mut self) -> anyhow::Result<()> {
        let res;

        (self.state, res) = match std::mem::replace(&mut self.state, PictureState::Invalid) {
            state @ PictureState::Ready(_) => (state, Ok(())),
            PictureState::Pending(picture) => match picture.sync() {
                Ok(picture) => (PictureState::Ready(picture), Ok(())),
                Err((e, picture)) => (PictureState::Pending(picture), Err(e)),
            },
            PictureState::Invalid => unreachable!(),
        };

        res
    }

    /// Returns a mapped VAImage. this maps the VASurface onto our address space.
    /// This can be used in place of "DynMappableHandle::map()" if the client
    /// wants to access the backend mapping directly for any reason.
    ///
    /// Note that DynMappableHandle is downcastable.
    pub fn image(&mut self) -> anyhow::Result<Image> {
        // Image can only be retrieved in the `Ready` state.
        self.sync()?;

        match &mut self.state {
            PictureState::Ready(picture) => {
                // Get the associated VAImage, which will map the
                // VASurface onto our address space.
                let image = libva::Image::new(
                    picture,
                    *self.map_format,
                    self.display_resolution.width,
                    self.display_resolution.height,
                    false,
                )?;

                Ok(image)
            }
            // Either we are in `Ready` state or `sync` failed and we returned.
            PictureState::Pending(_) | PictureState::Invalid => unreachable!(),
        }
    }

    /// Returns the picture of this handle.
    pub fn picture(&self) -> Option<&libva::Picture<PictureSync>> {
        match &self.state {
            PictureState::Ready(picture) => Some(picture),
            PictureState::Pending(_) => None,
            PictureState::Invalid => unreachable!(),
        }
    }

    /// Returns the timestamp of this handle.
    pub fn timestamp(&self) -> u64 {
        match &self.state {
            PictureState::Ready(picture) => picture.timestamp(),
            PictureState::Pending(picture) => picture.timestamp(),
            PictureState::Invalid => unreachable!(),
        }
    }

    /// Returns the id of the VA surface backing this handle.
    pub fn surface_id(&self) -> libva::VASurfaceID {
        match &self.state {
            PictureState::Ready(picture) => picture.surface_id(),
            PictureState::Pending(picture) => picture.surface_id(),
            PictureState::Invalid => unreachable!(),
        }
    }

    pub fn is_va_ready(&self) -> anyhow::Result<bool> {
        match &self.state {
            PictureState::Ready(_) => Ok(true),
            PictureState::Pending(picture) => picture
                .query_status()
                .map(|s| s == libva::VASurfaceStatus::VASurfaceReady),
            PictureState::Invalid => unreachable!(),
        }
    }
}

impl DynHandle for GenericBackendHandle {
    fn dyn_mappable_handle_mut<'a>(&'a mut self) -> Box<dyn MappableHandle + 'a> {
        Box::new(self.image().unwrap())
    }
}

/// Rendering state of a VA picture.
enum PictureState {
    Ready(libva::Picture<PictureSync>),
    Pending(libva::Picture<PictureEnd>),
    // Only set in the destructor when we take ownership of the VA picture.
    Invalid,
}

impl<'a> MappableHandle for Image<'a> {
    fn read(&mut self, buffer: &mut [u8]) -> VideoDecoderResult<()> {
        let image_size = self.image_size();
        let image_inner = self.image();

        let width = image_inner.width as u32;
        let height = image_inner.height as u32;

        if buffer.len() != image_size {
            return Err(VideoDecoderError::StatelessBackendError(
                StatelessBackendError::Other(anyhow!(
                    "buffer size is {} while image size is {}",
                    buffer.len(),
                    image_size
                )),
            ));
        }

        match image_inner.format.fourcc {
            libva::constants::VA_FOURCC_NV12 => {
                nv12_copy(
                    self.as_ref(),
                    buffer,
                    width,
                    height,
                    image_inner.pitches,
                    image_inner.offsets,
                );
            }
            libva::constants::VA_FOURCC_I420 => {
                i420_copy(
                    self.as_ref(),
                    buffer,
                    width,
                    height,
                    image_inner.pitches,
                    image_inner.offsets,
                );
            }
            _ => {
                return Err(crate::decoders::Error::StatelessBackendError(
                    StatelessBackendError::UnsupportedFormat,
                ))
            }
        }

        Ok(())
    }

    fn image_size(&mut self) -> usize {
        let image = self.image();

        crate::decoded_frame_size(
            (&image.format).try_into().unwrap(),
            image.width,
            image.height,
        )
    }
}

impl TryFrom<&libva::VAImageFormat> for DecodedFormat {
    type Error = anyhow::Error;

    fn try_from(value: &libva::VAImageFormat) -> Result<Self, Self::Error> {
        match value.fourcc {
            libva::constants::VA_FOURCC_NV12 => Ok(DecodedFormat::NV12),
            libva::constants::VA_FOURCC_I420 => Ok(DecodedFormat::I420),
            _ => Err(anyhow!("Unsupported format")),
        }
    }
}

/// Keeps track of where the backend is in the negotiation process.
///
/// The generic parameter is the data that the decoder wishes to pass from the `Possible` to the
/// `Negotiated` state - typically, the properties of the stream like its resolution as they have
/// been parsed.
#[derive(Clone, Default)]
pub(crate) enum NegotiationStatus<T> {
    /// No property about the stream has been parsed yet.
    #[default]
    NonNegotiated,
    /// Properties of the stream have been parsed and the client may query and change them.
    Possible(T),
    /// Stream is actively decoding and its properties cannot be changed by the client.
    Negotiated,
}

impl<T> Debug for NegotiationStatus<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NegotiationStatus::NonNegotiated => write!(f, "NonNegotiated"),
            NegotiationStatus::Possible(_) => write!(f, "Possible"),
            NegotiationStatus::Negotiated => write!(f, "Negotiated"),
        }
    }
}

pub(crate) struct VaapiBackend<StreamData>
where
    for<'a> &'a StreamData: StreamInfo,
{
    /// VA display in use for this stream.
    display: Rc<Display>,
    /// The metadata state. Updated whenever the decoder reads new data from the stream.
    pub(crate) metadata_state: StreamMetadataState,
    /// The negotiation status
    pub(crate) negotiation_status: NegotiationStatus<Box<StreamData>>,
}

impl<StreamData> VaapiBackend<StreamData>
where
    StreamData: Clone,
    for<'a> &'a StreamData: StreamInfo,
{
    pub(crate) fn new(display: Rc<libva::Display>) -> Self {
        Self {
            display,
            metadata_state: StreamMetadataState::Unparsed,
            negotiation_status: Default::default(),
        }
    }

    pub(crate) fn new_sequence(
        &mut self,
        stream_params: &StreamData,
    ) -> StatelessBackendResult<()> {
        self.metadata_state = StreamMetadataState::open(&self.display, stream_params, None)?;
        self.negotiation_status = NegotiationStatus::Possible(Box::new(stream_params.clone()));

        Ok(())
    }

    pub(crate) fn process_picture(
        &mut self,
        picture: libva::Picture<PictureNew>,
    ) -> StatelessBackendResult<<Self as VideoDecoderBackend>::Handle> {
        let metadata = self.metadata_state.get_parsed()?;

        Ok(Rc::new(RefCell::new(GenericBackendHandle::new(
            picture, metadata,
        )?)))
    }

    /// Gets a set of supported formats for the particular stream being
    /// processed. This requires that some buffers be processed before this call
    /// is made. Only formats that are compatible with the current color space,
    /// bit depth, and chroma format are returned such that no conversion is
    /// needed.
    fn supported_formats_for_stream(&self) -> anyhow::Result<HashSet<DecodedFormat>> {
        let metadata = self.metadata_state.get_parsed()?;
        let image_formats = self.display.query_image_formats()?;

        let formats = supported_formats_for_rt_format(
            &self.display,
            metadata.rt_format,
            metadata.profile,
            libva::VAEntrypoint::VAEntrypointVLD,
            &image_formats,
        )?;

        Ok(formats.into_iter().map(|f| f.decoded_format).collect())
    }
}

impl<StreamData> VideoDecoderBackend for VaapiBackend<StreamData>
where
    StreamData: Clone,
    for<'a> &'a StreamData: StreamInfo,
{
    type Handle = DecodedHandle;

    fn coded_resolution(&self) -> Option<Resolution> {
        self.metadata_state
            .get_parsed()
            .map(|m| m.surface_pool.coded_resolution())
            .ok()
    }

    fn display_resolution(&self) -> Option<Resolution> {
        self.metadata_state
            .get_parsed()
            .map(|m| m.display_resolution)
            .ok()
    }

    fn num_resources_total(&self) -> usize {
        self.metadata_state
            .get_parsed()
            .map(|m| m.min_num_surfaces)
            .unwrap_or(0)
    }

    fn num_resources_left(&self) -> usize {
        self.metadata_state
            .get_parsed()
            .map(|m| m.surface_pool.num_surfaces_left())
            .unwrap_or(0)
    }

    fn format(&self) -> Option<crate::DecodedFormat> {
        let map_format = self
            .metadata_state
            .get_parsed()
            .map(|m| &m.map_format)
            .ok()?;
        DecodedFormat::try_from(map_format.as_ref()).ok()
    }

    fn try_format(&mut self, format: crate::DecodedFormat) -> VideoDecoderResult<()> {
        let header = match &self.negotiation_status {
            NegotiationStatus::Possible(header) => header,
            _ => {
                return Err(VideoDecoderError::StatelessBackendError(
                    StatelessBackendError::NegotiationFailed(anyhow!(
                        "Negotiation is not possible at this stage {:?}",
                        self.negotiation_status
                    )),
                ))
            }
        };

        let supported_formats_for_stream = self.supported_formats_for_stream()?;

        if supported_formats_for_stream.contains(&format) {
            let map_format = FORMAT_MAP
                .iter()
                .find(|&map| map.decoded_format == format)
                .unwrap();

            self.metadata_state =
                StreamMetadataState::open(&self.display, header.as_ref(), Some(map_format))?;

            Ok(())
        } else {
            Err(VideoDecoderError::StatelessBackendError(
                StatelessBackendError::NegotiationFailed(anyhow!(
                    "Format {:?} is unsupported.",
                    format
                )),
            ))
        }
    }
}
