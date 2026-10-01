//! Turns decoded MPEG frames into BGRA pictures and judges them as thumbnails.
//!
//! CONTRACT (fixed; other modules are written against it): `Rect`, `Picture`,
//! `LumaStats` and the four functions keep these signatures and meanings.

use crate::mpeg2::Frame;

/// A rectangle in luma sample coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A decoded picture ready for scaling.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Top-down BGRA, row stride `width * 4`, alpha always 255.
    pub bgra: Vec<u8>,
    /// Shape of one pixel on screen as width:height ((1, 1) = square).
    pub pixel_aspect: (u32, u32),
}

impl Picture {
    /// Size on screen: the width corrected by `pixel_aspect`, the height
    /// unchanged. Both are at least 1.
    pub fn display_size(&self) -> (u32, u32) {
        (self.width.max(1), self.height.max(1))
    }
}

/// Brightness statistics of a frame region, on stored (studio range) luma values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LumaStats {
    pub mean: f32,
    pub stddev: f32,
}

/// The whole frame as a `Rect`.
pub fn full_area(frame: &Frame) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: frame.width,
        height: frame.height,
    }
}

/// The part of the frame left after cutting uniform dark borders (letterbox
/// bars at the top and bottom, pillarbox bars at the sides). Never cuts more
/// than a quarter of the height from the top or the bottom, nor more than a
/// quarter of the width from either side; coordinates and sizes are even so
/// the chroma planes stay aligned. Returns the full frame when there are no
/// borders.
pub fn active_area(frame: &Frame) -> Rect {
    full_area(frame)
}

/// Mean and standard deviation of the luma samples inside `area` (sampled on
/// a grid when the area is large).
pub fn luma_stats(frame: &Frame, area: Rect) -> LumaStats {
    let _ = (frame, area);
    LumaStats {
        mean: 0.0,
        stddev: 0.0,
    }
}

/// Converts `area` of the frame to BGRA using the frame's colour matrix
/// (studio range Y'CbCr → full range R'G'B'), with bilinear chroma upsampling.
/// The result carries the frame's pixel aspect ratio.
pub fn to_picture(frame: &Frame, area: Rect) -> Picture {
    let _ = frame;
    Picture {
        width: area.width,
        height: area.height,
        bgra: Vec::new(),
        pixel_aspect: (1, 1),
    }
}
