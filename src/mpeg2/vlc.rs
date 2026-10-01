//! Variable length code tables of ISO/IEC 13818-2 Annex B (shared with
//! ISO/IEC 11172-2) and their lookup tables.
//!
//! Each table is written down once as a list of `(code, length, symbol)`
//! entries, straight from the standard, and turned into a two-level lookup
//! table on first use. Sign bits that follow a code (motion codes, DCT
//! coefficients) are not part of the listed codes; the decoder reads them.

// Binary codes are grouped in fours from the left, as the standard prints
// them, so they can be compared with the tables at a glance.
#![allow(clippy::unusual_byte_groupings)]

use std::sync::OnceLock;

/// One codeword: the code right-aligned in `.0`, its length in bits, and the
/// symbol it stands for.
pub type Code = (u32, u8, i16);

/// `macroblock_escape` in the address increment table: adds 33.
pub const MBA_ESCAPE: i16 = 0x100;
/// `macroblock_stuffing` (MPEG-1 only): ignored.
pub const MBA_STUFFING: i16 = 0x101;

/// End of block in the DCT coefficient tables.
pub const DCT_EOB: i16 = -1;
/// Escape in the DCT coefficient tables: run and level follow as fixed-length
/// fields.
pub const DCT_ESCAPE: i16 = -2;

/// Symbol of a lookup entry that is no valid code.
const INVALID: i16 = i16::MIN;

/// Table B.1, `macroblock_address_increment` (symbols 1..=33), plus escape
/// and stuffing.
pub const MACROBLOCK_ADDRESS_INCREMENT: &[Code] = &[
    (0b1, 1, 1),
    (0b011, 3, 2),
    (0b010, 3, 3),
    (0b0011, 4, 4),
    (0b0010, 4, 5),
    (0b0001_1, 5, 6),
    (0b0001_0, 5, 7),
    (0b0000_111, 7, 8),
    (0b0000_110, 7, 9),
    (0b0000_1011, 8, 10),
    (0b0000_1010, 8, 11),
    (0b0000_1001, 8, 12),
    (0b0000_1000, 8, 13),
    (0b0000_0111, 8, 14),
    (0b0000_0110, 8, 15),
    (0b0000_0101_11, 10, 16),
    (0b0000_0101_10, 10, 17),
    (0b0000_0101_01, 10, 18),
    (0b0000_0101_00, 10, 19),
    (0b0000_0100_11, 10, 20),
    (0b0000_0100_10, 10, 21),
    (0b0000_0100_011, 11, 22),
    (0b0000_0100_010, 11, 23),
    (0b0000_0100_001, 11, 24),
    (0b0000_0100_000, 11, 25),
    (0b0000_0011_111, 11, 26),
    (0b0000_0011_110, 11, 27),
    (0b0000_0011_101, 11, 28),
    (0b0000_0011_100, 11, 29),
    (0b0000_0011_011, 11, 30),
    (0b0000_0011_010, 11, 31),
    (0b0000_0011_001, 11, 32),
    (0b0000_0011_000, 11, 33),
    (0b0000_0001_000, 11, MBA_ESCAPE),
    (0b0000_0001_111, 11, MBA_STUFFING),
];

/// Table B.10, `motion_code` magnitude (0..=16); a sign bit follows every
/// non-zero code.
pub const MOTION_CODE: &[Code] = &[
    (0b1, 1, 0),
    (0b01, 2, 1),
    (0b001, 3, 2),
    (0b0001, 4, 3),
    (0b0000_11, 6, 4),
    (0b0000_101, 7, 5),
    (0b0000_100, 7, 6),
    (0b0000_011, 7, 7),
    (0b0000_0101_1, 9, 8),
    (0b0000_0101_0, 9, 9),
    (0b0000_0100_1, 9, 10),
    (0b0000_0100_01, 10, 11),
    (0b0000_0100_00, 10, 12),
    (0b0000_0011_11, 10, 13),
    (0b0000_0011_10, 10, 14),
    (0b0000_0011_01, 10, 15),
    (0b0000_0011_00, 10, 16),
];

/// Table B.12, `dct_dc_size_luminance`.
pub const DCT_DC_SIZE_LUMINANCE: &[Code] = &[
    (0b100, 3, 0),
    (0b00, 2, 1),
    (0b01, 2, 2),
    (0b101, 3, 3),
    (0b110, 3, 4),
    (0b1110, 4, 5),
    (0b1111_0, 5, 6),
    (0b1111_10, 6, 7),
    (0b1111_110, 7, 8),
    (0b1111_1110, 8, 9),
    (0b1111_1111_0, 9, 10),
    (0b1111_1111_1, 9, 11),
];

/// Table B.13, `dct_dc_size_chrominance`.
pub const DCT_DC_SIZE_CHROMINANCE: &[Code] = &[
    (0b00, 2, 0),
    (0b01, 2, 1),
    (0b10, 2, 2),
    (0b110, 3, 3),
    (0b1110, 4, 4),
    (0b1111_0, 5, 5),
    (0b1111_10, 6, 6),
    (0b1111_110, 7, 7),
    (0b1111_1110, 8, 8),
    (0b1111_1111_0, 9, 9),
    (0b1111_1111_10, 10, 10),
    (0b1111_1111_11, 10, 11),
];

/// Highest level coded with a VLC for each run (index) of Tables B.14/B.15.
/// The `(run, level)` pairs in this order line up with `B14` and `B15`.
#[rustfmt::skip]
const MAX_LEVEL: [u8; 32] = [
    40, 18, 5, 4, 3, 3, 3,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

/// Table B.14 (DCT coefficients table zero), `(code, length)` for each
/// `(run, level)` in `MAX_LEVEL` order. Run 0 / level 1 is the `11` form
/// used for all but the first coefficient of a non-intra block.
#[rustfmt::skip]
const B14: [(u16, u8); 111] = [
    // run 0, levels 1..=40
    (0x3, 2), (0x4, 4), (0x5, 5), (0x6, 7), (0x26, 8), (0x21, 8), (0xa, 10), (0x1d, 12),
    (0x18, 12), (0x13, 12), (0x10, 12), (0x1a, 13), (0x19, 13), (0x18, 13), (0x17, 13), (0x1f, 14),
    (0x1e, 14), (0x1d, 14), (0x1c, 14), (0x1b, 14), (0x1a, 14), (0x19, 14), (0x18, 14), (0x17, 14),
    (0x16, 14), (0x15, 14), (0x14, 14), (0x13, 14), (0x12, 14), (0x11, 14), (0x10, 14), (0x18, 15),
    (0x17, 15), (0x16, 15), (0x15, 15), (0x14, 15), (0x13, 15), (0x12, 15), (0x11, 15), (0x10, 15),
    // run 1, levels 1..=18
    (0x3, 3), (0x6, 6), (0x25, 8), (0xc, 10), (0x1b, 12), (0x16, 13), (0x15, 13), (0x1f, 15),
    (0x1e, 15), (0x1d, 15), (0x1c, 15), (0x1b, 15), (0x1a, 15), (0x19, 15), (0x13, 16), (0x12, 16),
    (0x11, 16), (0x10, 16),
    // run 2, levels 1..=5
    (0x5, 4), (0x4, 7), (0xb, 10), (0x14, 12), (0x14, 13),
    // run 3, levels 1..=4
    (0x7, 5), (0x24, 8), (0x1c, 12), (0x13, 13),
    // run 4..=6, levels 1..=3
    (0x6, 5), (0xf, 10), (0x12, 12),
    (0x7, 6), (0x9, 10), (0x12, 13),
    (0x5, 6), (0x1e, 12), (0x14, 16),
    // run 7..=16, levels 1..=2
    (0x4, 6), (0x15, 12),
    (0x7, 7), (0x11, 12),
    (0x5, 7), (0x11, 13),
    (0x27, 8), (0x10, 13),
    (0x23, 8), (0x1a, 16),
    (0x22, 8), (0x19, 16),
    (0x20, 8), (0x18, 16),
    (0xe, 10), (0x17, 16),
    (0xd, 10), (0x16, 16),
    (0x8, 10), (0x15, 16),
    // run 17..=31, level 1
    (0x1f, 12), (0x1a, 12), (0x19, 12), (0x17, 12), (0x16, 12),
    (0x1f, 13), (0x1e, 13), (0x1d, 13), (0x1c, 13), (0x1b, 13),
    (0x1f, 16), (0x1e, 16), (0x1d, 16), (0x1c, 16), (0x1b, 16),
];

/// Table B.15 (DCT coefficients table one, `intra_vlc_format` = 1), same
/// layout as `B14`.
#[rustfmt::skip]
const B15: [(u16, u8); 111] = [
    // run 0, levels 1..=40
    (0x2, 2), (0x6, 3), (0x7, 4), (0x1c, 5), (0x1d, 5), (0x5, 6), (0x4, 6), (0x7b, 7),
    (0x7c, 7), (0x23, 8), (0x22, 8), (0xfa, 8), (0xfb, 8), (0xfe, 8), (0xff, 8), (0x1f, 14),
    (0x1e, 14), (0x1d, 14), (0x1c, 14), (0x1b, 14), (0x1a, 14), (0x19, 14), (0x18, 14), (0x17, 14),
    (0x16, 14), (0x15, 14), (0x14, 14), (0x13, 14), (0x12, 14), (0x11, 14), (0x10, 14), (0x18, 15),
    (0x17, 15), (0x16, 15), (0x15, 15), (0x14, 15), (0x13, 15), (0x12, 15), (0x11, 15), (0x10, 15),
    // run 1, levels 1..=18
    (0x2, 3), (0x6, 5), (0x79, 7), (0x27, 8), (0x20, 8), (0x16, 13), (0x15, 13), (0x1f, 15),
    (0x1e, 15), (0x1d, 15), (0x1c, 15), (0x1b, 15), (0x1a, 15), (0x19, 15), (0x13, 16), (0x12, 16),
    (0x11, 16), (0x10, 16),
    // run 2, levels 1..=5
    (0x5, 5), (0x7, 7), (0xfc, 8), (0xc, 10), (0x14, 13),
    // run 3, levels 1..=4
    (0x7, 5), (0x26, 8), (0x1c, 12), (0x13, 13),
    // run 4..=6, levels 1..=3
    (0x6, 6), (0xfd, 8), (0x12, 12),
    (0x7, 6), (0x4, 9), (0x12, 13),
    (0x6, 7), (0x1e, 12), (0x14, 16),
    // run 7..=16, levels 1..=2
    (0x4, 7), (0x15, 12),
    (0x5, 7), (0x11, 12),
    (0x78, 7), (0x11, 13),
    (0x7a, 7), (0x10, 13),
    (0x21, 8), (0x1a, 16),
    (0x25, 8), (0x19, 16),
    (0x24, 8), (0x18, 16),
    (0x5, 9), (0x17, 16),
    (0x7, 9), (0x16, 16),
    (0xd, 10), (0x15, 16),
    // run 17..=31, level 1
    (0x1f, 12), (0x1a, 12), (0x19, 12), (0x17, 12), (0x16, 12),
    (0x1f, 13), (0x1e, 13), (0x1d, 13), (0x1c, 13), (0x1b, 13),
    (0x1f, 16), (0x1e, 16), (0x1d, 16), (0x1c, 16), (0x1b, 16),
];

/// Symbol of a `(run, level)` pair in the DCT tables: `run << 8 | level`.
#[inline]
pub const fn run_level(run: u8, level: u8) -> i16 {
    ((run as i16) << 8) | level as i16
}

/// The codewords of Table B.14 (`one` = false) or Table B.15 (`one` = true),
/// including end of block and escape.
pub fn dct_codes(one: bool) -> Vec<Code> {
    let src = if one { &B15 } else { &B14 };
    let mut out = Vec::with_capacity(src.len() + 2);
    let mut i = 0;
    for (run, &max) in MAX_LEVEL.iter().enumerate() {
        for level in 1..=max {
            let (code, len) = src[i];
            out.push((u32::from(code), len, run_level(run as u8, level)));
            i += 1;
        }
    }
    out.push(if one {
        (0b0110, 4, DCT_EOB)
    } else {
        (0b10, 2, DCT_EOB)
    });
    out.push((0b0000_01, 6, DCT_ESCAPE));
    out
}

/// One lookup table slot. In the first level, `sub` != 0 marks a pointer to a
/// second-level table of `2^sub` slots starting at index `sym`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub sym: i16,
    /// Code length in bits; 0 for an invalid code.
    pub len: u8,
    sub: u8,
}

const INVALID_ENTRY: Entry = Entry {
    sym: INVALID,
    len: 0,
    sub: 0,
};

/// Two-level lookup table for one VLC.
pub struct Vlc {
    /// Bits indexing the first level.
    bits: u32,
    table: Vec<Entry>,
}

impl Vlc {
    /// Builds the lookup table. `bits` is the first-level width; longer codes
    /// go to second-level tables keyed by their first `bits` bits.
    pub fn new(codes: &[Code], bits: u32) -> Vlc {
        let mut table = vec![INVALID_ENTRY; 1 << bits];
        for &(code, len, sym) in codes {
            let len32 = u32::from(len);
            if len32 <= bits {
                let shift = bits - len32;
                let base = (code << shift) as usize;
                table[base..base + (1 << shift)].fill(Entry { sym, len, sub: 0 });
            }
        }
        // Second level: one table per first-level prefix, as wide as the
        // longest code sharing that prefix.
        for &(code, len, _) in codes {
            let len32 = u32::from(len);
            if len32 > bits {
                let prefix = (code >> (len32 - bits)) as usize;
                let extra = (len32 - bits) as u8;
                let slot = &mut table[prefix];
                if slot.sub < extra {
                    debug_assert!(slot.len == 0, "code prefix collides with a shorter code");
                    slot.sub = extra;
                }
            }
        }
        for prefix in 0..(1usize << bits) {
            let sub = table[prefix].sub;
            if sub != 0 {
                let offset = table.len();
                table[prefix].sym = offset as i16;
                table.resize(offset + (1 << sub), INVALID_ENTRY);
            }
        }
        for &(code, len, sym) in codes {
            let len32 = u32::from(len);
            if len32 > bits {
                let prefix = (code >> (len32 - bits)) as usize;
                let first = table[prefix];
                let rest_bits = len32 - bits;
                let rest = (code & ((1 << rest_bits) - 1)) as usize;
                let shift = u32::from(first.sub) - rest_bits;
                let base = first.sym as usize + (rest << shift);
                table[base..base + (1 << shift)].fill(Entry { sym, len, sub: 0 });
            }
        }
        Vlc { bits, table }
    }

    /// Looks up the code at the top of `bits` (the next 32 bits of the
    /// stream, MSB first). Returns an entry with `len` = 0 for an invalid code.
    #[inline(always)]
    pub fn decode(&self, bits: u32) -> Entry {
        let first = self.table[(bits >> (32 - self.bits)) as usize];
        if first.sub == 0 {
            return first;
        }
        let index =
            first.sym as usize + ((bits << self.bits) >> (32 - u32::from(first.sub))) as usize;
        self.table.get(index).copied().unwrap_or(INVALID_ENTRY)
    }
}

/// All lookup tables the decoder uses.
pub struct Tables {
    pub macroblock_address_increment: Vlc,
    pub motion_code: Vlc,
    pub dc_luminance: Vlc,
    pub dc_chrominance: Vlc,
    /// Table B.14.
    pub dct_zero: Vlc,
    /// Table B.15.
    pub dct_one: Vlc,
}

/// The lookup tables, built on first use.
pub fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables {
        macroblock_address_increment: Vlc::new(MACROBLOCK_ADDRESS_INCREMENT, 11),
        motion_code: Vlc::new(MOTION_CODE, 10),
        dc_luminance: Vlc::new(DCT_DC_SIZE_LUMINANCE, 9),
        dc_chrominance: Vlc::new(DCT_DC_SIZE_CHROMINANCE, 10),
        dct_zero: Vlc::new(&dct_codes(false), 10),
        dct_one: Vlc::new(&dct_codes(true), 10),
    })
}
