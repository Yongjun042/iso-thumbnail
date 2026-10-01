//! 8×8 inverse DCT for intra blocks.
//!
//! A separable floating-point transform: well inside the IEEE 1180-1990
//! accuracy limits (single precision keeps the error near 1e-4 of a sample),
//! and sparse, because intra blocks of real pictures have most of their
//! coefficients in a few rows. Work is proportional to the non-zero
//! coefficients and rows; a DC-only block is a plain fill.

use std::sync::OnceLock;

/// `BASIS[k][n]` = C(k)/2 · cos((2n + 1)kπ/16): the contribution of frequency
/// `k` to sample `n` in one dimension.
fn basis() -> &'static [[f32; 8]; 8] {
    static BASIS: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    BASIS.get_or_init(|| {
        let mut b = [[0f32; 8]; 8];
        for (k, row) in b.iter_mut().enumerate() {
            let c = if k == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            for (n, v) in row.iter_mut().enumerate() {
                let angle = ((2 * n + 1) * k) as f64 * core::f64::consts::PI / 16.0;
                *v = (c / 2.0 * angle.cos()) as f32;
            }
        }
        b
    })
}

/// Basis table handle, fetched once per picture.
#[derive(Clone, Copy)]
pub struct Idct {
    basis: &'static [[f32; 8]; 8],
}

impl Idct {
    pub fn new() -> Self {
        Idct { basis: basis() }
    }

    /// Transforms `coef` (raster order, `rows` = bit mask of the rows holding
    /// non-zero coefficients) and stores the clamped samples into an 8×8 area
    /// of `out` starting at `offset` with row step `stride`. Clears `coef`
    /// for the next block.
    #[inline]
    pub fn put(
        &self,
        coef: &mut [i32; 64],
        rows: u8,
        out: &mut [u8],
        offset: usize,
        stride: usize,
    ) {
        if rows == 1 && coef[1..8].iter().all(|&c| c == 0) {
            // DC only: f(x, y) = F(0, 0) / 8 everywhere.
            let v = clamp_round(coef[0] as f32 * 0.125);
            coef[0] = 0;
            for y in 0..8 {
                let at = offset + y * stride;
                out[at..at + 8].fill(v);
            }
            return;
        }

        let b = self.basis;
        let mut acc = [[0f32; 8]; 8];
        for v in 0..8 {
            if rows & (1 << v) == 0 {
                continue;
            }
            // Horizontal pass over one row of frequencies.
            let row = &mut coef[v * 8..v * 8 + 8];
            let mut t = [0f32; 8];
            for (u, c) in row.iter_mut().enumerate() {
                if *c != 0 {
                    let c32 = *c as f32;
                    *c = 0;
                    let bu = &b[u];
                    for x in 0..8 {
                        t[x] += bu[x] * c32;
                    }
                }
            }
            // Vertical pass: spread the row over the output lines.
            let bv = &b[v];
            for (y, line) in acc.iter_mut().enumerate() {
                let k = bv[y];
                for x in 0..8 {
                    line[x] += k * t[x];
                }
            }
        }
        for (y, line) in acc.iter().enumerate() {
            let at = offset + y * stride;
            let dst = &mut out[at..at + 8];
            for x in 0..8 {
                dst[x] = clamp_round(line[x]);
            }
        }
    }
}

/// Rounds to the nearest integer (halves up) and clamps to 0..=255.
#[inline(always)]
fn clamp_round(v: f32) -> u8 {
    // `as` saturates, and truncation equals floor for the non-negative values
    // that survive the clamp.
    ((v + 0.5) as i32).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference IDCT straight from the definition, in f64.
    fn reference(coef: &[i32; 64]) -> [f64; 64] {
        let mut out = [0f64; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0f64;
                for v in 0..8 {
                    for u in 0..8 {
                        let cu = if u == 0 { 0.5f64.sqrt() } else { 1.0 };
                        let cv = if v == 0 { 0.5f64.sqrt() } else { 1.0 };
                        s += cu
                            * cv
                            * f64::from(coef[v * 8 + u])
                            * (((2 * x + 1) * u) as f64 * core::f64::consts::PI / 16.0).cos()
                            * (((2 * y + 1) * v) as f64 * core::f64::consts::PI / 16.0).cos();
                    }
                }
                out[y * 8 + x] = s / 4.0;
            }
        }
        out
    }

    /// IEEE 1180-1990 style accuracy check: random coefficient blocks in the
    /// standard's ranges around a mid-grey DC, rounded and clamped the same way
    /// on both sides.
    #[test]
    fn accuracy_against_reference() {
        let idct = Idct::new();
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut max_err = 0f64;
        let mut errors = 0u32;
        let mut sq_err = 0f64;
        let trials = 10_000;
        for trial in 0..trials {
            let range = [256i64, 5, 300][trial % 3];
            let mut coef = [0i32; 64];
            let mut rows = 0u8;
            for (i, c) in coef.iter_mut().enumerate() {
                // Sparse in most trials, dense in some.
                if trial % 4 == 0 || next() % 5 == 0 {
                    *c = ((next() % (2 * range as u64 + 1)) as i64 - range) as i32 / 4;
                    if *c != 0 {
                        rows |= 1 << (i / 8);
                    }
                }
            }
            // Keep the samples away from the clamp limits.
            coef[0] = 1024;
            rows |= 1;
            let reference = reference(&coef);
            let mut out = [0u8; 64];
            idct.put(&mut coef.clone(), rows, &mut out, 0, 8);
            for (o, r) in out.iter().zip(reference.iter()) {
                let want = (r + 0.5).floor().clamp(0.0, 255.0);
                let err = f64::from(*o) - want;
                if err != 0.0 {
                    errors += 1;
                }
                sq_err += err * err;
                max_err = max_err.max(err.abs());
            }
        }
        let mse = sq_err / (trials as f64 * 64.0);
        assert!(max_err <= 1.0, "peak error {max_err}");
        // IEEE 1180 allows an overall mean square error of 0.02; a correct
        // float transform only differs from the reference on rounding ties.
        assert!(mse <= 0.002, "mse {mse}, {errors} samples off");
    }

    #[test]
    fn dc_only_fills_block_and_clears() {
        let idct = Idct::new();
        let mut coef = [0i32; 64];
        coef[0] = 8 * 77;
        let mut out = [0u8; 16 * 8];
        idct.put(&mut coef, 1, &mut out, 4, 16);
        for y in 0..8 {
            assert_eq!(&out[y * 16..y * 16 + 4], &[0; 4]);
            assert_eq!(&out[y * 16 + 4..y * 16 + 12], &[77; 8]);
        }
        assert!(coef.iter().all(|&c| c == 0));
    }

    #[test]
    fn sparse_rows_clear_coefficients() {
        let idct = Idct::new();
        let mut coef = [0i32; 64];
        coef[0] = 1024;
        coef[63] = 1;
        coef[17] = -40;
        let rows = 1 | 1 << 7 | 1 << 2;
        let want = reference(&coef);
        let mut out = [0u8; 64];
        idct.put(&mut coef, rows, &mut out, 0, 8);
        assert!(coef.iter().all(|&c| c == 0));
        for (o, r) in out.iter().zip(want.iter()) {
            assert!((f64::from(*o) - r).abs() <= 0.5 + 1e-3);
        }
    }
}
