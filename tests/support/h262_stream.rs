//! Independent MPEG-1 / MPEG-2 (H.262) intra stream writer and reference
//! reconstruction, used to cross-check `src/mpeg2` against ffmpeg.
//!
//! The tables are transcribed directly from ISO/IEC 13818-2 (Annex B, clause 7)
//! and ISO/IEC 11172-2 (Annex B, clause 2.4) and share nothing with the decoder
//! under test: a transcription error on either side then shows up as a
//! disagreement instead of cancelling out. Codes are written as strings of
//! '0'/'1' (spaces are ignored) exactly as the standard prints them, without
//! the trailing sign bit `s` of the DCT coefficient tables.
//!
//! The writer emits a complete elementary stream for one intra picture
//! (sequence header, sequence extension, GOP, picture header, picture coding
//! extension, optional quant matrix extension, slices, sequence end) from a
//! `Picture` description in which every block lists its (run, level) pairs.
//! `reconstruct` turns the same description into samples with the normative
//! integer arithmetic (inverse quantisation, saturation, mismatch control or
//! MPEG-1 oddification) and a double precision IDCT, so any conforming decoder
//! must match it within the IDCT accuracy (±1).
//!
//! This is test code: inputs are trusted, invalid descriptions panic.

// ----------------------------------------------------------------------------
// Tables
// ----------------------------------------------------------------------------

/// Table B.1: `MBA_INCREMENT[i]` codes macroblock_address_increment `i + 1`.
pub const MBA_INCREMENT: [&str; 33] = [
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
/// Table B.1: macroblock_escape (adds 33 to the increment that follows).
pub const MBA_ESCAPE: &str = "0000 0001 000";
/// ISO/IEC 11172-2 Table B.1: macroblock_stuffing (MPEG-1 only; removed in MPEG-2).
pub const MBA_STUFFING: &str = "0000 0001 111";

/// Table B.2 (I-pictures): macroblock_type "Intra".
pub const MB_TYPE_INTRA: &str = "1";
/// Table B.2 (I-pictures): macroblock_type "Intra, Quant".
pub const MB_TYPE_INTRA_QUANT: &str = "01";

/// Table B.10: `MOTION_CODE[i]` codes motion_code `i - 16`.
pub const MOTION_CODE: [&str; 33] = [
    "0000 0011 001", // -16
    "0000 0011 011", // -15
    "0000 0011 101", // -14
    "0000 0011 111", // -13
    "0000 0100 001", // -12
    "0000 0100 011", // -11
    "0000 0100 11",  // -10
    "0000 0101 01",  // -9
    "0000 0101 11",  // -8
    "0000 0111",     // -7
    "0000 1001",     // -6
    "0000 1011",     // -5
    "0000 111",      // -4
    "0001 1",        // -3
    "0011",          // -2
    "011",           // -1
    "1",             // 0
    "010",           // 1
    "0010",          // 2
    "0001 0",        // 3
    "0000 110",      // 4
    "0000 1010",     // 5
    "0000 1000",     // 6
    "0000 0110",     // 7
    "0000 0101 10",  // 8
    "0000 0101 00",  // 9
    "0000 0100 10",  // 10
    "0000 0100 010", // 11
    "0000 0100 000", // 12
    "0000 0011 110", // 13
    "0000 0011 100", // 14
    "0000 0011 010", // 15
    "0000 0011 000", // 16
];

/// Table B.12: `DC_SIZE_LUMA[size]` codes dct_dc_size_luminance. MPEG-1
/// (11172-2 Table B.5a) uses the first nine entries.
pub const DC_SIZE_LUMA: [&str; 12] = [
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

/// Table B.13: `DC_SIZE_CHROMA[size]` codes dct_dc_size_chrominance. MPEG-1
/// (11172-2 Table B.5b) uses the first nine entries.
pub const DC_SIZE_CHROMA: [&str; 12] = [
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

/// Escape code of Tables B.14 and B.15 (and 11172-2 Table B.5c).
pub const DCT_ESCAPE: &str = "0000 01";
/// Table B.14: End of Block.
pub const B14_EOB: &str = "10";
/// Table B.14: (0, 1) as the *first* coefficient of a non-intra block
/// (`1s`). Intra blocks never use it (their first coefficient is the DC
/// term), so the cross-check cannot exercise it; it only takes part in the
/// prefix check of the "first coefficient" variant of the table.
pub const B14_FIRST_RUN0_LEVEL1: &str = "1";
/// Table B.15: End of Block.
pub const B15_EOB: &str = "0110";

/// Table B.14 (= ISO/IEC 11172-2 Table B.5c-f): (code without sign, run, level).
/// (0, 1) is the `11s` form used for every coefficient of an intra block.
pub const B14: [(&str, u8, u8); 111] = [
    ("11", 0, 1),
    ("011", 1, 1),
    ("0100", 0, 2),
    ("0101", 2, 1),
    ("0010 1", 0, 3),
    ("0011 1", 3, 1),
    ("0011 0", 4, 1),
    ("0001 10", 1, 2),
    ("0001 11", 5, 1),
    ("0001 01", 6, 1),
    ("0001 00", 7, 1),
    ("0000 110", 0, 4),
    ("0000 100", 2, 2),
    ("0000 111", 8, 1),
    ("0000 101", 9, 1),
    ("0010 0110", 0, 5),
    ("0010 0001", 0, 6),
    ("0010 0101", 1, 3),
    ("0010 0100", 3, 2),
    ("0010 0111", 10, 1),
    ("0010 0011", 11, 1),
    ("0010 0010", 12, 1),
    ("0010 0000", 13, 1),
    ("0000 0010 10", 0, 7),
    ("0000 0011 00", 1, 4),
    ("0000 0010 11", 2, 3),
    ("0000 0011 11", 4, 2),
    ("0000 0010 01", 5, 2),
    ("0000 0011 10", 14, 1),
    ("0000 0011 01", 15, 1),
    ("0000 0010 00", 16, 1),
    ("0000 0001 1101", 0, 8),
    ("0000 0001 1000", 0, 9),
    ("0000 0001 0011", 0, 10),
    ("0000 0001 0000", 0, 11),
    ("0000 0001 1011", 1, 5),
    ("0000 0001 0100", 2, 4),
    ("0000 0001 1100", 3, 3),
    ("0000 0001 0010", 4, 3),
    ("0000 0001 1110", 6, 2),
    ("0000 0001 0101", 7, 2),
    ("0000 0001 0001", 8, 2),
    ("0000 0001 1111", 17, 1),
    ("0000 0001 1010", 18, 1),
    ("0000 0001 1001", 19, 1),
    ("0000 0001 0111", 20, 1),
    ("0000 0001 0110", 21, 1),
    ("0000 0000 1101 0", 0, 12),
    ("0000 0000 1100 1", 0, 13),
    ("0000 0000 1100 0", 0, 14),
    ("0000 0000 1011 1", 0, 15),
    ("0000 0000 1011 0", 1, 6),
    ("0000 0000 1010 1", 1, 7),
    ("0000 0000 1010 0", 2, 5),
    ("0000 0000 1001 1", 3, 4),
    ("0000 0000 1001 0", 5, 3),
    ("0000 0000 1000 1", 9, 2),
    ("0000 0000 1000 0", 10, 2),
    ("0000 0000 1111 1", 22, 1),
    ("0000 0000 1111 0", 23, 1),
    ("0000 0000 1110 1", 24, 1),
    ("0000 0000 1110 0", 25, 1),
    ("0000 0000 1101 1", 26, 1),
    ("0000 0000 0111 11", 0, 16),
    ("0000 0000 0111 10", 0, 17),
    ("0000 0000 0111 01", 0, 18),
    ("0000 0000 0111 00", 0, 19),
    ("0000 0000 0110 11", 0, 20),
    ("0000 0000 0110 10", 0, 21),
    ("0000 0000 0110 01", 0, 22),
    ("0000 0000 0110 00", 0, 23),
    ("0000 0000 0101 11", 0, 24),
    ("0000 0000 0101 10", 0, 25),
    ("0000 0000 0101 01", 0, 26),
    ("0000 0000 0101 00", 0, 27),
    ("0000 0000 0100 11", 0, 28),
    ("0000 0000 0100 10", 0, 29),
    ("0000 0000 0100 01", 0, 30),
    ("0000 0000 0100 00", 0, 31),
    ("0000 0000 0011 000", 0, 32),
    ("0000 0000 0010 111", 0, 33),
    ("0000 0000 0010 110", 0, 34),
    ("0000 0000 0010 101", 0, 35),
    ("0000 0000 0010 100", 0, 36),
    ("0000 0000 0010 011", 0, 37),
    ("0000 0000 0010 010", 0, 38),
    ("0000 0000 0010 001", 0, 39),
    ("0000 0000 0010 000", 0, 40),
    ("0000 0000 0011 111", 1, 8),
    ("0000 0000 0011 110", 1, 9),
    ("0000 0000 0011 101", 1, 10),
    ("0000 0000 0011 100", 1, 11),
    ("0000 0000 0011 011", 1, 12),
    ("0000 0000 0011 010", 1, 13),
    ("0000 0000 0011 001", 1, 14),
    ("0000 0000 0001 0011", 1, 15),
    ("0000 0000 0001 0010", 1, 16),
    ("0000 0000 0001 0001", 1, 17),
    ("0000 0000 0001 0000", 1, 18),
    ("0000 0000 0001 0100", 6, 3),
    ("0000 0000 0001 1010", 11, 2),
    ("0000 0000 0001 1001", 12, 2),
    ("0000 0000 0001 1000", 13, 2),
    ("0000 0000 0001 0111", 14, 2),
    ("0000 0000 0001 0110", 15, 2),
    ("0000 0000 0001 0101", 16, 2),
    ("0000 0000 0001 1111", 27, 1),
    ("0000 0000 0001 1110", 28, 1),
    ("0000 0000 0001 1101", 29, 1),
    ("0000 0000 0001 1100", 30, 1),
    ("0000 0000 0001 1011", 31, 1),
];

/// Table B.15 (MPEG-2 only, `intra_vlc_format` = 1): (code without sign, run, level).
pub const B15: [(&str, u8, u8); 111] = [
    ("10", 0, 1),
    ("010", 1, 1),
    ("110", 0, 2),
    ("0010 1", 2, 1),
    ("0111", 0, 3),
    ("0011 1", 3, 1),
    ("0001 10", 4, 1),
    ("0011 0", 1, 2),
    ("0001 11", 5, 1),
    ("0000 110", 6, 1),
    ("0000 100", 7, 1),
    ("1110 0", 0, 4),
    ("0000 111", 2, 2),
    ("0000 101", 8, 1),
    ("1111 000", 9, 1),
    ("1110 1", 0, 5),
    ("0001 01", 0, 6),
    ("1111 001", 1, 3),
    ("0010 0110", 3, 2),
    ("1111 010", 10, 1),
    ("0010 0001", 11, 1),
    ("0010 0101", 12, 1),
    ("0010 0100", 13, 1),
    ("0001 00", 0, 7),
    ("0010 0111", 1, 4),
    ("1111 1100", 2, 3),
    ("1111 1101", 4, 2),
    ("0000 0010 0", 5, 2),
    ("0000 0010 1", 14, 1),
    ("0000 0011 1", 15, 1),
    ("0000 0011 01", 16, 1),
    ("1111 011", 0, 8),
    ("1111 100", 0, 9),
    ("0010 0011", 0, 10),
    ("0010 0010", 0, 11),
    ("0010 0000", 1, 5),
    ("0000 0011 00", 2, 4),
    ("0000 0001 1100", 3, 3),
    ("0000 0001 0010", 4, 3),
    ("0000 0001 1110", 6, 2),
    ("0000 0001 0101", 7, 2),
    ("0000 0001 0001", 8, 2),
    ("0000 0001 1111", 17, 1),
    ("0000 0001 1010", 18, 1),
    ("0000 0001 1001", 19, 1),
    ("0000 0001 0111", 20, 1),
    ("0000 0001 0110", 21, 1),
    ("1111 1010", 0, 12),
    ("1111 1011", 0, 13),
    ("1111 1110", 0, 14),
    ("1111 1111", 0, 15),
    ("0000 0000 1011 0", 1, 6),
    ("0000 0000 1010 1", 1, 7),
    ("0000 0000 1010 0", 2, 5),
    ("0000 0000 1001 1", 3, 4),
    ("0000 0000 1001 0", 5, 3),
    ("0000 0000 1000 1", 9, 2),
    ("0000 0000 1000 0", 10, 2),
    ("0000 0000 1111 1", 22, 1),
    ("0000 0000 1111 0", 23, 1),
    ("0000 0000 1110 1", 24, 1),
    ("0000 0000 1110 0", 25, 1),
    ("0000 0000 1101 1", 26, 1),
    ("0000 0000 0111 11", 0, 16),
    ("0000 0000 0111 10", 0, 17),
    ("0000 0000 0111 01", 0, 18),
    ("0000 0000 0111 00", 0, 19),
    ("0000 0000 0110 11", 0, 20),
    ("0000 0000 0110 10", 0, 21),
    ("0000 0000 0110 01", 0, 22),
    ("0000 0000 0110 00", 0, 23),
    ("0000 0000 0101 11", 0, 24),
    ("0000 0000 0101 10", 0, 25),
    ("0000 0000 0101 01", 0, 26),
    ("0000 0000 0101 00", 0, 27),
    ("0000 0000 0100 11", 0, 28),
    ("0000 0000 0100 10", 0, 29),
    ("0000 0000 0100 01", 0, 30),
    ("0000 0000 0100 00", 0, 31),
    ("0000 0000 0011 000", 0, 32),
    ("0000 0000 0010 111", 0, 33),
    ("0000 0000 0010 110", 0, 34),
    ("0000 0000 0010 101", 0, 35),
    ("0000 0000 0010 100", 0, 36),
    ("0000 0000 0010 011", 0, 37),
    ("0000 0000 0010 010", 0, 38),
    ("0000 0000 0010 001", 0, 39),
    ("0000 0000 0010 000", 0, 40),
    ("0000 0000 0011 111", 1, 8),
    ("0000 0000 0011 110", 1, 9),
    ("0000 0000 0011 101", 1, 10),
    ("0000 0000 0011 100", 1, 11),
    ("0000 0000 0011 011", 1, 12),
    ("0000 0000 0011 010", 1, 13),
    ("0000 0000 0011 001", 1, 14),
    ("0000 0000 0001 0011", 1, 15),
    ("0000 0000 0001 0010", 1, 16),
    ("0000 0000 0001 0001", 1, 17),
    ("0000 0000 0001 0000", 1, 18),
    ("0000 0000 0001 0100", 6, 3),
    ("0000 0000 0001 1010", 11, 2),
    ("0000 0000 0001 1001", 12, 2),
    ("0000 0000 0001 1000", 13, 2),
    ("0000 0000 0001 0111", 14, 2),
    ("0000 0000 0001 0110", 15, 2),
    ("0000 0000 0001 0101", 16, 2),
    ("0000 0000 0001 1111", 27, 1),
    ("0000 0000 0001 1110", 28, 1),
    ("0000 0000 0001 1101", 29, 1),
    ("0000 0000 0001 1100", 30, 1),
    ("0000 0000 0001 1011", 31, 1),
];

/// Default intra quantiser matrix (13818-2 7.4.2.1 = 11172-2 2.4.3.2), natural
/// (raster) order, `[v * 8 + u]`.
pub const DEFAULT_INTRA_MATRIX: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, //
    16, 16, 22, 24, 27, 29, 34, 37, //
    19, 22, 26, 27, 29, 34, 34, 38, //
    22, 22, 26, 27, 29, 34, 37, 40, //
    22, 26, 27, 29, 32, 35, 40, 48, //
    26, 27, 29, 32, 35, 40, 48, 58, //
    26, 27, 29, 34, 38, 46, 56, 69, //
    27, 29, 35, 38, 46, 56, 69, 83, //
];

/// Figure 7-2 (zigzag scan): scan index of the coefficient at raster position
/// `[v * 8 + u]`. Quantiser matrices are always transmitted in this order.
pub const ZIGZAG_FIGURE: [u8; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, //
    2, 4, 7, 13, 16, 26, 29, 42, //
    3, 8, 12, 17, 25, 30, 41, 43, //
    9, 11, 18, 24, 31, 40, 44, 53, //
    10, 19, 23, 32, 39, 45, 52, 54, //
    20, 22, 33, 38, 46, 51, 55, 60, //
    21, 34, 37, 47, 50, 56, 59, 61, //
    35, 36, 48, 49, 57, 58, 62, 63, //
];

/// Figure 7-3 (alternate scan): scan index of the coefficient at raster
/// position `[v * 8 + u]`.
pub const ALTERNATE_FIGURE: [u8; 64] = [
    0, 4, 6, 20, 22, 36, 38, 52, //
    1, 5, 7, 21, 23, 37, 39, 53, //
    2, 8, 19, 24, 34, 40, 50, 54, //
    3, 9, 18, 25, 35, 41, 51, 55, //
    10, 17, 26, 30, 42, 46, 56, 60, //
    11, 16, 27, 31, 43, 47, 57, 61, //
    12, 15, 28, 32, 44, 48, 58, 62, //
    13, 14, 29, 33, 45, 49, 59, 63, //
];

/// Table 7-6, `q_scale_type` = 1: quantiser_scale for quantiser_scale_code
/// 1..=31 (index 0 is forbidden).
pub const NON_LINEAR_QSCALE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

/// Inverts a scan figure: `result[scan_index]` = raster position.
pub fn scan_order(figure: &[u8; 64]) -> [usize; 64] {
    let mut order = [usize::MAX; 64];
    for (raster, &index) in figure.iter().enumerate() {
        assert_eq!(
            order[index as usize],
            usize::MAX,
            "scan index {index} twice"
        );
        order[index as usize] = raster;
    }
    order
}

/// A code string as (value, length); spaces are ignored.
pub fn code_bits(code: &str) -> (u32, u32) {
    let mut value = 0u32;
    let mut len = 0u32;
    for c in code.bytes() {
        match c {
            b'0' | b'1' => {
                value = (value << 1) | u32::from(c - b'0');
                len += 1;
            }
            b' ' => {}
            _ => panic!("bad code string {code:?}"),
        }
    }
    assert!(len > 0 && len <= 32, "bad code length in {code:?}");
    (value, len)
}

/// The DCT coefficient table in force: Table B.15 for an MPEG-2 picture with
/// `intra_vlc_format` = 1, otherwise Table B.14.
pub fn dct_table(intra_vlc_format: bool) -> (&'static [(&'static str, u8, u8); 111], &'static str) {
    if intra_vlc_format {
        (&B15, B15_EOB)
    } else {
        (&B14, B14_EOB)
    }
}

/// The table code for (run, |level|), if the table has one.
pub fn lookup(table: &[(&'static str, u8, u8)], run: u8, level: i32) -> Option<&'static str> {
    let magnitude = level.unsigned_abs();
    table
        .iter()
        .find(|&&(_, r, l)| r == run && u32::from(l) == magnitude)
        .map(|&(code, _, _)| code)
}

// ----------------------------------------------------------------------------
// Bit writer
// ----------------------------------------------------------------------------

/// MSB-first bit writer.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    acc: u8,
    used: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    fn bit(&mut self, bit: u32) {
        self.acc = (self.acc << 1) | (bit & 1) as u8;
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.acc);
            self.acc = 0;
            self.used = 0;
        }
    }

    /// Writes the low `bits` bits of `value`, most significant first.
    pub fn put(&mut self, value: u32, bits: u32) {
        assert!(bits <= 32);
        assert!(
            bits == 32 || value >> bits == 0,
            "{value} does not fit {bits} bits"
        );
        for i in (0..bits).rev() {
            self.bit(value >> i);
        }
    }

    pub fn flag(&mut self, flag: bool) {
        self.bit(u32::from(flag));
    }

    /// Writes a code given as a '0'/'1' string.
    pub fn code(&mut self, code: &str) {
        let (value, len) = code_bits(code);
        self.put(value, len);
    }

    /// next_start_code(): zero bits up to the next byte boundary.
    pub fn align(&mut self) {
        while self.used != 0 {
            self.bit(0);
        }
    }

    /// Aligns, then writes the start code `00 00 01 value`.
    pub fn start_code(&mut self, value: u8) {
        self.align();
        self.bytes.extend_from_slice(&[0, 0, 1, value]);
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}

// ----------------------------------------------------------------------------
// Stream description
// ----------------------------------------------------------------------------

/// `picture_structure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structure {
    Frame,
    Top,
    Bottom,
}

impl Structure {
    fn code(self) -> u32 {
        match self {
            Structure::Top => 1,
            Structure::Bottom => 2,
            Structure::Frame => 3,
        }
    }

    /// Frame line parity of a field picture (0 = top), `None` for a frame.
    pub fn parity(self) -> Option<usize> {
        match self {
            Structure::Frame => None,
            Structure::Top => Some(0),
            Structure::Bottom => Some(1),
        }
    }
}

/// One AC coefficient: `run` zeros, then `level`. `escape` forces the escape
/// form even when the table has a code for the pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coef {
    pub run: u8,
    pub level: i32,
    pub escape: bool,
}

impl Coef {
    pub fn vlc(run: u8, level: i32) -> Self {
        Self {
            run,
            level,
            escape: false,
        }
    }

    pub fn esc(run: u8, level: i32) -> Self {
        Self {
            run,
            level,
            escape: true,
        }
    }
}

/// One intra block: the absolute DC level QF[0][0] (the writer derives the
/// differential from the predictor) and the AC coefficients in scan order.
#[derive(Clone, Debug, Default)]
pub struct Block {
    pub dc: i32,
    pub ac: Vec<Coef>,
}

/// Concealment motion vector of an intra macroblock (`concealment_motion_vectors`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Concealment {
    /// motion_code for horizontal and vertical, -16..=16.
    pub codes: [i32; 2],
    /// motion_residual, written when f_code > 1 and the code is not 0.
    pub residuals: [u32; 2],
    /// motion_vertical_field_select (field pictures only).
    pub field_select: bool,
}

/// One intra macroblock.
#[derive(Clone, Debug, Default)]
pub struct Macroblock {
    /// quantiser_scale_code of an "Intra, Quant" macroblock; it stays in force
    /// for the rest of the slice.
    pub quant: Option<u8>,
    /// dct_type (frame pictures with frame_pred_frame_dct = 0 only).
    pub field_dct: bool,
    /// Number of macroblock_stuffing codes before the address (MPEG-1 only).
    pub stuffing: u8,
    /// Concealment vector; zero codes when `None` and the picture uses them.
    pub mv: Option<Concealment>,
    /// Y0..Y3, Cb, Cr.
    pub blocks: [Block; 6],
    /// What the macroblock tests, for failure messages.
    pub label: String,
}

/// One slice: macroblocks starting at (`row`, `col`) in macroblock units of
/// the coded picture (field rows for a field picture).
#[derive(Clone, Debug)]
pub struct Slice {
    pub row: u32,
    pub col: u32,
    pub qcode: u8,
    pub mbs: Vec<Macroblock>,
}

/// One intra-coded picture and its sequence parameters.
#[derive(Clone, Debug)]
pub struct Picture {
    pub mpeg1: bool,
    pub width: u32,
    pub height: u32,
    /// Intra matrix loaded by the sequence header (natural order).
    pub seq_matrix: Option<[u8; 64]>,
    /// Intra matrix loaded by a quant matrix extension (MPEG-2, natural order).
    pub ext_matrix: Option<[u8; 64]>,
    pub dc_precision: u8,
    pub structure: Structure,
    pub frame_pred_frame_dct: bool,
    pub concealment: bool,
    /// f_code[0][0] and f_code[0][1] (used only with concealment vectors).
    pub f_code: [u8; 2],
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    /// For a field picture: also write the opposite field (mid-grey) so the
    /// frame is complete.
    pub second_field: bool,
    pub slices: Vec<Slice>,
}

impl Picture {
    pub fn mpeg2(width: u32, height: u32) -> Self {
        Self {
            mpeg1: false,
            width,
            height,
            seq_matrix: None,
            ext_matrix: None,
            dc_precision: 0,
            structure: Structure::Frame,
            frame_pred_frame_dct: true,
            concealment: false,
            f_code: [15, 15],
            q_scale_type: false,
            intra_vlc_format: false,
            alternate_scan: false,
            second_field: false,
            slices: Vec::new(),
        }
    }

    pub fn mpeg1(width: u32, height: u32) -> Self {
        Self {
            mpeg1: true,
            ..Self::mpeg2(width, height)
        }
    }

    pub fn mb_width(&self) -> u32 {
        self.width / 16
    }

    /// Macroblock rows of the coded picture (of one field for a field picture).
    pub fn mb_rows(&self) -> u32 {
        match self.structure {
            Structure::Frame => self.height / 16,
            _ => self.height / 32,
        }
    }

    /// Raster position of each scan index.
    pub fn scan(&self) -> [usize; 64] {
        if self.alternate_scan && !self.mpeg1 {
            scan_order(&ALTERNATE_FIGURE)
        } else {
            scan_order(&ZIGZAG_FIGURE)
        }
    }

    /// Intra quantiser matrix in force (natural order).
    pub fn matrix(&self) -> [u8; 64] {
        let ext = if self.mpeg1 { None } else { self.ext_matrix };
        ext.or(self.seq_matrix).unwrap_or(DEFAULT_INTRA_MATRIX)
    }

    /// quantiser_scale for a quantiser_scale_code.
    pub fn qscale(&self, code: u8) -> i32 {
        assert!((1..=31).contains(&code), "quantiser_scale_code {code}");
        if self.mpeg1 {
            i32::from(code)
        } else if self.q_scale_type {
            i32::from(NON_LINEAR_QSCALE[code as usize])
        } else {
            2 * i32::from(code)
        }
    }

    /// DC predictor reset value (2^(7 + intra_dc_precision)), in DC level units.
    pub fn dc_mid(&self) -> i32 {
        1 << (7 + self.precision())
    }

    /// Largest DC level.
    pub fn dc_max(&self) -> i32 {
        (1 << (8 + self.precision())) - 1
    }

    fn precision(&self) -> u32 {
        if self.mpeg1 {
            0
        } else {
            u32::from(self.dc_precision)
        }
    }

    fn table(&self) -> (&'static [(&'static str, u8, u8); 111], &'static str) {
        dct_table(self.intra_vlc_format && !self.mpeg1)
    }

    /// Dequantised magnitude of `level` at raster position `raster` with
    /// `qs`, before saturation (MPEG-1 before oddification).
    pub fn dequant_magnitude(&self, level: i32, raster: usize, qs: i32) -> i64 {
        let w = i64::from(self.matrix()[raster]);
        let level = i64::from(level.unsigned_abs());
        if self.mpeg1 {
            level * i64::from(qs) * w / 8
        } else {
            level * i64::from(qs) * w / 16
        }
    }

    /// Total macroblocks of the coded picture.
    pub fn total_macroblocks(&self) -> u32 {
        self.mb_width() * self.mb_rows()
    }
}

// ----------------------------------------------------------------------------
// Writer
// ----------------------------------------------------------------------------

fn write_matrix(w: &mut BitWriter, matrix: &[u8; 64]) {
    for raster in scan_order(&ZIGZAG_FIGURE) {
        w.put(u32::from(matrix[raster]), 8);
    }
}

/// Encodes the picture as a complete elementary stream ending with a
/// sequence end code.
pub fn encode(p: &Picture) -> Vec<u8> {
    assert!(p.width % 16 == 0 && p.height % 16 == 0);
    assert!(p.structure == Structure::Frame || p.height % 32 == 0);
    let mut w = BitWriter::new();

    // sequence_header()
    w.start_code(0xB3);
    w.put(p.width, 12);
    w.put(p.height, 12);
    w.put(if p.mpeg1 { 1 } else { 2 }, 4); // square pels / 4:3 display
    w.put(3, 4); // 25 Hz
    w.put(if p.mpeg1 { 0x3FFFF } else { 25_000 }, 18);
    w.put(1, 1); // marker
    w.put(112, 10); // vbv_buffer_size_value
    w.put(0, 1); // constrained_parameters_flag
    match &p.seq_matrix {
        Some(m) => {
            w.flag(true);
            write_matrix(&mut w, m);
        }
        None => w.flag(false),
    }
    w.flag(false); // load_non_intra_quantiser_matrix

    if !p.mpeg1 {
        // sequence_extension()
        w.start_code(0xB5);
        w.put(1, 4);
        w.put(0x44, 8); // Main profile @ High level
        w.flag(false); // progressive_sequence
        w.put(1, 2); // 4:2:0
        w.put(0, 2);
        w.put(0, 2);
        w.put(0, 12);
        w.put(1, 1); // marker
        w.put(0, 8);
        w.flag(false); // low_delay
        w.put(0, 2);
        w.put(0, 5);
    }

    // group_of_pictures_header()
    w.start_code(0xB8);
    w.put(0, 1); // drop_frame_flag
    w.put(0, 5);
    w.put(0, 6);
    w.put(1, 1); // marker
    w.put(0, 6);
    w.put(0, 6);
    w.flag(true); // closed_gop
    w.flag(false); // broken_link

    write_picture(&mut w, p, p.structure, &p.slices);
    if p.second_field {
        let other = match p.structure {
            Structure::Top => Structure::Bottom,
            Structure::Bottom => Structure::Top,
            Structure::Frame => panic!("second_field on a frame picture"),
        };
        let grey = grey_slices(p);
        write_picture(&mut w, p, other, &grey);
    }
    w.start_code(0xB7);
    w.into_bytes()
}

/// One DC-only mid-grey slice per macroblock row.
fn grey_slices(p: &Picture) -> Vec<Slice> {
    let mid = p.dc_mid();
    (0..p.mb_rows())
        .map(|row| Slice {
            row,
            col: 0,
            qcode: 1,
            mbs: (0..p.mb_width())
                .map(|_| Macroblock {
                    blocks: core::array::from_fn(|_| Block {
                        dc: mid,
                        ac: Vec::new(),
                    }),
                    ..Macroblock::default()
                })
                .collect(),
        })
        .collect()
}

fn write_picture(w: &mut BitWriter, p: &Picture, structure: Structure, slices: &[Slice]) {
    // picture_header()
    w.start_code(0x00);
    w.put(0, 10); // temporal_reference
    w.put(1, 3); // I
    w.put(0xFFFF, 16); // vbv_delay
    w.flag(false); // extra_bit_picture

    if !p.mpeg1 {
        // picture_coding_extension()
        w.start_code(0xB5);
        w.put(8, 4);
        let f = if p.concealment { p.f_code } else { [15, 15] };
        assert!(!p.concealment || f.iter().all(|f| (1..=9).contains(f)));
        w.put(u32::from(f[0]), 4);
        w.put(u32::from(f[1]), 4);
        w.put(15, 4);
        w.put(15, 4);
        assert!(p.dc_precision <= 3);
        w.put(u32::from(p.dc_precision), 2);
        w.put(structure.code(), 2);
        w.flag(structure == Structure::Frame); // top_field_first (0 in field pictures)
        w.flag(structure == Structure::Frame && p.frame_pred_frame_dct);
        w.flag(p.concealment);
        w.flag(p.q_scale_type);
        w.flag(p.intra_vlc_format);
        w.flag(p.alternate_scan);
        w.flag(false); // repeat_first_field
        w.flag(false); // chroma_420_type
        w.flag(false); // progressive_frame
        w.flag(false); // composite_display_flag

        if let Some(m) = &p.ext_matrix {
            // quant_matrix_extension()
            w.start_code(0xB5);
            w.put(3, 4);
            w.flag(true);
            write_matrix(w, m);
            w.flag(false);
            w.flag(false);
            w.flag(false);
        }
    }

    for slice in slices {
        write_slice(w, p, structure, slice);
    }
}

fn write_slice(w: &mut BitWriter, p: &Picture, structure: Structure, slice: &Slice) {
    assert!(slice.row < 175 && slice.row < p.mb_rows());
    assert!(slice.col < p.mb_width());
    let end = (slice.row * p.mb_width() + slice.col) as usize + slice.mbs.len();
    assert!(
        end <= p.total_macroblocks() as usize,
        "slice runs past the picture"
    );
    w.start_code(slice.row as u8 + 1);
    w.put(u32::from(slice.qcode), 5);
    w.flag(false); // extra_bit_slice (no intra_slice_flag)
    let mut pred = [p.dc_mid(); 3];
    let (table, eob) = p.table();
    for (i, mb) in slice.mbs.iter().enumerate() {
        if p.mpeg1 {
            for _ in 0..mb.stuffing {
                w.code(MBA_STUFFING);
            }
        } else {
            assert_eq!(mb.stuffing, 0, "macroblock_stuffing is MPEG-1 only");
        }
        // The first increment counts from the macroblock before the row start.
        let mut increment = if i == 0 { slice.col + 1 } else { 1 };
        while increment > 33 {
            w.code(MBA_ESCAPE);
            increment -= 33;
        }
        w.code(MBA_INCREMENT[increment as usize - 1]);
        w.code(if mb.quant.is_some() {
            MB_TYPE_INTRA_QUANT
        } else {
            MB_TYPE_INTRA
        });
        let dct_type_present = !p.mpeg1 && structure == Structure::Frame && !p.frame_pred_frame_dct;
        if dct_type_present {
            w.flag(mb.field_dct);
        } else {
            assert!(!mb.field_dct, "dct_type not present in this picture");
        }
        if let Some(code) = mb.quant {
            assert!((1..=31).contains(&code));
            w.put(u32::from(code), 5);
        }
        if p.concealment && !p.mpeg1 {
            let mv = mb.mv.unwrap_or_default();
            if structure != Structure::Frame {
                w.flag(mv.field_select);
            }
            for t in 0..2 {
                let code = mv.codes[t];
                assert!((-16..=16).contains(&code));
                w.code(MOTION_CODE[(code + 16) as usize]);
                let f = u32::from(p.f_code[t]);
                if f != 1 && code != 0 {
                    w.put(mv.residuals[t], f - 1);
                } else {
                    assert_eq!(mv.residuals[t], 0, "residual without motion_residual");
                }
            }
            w.put(1, 1); // marker_bit
        }
        for (b, block) in mb.blocks.iter().enumerate() {
            let comp = if b < 4 { 0 } else { b - 3 };
            write_block(w, p, comp, block, &mut pred[comp], table, eob);
        }
    }
}

fn write_block(
    w: &mut BitWriter,
    p: &Picture,
    comp: usize,
    block: &Block,
    pred: &mut i32,
    table: &[(&'static str, u8, u8)],
    eob: &str,
) {
    assert!(
        (0..=p.dc_max()).contains(&block.dc),
        "DC {} out of range",
        block.dc
    );
    let diff = block.dc - *pred;
    *pred = block.dc;
    let size = 32 - diff.unsigned_abs().leading_zeros();
    w.code(if comp == 0 {
        DC_SIZE_LUMA[size as usize]
    } else {
        DC_SIZE_CHROMA[size as usize]
    });
    if size > 0 {
        let bits = if diff > 0 {
            diff
        } else {
            diff + (1 << size) - 1
        };
        w.put(bits as u32, size);
    }
    let mut pos = 0u32;
    for c in &block.ac {
        pos += u32::from(c.run) + 1;
        assert!(pos <= 63, "coefficient past the end of the block");
        assert!(c.level != 0);
        match lookup(table, c.run, c.level).filter(|_| !c.escape) {
            Some(code) => {
                w.code(code);
                w.flag(c.level < 0);
            }
            None => write_escape(w, p.mpeg1, *c),
        }
    }
    w.code(eob);
}

fn write_escape(w: &mut BitWriter, mpeg1: bool, c: Coef) {
    w.code(DCT_ESCAPE);
    w.put(u32::from(c.run), 6);
    if mpeg1 {
        // 11172-2 2.4.3.7: 8-bit two's complement for -127..=127, else a
        // 0x00 (positive) or 0x80 (negative) marker and 8 more bits.
        match c.level {
            -127..=127 => w.put((c.level & 0xFF) as u32, 8),
            128..=255 => {
                w.put(0, 8);
                w.put(c.level as u32, 8);
            }
            -255..=-128 => {
                w.put(0x80, 8);
                w.put((c.level + 256) as u32, 8);
            }
            _ => panic!("MPEG-1 level {} out of range", c.level),
        }
    } else {
        // 13818-2 Table B.16: 12-bit two's complement, -2048 forbidden.
        assert!((-2047..=2047).contains(&c.level), "level {}", c.level);
        w.put((c.level & 0xFFF) as u32, 12);
    }
}

// ----------------------------------------------------------------------------
// Reference reconstruction
// ----------------------------------------------------------------------------

/// Decoded 4:2:0 planes of a whole frame. For a field picture only the rows of
/// that field's parity are written (the others stay 0).
#[derive(Clone, Debug)]
pub struct Planes {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
}

impl Planes {
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        Self {
            width,
            height,
            y: vec![0; width * height],
            cb: vec![0; cw * ch],
            cr: vec![0; cw * ch],
        }
    }

    /// (plane, width, height) for plane index 0 = Y, 1 = Cb, 2 = Cr.
    pub fn plane(&self, index: usize) -> (&[u8], usize, usize) {
        match index {
            0 => (&self.y, self.width, self.height),
            1 => (&self.cb, self.width.div_ceil(2), self.height.div_ceil(2)),
            _ => (&self.cr, self.width.div_ceil(2), self.height.div_ceil(2)),
        }
    }
}

/// Basis functions of the 8-point IDCT: `[x][u]` = C(u) / 2 · cos((2x + 1)uπ / 16).
pub fn idct_basis() -> [[f64; 8]; 8] {
    let mut c = [[0.0; 8]; 8];
    for (x, row) in c.iter_mut().enumerate() {
        for (u, value) in row.iter_mut().enumerate() {
            let cu = if u == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            *value =
                cu / 2.0 * ((2 * x + 1) as f64 * u as f64 * core::f64::consts::PI / 16.0).cos();
        }
    }
    c
}

/// Inverse quantisation of one intra block (13818-2 7.4 / 11172-2 2.4.4.1),
/// result in natural order.
pub fn dequantise(p: &Picture, block: &Block, qs: i32) -> [i32; 64] {
    let scan = p.scan();
    let matrix = p.matrix();
    let mut f = [0i32; 64];
    if p.mpeg1 {
        f[0] = block.dc * 8;
        let mut pos = 0usize;
        for c in &block.ac {
            pos += usize::from(c.run) + 1;
            let raster = scan[pos];
            let mut v = 2 * c.level * qs * i32::from(matrix[raster]) / 16;
            if v & 1 == 0 {
                v -= v.signum();
            }
            f[raster] = v.clamp(-2048, 2047);
        }
    } else {
        f[0] = block.dc * (8 >> p.dc_precision);
        let mut pos = 0usize;
        for c in &block.ac {
            pos += usize::from(c.run) + 1;
            let raster = scan[pos];
            // (2 · QF · W · quantiser_scale) / 32 with k = 0, truncating toward zero.
            let v = 2 * i64::from(c.level) * i64::from(matrix[raster]) * i64::from(qs) / 32;
            f[raster] = v.clamp(-2048, 2047) as i32;
        }
        for v in f.iter_mut() {
            *v = (*v).clamp(-2048, 2047);
        }
        // Mismatch control: the sum of all coefficients must be odd.
        let sum: i32 = f.iter().sum();
        if sum & 1 == 0 {
            if f[63] & 1 != 0 {
                f[63] -= 1;
            } else {
                f[63] += 1;
            }
        }
    }
    f
}

/// Exact (double precision) IDCT, row-major, before rounding.
pub fn idct_exact(f: &[i32; 64], basis: &[[f64; 8]; 8]) -> [f64; 64] {
    let mut tmp = [[0.0f64; 8]; 8]; // [y][u]
    for (y, row) in tmp.iter_mut().enumerate() {
        for (u, value) in row.iter_mut().enumerate() {
            *value = (0..8).map(|v| basis[y][v] * f64::from(f[v * 8 + u])).sum();
        }
    }
    let mut out = [0.0f64; 64];
    for (y, row) in tmp.iter().enumerate() {
        for x in 0..8 {
            out[y * 8 + x] = (0..8).map(|u| basis[x][u] * row[u]).sum();
        }
    }
    out
}

/// IDCT rounded to the nearest integer and clamped to 0..=255 (intra blocks
/// have no prediction to add). Output is row-major.
pub fn idct(f: &[i32; 64], basis: &[[f64; 8]; 8]) -> [u8; 64] {
    idct_exact(f, basis).map(|s| s.round().clamp(0.0, 255.0) as u8)
}

/// Reconstructs the picture's first (or only) field or frame.
pub fn reconstruct(p: &Picture) -> Planes {
    let mut out = Planes::new(p.width as usize, p.height as usize);
    let basis = idct_basis();
    let mbw = p.mb_width() as usize;
    for slice in &p.slices {
        let mut qcode = slice.qcode;
        let mut addr = slice.row as usize * mbw + slice.col as usize;
        for mb in &slice.mbs {
            if let Some(code) = mb.quant {
                qcode = code;
            }
            let qs = p.qscale(qcode);
            let (mbx, mby) = (addr % mbw, addr / mbw);
            for (b, block) in mb.blocks.iter().enumerate() {
                let samples = idct(&dequantise(p, block, qs), &basis);
                place(p, &mut out, b, mbx, mby, mb.field_dct, &samples);
            }
            addr += 1;
        }
    }
    out
}

/// Writes one 8x8 block into the frame (13818-2 6.1.3, Figures 6-13 / 6-14).
fn place(
    p: &Picture,
    out: &mut Planes,
    b: usize,
    mbx: usize,
    mby: usize,
    field_dct: bool,
    s: &[u8; 64],
) {
    let (plane, stride, x0, y0, step) = if b < 4 {
        let x0 = mbx * 16 + (b & 1) * 8;
        let (y0, step) = if field_dct {
            (mby * 16 + (b >> 1), 2)
        } else {
            (mby * 16 + (b >> 1) * 8, 1)
        };
        (&mut out.y, out.width, x0, y0, step)
    } else {
        let stride = out.width.div_ceil(2);
        let plane = if b == 4 { &mut out.cb } else { &mut out.cr };
        (plane, stride, mbx * 8, mby * 8, 1)
    };
    for r in 0..8 {
        // Row inside the coded picture, then inside the frame.
        let coded = y0 + r * step;
        let row = match p.structure.parity() {
            None => coded,
            Some(parity) => coded * 2 + parity,
        };
        let start = row * stride + x0;
        plane[start..start + 8].copy_from_slice(&s[r * 8..r * 8 + 8]);
    }
}

// ----------------------------------------------------------------------------
// Test data helpers
// ----------------------------------------------------------------------------

/// Small deterministic generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        assert!(lo <= hi);
        let span = (i64::from(hi) - i64::from(lo) + 1) as u64;
        (i64::from(lo) + (self.next_u64() >> 11).rem_euclid(span) as i64) as i32
    }

    pub fn coin(&mut self) -> bool {
        self.next_u64() >> 63 == 1
    }

    /// True with probability 1/n.
    pub fn one_in(&mut self, n: u32) -> bool {
        self.range(1, n as i32) == 1
    }
}

/// Largest |level| whose dequantised value at `raster` stays inside the
/// saturation range (so no decoder has to saturate).
pub fn max_level(p: &Picture, raster: usize, qs: i32) -> i32 {
    let limit = if p.mpeg1 { 255 } else { 2047 };
    let mut hi = limit;
    while hi > 1 && p.dequant_magnitude(hi, raster, qs) > 2047 {
        hi = (hi * 3 / 4).max(1);
    }
    while hi < limit && p.dequant_magnitude(hi + 1, raster, qs) <= 2047 {
        hi += 1;
    }
    hi
}

/// A random block: DC anywhere in range, `count` AC coefficients whose
/// dequantised values never saturate, a mix of table codes and escapes.
pub fn random_block(rng: &mut Rng, p: &Picture, qs: i32, count: usize) -> Block {
    let scan = p.scan();
    let (table, _) = p.table();
    let mid = p.dc_mid();
    let dc = if rng.one_in(4) {
        rng.range(0, p.dc_max())
    } else {
        (mid + rng.range(-mid / 3, mid / 3)).clamp(0, p.dc_max())
    };
    let mut ac = Vec::with_capacity(count);
    let mut pos = 0usize;
    for _ in 0..count {
        let run = if rng.one_in(3) {
            rng.range(0, 20)
        } else {
            rng.range(0, 3)
        } as usize;
        if pos + run + 1 > 63 {
            break;
        }
        pos += run + 1;
        let top = max_level(p, scan[pos], qs);
        // Mostly small levels so most samples stay inside 0..=255; now and
        // then anything up to the saturation limit.
        let magnitude = if rng.one_in(5) {
            rng.range(1, top)
        } else {
            rng.range(1, top.min(6))
        };
        let level = if rng.coin() { magnitude } else { -magnitude };
        let escape = lookup(table, run as u8, level).is_none() || rng.one_in(10);
        ac.push(Coef {
            run: run as u8,
            level,
            escape,
        });
    }
    let mut block = Block { dc, ac };
    // Real encoders never produce blocks whose samples land far outside
    // 0..=255, and fixed-point IDCTs (ffmpeg's included) wrap around on them
    // instead of saturating. Drop coefficients until the exact output stays
    // within a margin of the sample range; the clamp is still exercised.
    let basis = idct_basis();
    while !block.ac.is_empty() {
        let out = idct_exact(&dequantise(p, &block, qs), &basis);
        if out
            .iter()
            .all(|s| (-SAMPLE_MARGIN..=255.0 + SAMPLE_MARGIN).contains(s))
        {
            break;
        }
        let largest = (0..block.ac.len())
            .max_by_key(|&i| block.ac[i].level.abs())
            .unwrap();
        // Removing a coefficient moves the next one's position: fold its run
        // into the successor so the others keep their positions.
        let removed = block.ac.remove(largest);
        if let Some(next) = block.ac.get_mut(largest) {
            next.run += removed.run + 1;
            let (table, _) = p.table();
            next.escape |= lookup(table, next.run, next.level).is_none();
        }
    }
    block
}

/// How far outside 0..=255 the unclamped samples of a random block may go.
pub const SAMPLE_MARGIN: f64 = 96.0;
