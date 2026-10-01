//! Turns a decoded video picture as Windows decoders deliver it (NV12, or
//! P010 for 10-bit streams; any of the Blu-ray colour spaces) into the 8-bit
//! BT.709 studio-range 4:2:0 `Frame` that the frame selection and the picture
//! conversion work on (`crate::picture`).
//!
//! - Pictures wider than `MAX_OUTPUT_WIDTH` (UHD) are box-filtered down by an
//!   integer factor first, which bounds the time and memory of what follows.
//! - BT.709 / BT.601 SDR pictures are only rescaled to 8 bits.
//! - HDR (PQ / HLG) and BT.2020 pictures are converted through linear light:
//!   PQ and HLG are tone-mapped to SDR (reference white 203 cd/m², as in
//!   ITU-R BT.2408) and BT.2020 primaries are mapped to BT.709, so a UHD frame
//!   does not come out dull or dark.
//!
//! The input comes from a decoder fed with untrusted data: every size, offset
//! and stride is checked against the buffer before it is used.

use crate::mpeg2::{ColorMatrix, Frame};

/// Widest picture produced; wider pictures are scaled down by a whole factor.
pub const MAX_OUTPUT_WIDTH: u32 = 1920;
/// Nominal peak of PQ content, used by the tone mapping when nothing better
/// is known (most UHD Blu-rays are mastered to 1000 cd/m²).
const PQ_ASSUMED_PEAK: f32 = 1000.0;
/// cd/m² shown as SDR white.
const SDR_REFERENCE_WHITE: f32 = 203.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// BT.709 / BT.1886 gamma.
    Sdr,
    /// SMPTE ST 2084 (HDR10, Dolby Vision base layer).
    Pq,
    /// ARIB STD-B67 hybrid log-gamma.
    Hlg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
    Bt709,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// How the samples of a picture are to be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub transfer: Transfer,
    pub primaries: Primaries,
    pub matrix: Matrix,
    /// Full-range (0..255) samples instead of studio range (16..235).
    pub full_range: bool,
}

impl Colour {
    /// Plain HD video.
    pub const BT709: Colour = Colour {
        transfer: Transfer::Sdr,
        primaries: Primaries::Bt709,
        matrix: Matrix::Bt709,
        full_range: false,
    };

    fn is_plain(&self) -> bool {
        self.transfer == Transfer::Sdr
            && self.primaries == Primaries::Bt709
            && self.matrix != Matrix::Bt2020
            && !self.full_range
    }
}

/// A semi-planar 4:2:0 picture (NV12 or P010) in a decoder's output buffer.
#[derive(Debug, Clone, Copy)]
pub struct SemiPlanar<'a> {
    pub data: &'a [u8],
    /// Bytes per row of both planes.
    pub stride: usize,
    /// Offset of the interleaved CbCr plane.
    pub chroma_offset: usize,
    /// True for P010: 16-bit little-endian samples holding 10 bits in the top bits.
    pub ten_bit: bool,
    /// The displayed area (minimum display aperture) inside the coded picture.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl SemiPlanar<'_> {
    fn bytes_per_sample(&self) -> usize {
        if self.ten_bit {
            2
        } else {
            1
        }
    }

    /// Checks that every sample of the displayed area lies inside the buffer.
    fn valid(&self) -> bool {
        let bps = self.bytes_per_sample();
        let (Some(right), Some(bottom)) = (
            (self.x as usize).checked_add(self.width as usize),
            (self.y as usize).checked_add(self.height as usize),
        ) else {
            return false;
        };
        let Some(row_bytes) = right.checked_mul(bps) else {
            return false;
        };
        // The chroma rows of the area: x and y are even in practice; round
        // outward so an odd offset still stays inside.
        let chroma_rows = bottom.div_ceil(2);
        let chroma_row_bytes = right.div_ceil(2).checked_mul(2 * bps);
        let luma_end = bottom.checked_mul(self.stride);
        let chroma_end = chroma_rows
            .checked_mul(self.stride)
            .and_then(|v| v.checked_add(self.chroma_offset));
        self.width >= 2
            && self.height >= 2
            && row_bytes <= self.stride
            && chroma_row_bytes.is_some_and(|c| c <= self.stride)
            && luma_end.is_some_and(|e| e <= self.chroma_offset && e <= self.data.len())
            && chroma_end.is_some_and(|e| e <= self.data.len())
    }

    /// Luma sample at (x, y) of the coded picture, as a 10-bit value.
    fn luma(&self, x: usize, y: usize) -> u32 {
        self.sample(y * self.stride + x * self.bytes_per_sample())
    }

    /// (Cb, Cr) at chroma position (x, y), as 10-bit values.
    fn chroma(&self, x: usize, y: usize) -> (u32, u32) {
        let bps = self.bytes_per_sample();
        let at = self.chroma_offset + y * self.stride + x * 2 * bps;
        (self.sample(at), self.sample(at + bps))
    }

    fn sample(&self, at: usize) -> u32 {
        if self.ten_bit {
            let v = u16::from_le_bytes([self.data[at], self.data[at + 1]]);
            u32::from(v >> 6)
        } else {
            u32::from(self.data[at]) << 2
        }
    }
}

/// Converts the displayed area of `src` into an 8-bit BT.709 frame no wider
/// than `MAX_OUTPUT_WIDTH`. `None` when the buffer description is inconsistent.
pub fn to_frame(src: &SemiPlanar, colour: Colour, pixel_aspect: (u32, u32)) -> Option<Frame> {
    if !src.valid() {
        return None;
    }
    let factor = src.width.div_ceil(MAX_OUTPUT_WIDTH).max(1);
    // At least two output samples each way, so every box of `factor` x
    // `factor` source samples lies inside the area.
    if src.width < 2 * factor || src.height < 2 * factor {
        return None;
    }
    let (width, height) = (src.width / factor, src.height / factor);
    let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
    let f = factor as usize;
    let (x0, y0) = (src.x as usize, src.y as usize);
    let (cx0, cy0) = (x0 / 2, y0 / 2);
    // Last chroma sample of the area, so a block at the bottom-right edge of
    // an odd-sized area never reads outside it.
    let (cx_max, cy_max) = (
        (x0 + src.width as usize - 1) / 2,
        (y0 + src.height as usize - 1) / 2,
    );
    let area = (f * f) as u32;

    let mut y_plane = vec![0u8; width as usize * height as usize];
    let mut cb_plane = vec![0u8; cw * ch];
    let mut cr_plane = vec![0u8; cw * ch];
    if f == 1 && !src.ten_bit && colour.is_plain() {
        // 8-bit SDR at full size (HD streams): the planes are copied as they
        // are, row by row.
        let w = width as usize;
        for (y, row) in y_plane.chunks_exact_mut(w).enumerate() {
            let at = (y0 + y) * src.stride + x0;
            row.copy_from_slice(&src.data[at..at + w]);
        }
        for cy in 0..ch {
            let at = src.chroma_offset + (cy0 + cy).min(cy_max) * src.stride + cx0 * 2;
            for (cx, pair) in src.data[at..at + cw * 2].chunks_exact(2).enumerate() {
                cb_plane[cy * cw + cx] = pair[0];
                cr_plane[cy * cw + cx] = pair[1];
            }
        }
    } else {
        // Box-filtered 10-bit chroma, needed per pixel by the colour conversion.
        let mut chroma10 = vec![(0u32, 0u32); cw * ch];
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut sb, mut sr) = (0u32, 0u32);
                for j in 0..f {
                    for i in 0..f {
                        let x = (cx0 + cx * f + i).min(cx_max);
                        let y = (cy0 + cy * f + j).min(cy_max);
                        let (b, r) = src.chroma(x, y);
                        sb += b;
                        sr += r;
                    }
                }
                chroma10[cy * cw + cx] = (sb / area, sr / area);
            }
        }
        let luma10 = |x: usize, y: usize| -> u32 {
            let mut s = 0;
            for j in 0..f {
                for i in 0..f {
                    s += src.luma(x0 + x * f + i, y0 + y * f + j);
                }
            }
            s / area
        };

        if colour.is_plain() {
            for y in 0..height as usize {
                for x in 0..width as usize {
                    y_plane[y * width as usize + x] = to8(luma10(x, y));
                }
            }
            for (i, &(b, r)) in chroma10.iter().enumerate() {
                cb_plane[i] = to8(b);
                cr_plane[i] = to8(r);
            }
        } else {
            let convert = Converter::new(colour);
            let mut sums = vec![(0u32, 0u32, 0u32); cw * ch];
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let (b, r) = chroma10[(y / 2) * cw + x / 2];
                    let (yy, cb, cr) = convert.pixel(luma10(x, y), b, r);
                    y_plane[y * width as usize + x] = yy;
                    let s = &mut sums[(y / 2) * cw + x / 2];
                    s.0 += u32::from(cb);
                    s.1 += u32::from(cr);
                    s.2 += 1;
                }
            }
            for (i, &(b, r, n)) in sums.iter().enumerate() {
                let n = n.max(1);
                cb_plane[i] = ((b + n / 2) / n) as u8;
                cr_plane[i] = ((r + n / 2) / n) as u8;
            }
        }
    }
    let matrix = if colour.is_plain() && colour.matrix == Matrix::Bt601 {
        ColorMatrix::Bt601
    } else {
        ColorMatrix::Bt709
    };
    let macroblocks = (width.div_ceil(16) * height.div_ceil(16)).max(1);
    Some(Frame {
        width,
        height,
        y: y_plane,
        cb: cb_plane,
        cr: cr_plane,
        pixel_aspect,
        matrix,
        field_doubled: false,
        mpeg1: false,
        concealed_macroblocks: 0,
        total_macroblocks: macroblocks,
    })
}

/// 10-bit sample to 8 bits, rounded.
fn to8(v: u32) -> u8 {
    ((v + 2) >> 2).min(255) as u8
}

/// Per-pixel conversion of HDR or wide-gamut Y'CbCr to BT.709 SDR Y'CbCr.
struct Converter {
    colour: Colour,
    /// Decoding of the source transfer: 4096 steps of the non-linear value to
    /// linear light, relative to SDR reference white (1.0).
    to_linear: Vec<f32>,
    /// PQ only: tone-mapping gain by the largest non-linear component
    /// (ITU-R BT.2390 EETF, see `pq_gain`).
    gain: Vec<f32>,
    /// BT.1886 encoding: linear light (0..1) on a square-root scale to the
    /// non-linear value.
    to_gamma: Vec<f32>,
}

const LUT: usize = 4096;

impl Converter {
    fn new(colour: Colour) -> Self {
        let step = |i: usize| i as f32 / (LUT - 1) as f32;
        let to_linear = (0..LUT)
            .map(|i| match colour.transfer {
                Transfer::Pq => pq_eotf(step(i)) / SDR_REFERENCE_WHITE,
                // HLG is built to be shown as SDR by SDR equipment; a
                // display gamma is the usual way to view it without HDR.
                Transfer::Sdr | Transfer::Hlg => step(i).powf(2.4),
            })
            .collect();
        let gain = if colour.transfer == Transfer::Pq {
            (0..LUT).map(|i| pq_gain(step(i))).collect()
        } else {
            Vec::new()
        };
        // Sampled on a square-root scale so the dark end keeps its precision.
        let to_gamma = (0..LUT)
            .map(|i| (step(i) * step(i)).powf(1.0 / 2.4))
            .collect();
        Self {
            colour,
            to_linear,
            gain,
            to_gamma,
        }
    }

    fn index(v: f32) -> usize {
        (v.clamp(0.0, 1.0) * (LUT - 1) as f32) as usize
    }

    fn linear(&self, v: f32) -> f32 {
        self.to_linear[Self::index(v)]
    }

    fn gamma(&self, v: f32) -> f32 {
        self.to_gamma[Self::index(v.max(0.0).sqrt())]
    }

    /// One pixel: 10-bit Y'CbCr in, 8-bit BT.709 studio-range Y'CbCr out.
    fn pixel(&self, y: u32, cb: u32, cr: u32) -> (u8, u8, u8) {
        let c = self.colour;
        let (yn, cbn, crn) = if c.full_range {
            (
                y as f32 / 1023.0,
                (cb as f32 - 512.0) / 1023.0,
                (cr as f32 - 512.0) / 1023.0,
            )
        } else {
            (
                (y as f32 - 64.0) / 876.0,
                (cb as f32 - 512.0) / 896.0,
                (cr as f32 - 512.0) / 896.0,
            )
        };
        let (kr, kb) = match c.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let r = yn + 2.0 * (1.0 - kr) * crn;
        let b = yn + 2.0 * (1.0 - kb) * cbn;
        let g = (yn - kr * r - kb * b) / kg;
        // One gain for the three components keeps the hue.
        let gain = if self.gain.is_empty() {
            1.0
        } else {
            self.gain[Self::index(r.max(g).max(b))]
        };
        let (mut r, mut g, mut b) = (
            self.linear(r) * gain,
            self.linear(g) * gain,
            self.linear(b) * gain,
        );
        if c.primaries == Primaries::Bt2020 {
            // BT.2020 → BT.709 primaries (ITU-R BT.2087), in linear light.
            let (r2, g2, b2) = (r, g, b);
            r = 1.6605 * r2 - 0.5876 * g2 - 0.0728 * b2;
            g = -0.1246 * r2 + 1.1329 * g2 - 0.0083 * b2;
            b = -0.0182 * r2 - 0.1006 * g2 + 1.1187 * b2;
        }
        let (r, g, b) = (self.gamma(r), self.gamma(g), self.gamma(b));
        let yy = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let cbv = (b - yy) / 1.8556;
        let crv = (r - yy) / 1.5748;
        // Not `round()`, a library call per sample; the cast saturates
        // (negative and NaN to 0, above 255 to 255).
        let code = |v: f32| (v + 0.5) as u8;
        (
            code(16.0 + 219.0 * yy),
            code(128.0 + 224.0 * cbv),
            code(128.0 + 224.0 * crv),
        )
    }
}

/// SMPTE ST 2084 EOTF: non-linear value (0..1) to cd/m².
fn pq_eotf(v: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let p = v.max(0.0).powf(1.0 / M2);
    let num = (p - C1).max(0.0);
    let den = C2 - C3 * p;
    if den <= 0.0 {
        return 10_000.0;
    }
    10_000.0 * (num / den).powf(1.0 / M1)
}

/// SMPTE ST 2084 inverse EOTF: cd/m² to the non-linear value (0..1).
fn pq_inverse(nits: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.6875;
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// Tone-mapping gain for a pixel whose largest PQ component is `e`: the
/// ITU-R BT.2390 EETF maps the content range (black to `PQ_ASSUMED_PEAK`)
/// onto SDR (black to reference white) in the PQ domain, keeping everything
/// below the knee unchanged; the gain is output light over input light.
fn pq_gain(e: f32) -> f32 {
    let source_peak = pq_inverse(PQ_ASSUMED_PEAK);
    let max_lum = pq_inverse(SDR_REFERENCE_WHITE) / source_peak;
    let knee = 1.5 * max_lum - 0.5;
    let x = (e / source_peak).min(1.0);
    let mapped = if x <= knee {
        x
    } else {
        let t = (x - knee) / (1.0 - knee);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    let input = pq_eotf(e);
    if input <= 0.0 {
        return 1.0;
    }
    pq_eotf(mapped * source_peak) / input
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A uniform NV12 or P010 picture of 10-bit code values.
    fn uniform(w: usize, h: usize, ten_bit: bool, y: u16, cb: u16, cr: u16) -> Vec<u8> {
        let bps = if ten_bit { 2 } else { 1 };
        let stride = w * bps;
        let mut d = vec![0u8; stride * h + stride * h.div_ceil(2)];
        let put = |d: &mut Vec<u8>, at: usize, v: u16| {
            if ten_bit {
                d[at..at + 2].copy_from_slice(&(v << 6).to_le_bytes());
            } else {
                d[at] = (v >> 2) as u8;
            }
        };
        for row in 0..h {
            for x in 0..w {
                put(&mut d, row * stride + x * bps, y);
            }
        }
        for row in 0..h.div_ceil(2) {
            for x in 0..w / 2 {
                let at = stride * h + row * stride + x * 2 * bps;
                put(&mut d, at, cb);
                put(&mut d, at + bps, cr);
            }
        }
        d
    }

    fn picture(d: &[u8], w: u32, h: u32, ten_bit: bool) -> SemiPlanar<'_> {
        let stride = w as usize * if ten_bit { 2 } else { 1 };
        SemiPlanar {
            data: d,
            stride,
            chroma_offset: stride * h as usize,
            ten_bit,
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn plain_nv12_is_copied_at_eight_bits() {
        let d = uniform(64, 32, false, 500, 300, 700);
        let f = to_frame(&picture(&d, 64, 32, false), Colour::BT709, (1, 1)).unwrap();
        assert_eq!((f.width, f.height), (64, 32));
        assert!(f.y.iter().all(|&v| v == 125));
        assert!(f.cb.iter().all(|&v| v == 75));
        assert!(f.cr.iter().all(|&v| v == 175));
        assert_eq!(f.matrix, ColorMatrix::Bt709);
    }

    #[test]
    fn uhd_pictures_are_scaled_down() {
        let d = uniform(3840, 32, true, 600, 512, 512);
        let f = to_frame(&picture(&d, 3840, 32, true), Colour::BT709, (1, 1)).unwrap();
        assert_eq!((f.width, f.height), (1920, 16));
        assert!(f.y.iter().all(|&v| v == 150));
    }

    #[test]
    fn pq_white_and_grey_map_to_neutral_sdr() {
        let hdr10 = Colour {
            transfer: Transfer::Pq,
            primaries: Primaries::Bt2020,
            matrix: Matrix::Bt2020,
            full_range: false,
        };
        // PQ code of 203 cd/m² (reference white) is about 0.58, i.e. 10-bit 572.
        let d = uniform(32, 32, true, 572, 512, 512);
        let f = to_frame(&picture(&d, 32, 32, true), hdr10, (1, 1)).unwrap();
        let y = f.y[0];
        assert!((150..=235).contains(&y), "reference white came out as {y}");
        assert!(f.cb.iter().all(|&v| (126..=130).contains(&v)));
        assert!(f.cr.iter().all(|&v| (126..=130).contains(&v)));
        // Peak white must not clip to more than SDR white.
        let d = uniform(32, 32, true, 940, 512, 512);
        let f = to_frame(&picture(&d, 32, 32, true), hdr10, (1, 1)).unwrap();
        assert!(f.y[0] <= 235);
        // Black stays black.
        let d = uniform(32, 32, true, 64, 512, 512);
        let f = to_frame(&picture(&d, 32, 32, true), hdr10, (1, 1)).unwrap();
        assert!(f.y[0] <= 18);
    }

    #[test]
    fn inconsistent_buffers_are_rejected() {
        let d = uniform(64, 32, false, 500, 512, 512);
        let mut p = picture(&d, 64, 32, false);
        p.width = 65;
        assert!(to_frame(&p, Colour::BT709, (1, 1)).is_none());
        let mut p = picture(&d, 64, 32, false);
        p.chroma_offset = d.len();
        assert!(to_frame(&p, Colour::BT709, (1, 1)).is_none());
        let mut p = picture(&d, 64, 32, false);
        p.stride = 10;
        assert!(to_frame(&p, Colour::BT709, (1, 1)).is_none());
        let p = picture(&d[..100], 64, 32, false);
        assert!(to_frame(&p, Colour::BT709, (1, 1)).is_none());
        // A wide area too low for the downscale's boxes.
        for h in [2u32, 3] {
            let d = uniform(3840, h as usize, false, 500, 512, 512);
            assert!(to_frame(&picture(&d, 3840, h, false), Colour::BT709, (1, 1)).is_none());
        }
        let d = uniform(3840, 6, false, 500, 512, 512);
        let f = to_frame(&picture(&d, 3840, 6, false), Colour::BT709, (1, 1)).unwrap();
        assert_eq!((f.width, f.height), (1920, 3));
    }

    #[test]
    fn eight_bit_planes_are_copied_sample_for_sample() {
        // Distinct samples everywhere, an odd crop: every output sample must
        // be the source sample at its place.
        let (w, h) = (64usize, 34usize);
        let mut d = vec![0u8; w * h + w * h.div_ceil(2)];
        for (i, v) in d.iter_mut().enumerate() {
            *v = (i * 7 % 251) as u8;
        }
        let mut p = picture(&d, w as u32, h as u32, false);
        (p.x, p.y, p.width, p.height) = (3, 1, 59, 31);
        let f = to_frame(&p, Colour::BT709, (1, 1)).unwrap();
        assert_eq!((f.width, f.height), (59, 31));
        for y in 0..31 {
            for x in 0..59 {
                assert_eq!(f.y[y * 59 + x], d[(y + 1) * w + x + 3], "luma {x},{y}");
            }
        }
        let cw = 30;
        for cy in 0..16 {
            for cx in 0..cw {
                let at = w * h + cy * w + (1 + cx) * 2;
                assert_eq!(f.cb[cy * cw + cx], d[at], "cb {cx},{cy}");
                assert_eq!(f.cr[cy * cw + cx], d[at + 1], "cr {cx},{cy}");
            }
        }
    }

    #[test]
    fn odd_crop_stays_inside_the_planes() {
        let d = uniform(64, 34, false, 400, 512, 512);
        let mut p = picture(&d, 64, 34, false);
        p.x = 3;
        p.y = 1;
        p.width = 61;
        p.height = 33;
        let f = to_frame(&p, Colour::BT709, (1, 1)).unwrap();
        assert_eq!((f.width, f.height), (61, 33));
    }
}
