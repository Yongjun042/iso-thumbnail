//! Slice, macroblock and block layers of an intra picture (ISO/IEC 13818-2
//! §6.2.4–6.2.6, §7.2–7.5; ISO/IEC 11172-2 §2.4.2.7–2.4.4).
//!
//! Every slice is decoded independently. A slice that turns out corrupt is
//! abandoned at the failing macroblock; macroblocks no slice decoded in full
//! are filled with mid-grey when the picture is finished.

use super::bits::BitReader;
use super::headers::{PictureCoding, ALTERNATE, FRAME_PICTURE, ZIGZAG};
use super::idct::Idct;
use super::vlc::{Tables, Vlc, DCT_EOB, DCT_ESCAPE, MBA_ESCAPE, MBA_STUFFING};

/// `quantiser_scale` for `q_scale_type` = 1 (Table 7-6), by
/// `quantiser_scale_code`.
#[rustfmt::skip]
const NON_LINEAR_QUANTISER_SCALE: [u8; 32] = [
     0,  1,  2,  3,  4,  5,  6,  7,  8, 10, 12, 14, 16, 18, 20, 22,
    24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 104, 112,
];

/// What the picture decoder needs to know about the picture.
pub struct Params {
    pub mpeg1: bool,
    pub mb_width: u32,
    /// Macroblock rows of the picture (of the field for a field picture).
    pub mb_rows: u32,
    pub coding: PictureCoding,
    /// Intra quantiser matrix, raster order.
    pub intra_matrix: [u8; 64],
}

/// The slice data does not follow the syntax (or ran out): the rest of the
/// slice is lost.
struct Corrupt;

type Step<T> = core::result::Result<T, Corrupt>;

/// A decoded picture before cropping: planes padded to whole macroblocks.
pub struct Planes {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub y_stride: usize,
    pub c_stride: usize,
    /// Luma rows in `y` (chroma has half as many).
    pub rows: usize,
    pub decoded: u32,
    pub concealed: u32,
    pub total: u32,
}

pub struct PictureDecoder {
    tables: &'static Tables,
    idct: Idct,
    dct: &'static Vlc,
    scan: &'static [u8; 64],
    matrix: [i32; 64],
    mpeg1: bool,
    mb_width: usize,
    total: usize,
    frame_picture: bool,
    /// `dct_type` is coded per macroblock (frame picture, field/frame DCT
    /// chosen per macroblock).
    dct_type_coded: bool,
    concealment_motion_vectors: bool,
    /// Bits of `motion_residual` for the horizontal and vertical component.
    residual_bits: [u32; 2],
    q_scale_type: bool,
    dc_reset: i32,
    dc_mult: i32,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
    y_stride: usize,
    c_stride: usize,
    /// Macroblocks decoded in full.
    done: Vec<bool>,
    coef: [i32; 64],
    dc_pred: [i32; 3],
}

impl PictureDecoder {
    pub fn new(tables: &'static Tables, p: &Params) -> PictureDecoder {
        let c = &p.coding;
        let mb_width = p.mb_width as usize;
        let mb_rows = p.mb_rows as usize;
        let y_stride = mb_width * 16;
        let c_stride = mb_width * 8;
        let precision = if p.mpeg1 { 0 } else { c.intra_dc_precision & 3 };
        let frame_picture = c.picture_structure == FRAME_PICTURE;
        PictureDecoder {
            tables,
            idct: Idct::new(),
            dct: if c.intra_vlc_format && !p.mpeg1 {
                &tables.dct_one
            } else {
                &tables.dct_zero
            },
            scan: if c.alternate_scan && !p.mpeg1 {
                &ALTERNATE
            } else {
                &ZIGZAG
            },
            matrix: p.intra_matrix.map(i32::from),
            mpeg1: p.mpeg1,
            mb_width,
            total: mb_width * mb_rows,
            frame_picture,
            dct_type_coded: !p.mpeg1 && frame_picture && !c.frame_pred_frame_dct,
            concealment_motion_vectors: !p.mpeg1 && c.concealment_motion_vectors,
            residual_bits: [
                u32::from(c.f_code[0][0].saturating_sub(1)),
                u32::from(c.f_code[0][1].saturating_sub(1)),
            ],
            q_scale_type: c.q_scale_type && !p.mpeg1,
            dc_reset: 1 << (7 + precision),
            dc_mult: 8 >> precision,
            // Mid-grey, so macroblocks no slice reaches are concealed already.
            y: vec![128; y_stride * mb_rows * 16],
            cb: vec![128; c_stride * mb_rows * 8],
            cr: vec![128; c_stride * mb_rows * 8],
            y_stride,
            c_stride,
            done: vec![false; mb_width * mb_rows],
            coef: [0; 64],
            dc_pred: [0; 3],
        }
    }

    /// Decodes one slice: `start_code` is the slice start code value
    /// (`slice_vertical_position`), `data` the bytes up to the next start code.
    pub fn decode_slice(&mut self, start_code: u8, data: &[u8]) {
        let row = usize::from(start_code).wrapping_sub(1);
        if self.mb_width == 0 || row >= self.total / self.mb_width {
            return;
        }
        let mut br = BitReader::new(data);
        if self.slice(&mut br, row).is_err() {
            // A block may have been left half parsed.
            self.coef = [0; 64];
        }
    }

    fn slice(&mut self, br: &mut BitReader, row: usize) -> Step<()> {
        let code = br.read(5);
        if !self.mpeg1 && br.flag() {
            // intra_slice_flag set: intra_slice 1, reserved_bits 7, then the
            // extra_information_slice loop.
            br.read(8);
            skip_extra_information(br);
        } else if self.mpeg1 {
            skip_extra_information(br);
        }
        // (MPEG-2 with intra_slice_flag 0: that bit was the terminating
        // extra_bit_slice.)
        let mut qscale = self.quantiser_scale(code)?;
        self.dc_pred = [self.dc_reset; 3];

        let mut addr = row * self.mb_width;
        let mut first = true;
        loop {
            let increment = self.address_increment(br)?;
            if first {
                addr += increment - 1;
                first = false;
            } else if increment != 1 {
                // Skipped macroblocks are not allowed in intra pictures.
                return Err(Corrupt);
            } else {
                addr += 1;
            }
            if addr >= self.total {
                return Err(Corrupt);
            }

            // macroblock_type (Table B.2): '1' intra, '01' intra + quant.
            let quant = match br.peek(2) {
                2 | 3 => {
                    br.skip(1);
                    false
                }
                1 => {
                    br.skip(2);
                    true
                }
                _ => return Err(Corrupt),
            };
            let field_dct = self.dct_type_coded && br.flag();
            if quant {
                qscale = self.quantiser_scale(br.read(5))?;
            }
            if self.concealment_motion_vectors {
                self.skip_concealment_vectors(br)?;
            }

            self.done[addr] = false;
            self.macroblock(br, addr, field_dct, qscale)?;
            if br.overrun() {
                return Err(Corrupt);
            }
            self.done[addr] = true;

            // The slice ends where 23 zero bits (the next start code prefix
            // or the zero padding before it) follow.
            if br.peek(23) == 0 {
                return Ok(());
            }
        }
    }

    fn quantiser_scale(&self, code: u32) -> Step<i32> {
        let code = (code & 31) as usize;
        if code == 0 {
            return Err(Corrupt);
        }
        Ok(if self.mpeg1 {
            code as i32
        } else if self.q_scale_type {
            i32::from(NON_LINEAR_QUANTISER_SCALE[code])
        } else {
            2 * code as i32
        })
    }

    /// `macroblock_escape`s, `macroblock_stuffing` (MPEG-1) and
    /// `macroblock_address_increment`.
    fn address_increment(&self, br: &mut BitReader) -> Step<usize> {
        let mut increment = 0usize;
        loop {
            let e = self.tables.macroblock_address_increment.decode(br.peek32());
            if e.len == 0 {
                return Err(Corrupt);
            }
            br.skip(u32::from(e.len));
            match e.sym {
                MBA_ESCAPE => {
                    increment += 33;
                    if increment > self.total {
                        return Err(Corrupt);
                    }
                }
                MBA_STUFFING if self.mpeg1 => {}
                MBA_STUFFING => return Err(Corrupt),
                v => return Ok(increment + v as usize),
            }
        }
    }

    /// Parses and drops the concealment motion vectors of an intra
    /// macroblock: `motion_vectors(0)` with one vector, then `marker_bit`.
    fn skip_concealment_vectors(&self, br: &mut BitReader) -> Step<()> {
        if !self.frame_picture {
            br.read(1); // motion_vertical_field_select[0][0]
        }
        for &residual_bits in &self.residual_bits {
            let e = self.tables.motion_code.decode(br.peek32());
            if e.len == 0 {
                return Err(Corrupt);
            }
            if e.sym == 0 {
                br.skip(u32::from(e.len));
            } else {
                // Code plus sign bit, then motion_residual.
                br.skip(u32::from(e.len) + 1);
                br.read(residual_bits);
            }
        }
        br.read(1); // marker_bit
        Ok(())
    }

    fn macroblock(
        &mut self,
        br: &mut BitReader,
        addr: usize,
        field_dct: bool,
        qscale: i32,
    ) -> Step<()> {
        let mbx = addr % self.mb_width;
        let mby = addr / self.mb_width;
        let ys = self.y_stride;
        let luma = mby * 16 * ys + mbx * 16;
        for k in 0..4 {
            let (offset, stride) = if field_dct {
                // Blocks 0/1 hold the top field lines, 2/3 the bottom ones.
                (luma + (k >> 1) * ys + (k & 1) * 8, 2 * ys)
            } else {
                (luma + (k >> 1) * 8 * ys + (k & 1) * 8, ys)
            };
            let rows = self.block(br, 0, qscale)?;
            self.idct
                .put(&mut self.coef, rows, &mut self.y, offset, stride);
        }
        let chroma = mby * 8 * self.c_stride + mbx * 8;
        let rows = self.block(br, 1, qscale)?;
        self.idct
            .put(&mut self.coef, rows, &mut self.cb, chroma, self.c_stride);
        let rows = self.block(br, 2, qscale)?;
        self.idct
            .put(&mut self.coef, rows, &mut self.cr, chroma, self.c_stride);
        Ok(())
    }

    /// Parses one intra block into `self.coef` (dequantised, raster order)
    /// and returns the mask of rows holding non-zero coefficients.
    #[inline]
    fn block(&mut self, br: &mut BitReader, component: usize, qscale: i32) -> Step<u8> {
        // DC coefficient: size, then the differential to the predictor.
        let dc_table = if component == 0 {
            &self.tables.dc_luminance
        } else {
            &self.tables.dc_chrominance
        };
        let e = dc_table.decode(br.peek32());
        if e.len == 0 {
            return Err(Corrupt);
        }
        br.skip(u32::from(e.len));
        let size = e.sym as u32;
        let diff = if size == 0 {
            0
        } else {
            let v = br.read(size) as i32;
            if v >> (size - 1) == 0 {
                v - (1 << size) + 1
            } else {
                v
            }
        };
        // Clamped only so a corrupt stream cannot overflow; valid streams
        // stay within 0..2^(8 + intra_dc_precision).
        let pred = (self.dc_pred[component] + diff).clamp(-(1 << 16), 1 << 16);
        self.dc_pred[component] = pred;
        let dc = (pred * self.dc_mult).clamp(-2048, 2047);
        self.coef[0] = dc;
        let mut parity = dc;
        let mut rows = 1u8;

        // AC coefficients.
        let dct = self.dct;
        let scan = self.scan;
        let mut i = 0usize;
        loop {
            let bits = br.peek32();
            let e = dct.decode(bits);
            let run;
            let level: i32;
            if e.sym >= 0 {
                let len = u32::from(e.len);
                run = (e.sym >> 8) as usize;
                let magnitude = i32::from(e.sym & 0xFF);
                level = if (bits >> (31 - len)) & 1 != 0 {
                    -magnitude
                } else {
                    magnitude
                };
                br.skip(len + 1);
            } else if e.sym == DCT_EOB {
                br.skip(u32::from(e.len));
                break;
            } else if e.sym == DCT_ESCAPE {
                run = ((bits >> 20) & 63) as usize;
                if self.mpeg1 {
                    // 8-bit level; 0x00 and 0x80 announce a 16-bit form.
                    let short = (bits >> 12) & 0xFF;
                    let long = ((bits >> 4) & 0xFF) as i32;
                    match short {
                        0 => {
                            level = long;
                            if level < 128 {
                                return Err(Corrupt);
                            }
                            br.skip(28);
                        }
                        0x80 => {
                            level = long - 256;
                            if !(-255..=-128).contains(&level) {
                                return Err(Corrupt);
                            }
                            br.skip(28);
                        }
                        _ => {
                            level = i32::from(short as u8 as i8);
                            br.skip(20);
                        }
                    }
                } else {
                    // 12-bit two's complement level.
                    level = ((bits << 12) as i32) >> 20;
                    if level == 0 || level == -2048 {
                        return Err(Corrupt);
                    }
                    br.skip(24);
                }
            } else {
                return Err(Corrupt);
            }

            i += run + 1;
            if i > 63 {
                return Err(Corrupt);
            }
            let pos = usize::from(scan[i]);
            let weight = self.matrix[pos];
            let f = if self.mpeg1 {
                let mut f = (2 * level * qscale * weight) / 16;
                // Oddification (MPEG-1 mismatch control).
                if f & 1 == 0 && f != 0 {
                    f -= f.signum();
                }
                f.clamp(-2048, 2047)
            } else {
                ((level * weight * qscale) / 16).clamp(-2048, 2047)
            };
            self.coef[pos] = f;
            parity ^= f;
            rows |= 1 << (pos >> 3);
        }

        if !self.mpeg1 && parity & 1 == 0 {
            // MPEG-2 mismatch control: make the coefficient sum odd.
            self.coef[63] ^= 1;
            rows |= 0x80;
        }
        Ok(rows)
    }

    /// Fills every macroblock not decoded in full with mid-grey and hands the
    /// planes over.
    pub fn finish(mut self) -> Planes {
        let mut decoded = 0u32;
        let mut concealed = 0u32;
        for addr in 0..self.total {
            if self.done[addr] {
                decoded += 1;
                continue;
            }
            concealed += 1;
            let mbx = addr % self.mb_width;
            let mby = addr / self.mb_width;
            for line in 0..16 {
                let at = (mby * 16 + line) * self.y_stride + mbx * 16;
                self.y[at..at + 16].fill(128);
            }
            for line in 0..8 {
                let at = (mby * 8 + line) * self.c_stride + mbx * 8;
                self.cb[at..at + 8].fill(128);
                self.cr[at..at + 8].fill(128);
            }
        }
        let rows = if self.y_stride == 0 {
            0
        } else {
            self.y.len() / self.y_stride
        };
        Planes {
            y: self.y,
            cb: self.cb,
            cr: self.cr,
            y_stride: self.y_stride,
            c_stride: self.c_stride,
            rows,
            decoded,
            concealed,
            total: self.total as u32,
        }
    }
}

/// `extra_bit_slice` / `extra_information_slice` loop: 8 bits follow every
/// 1 bit. Ends at the first 0 bit (also at the zeros served past the data).
fn skip_extra_information(br: &mut BitReader) {
    while br.flag() {
        br.read(8);
    }
}
