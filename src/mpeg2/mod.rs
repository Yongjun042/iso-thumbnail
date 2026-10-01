//! Intra-only MPEG-1 / MPEG-2 video decoder (ISO/IEC 11172-2, ISO/IEC 13818-2 / ITU-T H.262).
//!
//! Decodes a single I-picture from a video elementary stream: enough for a
//! thumbnail of a DVD title or a DVD jacket picture (`JACKET_P/*.MP2`).
//! Predicted pictures (P/B), scalable extensions and chroma formats other than
//! 4:2:0 are not supported. Everything is bounds-checked: the input comes from
//! untrusted disc images.
//!
//! CONTRACT (fixed; other modules are written against it):
//! - `find_intra_picture` and `decode_intra` keep these signatures.
//! - `Frame` keeps these public fields and their meaning.

use core::ops::Range;

use crate::error::{Error, Result};

/// Largest picture accepted, in luma samples (MPEG-2 Main Profile @ High Level).
pub const MAX_WIDTH: u32 = 1920;
/// Largest picture accepted, in luma samples (MPEG-2 Main Profile @ High Level).
pub const MAX_HEIGHT: u32 = 1152;

/// Colour matrix for the Y'CbCr → R'G'B' conversion (studio range in both cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    /// ITU-R BT.601 (SD video, DVD). Also the default when nothing is signalled.
    Bt601,
    /// ITU-R BT.709 (`matrix_coefficients` = 1 in a sequence display extension).
    Bt709,
}

/// One decoded intra picture, 4:2:0, 8 bits per sample.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Picture width in luma samples (`horizontal_size`).
    pub width: u32,
    /// Picture height in luma samples (`vertical_size`), always the full frame
    /// height, also when only one field was decoded.
    pub height: u32,
    /// Luma plane: `width * height` bytes, row stride `width`.
    pub y: Vec<u8>,
    /// Cb plane: `chroma_width() * chroma_height()` bytes, row stride `chroma_width()`.
    pub cb: Vec<u8>,
    /// Cr plane: same layout as `cb`.
    pub cr: Vec<u8>,
    /// Shape of one luma sample on screen as width:height, reduced (e.g. 8:9
    /// for 4:3 NTSC DVD, 32:27 for 16:9 NTSC DVD). (1, 1) for square samples or
    /// when the stream does not say.
    pub pixel_aspect: (u32, u32),
    /// Colour matrix signalled by the stream (BT.601 when absent).
    pub matrix: ColorMatrix,
    /// True when the I-picture was a single field picture: its lines were
    /// doubled to fill the frame height.
    pub field_doubled: bool,
    /// True for an MPEG-1 stream (no sequence extension).
    pub mpeg1: bool,
    /// Macroblocks that could not be decoded (corrupt slice data) and were
    /// filled with mid-grey (Y = Cb = Cr = 128).
    pub concealed_macroblocks: u32,
    /// Macroblocks in the decoded picture (field or frame).
    pub total_macroblocks: u32,
}

impl Frame {
    /// Width of the chroma planes.
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    /// Height of the chroma planes.
    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }
}

/// Finds the first complete intra-coded picture in a video elementary stream.
///
/// Returns the byte range that starts at a sequence header (`00 00 01 B3`) and
/// ends right before the start code that terminates the first I-picture after
/// that header: the next picture start code (`00 00 01 00`), group start code
/// (`B8`), sequence header (`B3`) or sequence end code (`B7`). Pictures that
/// are not intra coded between the header and the I-picture stay inside the
/// range (`decode_intra` skips them). Returns `None` when there is no sequence
/// header, no I-picture after one, or the I-picture is not yet terminated
/// (more data is needed). A caller holding a whole file appends a sequence end
/// code (`00 00 01 B7`) so the last picture counts as terminated.
pub fn find_intra_picture(es: &[u8]) -> Option<Range<usize>> {
    let _ = es;
    None
}

/// Decodes the first I-picture after the first sequence header in `es`
/// (normally a range returned by `find_intra_picture`).
///
/// A field I-picture is decoded alone and line-doubled (`field_doubled`).
/// Corrupt slices are concealed and counted; the call fails only when the
/// headers are unusable, the format is unsupported, the size exceeds
/// `MAX_WIDTH` × `MAX_HEIGHT`, or no macroblock at all could be decoded.
pub fn decode_intra(es: &[u8]) -> Result<Frame> {
    let _ = es;
    Err(Error::Unsupported(
        "MPEG video decoding is not implemented yet",
    ))
}
