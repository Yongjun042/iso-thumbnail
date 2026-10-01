//! Minimal intra-only MPEG-1 / MPEG-2 video elementary stream writer for tests.
//!
//! Writes I-pictures made of DC-only blocks: every 8×8 block is flat, so the
//! decoded samples are exactly the values asked for (in MPEG-2 the mismatch
//! control toggles coefficient F[7][7], which moves samples by less than 0.25
//! and so never changes the rounded result). Content is given per macroblock.
//!
//! The file is standalone (no crate imports): integration tests include it
//! with `#[path = "support/mpeg2_writer.rs"] mod mpeg2_writer;` and unit tests
//! inside the crate with a relative `#[path]`.
//!
//! The variable length codes are written out again here, as bit strings, from
//! ISO/IEC 13818-2 Annex B, independently of the decoder's tables.

/// Sequence end code, to append when a stream must be complete.
pub const SEQUENCE_END: [u8; 4] = [0, 0, 1, 0xB7];

/// MSB-first bit writer.
pub struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        }
    }

    /// Appends the low `n` bits (0..=32) of `value`.
    pub fn put(&mut self, value: u32, n: u32) {
        assert!(n <= 32);
        if n == 0 {
            return;
        }
        let value = u64::from(value) & ((1u64 << n) - 1);
        self.acc = (self.acc << n) | value;
        self.bits += n;
        while self.bits >= 8 {
            self.bits -= 8;
            self.out.push((self.acc >> self.bits) as u8);
        }
        self.acc &= (1u64 << self.bits) - 1;
    }

    /// Appends a code written as a string of '0' and '1' (spaces ignored).
    pub fn code(&mut self, bits: &str) {
        for c in bits.chars() {
            match c {
                '0' => self.put(0, 1),
                '1' => self.put(1, 1),
                _ => {}
            }
        }
    }

    pub fn flag(&mut self, on: bool) {
        self.put(u32::from(on), 1);
    }

    /// Pads with zero bits to a byte boundary.
    pub fn align(&mut self) {
        if self.bits > 0 {
            self.put(0, 8 - self.bits);
        }
    }

    /// Byte-aligns, then writes `00 00 01 code`.
    pub fn start_code(&mut self, code: u8) {
        self.align();
        self.out.extend_from_slice(&[0, 0, 1, code]);
    }

    pub fn bytes(&mut self, data: &[u8]) {
        self.align();
        self.out.extend_from_slice(data);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

/// Picture structure of the written I-picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structure {
    Frame,
    TopField,
    BottomField,
}

/// Sequence display extension contents (always with a colour description).
#[derive(Clone, Copy, Debug)]
pub struct DisplayExtension {
    pub matrix_coefficients: u8,
    pub display_width: u32,
    pub display_height: u32,
}

/// What to write. `WriterConfig::mpeg2()` and `WriterConfig::mpeg1()` give
/// plain defaults to adjust.
#[derive(Clone, Debug)]
pub struct WriterConfig {
    /// MPEG-1 stream: no extensions; the MPEG-2 only fields below are ignored.
    pub mpeg1: bool,
    /// `aspect_ratio_information` (MPEG-2) or `pel_aspect_ratio` (MPEG-1).
    pub aspect_code: u8,
    pub progressive_sequence: bool,
    pub structure: Structure,
    /// 0..=3 (8 to 11 bits).
    pub intra_dc_precision: u8,
    pub q_scale_type: bool,
    /// 1..=31.
    pub quantiser_scale_code: u8,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    /// When false (frame pictures), every macroblock carries `dct_type`
    /// (`MbContent::field_dct`).
    pub frame_pred_frame_dct: bool,
    /// Write concealment motion vectors (pseudo-random codes covering the
    /// whole motion code table) in every macroblock.
    pub concealment_motion_vectors: bool,
    /// `f_code[0][0]` and `f_code[0][1]` (1..=9) used with concealment vectors.
    pub f_code: [u8; 2],
    pub display: Option<DisplayExtension>,
    /// Custom intra quantiser matrix, raster order.
    pub intra_matrix: Option<[u8; 64]>,
    /// Slices per macroblock row, starting at evenly spread columns (so later
    /// slices start with large address increments, with escapes in wide
    /// pictures). 0 writes one slice for the whole picture (as MPEG-1 allows).
    pub slices_per_row: u32,
    /// Insert `macroblock_stuffing` before every macroblock (MPEG-1 only).
    pub macroblock_stuffing: bool,
    /// Every second macroblock is "intra + quant" and changes the scale.
    pub quant_per_macroblock: bool,
    /// Slices carry `intra_slice_flag` and one byte of extra information.
    pub slice_extra_information: bool,
    pub gop_header: bool,
    pub sequence_end: bool,
}

impl WriterConfig {
    pub fn mpeg2() -> WriterConfig {
        WriterConfig {
            mpeg1: false,
            aspect_code: 2,
            progressive_sequence: true,
            structure: Structure::Frame,
            intra_dc_precision: 0,
            q_scale_type: false,
            quantiser_scale_code: 8,
            intra_vlc_format: false,
            alternate_scan: false,
            frame_pred_frame_dct: true,
            concealment_motion_vectors: false,
            f_code: [3, 2],
            display: None,
            intra_matrix: None,
            slices_per_row: 1,
            macroblock_stuffing: false,
            quant_per_macroblock: false,
            slice_extra_information: false,
            gop_header: true,
            sequence_end: true,
        }
    }

    pub fn mpeg1() -> WriterConfig {
        WriterConfig {
            mpeg1: true,
            aspect_code: 1,
            ..WriterConfig::mpeg2()
        }
    }

    fn field(&self) -> bool {
        !self.mpeg1 && self.structure != Structure::Frame
    }

    fn dct_type_coded(&self) -> bool {
        !self.mpeg1 && self.structure == Structure::Frame && !self.frame_pred_frame_dct
    }
}

/// Contents of one macroblock: the four luma blocks (top-left, top-right,
/// bottom-left, bottom-right; with `field_dct` the top-field and bottom-field
/// halves instead of the top and bottom halves) and the two chroma blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbContent {
    pub y: [u8; 4],
    pub cb: u8,
    pub cr: u8,
    pub field_dct: bool,
}

impl MbContent {
    pub fn flat(y: u8, cb: u8, cr: u8) -> MbContent {
        MbContent {
            y: [y; 4],
            cb,
            cr,
            field_dct: false,
        }
    }
}

/// Macroblock columns of a picture `width` samples wide.
pub fn mb_width(width: u32) -> u32 {
    width.div_ceil(16)
}

/// Macroblock rows of the coded picture (of one field for field pictures).
pub fn mb_rows(cfg: &WriterConfig, height: u32) -> u32 {
    let frame_rows = if !cfg.mpeg1 && !cfg.progressive_sequence {
        2 * height.div_ceil(32)
    } else {
        height.div_ceil(16)
    };
    if cfg.field() {
        frame_rows.div_ceil(2)
    } else {
        frame_rows
    }
}

/// A complete stream: sequence header (and extensions), GOP header, one
/// I-picture with flat macroblocks of the given (Y, Cb, Cr) colours, and the
/// sequence end code when `cfg.sequence_end` is set.
pub fn intra_picture(
    cfg: &WriterConfig,
    width: u32,
    height: u32,
    mb_color: impl Fn(u32, u32) -> (u8, u8, u8),
) -> Vec<u8> {
    intra_picture_blocks(cfg, width, height, |x, y| {
        let (l, cb, cr) = mb_color(x, y);
        MbContent::flat(l, cb, cr)
    })
}

/// Like `intra_picture`, with per-block luma and `dct_type` control.
pub fn intra_picture_blocks(
    cfg: &WriterConfig,
    width: u32,
    height: u32,
    mb: impl Fn(u32, u32) -> MbContent,
) -> Vec<u8> {
    let mut out = sequence_headers(cfg, width, height);
    out.extend(picture(cfg, width, height, mb));
    if cfg.sequence_end {
        out.extend_from_slice(&SEQUENCE_END);
    }
    out
}

/// Sequence header, sequence extension (MPEG-2), optional sequence display
/// extension and (when `cfg.gop_header`) a GOP header.
pub fn sequence_headers(cfg: &WriterConfig, width: u32, height: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.start_code(0xB3);
    w.put(width & 0xFFF, 12);
    w.put(height & 0xFFF, 12);
    w.put(u32::from(cfg.aspect_code), 4);
    w.put(3, 4); // frame_rate_code: 25 Hz
    w.put(20_000, 18); // bit_rate_value (×400 bit/s)
    w.put(1, 1); // marker
    w.put(112, 10); // vbv_buffer_size_value
    w.put(0, 1); // constrained_parameters_flag
    match &cfg.intra_matrix {
        Some(m) => {
            w.put(1, 1);
            for &pos in ZIGZAG.iter() {
                w.put(u32::from(m[pos as usize]), 8);
            }
        }
        None => w.put(0, 1),
    }
    w.put(0, 1); // load_non_intra_quantiser_matrix
    if !cfg.mpeg1 {
        w.start_code(0xB5);
        w.put(1, 4); // sequence extension
        w.put(0x48, 8); // Main profile @ Main level
        w.flag(cfg.progressive_sequence);
        w.put(1, 2); // chroma_format 4:2:0
        w.put(width >> 12, 2);
        w.put(height >> 12, 2);
        w.put(0, 12); // bit_rate_extension
        w.put(1, 1); // marker
        w.put(0, 8); // vbv_buffer_size_extension
        w.put(0, 1); // low_delay
        w.put(0, 2); // frame_rate_extension_n
        w.put(0, 5); // frame_rate_extension_d
        if let Some(d) = &cfg.display {
            w.start_code(0xB5);
            w.put(2, 4); // sequence display extension
            w.put(5, 3); // video_format: unspecified
            w.put(1, 1); // colour_description
            w.put(1, 8); // colour_primaries
            w.put(1, 8); // transfer_characteristics
            w.put(u32::from(d.matrix_coefficients), 8);
            w.put(d.display_width, 14);
            w.put(1, 1); // marker
            w.put(d.display_height, 14);
        }
    }
    if cfg.gop_header {
        w.start_code(0xB8);
        w.put(0, 12); // drop_frame_flag, hours, minutes
        w.put(1, 1); // marker
        w.put(0, 12); // seconds, pictures
        w.put(1, 1); // closed_gop
        w.put(0, 1); // broken_link
    }
    w.finish()
}

/// Picture header, picture coding extension (MPEG-2) and the slices of one
/// I-picture (no sequence header).
pub fn picture(
    cfg: &WriterConfig,
    width: u32,
    height: u32,
    mb: impl Fn(u32, u32) -> MbContent,
) -> Vec<u8> {
    let mut w = BitWriter::new();
    picture_header(&mut w, cfg, 1);
    let mbw = mb_width(width);
    let rows = mb_rows(cfg, height);
    let mut index = 0u32;
    let slices: Vec<(u32, u32, u32)> = if cfg.slices_per_row == 0 {
        // One slice for everything: (row, first column, macroblocks).
        vec![(0, 0, mbw * rows)]
    } else {
        let n = cfg.slices_per_row.min(mbw);
        let mut v = Vec::new();
        for row in 0..rows {
            for k in 0..n {
                let first = k * mbw / n;
                let end = (k + 1) * mbw / n;
                v.push((row, first, end - first));
            }
        }
        v
    };
    for (row, first, count) in slices {
        w.start_code(row as u8 + 1);
        let mut qcode = cfg.quantiser_scale_code;
        w.put(u32::from(qcode), 5);
        if cfg.slice_extra_information {
            if !cfg.mpeg1 {
                w.put(1, 1); // intra_slice_flag
                w.put(1, 1); // intra_slice
                w.put(0, 7); // reserved_bits
            }
            w.put(1, 1); // extra_bit_slice
            w.put(0xA5, 8); // extra_information_slice
        }
        w.put(0, 1); // extra_bit_slice
        let reset = 1i32 << (7 + precision(cfg));
        let mut pred = [reset; 3];
        for i in 0..count {
            let addr = row * mbw + first + i;
            let (x, y) = (addr % mbw, addr / mbw);
            let content = mb(x, y);
            if cfg.mpeg1 && cfg.macroblock_stuffing {
                w.code("0000 0001 111");
            }
            let mut increment = if i == 0 { first + 1 } else { 1 };
            while increment > 33 {
                w.code("0000 0001 000"); // macroblock_escape
                increment -= 33;
            }
            w.code(ADDRESS_INCREMENT[increment as usize - 1]);
            let quant = cfg.quant_per_macroblock && index % 2 == 1;
            w.code(if quant { "01" } else { "1" });
            if cfg.dct_type_coded() {
                w.flag(content.field_dct);
            }
            if quant {
                qcode = qcode % 31 + 1;
                w.put(u32::from(qcode), 5);
            }
            if cfg.concealment_motion_vectors && !cfg.mpeg1 {
                concealment_vectors(&mut w, cfg, index);
            }
            for k in 0..4 {
                block(&mut w, cfg, true, &mut pred[0], content.y[k]);
            }
            block(&mut w, cfg, false, &mut pred[1], content.cb);
            block(&mut w, cfg, false, &mut pred[2], content.cr);
            index += 1;
        }
    }
    w.finish()
}

/// A predicted picture (`coding_type` 2 = P, 3 = B, 4 = D) with a slice of
/// filler bytes, for streams where the I-picture is not first.
pub fn predicted_picture(cfg: &WriterConfig, coding_type: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    picture_header(&mut w, cfg, coding_type);
    w.start_code(0x01);
    w.bytes(&[0x42, 0x99, 0xFF, 0x13, 0x37]);
    w.finish()
}

fn precision(cfg: &WriterConfig) -> u8 {
    if cfg.mpeg1 {
        0
    } else {
        cfg.intra_dc_precision & 3
    }
}

fn picture_header(w: &mut BitWriter, cfg: &WriterConfig, coding_type: u8) {
    w.start_code(0x00);
    w.put(0, 10); // temporal_reference
    w.put(u32::from(coding_type), 3);
    w.put(0xFFFF, 16); // vbv_delay
    if coding_type == 2 || coding_type == 3 {
        w.put(0, 1); // full_pel_forward_vector
        w.put(if cfg.mpeg1 { 1 } else { 7 }, 3); // forward_f_code
    }
    if coding_type == 3 {
        w.put(0, 1);
        w.put(if cfg.mpeg1 { 1 } else { 7 }, 3);
    }
    w.put(0, 1); // extra_bit_picture
    if cfg.mpeg1 {
        return;
    }
    w.start_code(0xB5);
    w.put(8, 4); // picture coding extension
    let forward = if cfg.concealment_motion_vectors || coding_type != 1 {
        cfg.f_code
    } else {
        [15, 15]
    };
    w.put(u32::from(forward[0]), 4);
    w.put(u32::from(forward[1]), 4);
    let backward = if coding_type == 3 {
        cfg.f_code
    } else {
        [15, 15]
    };
    w.put(u32::from(backward[0]), 4);
    w.put(u32::from(backward[1]), 4);
    w.put(u32::from(precision(cfg)), 2);
    w.put(
        match cfg.structure {
            Structure::TopField => 1,
            Structure::BottomField => 2,
            Structure::Frame => 3,
        },
        2,
    );
    w.flag(!cfg.progressive_sequence && cfg.structure == Structure::Frame); // top_field_first
    w.flag(cfg.frame_pred_frame_dct || cfg.structure != Structure::Frame);
    w.flag(cfg.concealment_motion_vectors);
    w.flag(cfg.q_scale_type);
    w.flag(cfg.intra_vlc_format);
    w.flag(cfg.alternate_scan);
    w.put(0, 1); // repeat_first_field
    let progressive_frame = cfg.progressive_sequence && cfg.structure == Structure::Frame;
    w.flag(progressive_frame); // chroma_420_type
    w.flag(progressive_frame);
    w.put(0, 1); // composite_display_flag
}

/// `motion_vectors(0)` with a single vector, then `marker_bit`. The motion
/// codes cycle through the whole table as macroblocks go by.
fn concealment_vectors(w: &mut BitWriter, cfg: &WriterConfig, index: u32) {
    if cfg.structure != Structure::Frame {
        w.put(index & 1, 1); // motion_vertical_field_select
    }
    for t in 0..2u32 {
        let step = (index * 2 + t) % 33;
        let code = step as i32 - 16;
        w.code(MOTION_CODE[code.unsigned_abs() as usize]);
        if code != 0 {
            w.flag(code < 0);
            let r = u32::from(cfg.f_code[t as usize].max(1) - 1);
            w.put(index.wrapping_mul(2_654_435_761) >> 7, r);
        }
    }
    w.put(1, 1); // marker_bit
}

/// One DC-only intra block of flat value `value`.
fn block(w: &mut BitWriter, cfg: &WriterConfig, luma: bool, pred: &mut i32, value: u8) {
    let dc = i32::from(value) << precision(cfg);
    let diff = dc - *pred;
    *pred = dc;
    let size = 32 - diff.unsigned_abs().leading_zeros();
    w.code(if luma {
        DC_SIZE_LUMINANCE[size as usize]
    } else {
        DC_SIZE_CHROMINANCE[size as usize]
    });
    if size > 0 {
        let bits = if diff > 0 {
            diff as u32
        } else {
            (diff + (1 << size) - 1) as u32
        };
        w.put(bits, size);
    }
    // End of block.
    w.code(if cfg.intra_vlc_format && !cfg.mpeg1 {
        "0110"
    } else {
        "10"
    });
}

/// Samples the decoder must produce for a stream written with `cfg`: planes
/// of `width` × `height` and `ceil(width/2)` × `ceil(height/2)`; a field
/// picture is line-doubled to the frame height.
pub fn expected_planes(
    cfg: &WriterConfig,
    width: u32,
    height: u32,
    mb: impl Fn(u32, u32) -> MbContent,
) -> [Vec<u8>; 3] {
    let field = cfg.field();
    let coded_rows = mb_rows(cfg, height);
    let mut y = Vec::with_capacity((width * height) as usize);
    for line in 0..height {
        let coded = if field {
            (line / 2).min(coded_rows * 16 - 1)
        } else {
            line
        };
        for x in 0..width {
            let c = mb(x / 16, coded / 16);
            let (bx, l) = (x % 16 / 8, coded % 16);
            let by = if c.field_dct && cfg.dct_type_coded() {
                l % 2
            } else {
                l / 8
            };
            y.push(c.y[(by * 2 + bx) as usize]);
        }
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut cb = Vec::with_capacity((cw * ch) as usize);
    let mut cr = Vec::with_capacity((cw * ch) as usize);
    for line in 0..ch {
        let coded = if field {
            (line / 2).min(coded_rows * 8 - 1)
        } else {
            line
        };
        for x in 0..cw {
            let c = mb(x / 8, coded / 8);
            cb.push(c.cb);
            cr.push(c.cr);
        }
    }
    [y, cb, cr]
}

/// Next value of a xorshift64 generator (`seed` must not be 0).
pub fn xorshift(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// A randomly damaged copy of `base`: one to three of bit flips, byte
/// overwrites, truncation, inserted start codes, removed or duplicated
/// chunks and zeroed runs.
pub fn mutate(base: &[u8], seed: &mut u64) -> Vec<u8> {
    let mut d = base.to_vec();
    let rounds = 1 + xorshift(seed) % 3;
    for _ in 0..rounds {
        if d.is_empty() {
            break;
        }
        let len = d.len() as u64;
        let at = (xorshift(seed) % len) as usize;
        match xorshift(seed) % 6 {
            0 => {
                for _ in 0..1 + xorshift(seed) % 8 {
                    let i = (xorshift(seed) % len) as usize;
                    d[i] ^= 1 << (xorshift(seed) % 8);
                }
            }
            1 => {
                for i in at..(at + 1 + (xorshift(seed) % 16) as usize).min(d.len()) {
                    d[i] = xorshift(seed) as u8;
                }
            }
            2 => d.truncate(at),
            3 => {
                let code = [0, 0, 1, xorshift(seed) as u8];
                d.splice(at..at, code);
            }
            4 => {
                let end = (at + 1 + (xorshift(seed) % 200) as usize).min(d.len());
                if xorshift(seed) % 2 == 0 {
                    d.drain(at..end);
                } else {
                    let chunk = d[at..end].to_vec();
                    d.splice(at..at, chunk);
                }
            }
            _ => {
                let end = (at + 1 + (xorshift(seed) % 64) as usize).min(d.len());
                d[at..end].fill(0);
            }
        }
    }
    d
}

/// Zigzag scan: scan index → raster index.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Table B.1, increments 1..=33.
const ADDRESS_INCREMENT: [&str; 33] = [
    "1",
    "011",
    "010",
    "0011",
    "0010",
    "0001 1",
    "0001 0",
    "0000 111",
    "0000 110",
    "0000 1011",
    "0000 1010",
    "0000 1001",
    "0000 1000",
    "0000 0111",
    "0000 0110",
    "0000 0101 11",
    "0000 0101 10",
    "0000 0101 01",
    "0000 0101 00",
    "0000 0100 11",
    "0000 0100 10",
    "0000 0100 011",
    "0000 0100 010",
    "0000 0100 001",
    "0000 0100 000",
    "0000 0011 111",
    "0000 0011 110",
    "0000 0011 101",
    "0000 0011 100",
    "0000 0011 011",
    "0000 0011 010",
    "0000 0011 001",
    "0000 0011 000",
];

/// Table B.10, motion code magnitudes 0..=16 (sign bit not included).
const MOTION_CODE: [&str; 17] = [
    "1",
    "01",
    "001",
    "0001",
    "0000 11",
    "0000 101",
    "0000 100",
    "0000 011",
    "0000 0101 1",
    "0000 0101 0",
    "0000 0100 1",
    "0000 0100 01",
    "0000 0100 00",
    "0000 0011 11",
    "0000 0011 10",
    "0000 0011 01",
    "0000 0011 00",
];

/// Table B.12, sizes 0..=11.
const DC_SIZE_LUMINANCE: [&str; 12] = [
    "100",
    "00",
    "01",
    "101",
    "110",
    "1110",
    "1111 0",
    "1111 10",
    "1111 110",
    "1111 1110",
    "1111 1111 0",
    "1111 1111 1",
];

/// Table B.13, sizes 0..=11.
const DC_SIZE_CHROMINANCE: [&str; 12] = [
    "00",
    "01",
    "10",
    "110",
    "1110",
    "1111 0",
    "1111 10",
    "1111 110",
    "1111 1110",
    "1111 1111 0",
    "1111 1111 10",
    "1111 1111 11",
];
