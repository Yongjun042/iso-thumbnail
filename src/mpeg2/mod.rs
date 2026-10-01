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

mod bits;
mod headers;
mod idct;
mod slice;
mod vlc;

#[cfg(test)]
mod tests;

use bits::{find_start_code, Unit, Units};
use headers::{
    PictureCoding, PictureHeader, Sequence, EXT_PICTURE_CODING, EXT_PICTURE_SPATIAL_SCALABLE,
    EXT_PICTURE_TEMPORAL_SCALABLE, EXT_QUANT_MATRIX, EXT_SEQUENCE, EXT_SEQUENCE_DISPLAY,
    EXT_SEQUENCE_SCALABLE, FRAME_PICTURE, PICTURE_TYPE_I,
};
use slice::{Params, PictureDecoder, Planes};

pub use headers::pixel_aspect;

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
    let sequence = first_sequence_header(es)?;
    let mut intra = false;
    let mut at = sequence;
    while let Some(next) = find_start_code(es, at + 4) {
        let code = *es.get(next + 3)?;
        if intra {
            if matches!(code, 0x00 | 0xB3 | 0xB7 | 0xB8) {
                return Some(sequence..next);
            }
        } else if code == 0x00 {
            // picture_coding_type: the 3 bits after temporal_reference.
            intra = (*es.get(next + 5)? >> 3) & 7 == PICTURE_TYPE_I;
        }
        at = next;
    }
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
    decode_intra_within(es, MAX_WIDTH, MAX_HEIGHT)
}

/// Like `decode_intra`, but fails with `TooLarge` before allocating anything
/// when the sequence header that applies to the I-picture declares a picture
/// wider than `max_width` or taller than `max_height` (both capped at
/// `MAX_WIDTH` × `MAX_HEIGHT`). Callers that know their content, such as DVD
/// (720 × 576 at most), use it to keep foreign pictures from costing more.
pub fn decode_intra_within(es: &[u8], max_width: u32, max_height: u32) -> Result<Frame> {
    let limit = (max_width.min(MAX_WIDTH), max_height.min(MAX_HEIGHT));
    let start = first_sequence_header(es).ok_or(Error::Corrupt("no MPEG sequence header"))?;
    let mut units = Units::new(es, start);
    let first = units
        .next()
        .ok_or(Error::Corrupt("no MPEG sequence header"))?;
    let mut seq = Sequence::parse(payload(es, &first))?;
    // Where the extensions that follow belong.
    let mut context = Context::Sequence;
    // Inside an I-picture: its picture coding extension once seen.
    let mut intra: Option<Option<PictureCoding>> = None;
    let mut picture: Option<(PictureDecoder, Layout)> = None;

    for unit in units {
        let data = payload(es, &unit);
        if (0x01..=0xAF).contains(&unit.code) {
            let Some(coding) = intra else {
                continue;
            };
            if picture.is_none() {
                picture = Some(start_picture(&seq, coding, limit)?);
            }
            if let Some((decoder, _)) = picture.as_mut() {
                decoder.decode_slice(unit.code, data);
            }
            continue;
        }
        if picture.is_some() {
            // Any other start code ends the picture.
            break;
        }
        match unit.code {
            0xB3 => {
                seq = Sequence::parse(data)?;
                context = Context::Sequence;
                intra = None;
            }
            0xB8 => {
                context = Context::Group;
                intra = None;
            }
            0x00 => {
                context = Context::Picture;
                // A picture header too short to read is skipped like any
                // picture that is not intra coded.
                intra = PictureHeader::parse(data)
                    .ok()
                    .filter(|h| h.coding_type == PICTURE_TYPE_I)
                    .map(|_| None);
            }
            0xB5 => {
                let id = data.first().map_or(0, |b| b >> 4);
                match (context, id) {
                    (Context::Sequence, EXT_SEQUENCE) => seq.apply_sequence_extension(data)?,
                    (Context::Sequence, EXT_SEQUENCE_DISPLAY) => {
                        // Only the aspect ratio and colour matrix depend on it.
                        let _ = seq.apply_display_extension(data);
                    }
                    (Context::Sequence, EXT_SEQUENCE_SCALABLE) => {
                        return Err(Error::Unsupported("scalable MPEG-2 video"));
                    }
                    // Matrices persist, also from pictures that are skipped;
                    // a damaged one only matters for the I-picture itself (a
                    // failed update leaves the matrix unchanged).
                    (_, EXT_QUANT_MATRIX) => {
                        if let Err(e) = seq.apply_quant_matrix_extension(data) {
                            if intra.is_some() {
                                return Err(e);
                            }
                        }
                    }
                    (Context::Picture, EXT_PICTURE_CODING) => {
                        if let Some(coding) = intra.as_mut() {
                            *coding = Some(PictureCoding::parse(data)?);
                        }
                    }
                    (
                        Context::Picture,
                        EXT_PICTURE_SPATIAL_SCALABLE | EXT_PICTURE_TEMPORAL_SCALABLE,
                    ) if intra.is_some() => {
                        return Err(Error::Unsupported("scalable MPEG-2 video"));
                    }
                    _ => {}
                }
            }
            0xB7 => break,
            // User data, sequence error and reserved codes.
            _ => {}
        }
    }

    let (decoder, layout) = picture.ok_or(Error::NotFound)?;
    let planes = decoder.finish();
    if planes.decoded == 0 {
        return Err(Error::Corrupt("no MPEG macroblock could be decoded"));
    }
    Ok(assemble(&seq, &layout, planes))
}

/// Which header the extensions that follow belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Sequence,
    Group,
    Picture,
}

/// How the decoded picture maps onto the output frame.
struct Layout {
    /// A single field: line-double it to the frame height.
    field: bool,
}

/// Offset of the first sequence header start code.
fn first_sequence_header(es: &[u8]) -> Option<usize> {
    let mut at = 0;
    loop {
        let found = find_start_code(es, at)?;
        if *es.get(found + 3)? == 0xB3 {
            return Some(found);
        }
        at = found + 3;
    }
}

fn payload<'a>(es: &'a [u8], unit: &Unit) -> &'a [u8] {
    es.get(unit.payload.0..unit.payload.1).unwrap_or(&[])
}

/// Checks the headers of the I-picture whose first slice was reached and
/// sets up its decoder.
fn start_picture(
    seq: &Sequence,
    coding: Option<PictureCoding>,
    limit: (u32, u32),
) -> Result<(PictureDecoder, Layout)> {
    let coding = if seq.mpeg2 {
        coding.ok_or(Error::Corrupt("MPEG-2 picture without coding extension"))?
    } else {
        PictureCoding::MPEG1
    };
    if seq.mpeg2 && seq.chroma_format != 1 {
        return Err(Error::Unsupported("MPEG-2 chroma format other than 4:2:0"));
    }
    if seq.width == 0 || seq.height == 0 {
        return Err(Error::Corrupt("MPEG picture size is zero"));
    }
    if seq.width > limit.0 || seq.height > limit.1 {
        return Err(Error::TooLarge);
    }
    if coding.picture_structure == 0 {
        return Err(Error::Corrupt("invalid MPEG-2 picture structure"));
    }
    let field = coding.picture_structure != FRAME_PICTURE;
    let mb_width = seq.width.div_ceil(16);
    // Interlaced MPEG-2 sequences code an even number of macroblock rows so
    // that both fields have whole macroblock rows.
    let frame_rows = if seq.mpeg2 && !seq.progressive_sequence {
        2 * seq.height.div_ceil(32)
    } else {
        seq.height.div_ceil(16)
    };
    let mb_rows = if field {
        frame_rows.div_ceil(2)
    } else {
        frame_rows
    };
    let params = Params {
        mpeg1: !seq.mpeg2,
        mb_width,
        mb_rows,
        coding,
        intra_matrix: seq.intra_matrix,
    };
    Ok((
        PictureDecoder::new(vlc::tables(), &params),
        Layout { field },
    ))
}

/// Crops the macroblock-padded planes to the picture size, line-doubling a
/// field picture to the frame height.
fn assemble(seq: &Sequence, layout: &Layout, planes: Planes) -> Frame {
    let width = seq.width as usize;
    let height = seq.height as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let chroma_rows = planes.rows / 2;
    let (y, cb, cr) = if layout.field {
        (
            double_lines(&planes.y, planes.y_stride, planes.rows, width, height),
            double_lines(
                &planes.cb,
                planes.c_stride,
                chroma_rows,
                chroma_width,
                chroma_height,
            ),
            double_lines(
                &planes.cr,
                planes.c_stride,
                chroma_rows,
                chroma_width,
                chroma_height,
            ),
        )
    } else {
        (
            crop(planes.y, planes.y_stride, width, height),
            crop(planes.cb, planes.c_stride, chroma_width, chroma_height),
            crop(planes.cr, planes.c_stride, chroma_width, chroma_height),
        )
    };
    Frame {
        width: seq.width,
        height: seq.height,
        y,
        cb,
        cr,
        pixel_aspect: seq.pixel_aspect(),
        matrix: if seq.bt709 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt601
        },
        field_doubled: layout.field,
        mpeg1: !seq.mpeg2,
        concealed_macroblocks: planes.concealed,
        total_macroblocks: planes.total,
    }
}

/// The top-left `width` × `height` samples of a plane with row step `stride`
/// (at least `width`, with at least `height` rows).
fn crop(mut plane: Vec<u8>, stride: usize, width: usize, height: usize) -> Vec<u8> {
    if stride == width {
        plane.truncate(width * height);
        return plane;
    }
    let mut out = Vec::with_capacity(width * height);
    for row in plane.chunks_exact(stride).take(height) {
        out.extend_from_slice(&row[..width]);
    }
    out
}

/// Builds a `height`-line plane from the `rows` lines of one field: output
/// line `n` repeats field line `n / 2`.
fn double_lines(field: &[u8], stride: usize, rows: usize, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for line in 0..height {
        let src = (line / 2).min(rows.saturating_sub(1)) * stride;
        out.extend_from_slice(&field[src..src + width]);
    }
    out
}
