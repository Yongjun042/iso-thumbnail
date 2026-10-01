//! Sequence, picture and extension headers (ISO/IEC 13818-2 §6.2.2–6.2.3,
//! ISO/IEC 11172-2 §2.4.2).

use super::bits::BitReader;
use crate::error::{Error, Result};

/// Zigzag scan (`alternate_scan` = 0): scan index → raster index `v * 8 + u`.
#[rustfmt::skip]
pub const ZIGZAG: [u8; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10, 17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Alternate scan (`alternate_scan` = 1), for interlaced material.
#[rustfmt::skip]
pub const ALTERNATE: [u8; 64] = [
     0,  8, 16, 24,  1,  9,  2, 10, 17, 25, 32, 40, 48, 56, 57, 49,
    41, 33, 26, 18,  3, 11,  4, 12, 19, 27, 34, 42, 50, 58, 35, 43,
    51, 59, 20, 28,  5, 13,  6, 14, 21, 29, 36, 44, 52, 60, 37, 45,
    53, 61, 22, 30,  7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

/// Default intra quantiser matrix, raster order.
#[rustfmt::skip]
pub const DEFAULT_INTRA_MATRIX: [u8; 64] = [
     8, 16, 19, 22, 26, 27, 29, 34,
    16, 16, 22, 24, 27, 29, 34, 37,
    19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40,
    22, 26, 27, 29, 32, 35, 40, 48,
    26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69,
    27, 29, 35, 38, 46, 56, 69, 83,
];

const TRUNCATED: Error = Error::Corrupt("truncated MPEG video header");

/// Picture coding types (`picture_coding_type`).
pub const PICTURE_TYPE_I: u8 = 1;

/// `picture_structure` of a frame picture (1 and 2 are the top and bottom
/// field, 0 is reserved).
pub const FRAME_PICTURE: u8 = 3;

/// Extension ids (`extension_start_code_identifier`).
pub const EXT_SEQUENCE: u8 = 1;
pub const EXT_SEQUENCE_DISPLAY: u8 = 2;
pub const EXT_QUANT_MATRIX: u8 = 3;
pub const EXT_SEQUENCE_SCALABLE: u8 = 5;
pub const EXT_PICTURE_CODING: u8 = 8;
pub const EXT_PICTURE_SPATIAL_SCALABLE: u8 = 9;
pub const EXT_PICTURE_TEMPORAL_SCALABLE: u8 = 10;

/// Sequence-level state: the latest sequence header and its extensions.
#[derive(Debug, Clone)]
pub struct Sequence {
    /// Full sizes (header bits plus the sequence extension's upper bits).
    pub width: u32,
    pub height: u32,
    /// `aspect_ratio_information` (MPEG-2) or `pel_aspect_ratio` (MPEG-1).
    pub aspect_code: u8,
    /// Intra quantiser matrix in raster order (for 4:2:0 used by all blocks).
    pub intra_matrix: [u8; 64],
    /// A sequence extension followed the sequence header.
    pub mpeg2: bool,
    pub progressive_sequence: bool,
    pub chroma_format: u8,
    /// `display_horizontal_size` × `display_vertical_size`, when signalled.
    pub display_size: Option<(u32, u32)>,
    /// `colour_description` present with `matrix_coefficients` = 1.
    pub bt709: bool,
}

impl Sequence {
    /// Parses `sequence_header()` (the payload after `00 00 01 B3`). Every
    /// sequence header starts the sequence-level state afresh: an MPEG-2
    /// stream repeats its extensions after it.
    pub fn parse(payload: &[u8]) -> Result<Sequence> {
        let mut r = BitReader::new(payload);
        let width = r.read(12);
        let height = r.read(12);
        let aspect_code = r.read(4) as u8;
        // frame_rate_code 4, bit_rate_value 18, marker 1, vbv_buffer_size 10,
        // constrained_parameters_flag 1.
        r.read(4);
        r.read(18);
        r.read(1);
        r.read(10);
        r.read(1);
        let mut intra_matrix = DEFAULT_INTRA_MATRIX;
        if r.flag() {
            read_matrix(&mut r, &mut intra_matrix);
        }
        if r.flag() {
            // The non-intra matrix is not needed for intra pictures.
            skip_matrix(&mut r);
        }
        if r.overrun() {
            return Err(TRUNCATED);
        }
        Ok(Sequence {
            width,
            height,
            aspect_code,
            intra_matrix,
            mpeg2: false,
            progressive_sequence: true,
            chroma_format: 1,
            display_size: None,
            bt709: false,
        })
    }

    /// Applies `sequence_extension()` (payload starting with the extension id).
    pub fn apply_sequence_extension(&mut self, payload: &[u8]) -> Result<()> {
        let mut r = BitReader::new(payload);
        r.read(4); // extension_start_code_identifier
        r.read(8); // profile_and_level_indication
        let progressive_sequence = r.flag();
        let chroma_format = r.read(2) as u8;
        let h_ext = r.read(2);
        let v_ext = r.read(2);
        // bit_rate_extension 12, marker 1, vbv_buffer_size_extension 8,
        // low_delay 1, frame_rate_extension_n 2, frame_rate_extension_d 5.
        r.read(12);
        r.read(1);
        r.read(8);
        r.read(1);
        r.read(2);
        r.read(5);
        if r.overrun() {
            return Err(TRUNCATED);
        }
        self.mpeg2 = true;
        self.progressive_sequence = progressive_sequence;
        self.chroma_format = chroma_format;
        self.width = (self.width & 0xFFF) | (h_ext << 12);
        self.height = (self.height & 0xFFF) | (v_ext << 12);
        Ok(())
    }

    /// Applies `sequence_display_extension()`.
    pub fn apply_display_extension(&mut self, payload: &[u8]) -> Result<()> {
        let mut r = BitReader::new(payload);
        r.read(4); // extension_start_code_identifier
        r.read(3); // video_format
        let mut bt709 = false;
        if r.flag() {
            r.read(8); // colour_primaries
            r.read(8); // transfer_characteristics
            bt709 = r.read(8) == 1;
        }
        let display_width = r.read(14);
        r.read(1); // marker_bit
        let display_height = r.read(14);
        if r.overrun() {
            return Err(TRUNCATED);
        }
        self.bt709 = bt709;
        self.display_size = Some((display_width, display_height));
        Ok(())
    }

    /// Applies `quant_matrix_extension()`. Only the intra matrix matters for
    /// intra pictures; in 4:2:0 the chroma blocks use it too, so a chroma
    /// intra matrix (not allowed in 4:2:0 streams) is ignored.
    pub fn apply_quant_matrix_extension(&mut self, payload: &[u8]) -> Result<()> {
        let mut r = BitReader::new(payload);
        r.read(4); // extension_start_code_identifier
        let mut intra = self.intra_matrix;
        if r.flag() {
            read_matrix(&mut r, &mut intra);
        }
        for _ in 0..3 {
            // non-intra, chroma intra, chroma non-intra
            if r.flag() {
                skip_matrix(&mut r);
            }
        }
        if r.overrun() {
            return Err(TRUNCATED);
        }
        self.intra_matrix = intra;
        Ok(())
    }

    /// Shape of one luma sample as width:height, reduced.
    pub fn pixel_aspect(&self) -> (u32, u32) {
        pixel_aspect(
            self.mpeg2,
            self.aspect_code,
            (self.width, self.height),
            self.display_size,
        )
    }
}

/// Reads a quantiser matrix, transmitted in zigzag order, into raster order.
fn read_matrix(r: &mut BitReader, matrix: &mut [u8; 64]) {
    for &pos in ZIGZAG.iter() {
        matrix[usize::from(pos)] = r.read(8) as u8;
    }
}

fn skip_matrix(r: &mut BitReader) {
    for _ in 0..16 {
        r.read(32);
    }
}

/// The fields of `picture_header()` this decoder needs.
#[derive(Debug, Clone, Copy)]
pub struct PictureHeader {
    pub coding_type: u8,
}

impl PictureHeader {
    pub fn parse(payload: &[u8]) -> Result<PictureHeader> {
        let mut r = BitReader::new(payload);
        r.read(10); // temporal_reference
        let coding_type = r.read(3) as u8;
        if r.overrun() {
            return Err(TRUNCATED);
        }
        // vbv_delay, the forward/backward vector fields of P and B pictures
        // and extra_information_picture are not needed.
        Ok(PictureHeader { coding_type })
    }
}

/// `picture_coding_extension()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureCoding {
    /// `f_code[s][t]`: s = 0 forward / 1 backward, t = 0 horizontal / 1 vertical.
    pub f_code: [[u8; 2]; 2],
    pub intra_dc_precision: u8,
    pub picture_structure: u8,
    pub frame_pred_frame_dct: bool,
    pub concealment_motion_vectors: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
}

impl PictureCoding {
    /// What an MPEG-1 picture implies.
    pub const MPEG1: PictureCoding = PictureCoding {
        f_code: [[15, 15], [15, 15]],
        intra_dc_precision: 0,
        picture_structure: FRAME_PICTURE,
        frame_pred_frame_dct: true,
        concealment_motion_vectors: false,
        q_scale_type: false,
        intra_vlc_format: false,
        alternate_scan: false,
    };

    pub fn parse(payload: &[u8]) -> Result<PictureCoding> {
        let mut r = BitReader::new(payload);
        r.read(4); // extension_start_code_identifier
        let mut f_code = [[0u8; 2]; 2];
        for s in &mut f_code {
            for t in s.iter_mut() {
                *t = r.read(4) as u8;
            }
        }
        let intra_dc_precision = r.read(2) as u8;
        let picture_structure = r.read(2) as u8;
        r.read(1); // top_field_first
        let frame_pred_frame_dct = r.flag();
        let concealment_motion_vectors = r.flag();
        let q_scale_type = r.flag();
        let intra_vlc_format = r.flag();
        let alternate_scan = r.flag();
        // repeat_first_field, chroma_420_type, progressive_frame
        r.read(3);
        if r.flag() {
            // composite_display_flag: v_axis 1, field_sequence 3,
            // sub_carrier 1, burst_amplitude 7, sub_carrier_phase 8.
            r.read(20);
        }
        if r.overrun() {
            return Err(TRUNCATED);
        }
        Ok(PictureCoding {
            f_code,
            intra_dc_precision,
            picture_structure,
            frame_pred_frame_dct,
            concealment_motion_vectors,
            q_scale_type,
            intra_vlc_format,
            alternate_scan,
        })
    }
}

/// MPEG-1 `pel_aspect_ratio` values ×10000 (height/width of a pel), codes 1..=14.
const MPEG1_PEL_ASPECT: [u32; 14] = [
    10000, 6735, 7031, 7615, 8055, 8437, 8935, 9157, 9815, 10255, 10695, 10950, 11575, 12015,
];

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn reduce(w: u64, h: u64) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let g = gcd(w, h);
    match (u32::try_from(w / g), u32::try_from(h / g)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => (1, 1),
    }
}

/// Pixel (sample) aspect ratio as width:height, reduced.
///
/// MPEG-2 signals the display aspect ratio of the display rectangle
/// (`display_size` when signalled and non-zero, else the picture), so a sample
/// is DAR × display height / display width wide. MPEG-1 signals the sample
/// shape directly as height/width. Unknown codes give square samples.
pub fn pixel_aspect(
    mpeg2: bool,
    code: u8,
    picture: (u32, u32),
    display_size: Option<(u32, u32)>,
) -> (u32, u32) {
    if !mpeg2 {
        return match MPEG1_PEL_ASPECT.get(usize::from(code).wrapping_sub(1)) {
            Some(&v) => reduce(10000, u64::from(v)),
            None => (1, 1),
        };
    }
    let (dar_w, dar_h): (u64, u64) = match code {
        2 => (4, 3),
        3 => (16, 9),
        4 => (221, 100),
        _ => return (1, 1),
    };
    let (dw, dh) = match display_size {
        Some((w, h)) if w != 0 && h != 0 => (w, h),
        _ => picture,
    };
    reduce(dar_w * u64::from(dh), dar_h * u64::from(dw))
}
