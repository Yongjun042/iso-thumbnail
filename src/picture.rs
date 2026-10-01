//! Turns decoded MPEG frames into BGRA pictures and judges them as thumbnails.
//!
//! All the arithmetic is integer: the colour conversion uses fixed-point
//! coefficients, the statistics use exact sums. Frames come from the decoder,
//! but their fields are public, so every function also copes with planes that
//! are shorter than the dimensions promise and with rectangles that leave the
//! frame (they are clipped).
//!
//! CONTRACT (fixed; other modules are written against it): `Rect`, `Picture`,
//! `LumaStats` and the four functions keep these signatures and meanings.

use crate::mpeg2::{ColorMatrix, Frame};

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
    ///
    /// The ratio is clamped to 1:4 … 4:1 (a crafted stream must not turn a
    /// small picture into a huge one) and a ratio with a zero term counts as
    /// square.
    pub fn display_size(&self) -> (u32, u32) {
        let (mut num, mut den) = (self.pixel_aspect.0 as u64, self.pixel_aspect.1 as u64);
        if num == 0 || den == 0 {
            (num, den) = (1, 1);
        } else if num > den.saturating_mul(4) {
            (num, den) = (4, 1);
        } else if den > num.saturating_mul(4) {
            (num, den) = (1, 4);
        }
        // width < 2^32 and num ≤ 4 den ≤ 2^34, so the product fits in u64.
        let width = (self.width as u64 * num + den / 2) / den;
        let width = width.clamp(1, u32::MAX as u64) as u32;
        (width, self.height.max(1))
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

/// Mean luma at or below which a line counts as black (studio black is 16).
const BORDER_MAX_MEAN: u64 = 26;
/// Largest luma variance (standard deviation 6) of a border line: bars are
/// flat, a dark picture line has texture.
const BORDER_MAX_VARIANCE: u64 = 36;

/// The part of the frame left after cutting uniform dark borders (letterbox
/// bars at the top and bottom, pillarbox bars at the sides). Never cuts more
/// than a quarter of the height from the top or the bottom, nor more than a
/// quarter of the width from either side; coordinates and sizes are even so
/// the chroma planes stay aligned. Returns the full frame when there are no
/// borders.
///
/// A line is a border when its sampled luma has a mean of at most 26 and a
/// standard deviation of at most 6. Each side is scanned inward and stops at
/// the first line that is not a border. Real bars come in pairs of about the
/// same thickness that stay below a quarter of the frame, while dark picture
/// content at an edge (a night sky, a dark wall, black around a logo) does
/// not: see `bar_pair`. A frame whose remaining middle is just as dark (a
/// black or nearly black frame) is returned whole: there is nothing to frame
/// in it.
pub fn active_area(frame: &Frame) -> Rect {
    let full = full_area(frame);
    if !luma_ok(frame) || frame.width < 4 || frame.height < 4 {
        return full;
    }
    let (w, h) = (frame.width, frame.height);
    let row = |y: u32| line_is_border(frame, 0, y, 1, 0, w);
    let top = (0..h / 4).take_while(|&y| row(y)).count() as u32;
    let bottom = (0..h / 4).take_while(|&i| row(h - 1 - i)).count() as u32;
    let (top, bottom) = bar_pair(top, bottom, h / 4, (h / 60).max(4));
    // Columns are judged on the rows that remain, so letterbox bars do not
    // make every column look dark.
    let (y0, y1) = (top, h - bottom);
    let rows = y1 - y0;
    let col = |x: u32| line_is_border(frame, x, y0, 0, 1, rows);
    let left = (0..w / 4).take_while(|&x| col(x)).count() as u32;
    let right = (0..w / 4).take_while(|&i| col(w - 1 - i)).count() as u32;
    let (left, right) = bar_pair(left, right, w / 4, (w / 60).max(4));
    if top + bottom + left + right == 0 {
        return full;
    }
    // Round the cuts down to even numbers: at worst one dark line stays.
    let (x0, x1) = (left & !1, even_end(w, right));
    let (y0, y1) = (top & !1, even_end(h, bottom));
    let area = Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    };
    if is_dark(&luma_stats(frame, area)) {
        return full;
    }
    area
}

/// The cuts to make for a pair of opposite borders `a` and `b` lines thick,
/// each found by a scan limited to `cap` lines.
///
/// A border that reaches the cap is dark picture content, not a bar: no cut on
/// either side (a 16:9 film letterboxed in 4:3 has bars of an eighth, even
/// 2.4:1 stays below a quarter). Bars of clearly different thickness (more
/// than `tolerance` lines apart) are trimmed to the thinner one, so dark
/// content next to one bar, or a one-sided dark edge, is kept.
fn bar_pair(a: u32, b: u32, cap: u32, tolerance: u32) -> (u32, u32) {
    if a >= cap || b >= cap {
        (0, 0)
    } else if a.abs_diff(b) > tolerance {
        let thinner = a.min(b);
        (thinner, thinner)
    } else {
        (a, b)
    }
}

/// End of the kept range when `cut` lines are cut from the far side of a
/// `len`-line dimension, moved outward to an even coordinate (unless nothing
/// is cut, where it stays at `len`).
fn even_end(len: u32, cut: u32) -> u32 {
    if cut == 0 {
        len
    } else {
        ((len - cut + 1) & !1).min(len)
    }
}

fn is_dark(s: &LumaStats) -> bool {
    s.mean <= BORDER_MAX_MEAN as f32 && s.stddev * s.stddev <= BORDER_MAX_VARIANCE as f32
}

/// Whether `count` luma samples from (x, y) in steps of (dx, dy) look like a
/// black bar. Long lines are sampled every 4th sample, short ones every 2nd.
fn line_is_border(frame: &Frame, x: u32, y: u32, dx: u32, dy: u32, count: u32) -> bool {
    let step = if count >= 256 { 4 } else { 2 };
    let stride = frame.width as usize;
    let (mut n, mut sum, mut sq) = (0u64, 0u64, 0u64);
    for i in (0..count).step_by(step) {
        let (sx, sy) = ((x + i * dx) as usize, (y + i * dy) as usize);
        let v = frame.y[sy * stride + sx] as u64;
        n += 1;
        sum += v;
        sq += v * v;
    }
    if n == 0 {
        return false;
    }
    // mean ≤ M  ⇔  sum ≤ M n;  variance ≤ V  ⇔  n sq − sum² ≤ V n² (u128:
    // n sq is not bounded by u64 for an absurdly long line).
    let (n, sum, sq) = (n as u128, sum as u128, sq as u128);
    sum <= BORDER_MAX_MEAN as u128 * n && n * sq - sum * sum <= BORDER_MAX_VARIANCE as u128 * n * n
}

/// Mean and standard deviation of the luma samples inside `area` (sampled on
/// a grid when the area is large).
///
/// Areas up to 128 × 128 samples are measured exactly; larger ones every 2nd
/// sample in both directions, and areas above 512 × 512 every 4th. The
/// standard deviation is the population one. `area` is clipped to the frame;
/// an empty area gives zeros.
pub fn luma_stats(frame: &Frame, area: Rect) -> LumaStats {
    let zero = LumaStats {
        mean: 0.0,
        stddev: 0.0,
    };
    if !luma_ok(frame) {
        return zero;
    }
    let area = clip(frame, area);
    let samples = area.width as u64 * area.height as u64;
    let step = match samples {
        0 => return zero,
        1..=16_384 => 1,
        16_385..=262_144 => 2,
        _ => 4,
    };
    let stride = frame.width as usize;
    let (x0, x1) = (area.x as usize, (area.x + area.width) as usize);
    let (mut n, mut sum, mut sq) = (0u64, 0u64, 0u64);
    for y in (area.y..area.y + area.height).step_by(step) {
        let line = &frame.y[y as usize * stride..][x0..x1];
        for &v in line.iter().step_by(step) {
            let v = v as u64;
            n += 1;
            sum += v;
            sq += v * v;
        }
    }
    // n ≥ 1 here. Exact integer variance, then one conversion to float.
    let mean = sum as f64 / n as f64;
    let var = (n as u128 * sq as u128 - sum as u128 * sum as u128) as f64 / (n as f64 * n as f64);
    LumaStats {
        mean: mean as f32,
        stddev: var.sqrt() as f32,
    }
}

/// Converts `area` of the frame to BGRA using the frame's colour matrix
/// (studio range Y'CbCr → full range R'G'B'), with bilinear chroma upsampling.
/// The result carries the frame's pixel aspect ratio.
///
/// Chroma is sited as in MPEG-2 4:2:0: horizontally on the even luma columns,
/// vertically halfway between two luma rows; samples beyond the plane edges
/// repeat the edge. `area` is clipped to the frame; a frame whose planes are
/// shorter than its dimensions gives an empty (0 × 0) picture.
pub fn to_picture(frame: &Frame, area: Rect) -> Picture {
    let area = if planes_ok(frame) {
        clip(frame, area)
    } else {
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    };
    let (w, h) = (area.width as usize, area.height as usize);
    let mut bgra = Vec::with_capacity(w * h * 4);
    if w > 0 && h > 0 {
        convert(frame, area, &mut bgra);
    }
    Picture {
        width: if h > 0 { area.width } else { 0 },
        height: if w > 0 { area.height } else { 0 },
        bgra,
        pixel_aspect: frame.pixel_aspect,
    }
}

/// Fraction bits of the chroma coefficients.
const CHROMA_SHIFT: u32 = 16;
/// Chroma samples are interpolated with weights summing to 8 (3 bits).
const INTERP_BITS: u32 = 3;
/// Fraction bits of the final sum: coefficients × 8× chroma.
const SUM_SHIFT: u32 = CHROMA_SHIFT + INTERP_BITS;

/// Fixed-point Y'CbCr → R'G'B' coefficients for studio range input.
struct Coefficients {
    /// 255/219, in units of 2^-SUM_SHIFT (applied to Y' − 16).
    y: i32,
    /// Chroma terms in units of 2^-CHROMA_SHIFT, applied to 8 × (C − 128).
    cr_r: i32,
    cb_g: i32,
    cr_g: i32,
    cb_b: i32,
}

impl Coefficients {
    fn new(matrix: ColorMatrix) -> Self {
        let (kr, kb) = match matrix {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        // Chroma excursion 224 (16..240) maps to 255.
        let c = 255.0 / 224.0;
        let q = |v: f64| (v * (1u32 << CHROMA_SHIFT) as f64).round() as i32;
        Self {
            y: ((255.0 / 219.0) * (1u32 << SUM_SHIFT) as f64).round() as i32,
            cr_r: q(c * 2.0 * (1.0 - kr)),
            cb_g: q(c * 2.0 * (1.0 - kb) * kb / kg),
            cr_g: q(c * 2.0 * (1.0 - kr) * kr / kg),
            cb_b: q(c * 2.0 * (1.0 - kb)),
        }
    }
}

/// Rounds a sum in units of 2^-SUM_SHIFT and clamps it to 0..=255. The terms
/// stay below 2^29 in magnitude, so i32 is enough.
#[inline]
fn to_u8(sum: i32) -> u8 {
    ((sum + (1 << (SUM_SHIFT - 1))) >> SUM_SHIFT).clamp(0, 255) as u8
}

/// Converts the (clipped, non-empty) `area`; the planes are known to be complete.
fn convert(frame: &Frame, area: Rect, out: &mut Vec<u8>) {
    let k = Coefficients::new(frame.matrix);
    let stride = frame.width as usize;
    let cw = frame.chroma_width() as usize;
    let ch = frame.chroma_height() as usize;
    let (x0, x1) = (area.x as usize, (area.x + area.width) as usize);
    // Chroma columns the area touches: the odd last column also needs the next one.
    let (c0, c1) = (x0 / 2, (x1 / 2 + 1).min(cw));
    // One chroma row, interpolated vertically (weights summing to 4).
    let mut cb_row = vec![0u16; cw];
    let mut cr_row = vec![0u16; cw];
    for y in area.y as usize..(area.y + area.height) as usize {
        // Chroma row r sits at luma row 2r + 0.5: an even luma row 2r lies
        // between chroma rows r − 1 (¼) and r (¾), an odd row 2r + 1 between
        // r (¾) and r + 1 (¼).
        let r = y / 2;
        let (a, wa, b, wb) = if y % 2 == 0 {
            (r.saturating_sub(1), 1, r, 3)
        } else {
            (r, 3, (r + 1).min(ch - 1), 1)
        };
        for (plane, row) in [(&frame.cb, &mut cb_row), (&frame.cr, &mut cr_row)] {
            let (pa, pb) = (&plane[a * cw..][..cw], &plane[b * cw..][..cw]);
            for j in c0..c1 {
                row[j] = pa[j] as u16 * wa + pb[j] as u16 * wb;
            }
        }
        let luma = &frame.y[y * stride..][x0..x1];
        for (x, &yv) in (x0..x1).zip(luma) {
            // Even columns sit on a chroma sample, odd ones halfway to the next.
            let j = x / 2;
            let (cb, cr) = if x % 2 == 0 {
                (2 * cb_row[j], 2 * cr_row[j])
            } else {
                let n = (j + 1).min(cw - 1);
                (cb_row[j] + cb_row[n], cr_row[j] + cr_row[n])
            };
            // Both are 8 × chroma now: centre them on 8 × 128.
            let cb = cb as i32 - (128 << INTERP_BITS);
            let cr = cr as i32 - (128 << INTERP_BITS);
            let luma = (yv as i32 - 16) * k.y;
            let r = luma + k.cr_r * cr;
            let g = luma - k.cb_g * cb - k.cr_g * cr;
            let b = luma + k.cb_b * cb;
            out.extend_from_slice(&[to_u8(b), to_u8(g), to_u8(r), 255]);
        }
    }
}

/// `area` clipped to the frame.
fn clip(frame: &Frame, area: Rect) -> Rect {
    let x = area.x.min(frame.width);
    let y = area.y.min(frame.height);
    Rect {
        x,
        y,
        width: area.width.min(frame.width - x),
        height: area.height.min(frame.height - y),
    }
}

/// Whether the luma plane holds `width × height` samples.
fn luma_ok(frame: &Frame) -> bool {
    frame.y.len() as u64 >= frame.width as u64 * frame.height as u64
}

/// Whether all three planes hold the samples the dimensions promise.
fn planes_ok(frame: &Frame) -> bool {
    let chroma = frame.chroma_width() as u64 * frame.chroma_height() as u64;
    luma_ok(frame) && frame.cb.len() as u64 >= chroma && frame.cr.len() as u64 >= chroma
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, matrix: ColorMatrix) -> Frame {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        Frame {
            width,
            height,
            y: vec![16; (width * height) as usize],
            cb: vec![128; (cw * ch) as usize],
            cr: vec![128; (cw * ch) as usize],
            pixel_aspect: (1, 1),
            matrix,
            field_doubled: false,
            mpeg1: false,
            concealed_macroblocks: 0,
            total_macroblocks: 0,
        }
    }

    /// Converts one uniform colour and returns (R, G, B).
    fn rgb_of(matrix: ColorMatrix, y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
        let mut f = frame(2, 2, matrix);
        f.y.fill(y);
        f.cb.fill(cb);
        f.cr.fill(cr);
        let p = to_picture(&f, full_area(&f));
        assert_eq!(p.bgra.len(), 16);
        for px in p.bgra.chunks_exact(4) {
            assert_eq!(px, &p.bgra[..4]);
        }
        (p.bgra[2], p.bgra[1], p.bgra[0])
    }

    /// The textbook floating-point conversion.
    fn reference(matrix: ColorMatrix, y: f64, cb: f64, cr: f64) -> (f64, f64, f64) {
        let (kr, kb) = match matrix {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let yy = (y - 16.0) * 255.0 / 219.0;
        let pb = (cb - 128.0) * 255.0 / 224.0;
        let pr = (cr - 128.0) * 255.0 / 224.0;
        let r = yy + 2.0 * (1.0 - kr) * pr;
        let b = yy + 2.0 * (1.0 - kb) * pb;
        let g = yy - 2.0 * (1.0 - kb) * kb / kg * pb - 2.0 * (1.0 - kr) * kr / kg * pr;
        let c = |v: f64| v.round().clamp(0.0, 255.0);
        (c(r), c(g), c(b))
    }

    fn close(a: u8, b: f64, tol: f64) -> bool {
        (a as f64 - b).abs() <= tol
    }

    #[test]
    fn black_and_white_map_to_full_range() {
        for m in [ColorMatrix::Bt601, ColorMatrix::Bt709] {
            assert_eq!(rgb_of(m, 16, 128, 128), (0, 0, 0));
            assert_eq!(rgb_of(m, 235, 128, 128), (255, 255, 255));
            assert_eq!(rgb_of(m, 0, 128, 128), (0, 0, 0));
            assert_eq!(rgb_of(m, 255, 128, 128), (255, 255, 255));
            let grey = rgb_of(m, 126, 128, 128);
            assert_eq!(grey, (128, 128, 128));
        }
    }

    #[test]
    fn colour_bars_match_both_matrices() {
        // 100 % colour bars in studio range Y'CbCr, per matrix: (Y, Cb, Cr, nominal RGB).
        let bars601 = [
            (81, 90, 240, (255, 0, 0)),
            (145, 54, 34, (0, 255, 0)),
            (41, 240, 110, (0, 0, 255)),
            (210, 16, 146, (255, 255, 0)),
        ];
        let bars709 = [
            (63, 102, 240, (255, 0, 0)),
            (173, 42, 26, (0, 255, 0)),
            (32, 240, 118, (0, 0, 255)),
            (219, 16, 138, (255, 255, 0)),
        ];
        for (m, bars) in [(ColorMatrix::Bt601, bars601), (ColorMatrix::Bt709, bars709)] {
            for (y, cb, cr, nominal) in bars {
                let got = rgb_of(m, y, cb, cr);
                let want = reference(m, y as f64, cb as f64, cr as f64);
                assert!(
                    close(got.0, want.0, 1.0)
                        && close(got.1, want.1, 1.0)
                        && close(got.2, want.2, 1.0),
                    "{m:?} {y}/{cb}/{cr}: got {got:?}, formula {want:?}"
                );
                // The integer bar values are off the exact primaries by a little.
                let n = (nominal.0 as f64, nominal.1 as f64, nominal.2 as f64);
                assert!(
                    close(got.0, n.0, 3.0) && close(got.1, n.1, 3.0) && close(got.2, n.2, 3.0),
                    "{m:?} {y}/{cb}/{cr}: got {got:?}, nominal {nominal:?}"
                );
            }
        }
        // The matrices really differ: BT.601 red decoded as BT.709 is not red.
        let wrong = rgb_of(ColorMatrix::Bt709, 81, 90, 240);
        assert!(wrong.1 > 20, "{wrong:?}");
    }

    #[test]
    fn every_value_stays_within_one_of_the_formula() {
        for m in [ColorMatrix::Bt601, ColorMatrix::Bt709] {
            for y in (0..=255).step_by(17) {
                for cb in (0..=255).step_by(15) {
                    for cr in (0..=255).step_by(15) {
                        let got = rgb_of(m, y, cb, cr);
                        let want = reference(m, y as f64, cb as f64, cr as f64);
                        assert!(
                            close(got.0, want.0, 1.0)
                                && close(got.1, want.1, 1.0)
                                && close(got.2, want.2, 1.0),
                            "{m:?} {y}/{cb}/{cr}: {got:?} vs {want:?}"
                        );
                    }
                }
            }
        }
    }

    /// Chroma at luma (x, y) by the MPEG-2 4:2:0 siting, in floating point.
    fn sited(plane: &[u8], cw: usize, ch: usize, x: usize, y: usize) -> f64 {
        let cx = x as f64 / 2.0;
        let cy = (y as f64 - 0.5) / 2.0;
        let at = |i: f64, j: f64| {
            let i = (i.max(0.0) as usize).min(cw - 1);
            let j = (j.max(0.0) as usize).min(ch - 1);
            plane[j * cw + i] as f64
        };
        let (fx, fy) = (cx - cx.floor(), cy - cy.floor());
        let (x0, y0) = (cx.floor(), cy.floor());
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1.0, y0) * fx;
        let bottom = at(x0, y0 + 1.0) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0) * fx;
        top * (1.0 - fy) + bottom * fy
    }

    #[test]
    fn chroma_siting_and_odd_sizes() {
        for (w, h) in [(4u32, 4u32), (5, 3), (7, 5), (1, 1), (2, 7)] {
            let mut f = frame(w, h, ColorMatrix::Bt601);
            let (cw, ch) = (f.chroma_width() as usize, f.chroma_height() as usize);
            f.y.fill(126);
            for (i, v) in f.cb.iter_mut().enumerate() {
                *v = 112 + ((i * 7) % 33) as u8;
            }
            for (i, v) in f.cr.iter_mut().enumerate() {
                *v = 140 - ((i * 5) % 29) as u8;
            }
            let p = to_picture(&f, full_area(&f));
            assert_eq!((p.width, p.height), (w, h));
            assert_eq!(p.bgra.len(), (w * h * 4) as usize);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let cb = sited(&f.cb, cw, ch, x, y);
                    let cr = sited(&f.cr, cw, ch, x, y);
                    let want = reference(ColorMatrix::Bt601, 126.0, cb, cr);
                    let px = &p.bgra[(y * w as usize + x) * 4..][..4];
                    assert!(
                        close(px[2], want.0, 1.0)
                            && close(px[1], want.1, 1.0)
                            && close(px[0], want.2, 1.0)
                            && px[3] == 255,
                        "{w}x{h} at ({x},{y}): {px:?} vs {want:?}"
                    );
                }
            }
            // Any sub-rectangle equals the same crop of the full conversion.
            for (rx, ry, rw, rh) in [(1, 1, 3, 1), (0, 1, 1, 2), (1, 0, 4, 3)] {
                let area = Rect {
                    x: rx,
                    y: ry,
                    width: rw,
                    height: rh,
                };
                let c = clip(&f, area);
                let part = to_picture(&f, area);
                if c.width == 0 || c.height == 0 {
                    assert_eq!((part.width, part.height, part.bgra.len()), (0, 0, 0));
                    continue;
                }
                assert_eq!((part.width, part.height), (c.width, c.height));
                for yy in 0..c.height as usize {
                    for xx in 0..c.width as usize {
                        let a = &part.bgra[(yy * c.width as usize + xx) * 4..][..4];
                        let full = ((c.y as usize + yy) * w as usize + c.x as usize + xx) * 4;
                        assert_eq!(a, &p.bgra[full..full + 4]);
                    }
                }
            }
        }
    }

    #[test]
    fn out_of_range_areas_and_broken_frames_do_not_panic() {
        let mut f = frame(8, 6, ColorMatrix::Bt709);
        f.pixel_aspect = (32, 27);
        let p = to_picture(
            &f,
            Rect {
                x: 6,
                y: 4,
                width: 100,
                height: 100,
            },
        );
        assert_eq!((p.width, p.height, p.bgra.len()), (2, 2, 16));
        assert_eq!(p.pixel_aspect, (32, 27));
        let p = to_picture(
            &f,
            Rect {
                x: 9,
                y: u32::MAX,
                width: u32::MAX,
                height: 3,
            },
        );
        assert_eq!((p.width, p.height, p.bgra.len()), (0, 0, 0));
        f.cr.pop();
        let p = to_picture(&f, full_area(&f));
        assert_eq!((p.width, p.height, p.bgra.len()), (0, 0, 0));
        f.y.truncate(10);
        assert_eq!(active_area(&f), full_area(&f));
        assert_eq!(luma_stats(&f, full_area(&f)).mean, 0.0);
    }

    /// A deterministic textured picture: luma in 40..=220.
    fn textured(width: u32, height: u32) -> Frame {
        let mut f = frame(width, height, ColorMatrix::Bt601);
        let mut seed = 0x1234_5678u32;
        for v in f.y.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = 40 + ((seed >> 24) % 181) as u8;
        }
        f
    }

    /// Paints rows `y0..y1` × columns `x0..x1` near-black with a little noise.
    fn bar(f: &mut Frame, x0: u32, x1: u32, y0: u32, y1: u32) {
        for y in y0..y1 {
            for x in x0..x1 {
                f.y[(y * f.width + x) as usize] = 16 + ((x * 3 + y * 7) % 5) as u8;
            }
        }
    }

    #[test]
    fn letterbox_and_pillarbox_are_cut() {
        let full = Rect {
            x: 0,
            y: 0,
            width: 720,
            height: 480,
        };
        let mut f = textured(720, 480);
        assert_eq!(active_area(&f), full, "noise frame");
        bar(&mut f, 0, 720, 0, 60);
        bar(&mut f, 0, 720, 420, 480);
        assert_eq!(
            active_area(&f),
            Rect {
                x: 0,
                y: 60,
                width: 720,
                height: 360
            }
        );

        let mut f = textured(720, 480);
        bar(&mut f, 0, 90, 0, 480);
        bar(&mut f, 630, 720, 0, 480);
        assert_eq!(
            active_area(&f),
            Rect {
                x: 90,
                y: 0,
                width: 540,
                height: 480
            }
        );

        // Both at once, with odd bar sizes: the cuts are rounded to keep even
        // coordinates, leaving at most one dark line on a side.
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 61);
        bar(&mut f, 0, 720, 421, 480);
        bar(&mut f, 0, 45, 0, 480);
        bar(&mut f, 675, 720, 0, 480);
        let a = active_area(&f);
        assert_eq!(
            a,
            Rect {
                x: 44,
                y: 60,
                width: 632,
                height: 362
            }
        );

        // Dark borders reaching a quarter are picture content (black around
        // a logo or a title), not bars.
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 150);
        bar(&mut f, 0, 720, 330, 480);
        assert_eq!(active_area(&f), full);
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 120);
        bar(&mut f, 0, 720, 360, 480);
        bar(&mut f, 0, 180, 0, 480);
        bar(&mut f, 540, 720, 0, 480);
        assert_eq!(active_area(&f), full, "bright box centred on black");

        // Bars of different thickness are trimmed to the thinner one.
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 60);
        bar(&mut f, 0, 720, 450, 480);
        assert_eq!(
            active_area(&f),
            Rect {
                x: 0,
                y: 30,
                width: 720,
                height: 420
            }
        );
    }

    #[test]
    fn one_sided_dark_edges_are_picture_content() {
        let full = Rect {
            x: 0,
            y: 0,
            width: 720,
            height: 480,
        };
        // A dark sky above the scene, a dark wall at one side.
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 100);
        assert_eq!(active_area(&f), full, "dark top only");
        let mut f = textured(720, 480);
        bar(&mut f, 0, 170, 0, 480);
        assert_eq!(active_area(&f), full, "dark left side only");
        // A letterboxed frame whose top bar continues into a dark sky: only
        // the bar thickness of the bottom side is cut at the top.
        let mut f = textured(720, 480);
        bar(&mut f, 0, 720, 0, 100);
        bar(&mut f, 0, 720, 420, 480);
        assert_eq!(
            active_area(&f),
            Rect {
                x: 0,
                y: 60,
                width: 720,
                height: 360
            }
        );
    }

    #[test]
    fn dark_frames_and_dark_scenes_stay_whole() {
        let f = frame(720, 480, ColorMatrix::Bt601);
        assert_eq!(active_area(&f), full_area(&f));
        // A dark scene (mean ≈ 21, but textured) has no bars.
        let mut f = textured(720, 480);
        for v in f.y.iter_mut() {
            *v /= 6;
        }
        assert!(!line_is_border(&f, 0, 0, 1, 0, 720));
        assert_eq!(active_area(&f), full_area(&f));
        // Bars around a black picture: still the whole frame.
        let mut f = frame(720, 480, ColorMatrix::Bt601);
        bar(&mut f, 0, 720, 0, 60);
        assert_eq!(active_area(&f), full_area(&f));
    }

    #[test]
    fn luma_stats_are_exact_on_small_areas() {
        let mut f = frame(4, 2, ColorMatrix::Bt601);
        f.y.copy_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);
        let s = luma_stats(&f, full_area(&f));
        assert_eq!(s.mean, 45.0);
        assert!((s.stddev - 22.912_878).abs() < 1e-4, "{s:?}");
        let s = luma_stats(
            &f,
            Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
        );
        assert_eq!((s.mean, s.stddev), (65.0, 5.0));
        let s = luma_stats(
            &f,
            Rect {
                x: 4,
                y: 0,
                width: 3,
                height: 3,
            },
        );
        assert_eq!((s.mean, s.stddev), (0.0, 0.0));
    }

    #[test]
    fn luma_stats_sample_large_areas() {
        // Columns alternate in blocks of 4 between 16 and 235 and rows in
        // blocks of 4: a grid of every 2nd or 4th sample still sees both halves.
        let mut f = frame(1024, 768, ColorMatrix::Bt601);
        for y in 0..768u32 {
            for x in 0..1024u32 {
                f.y[(y * 1024 + x) as usize] = if (x / 4 + y / 4) % 2 == 0 { 16 } else { 235 };
            }
        }
        for area in [
            full_area(&f),
            Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 400,
            },
        ] {
            let s = luma_stats(&f, area);
            assert!((s.mean - 125.5).abs() < 3.0, "{s:?}");
            assert!((s.stddev - 109.5).abs() < 3.0, "{s:?}");
        }
    }

    #[test]
    fn display_size_applies_the_pixel_aspect() {
        let p = |width, height, pixel_aspect| Picture {
            width,
            height,
            bgra: Vec::new(),
            pixel_aspect,
        };
        assert_eq!(p(720, 480, (8, 9)).display_size(), (640, 480));
        assert_eq!(p(720, 480, (32, 27)).display_size(), (853, 480));
        assert_eq!(p(720, 576, (16, 15)).display_size(), (768, 576));
        assert_eq!(p(720, 576, (64, 45)).display_size(), (1024, 576));
        assert_eq!(p(1920, 1080, (1, 1)).display_size(), (1920, 1080));
        assert_eq!(p(0, 0, (1, 1)).display_size(), (1, 1));
        assert_eq!(p(100, 10, (0, 5)).display_size(), (100, 10));
        assert_eq!(p(100, 10, (1000, 1)).display_size(), (400, 10));
        assert_eq!(p(100, 10, (1, 1000)).display_size(), (25, 10));
        assert_eq!(p(1, 1, (1, 4)).display_size(), (1, 1));
        assert_eq!(
            p(u32::MAX, u32::MAX, (u32::MAX, 1)).display_size(),
            (u32::MAX, u32::MAX)
        );
    }
}
