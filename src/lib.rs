// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This crate provides tools to help decode and encode various video codecs, leveraging the
//! hardware acceleration available on the target.
//!
//! The [codec] module contains tools to parse encoded video streams like H.264 or VP9 and extract
//! the information useful in order to perform e.g. hardware-accelerated decoding.
//!
//! The [utils] module contains some useful code that is shared between different parts of this
//! crate and didn't fit any of the modules above.

#![allow(clippy::collapsible_if)]

pub mod codec;
pub mod utils;

/// Rounding modes for `Resolution`
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResolutionRoundMode {
    /// Rounds component-wise to the next even value.
    Even,
}

/// A frame resolution in pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    /// Whether `self` can contain `other`.
    pub fn can_contain(&self, other: Self) -> bool {
        self.width >= other.width && self.height >= other.height
    }

    /// Rounds `self` according to `rnd_mode`.
    pub fn round(mut self, rnd_mode: ResolutionRoundMode) -> Self {
        match rnd_mode {
            ResolutionRoundMode::Even => {
                if self.width % 2 != 0 {
                    self.width += 1;
                }

                if self.height % 2 != 0 {
                    self.height += 1;
                }
            }
        }

        self
    }
}

impl From<(u32, u32)> for Resolution {
    fn from(value: (u32, u32)) -> Self {
        Self {
            width: value.0,
            height: value.1,
        }
    }
}

impl From<Resolution> for (u32, u32) {
    fn from(value: Resolution) -> Self {
        (value.width, value.height)
    }
}
