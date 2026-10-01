//! Cross-check of the MPEG-1 / MPEG-2 intra syntax and VLC tables.
//!
//! `tests/support/h262_stream.rs` holds an independent transcription of the
//! tables of ISO/IEC 13818-2 Annex B and ISO/IEC 11172-2, a writer for intra
//! elementary streams and a reference reconstruction. Three layers use it:
//!
//! - `table_*`: self-checks of the transcribed tables (prefix-free codes,
//!   Kraft sums, scan permutations, internal consistency).
//! - `ffmpeg_*`: streams from the writer are decoded by ffmpeg, which must
//!   report no error and agree with the reference reconstruction within ±1.
//!   This validates the transcription against a decoder that is known to work.
//! - `decoder_*`: the same streams are decoded by `mpeg2::decode_intra`, which
//!   must agree with ffmpeg within ±1 per sample.
//!
//! The ffmpeg layers look ffmpeg up on PATH and skip (with a message) when it
//! is missing. Many cases are packed into one picture (one macroblock per
//! case) to keep the number of ffmpeg runs low.

extern crate IsoPreview as iso_preview;

#[path = "support/h262_stream.rs"]
mod h262;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use h262::{
    code_bits, dequantise, encode, idct_basis, idct_exact, lookup, max_level, random_block,
    reconstruct, scan_order, Block, Coef, Concealment, Macroblock, Picture, Planes, Rng, Slice,
    Structure, ALTERNATE_FIGURE, B14, B14_EOB, B14_FIRST_RUN0_LEVEL1, B15, B15_EOB, DCT_ESCAPE,
    DC_SIZE_CHROMA, DC_SIZE_LUMA, DEFAULT_INTRA_MATRIX, MBA_ESCAPE, MBA_INCREMENT, MBA_STUFFING,
    MB_TYPE_INTRA, MB_TYPE_INTRA_QUANT, MOTION_CODE, NON_LINEAR_QSCALE, SAMPLE_MARGIN,
    ZIGZAG_FIGURE,
};

// ----------------------------------------------------------------------------
// Table self-checks
// ----------------------------------------------------------------------------

/// Expands a table of codes into (bits, length) codewords; a `signed` entry
/// is followed by a sign bit and stands for two codewords.
fn expand(codes: &[&str], signed: bool) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(codes.len() * 2);
    for c in codes {
        let (v, l) = code_bits(c);
        if signed {
            out.push((v << 1, l + 1));
            out.push((v << 1 | 1, l + 1));
        } else {
            out.push((v, l));
        }
    }
    out
}

/// Asserts that no code is a prefix of another (sign bits excluded: a code and
/// its sign bit are one codeword, and adding the same suffix to every signed
/// code does not change prefix relations with unsigned ones that are longer).
fn assert_prefix_free(name: &str, codes: &[&str]) {
    let bits: Vec<(u32, u32)> = codes.iter().map(|c| code_bits(c)).collect();
    for (i, &(a, la)) in bits.iter().enumerate() {
        for (j, &(b, lb)) in bits.iter().enumerate() {
            if i != j && la <= lb && b >> (lb - la) == a {
                panic!("{name}: {:?} is a prefix of {:?}", codes[i], codes[j]);
            }
        }
    }
}

/// Kraft sum of the codes in units of 2^-24.
fn kraft(codes: &[(u32, u32)]) -> u64 {
    codes.iter().map(|&(_, l)| 1u64 << (24 - l)).sum()
}

const ONE: u64 = 1 << 24;

#[test]
fn table_macroblock_address_increment() {
    let mut codes: Vec<&str> = MBA_INCREMENT.to_vec();
    codes.push(MBA_ESCAPE);
    assert_prefix_free("B.1", &codes);
    // Incomplete: 0000 0000 xxx (start code emulation), 0000 0010 xxx and
    // 0000 0001 001..110 are unused. In units of 2^-11: 2024 for the 33
    // increments, 1 for the escape.
    assert_eq!(kraft(&expand(&codes, false)), 2025 * (ONE >> 11));
    codes.push(MBA_STUFFING);
    assert_prefix_free("B.1 + MPEG-1 stuffing", &codes);
    assert_eq!(kraft(&expand(&codes, false)), 2026 * (ONE >> 11));
}

#[test]
fn table_macroblock_type_i() {
    let codes = [MB_TYPE_INTRA, MB_TYPE_INTRA_QUANT];
    assert_prefix_free("B.2", &codes);
    // Incomplete: "00" is forbidden in I-pictures.
    assert_eq!(kraft(&expand(&codes, false)), 3 * (ONE >> 2));
}

#[test]
fn table_motion_code() {
    assert_prefix_free("B.10", &MOTION_CODE);
    // Incomplete in the same places as B.1 (escape, stuffing and the
    // 0000 0000 / 0000 0010 / 0000 0001 prefixes are unused).
    assert_eq!(kraft(&expand(&MOTION_CODE, false)), 2024 * (ONE >> 11));
    // Two independent transcriptions describe the same tree: motion_code 0 is
    // increment 1, -k is increment 2k and +k is increment 2k + 1.
    for k in 0..=16usize {
        assert_eq!(
            code_bits(MOTION_CODE[16 + k]),
            code_bits(MBA_INCREMENT[2 * k])
        );
        if k > 0 {
            assert_eq!(
                code_bits(MOTION_CODE[16 - k]),
                code_bits(MBA_INCREMENT[2 * k - 1])
            );
            // The last bit is the sign: 0 positive, 1 negative.
            let (pos, lp) = code_bits(MOTION_CODE[16 + k]);
            let (neg, ln) = code_bits(MOTION_CODE[16 - k]);
            assert_eq!((lp, pos & 1, neg & 1, pos | 1), (ln, 0, 1, neg));
        }
    }
}

#[test]
fn table_dc_size() {
    assert_prefix_free("B.12", &DC_SIZE_LUMA);
    assert_prefix_free("B.13", &DC_SIZE_CHROMA);
    // Both tables are complete.
    assert_eq!(kraft(&expand(&DC_SIZE_LUMA, false)), ONE);
    assert_eq!(kraft(&expand(&DC_SIZE_CHROMA, false)), ONE);
    // MPEG-1 (11172-2 B.5a/b) stops at size 8: its longest codes are the
    // MPEG-2 codes with the last bit of a run of ones dropped.
    assert_eq!(code_bits(DC_SIZE_LUMA[8]), (0b111_1110, 7));
    assert_eq!(code_bits(DC_SIZE_CHROMA[8]), (0b1111_1110, 8));
}

fn dct_codes(table: &[(&'static str, u8, u8)]) -> Vec<&'static str> {
    table.iter().map(|&(c, _, _)| c).collect()
}

#[test]
fn table_dct_coefficients_b14() {
    // Subsequent coefficients (and all coefficients of intra blocks).
    let mut codes = dct_codes(&B14);
    codes.push(B14_EOB);
    codes.push(DCT_ESCAPE);
    assert_prefix_free("B.14", &codes);
    let signed = expand(&dct_codes(&B14), true);
    let unsigned = expand(&[B14_EOB, DCT_ESCAPE], false);
    // Complete except the all-zero 12-bit prefix (start code emulation).
    assert_eq!(kraft(&signed) + kraft(&unsigned), ONE - (ONE >> 12));

    // First coefficient of a non-intra block: "1s" replaces "10" and "11s".
    let mut first: Vec<&str> = codes
        .iter()
        .copied()
        .filter(|&c| c != "11" && c != B14_EOB)
        .collect();
    first.push(B14_FIRST_RUN0_LEVEL1);
    assert_prefix_free("B.14 first coefficient", &first);
    assert_eq!(first.len(), B14.len() + 1);
}

#[test]
fn table_dct_coefficients_b15() {
    let mut codes = dct_codes(&B15);
    codes.push(B15_EOB);
    codes.push(DCT_ESCAPE);
    assert_prefix_free("B.15", &codes);
    let signed = expand(&dct_codes(&B15), true);
    let unsigned = expand(&[B15_EOB, DCT_ESCAPE], false);
    // B.15 gives (0, 8)..(0, 15), (1, 5) and (2, 4) short codes and leaves
    // their B.14 codes unused: six 12-bit and four 13-bit codes. With the
    // all-zero 12-bit prefix that is 9 · 2^-12 missing.
    assert_eq!(kraft(&signed) + kraft(&unsigned), ONE - 9 * (ONE >> 12));
    for (code, _, _) in B14.iter().filter(|&&(_, r, l)| {
        (r == 0 && (8..=15).contains(&l)) || (r, l) == (1, 5) || (r, l) == (2, 4)
    }) {
        assert!(!codes.contains(code), "{code} reused in B.15");
    }
}

#[test]
fn table_dct_b14_b15_same_pairs() {
    // Both tables code the same 111 (run, level) pairs; everything else
    // takes the escape.
    let pairs =
        |t: &[(&str, u8, u8)]| -> HashSet<(u8, u8)> { t.iter().map(|&(_, r, l)| (r, l)).collect() };
    let a = pairs(&B14);
    assert_eq!(a.len(), 111);
    assert_eq!(a, pairs(&B15));
    // Every run 0..=31 has level 1; the maximum level per run (Table B.14):
    let max = |run: u8| a.iter().filter(|p| p.0 == run).map(|p| p.1).max().unwrap();
    let expected = [
        40, 18, 5, 4, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1,
    ];
    for (run, &m) in expected.iter().enumerate() {
        assert_eq!(max(run as u8), m, "run {run}");
        for level in 1..=m {
            assert!(a.contains(&(run as u8, level)), "missing ({run}, {level})");
        }
    }
    // Long codes (13 bits and more) are shared between the tables.
    for &(code, run, level) in &B15 {
        if code_bits(code).1 >= 13 {
            assert_eq!(lookup(&B14, run, i32::from(level)), Some(code));
        }
    }
}

#[test]
fn table_scans() {
    let zigzag = scan_order(&ZIGZAG_FIGURE);
    let alternate = scan_order(&ALTERNATE_FIGURE);
    // Zigzag generated independently: walk the anti-diagonals, alternating
    // direction, starting to the right.
    let mut generated = Vec::with_capacity(64);
    for d in 0..15usize {
        let cells: Vec<usize> = (0..8)
            .filter_map(|v: usize| d.checked_sub(v).filter(|&u| u < 8).map(|u| v * 8 + u))
            .collect();
        if d % 2 == 0 {
            generated.extend(cells.iter().rev()); // bottom-left to top-right
        } else {
            generated.extend(cells.iter());
        }
    }
    assert_eq!(zigzag.to_vec(), generated);
    assert_eq!(&zigzag[..10], &[0, 1, 8, 16, 9, 2, 3, 10, 17, 24]);
    // Alternate scan: a permutation that goes down the first column first.
    assert_eq!(&alternate[..6], &[0, 8, 16, 24, 1, 9]);
    assert_eq!(alternate[63], 63);
}

#[test]
fn table_quantisation() {
    assert_eq!(DEFAULT_INTRA_MATRIX[0], 8);
    assert_eq!(DEFAULT_INTRA_MATRIX[63], 83);
    assert!(DEFAULT_INTRA_MATRIX[1..]
        .iter()
        .all(|&w| (16..=83).contains(&w)));
    assert_eq!(NON_LINEAR_QSCALE[1], 1);
    assert_eq!(NON_LINEAR_QSCALE[31], 112);
    assert!(NON_LINEAR_QSCALE[1..].windows(2).all(|w| w[0] < w[1]));
    // Step sizes double every 8 codes: 1, 2, 4, 8.
    for code in 1..31usize {
        let step = NON_LINEAR_QSCALE[code + 1] - NON_LINEAR_QSCALE[code];
        let expected = match code {
            1..=7 => 1,
            8..=15 => 2,
            16..=23 => 4,
            _ => 8,
        };
        assert_eq!(step, expected, "code {code}");
    }
}

// ----------------------------------------------------------------------------
// ffmpeg runner
// ----------------------------------------------------------------------------

fn ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| [dir.join("ffmpeg.exe"), dir.join("ffmpeg")])
        .find(|p| p.is_file())
}

/// A unique directory under the system temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "isopreview-vlc-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Decodes the first picture of `es` with ffmpeg. Any message at error level
/// counts as a failure. ffmpeg runs single-threaded: its slice threads split
/// the picture by macroblock rows and cut MPEG-1 slices that cross such a
/// boundary (then reporting "Warning MVs not available" and concealing).
fn ffmpeg_decode(
    ff: &Path,
    dir: &Path,
    name: &str,
    es: &[u8],
    w: usize,
    h: usize,
) -> Result<Planes, String> {
    let input = dir.join(format!("{name}.m2v"));
    let output = dir.join(format!("{name}.yuv"));
    std::fs::write(&input, es).map_err(|e| e.to_string())?;
    let run = Command::new(ff)
        .args(["-nostdin", "-hide_banner", "-v", "error", "-threads", "1"])
        .args(["-f", "mpegvideo", "-i"])
        .arg(&input)
        .args([
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&output)
        .output()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&run.stderr);
    if !run.status.success() || !stderr.trim().is_empty() {
        return Err(format!("ffmpeg failed ({}): {}", run.status, stderr.trim()));
    }
    let raw = std::fs::read(&output).map_err(|e| e.to_string())?;
    let mut planes = Planes::new(w, h);
    let (ly, lc) = (planes.y.len(), planes.cb.len());
    if raw.len() < ly + 2 * lc {
        return Err(format!(
            "ffmpeg produced {} bytes, expected {}",
            raw.len(),
            ly + 2 * lc
        ));
    }
    planes.y.copy_from_slice(&raw[..ly]);
    planes.cb.copy_from_slice(&raw[ly..ly + lc]);
    planes.cr.copy_from_slice(&raw[ly + lc..ly + 2 * lc]);
    Ok(planes)
}

/// Rows of the frame a picture codes (all, or one field's).
fn coded_row(pic: &Picture, row: usize) -> bool {
    pic.structure.parity().is_none_or(|p| row % 2 == p)
}

/// Label of the macroblock covering sample (x, y) of plane `plane`.
fn mb_label(pic: &Picture, plane: usize, x: usize, y: usize) -> String {
    let (mx, my) = if plane == 0 {
        (x / 16, y / 16)
    } else {
        (x / 8, y / 8)
    };
    let my = if pic.structure == Structure::Frame {
        my
    } else {
        my / 2
    };
    let mbw = pic.mb_width() as usize;
    let addr = my * mbw + mx;
    for s in &pic.slices {
        let start = (s.row * pic.mb_width() + s.col) as usize;
        if (start..start + s.mbs.len()).contains(&addr) {
            let mb = &s.mbs[addr - start];
            return format!("mb ({mx}, {my}) quant {:?} {}", mb.quant, mb.label);
        }
    }
    format!("mb ({mx}, {my}) not coded")
}

/// Compares the coded rows of two decodings within ±1.
fn compare(pic: &Picture, what: &str, want: &Planes, got: &Planes) -> Result<(), String> {
    let mut bad = 0usize;
    let mut first = Vec::new();
    for plane in 0..3 {
        let (a, w, h) = want.plane(plane);
        let (b, _, _) = got.plane(plane);
        for y in 0..h {
            // A chroma row belongs to the field of its own parity.
            if !coded_row(pic, y) {
                continue;
            }
            for x in 0..w {
                let (va, vb) = (a[y * w + x], b[y * w + x]);
                if va.abs_diff(vb) > 1 {
                    bad += 1;
                    if first.len() < 8 {
                        first.push(format!(
                            "  {} ({x}, {y}): want {va} got {vb} [{}]",
                            ["Y", "Cb", "Cr"][plane],
                            mb_label(pic, plane, x, y)
                        ));
                    }
                }
            }
        }
    }
    if bad == 0 {
        Ok(())
    } else {
        Err(format!(
            "{what}: {bad} samples differ by more than 1\n{}",
            first.join("\n")
        ))
    }
}

struct Case {
    name: String,
    pic: Picture,
}

fn case(name: impl Into<String>, pic: Picture) -> Case {
    Case {
        name: name.into(),
        pic,
    }
}

/// Layer 1: ffmpeg against the reference reconstruction.
fn layer1(suite: &str, cases: Vec<Case>) {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping {suite}: ffmpeg not found on PATH");
        return;
    };
    let dir = TempDir::new(suite);
    let mut failures = Vec::new();
    for c in &cases {
        let es = encode(&c.pic);
        let want = reconstruct(&c.pic);
        let result = ffmpeg_decode(&ff, &dir.0, &c.name, &es, want.width, want.height)
            .and_then(|got| compare(&c.pic, "ffmpeg vs reference", &want, &got));
        if let Err(e) = result {
            failures.push(format!("{}: {e}", c.name));
        }
    }
    assert!(failures.is_empty(), "{suite}:\n{}", failures.join("\n"));
}

/// Layer 2: `decode_intra` against ffmpeg.
fn layer2(suite: &str, cases: Vec<Case>) {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping {suite}: ffmpeg not found on PATH");
        return;
    };
    let dir = TempDir::new(suite);
    let mut failures = Vec::new();
    for c in &cases {
        let es = encode(&c.pic);
        let (w, h) = (c.pic.width as usize, c.pic.height as usize);
        let result = ffmpeg_decode(&ff, &dir.0, &c.name, &es, w, h).and_then(|want| {
            let frame = iso_preview::mpeg2::decode_intra(&es)
                .map_err(|e| format!("decode_intra failed: {e:?}"))?;
            check_frame(&c.pic, &frame)?;
            let got = Planes {
                width: w,
                height: h,
                y: frame.y,
                cb: frame.cb,
                cr: frame.cr,
            };
            compare(&c.pic, "decode_intra vs ffmpeg", &want, &got)?;
            if c.pic.structure != Structure::Frame {
                check_line_doubled(&got)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            failures.push(format!("{}: {e}", c.name));
        }
    }
    assert!(failures.is_empty(), "{suite}:\n{}", failures.join("\n"));
}

/// Checks the frame description against the stream.
fn check_frame(pic: &Picture, f: &iso_preview::mpeg2::Frame) -> Result<(), String> {
    let field = pic.structure != Structure::Frame;
    let (w, h) = (pic.width as usize, pic.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let got = (
        f.width,
        f.height,
        f.y.len(),
        f.cb.len(),
        f.cr.len(),
        f.mpeg1,
        f.field_doubled,
        f.concealed_macroblocks,
        f.total_macroblocks,
    );
    let want = (
        pic.width,
        pic.height,
        w * h,
        cw * ch,
        cw * ch,
        pic.mpeg1,
        field,
        0,
        pic.total_macroblocks(),
    );
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "frame (width, height, |y|, |cb|, |cr|, mpeg1, field_doubled, concealed, total) = {got:?}, expected {want:?}"
        ))
    }
}

/// A field picture must come back with every line doubled.
fn check_line_doubled(p: &Planes) -> Result<(), String> {
    for plane in 0..3 {
        let (data, w, h) = p.plane(plane);
        for y in (0..h - 1).step_by(2) {
            if data[y * w..y * w + w] != data[(y + 1) * w..(y + 2) * w] {
                return Err(format!(
                    "{} rows {y} and {} differ: field not line-doubled",
                    ["Y", "Cb", "Cr"][plane],
                    y + 1
                ));
            }
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Stream construction
// ----------------------------------------------------------------------------

/// Fills every macroblock of the coded picture, one slice per macroblock row
/// with a random slice quantiser. `gen` gets the quantiser_scale_code in
/// force and returns the next macroblock. Concealment vectors and dct_type are
/// randomised where the picture has them and the macroblock leaves them open.
fn layout(
    pic: &mut Picture,
    rng: &mut Rng,
    mut gen: impl FnMut(&mut Rng, &Picture, u8) -> Macroblock,
) {
    let mut slices = Vec::new();
    for row in 0..pic.mb_rows() {
        let qcode = rng.range(1, 31) as u8;
        let mut q = qcode;
        let mut mbs = Vec::new();
        for _ in 0..pic.mb_width() {
            let mut mb = gen(rng, pic, q);
            decorate(&mut mb, pic, rng);
            if let Some(code) = mb.quant {
                q = code;
            }
            mbs.push(mb);
        }
        slices.push(Slice {
            row,
            col: 0,
            qcode,
            mbs,
        });
    }
    pic.slices = slices;
}

fn decorate(mb: &mut Macroblock, pic: &Picture, rng: &mut Rng) {
    if !pic.mpeg1 && pic.structure == Structure::Frame && !pic.frame_pred_frame_dct {
        mb.field_dct = rng.coin();
    }
    if !pic.mpeg1 && pic.concealment && mb.mv.is_none() {
        mb.mv = Some(random_mv(rng, pic));
    }
}

fn random_mv(rng: &mut Rng, pic: &Picture) -> Concealment {
    let mut mv = Concealment {
        field_select: rng.coin(),
        ..Concealment::default()
    };
    for t in 0..2 {
        mv.codes[t] = rng.range(-16, 16);
        let f = u32::from(pic.f_code[t]);
        if f > 1 && mv.codes[t] != 0 {
            mv.residuals[t] = rng.range(0, (1 << (f - 1)) - 1) as u32;
        }
    }
    mv
}

/// A macroblock of random blocks for the quantiser in force.
fn filler(rng: &mut Rng, pic: &Picture, qcode: u8, max_coefs: usize) -> Macroblock {
    let qs = pic.qscale(qcode);
    Macroblock {
        blocks: core::array::from_fn(|_| {
            let n = rng.range(0, max_coefs as i32) as usize;
            random_block(rng, pic, qs, n)
        }),
        label: "filler".into(),
        ..Macroblock::default()
    }
}

/// Quantiser code that puts the largest coefficient of `block` near 400
/// after dequantisation (well visible, far from saturation) while every
/// unclamped sample stays within `SAMPLE_MARGIN` of 0..=255.
fn pick_qcode(pic: &Picture, block: &Block) -> u8 {
    let scan = pic.scan();
    let basis = idct_basis();
    let mut pos = 0usize;
    let mut cells = Vec::new();
    for c in &block.ac {
        pos += usize::from(c.run) + 1;
        cells.push((scan[pos], c.level));
    }
    let peak = |code: u8| {
        let qs = pic.qscale(code);
        cells
            .iter()
            .map(|&(raster, level)| pic.dequant_magnitude(level, raster, qs))
            .max()
            .unwrap_or(0)
    };
    let fits = |code: u8| {
        let out = idct_exact(&dequantise(pic, block, pic.qscale(code)), &basis);
        peak(code) <= 2047
            && out
                .iter()
                .all(|s| (-SAMPLE_MARGIN..=255.0 + SAMPLE_MARGIN).contains(s))
    };
    (1..=31u8)
        .filter(|&c| fits(c))
        .min_by_key(|&c| (peak(c) - 400).abs())
        .unwrap_or_else(|| panic!("no quantiser keeps {:?} in range", block.ac))
}

/// One macroblock per case: the case's coefficients go into block `index % 6`
/// (so luma and chroma blocks both see every code), the other blocks are flat.
fn case_mbs(pic: &Picture, rng: &mut Rng, cases: Vec<(String, Vec<Coef>)>) -> Vec<Macroblock> {
    let mid = pic.dc_mid();
    cases
        .into_iter()
        .enumerate()
        .map(|(i, (label, coefs))| {
            let mut blocks: [Block; 6] = core::array::from_fn(|_| Block {
                dc: (mid + rng.range(-mid / 4, mid / 4)).clamp(0, pic.dc_max()),
                ac: Vec::new(),
            });
            blocks[i % 6].ac = coefs;
            let quant = Some(pick_qcode(pic, &blocks[i % 6]));
            Macroblock {
                quant,
                blocks,
                label: format!("{label} in block {}", i % 6),
                ..Macroblock::default()
            }
        })
        .collect()
}

/// Lays the cases out in raster order and fills the rest of the picture.
fn with_cases(mut pic: Picture, seed: u64, cases: Vec<(String, Vec<Coef>)>) -> Picture {
    let mut rng = Rng::new(seed);
    let mbs = case_mbs(&pic, &mut rng, cases);
    assert!(
        mbs.len() <= pic.total_macroblocks() as usize,
        "too many cases"
    );
    let mut it = mbs.into_iter();
    layout(&mut pic, &mut rng, |rng, pic, q| {
        it.next().unwrap_or_else(|| filler(rng, pic, q, 4))
    });
    pic
}

/// Every entry of a DCT table with both signs, coded with the table.
fn table_cases(table: &[(&str, u8, u8)]) -> Vec<(String, Vec<Coef>)> {
    let mut cases = Vec::new();
    for &(code, run, level) in table {
        for sign in [1, -1] {
            let level = sign * i32::from(level);
            cases.push((
                format!("({run}, {level}) {code}"),
                vec![Coef::vlc(run, level)],
            ));
        }
    }
    cases
}

/// Escapes for every run that fits after the DC term (0..=62: a run of 63
/// needs a first coefficient at scan position 0, which only non-intra blocks
/// have) and the given levels with both signs.
fn escape_cases(levels: &[i32]) -> Vec<(String, Vec<Coef>)> {
    let mut cases = Vec::new();
    for run in 0..=62u8 {
        for &level in levels {
            for level in [level, -level] {
                cases.push((
                    format!("escape ({run}, {level})"),
                    vec![Coef::esc(run, level)],
                ));
            }
        }
    }
    cases
}

/// A matrix whose AC weights are all `w` (DC weight 8, as ffmpeg insists).
fn flat_matrix(w: u8) -> [u8; 64] {
    let mut m = [w; 64];
    m[0] = 8;
    m
}

fn random_matrix(rng: &mut Rng, min: i32) -> [u8; 64] {
    let mut m = [0u8; 64];
    for w in m.iter_mut() {
        *w = rng.range(min, 255) as u8;
    }
    m[0] = 8;
    m
}

/// DC-only picture whose differentials walk through every dct_dc_size with
/// both signs (size 0 has no sign) for luma and chroma. Panics if the walk
/// misses one.
fn dc_walk(mut pic: Picture, seed: u64) -> Picture {
    let mut rng = Rng::new(seed);
    let max_size = 8 + if pic.mpeg1 {
        0
    } else {
        i32::from(pic.dc_precision)
    };
    let targets: Vec<(i32, i32)> = std::iter::once((0, 1))
        .chain((1..=max_size).flat_map(|s| [(s, 1), (s, -1)]))
        .collect();
    let mut seen: HashSet<(bool, i32, i32)> = HashSet::new();
    let mut next = [0usize; 3];
    let (mid, max) = (pic.dc_mid(), pic.dc_max());
    let mut slices = Vec::new();
    for row in 0..pic.mb_rows() {
        let mut pred = [mid; 3];
        let mut mbs = Vec::new();
        for _ in 0..pic.mb_width() {
            let mut mb = Macroblock {
                label: "dc walk".into(),
                ..Macroblock::default()
            };
            for (b, block) in mb.blocks.iter_mut().enumerate() {
                let comp = if b < 4 { 0 } else { b - 3 };
                let (size, sign) = targets[next[comp] % targets.len()];
                next[comp] += 1;
                let p = pred[comp];
                let dc = if size == 0 {
                    p
                } else {
                    let lo = 1 << (size - 1);
                    let hi = (1 << size) - 1;
                    let up = p + lo <= max;
                    let down = p - lo >= 0;
                    let positive = if sign > 0 { up } else { !down };
                    if positive {
                        p + rng.range(lo, hi.min(max - p))
                    } else {
                        p - rng.range(lo, hi.min(p))
                    }
                };
                let diff = dc - p;
                seen.insert((
                    comp == 0,
                    32 - diff.unsigned_abs().leading_zeros() as i32,
                    diff.signum(),
                ));
                pred[comp] = dc;
                block.dc = dc;
            }
            decorate(&mut mb, &pic, &mut rng);
            mbs.push(mb);
        }
        slices.push(Slice {
            row,
            col: 0,
            qcode: rng.range(1, 31) as u8,
            mbs,
        });
    }
    for luma in [true, false] {
        assert!(seen.contains(&(luma, 0, 0)));
        for size in 1..=max_size {
            for sign in [1, -1] {
                assert!(
                    seen.contains(&(luma, size, sign)),
                    "dc walk missed size {size} sign {sign}"
                );
            }
        }
    }
    pic.slices = slices;
    pic
}

/// Slices that start at every column of a wide picture: row r starts slices
/// at column 0 and at each column c > 0 with c % rows == r, so the first
/// macroblock_address_increment of a slice takes every value 1..=mb_width
/// (with up to three macroblock_escapes at 1920 samples).
fn mba_layout(mut pic: Picture, seed: u64) -> Picture {
    let mut rng = Rng::new(seed);
    let (mbw, rows) = (pic.mb_width(), pic.mb_rows());
    let mut slices = Vec::new();
    for row in 0..rows {
        let mut starts: Vec<u32> = vec![0];
        starts.extend((1..mbw).filter(|c| c % rows == row));
        for (i, &col) in starts.iter().enumerate() {
            let end = starts.get(i + 1).copied().unwrap_or(mbw);
            let qcode = rng.range(1, 31) as u8;
            let mut q = qcode;
            let mut mbs = Vec::new();
            for _ in col..end {
                let mut mb = filler(&mut rng, &pic, q, 3);
                if rng.one_in(3) {
                    let code = rng.range(1, 31) as u8;
                    mb.quant = Some(code);
                    // Regenerate for the new quantiser.
                    let qs = pic.qscale(code);
                    for b in mb.blocks.iter_mut() {
                        let n = b.ac.len();
                        *b = random_block(&mut rng, &pic, qs, n);
                    }
                    q = code;
                }
                if pic.mpeg1 && rng.one_in(3) {
                    mb.stuffing = rng.range(1, 3) as u8;
                }
                decorate(&mut mb, &pic, &mut rng);
                mbs.push(mb);
            }
            mbs[0].label = format!("slice start increment {}", col + 1);
            slices.push(Slice {
                row,
                col,
                qcode,
                mbs,
            });
        }
    }
    let increments: HashSet<u32> = slices.iter().map(|s| s.col + 1).collect();
    assert_eq!(increments.len(), mbw as usize);
    pic.slices = slices;
    pic
}

// ----------------------------------------------------------------------------
// Suites
// ----------------------------------------------------------------------------

fn b14_entries() -> Vec<Case> {
    let pic = with_cases(Picture::mpeg2(720, 576), 14, table_cases(&B14));
    let mut nonlinear = Picture::mpeg2(720, 576);
    nonlinear.q_scale_type = true;
    nonlinear.dc_precision = 1;
    let nonlinear = with_cases(nonlinear, 141, table_cases(&B14));
    vec![case("b14_linear", pic), case("b14_nonlinear", nonlinear)]
}

fn b15_entries() -> Vec<Case> {
    let mut pic = Picture::mpeg2(720, 576);
    pic.intra_vlc_format = true;
    let linear = with_cases(pic.clone(), 15, table_cases(&B15));
    pic.q_scale_type = true;
    pic.dc_precision = 2;
    let nonlinear = with_cases(pic, 151, table_cases(&B15));
    vec![case("b15_linear", linear), case("b15_nonlinear", nonlinear)]
}

fn escapes() -> Vec<Case> {
    // With the non-linear scale quantiser_scale reaches 1. A flat weight of
    // 16 keeps small levels visible; large levels get a weight of 3 so that
    // even ±2047 dequantises to a value whose samples stay near 0..=255.
    let mut pic = Picture::mpeg2(720, 576);
    pic.q_scale_type = true;
    let mut out = Vec::new();
    for (ivf, small, large) in [
        (
            false,
            &[1, 40, 41, 127, 128][..],
            &[128, 255, 1024, 2046, 2047][..],
        ),
        (true, &[1, 2, 127][..], &[1024, 2047][..]),
    ] {
        pic.intra_vlc_format = ivf;
        let table = if ivf { "b15" } else { "b14" };
        let seed = 16 + 10 * u64::from(ivf);
        pic.seq_matrix = Some(flat_matrix(16));
        out.push(case(
            format!("escape_{table}_small"),
            with_cases(pic.clone(), seed, escape_cases(small)),
        ));
        pic.seq_matrix = Some(flat_matrix(3));
        out.push(case(
            format!("escape_{table}_large"),
            with_cases(pic.clone(), seed + 1, escape_cases(large)),
        ));
    }
    out
}

fn dc_sizes() -> Vec<Case> {
    (0..=3u8)
        .map(|p| {
            let mut pic = Picture::mpeg2(352, 288);
            pic.dc_precision = p;
            case(format!("dc_precision_{p}"), dc_walk(pic, 20 + u64::from(p)))
        })
        .collect()
}

fn macroblock_address() -> Vec<Case> {
    vec![
        case("mba_mpeg2", mba_layout(Picture::mpeg2(1920, 128), 30)),
        case(
            "mba_mpeg1_stuffing",
            mba_layout(Picture::mpeg1(1920, 128), 31),
        ),
    ]
}

fn quantiser_scale() -> Vec<Case> {
    [false, true]
        .into_iter()
        .map(|nonlinear| {
            let mut pic = Picture::mpeg2(720, 576);
            pic.q_scale_type = nonlinear;
            let mut rng = Rng::new(40 + u64::from(nonlinear));
            let mut slices = Vec::new();
            let mut n = 0u32;
            for row in 0..pic.mb_rows() {
                // Slice codes 1..=31 over the rows, MB codes 1..=31 over the
                // intra+quant macroblocks.
                let qcode = (row % 31 + 1) as u8;
                let mut q = qcode;
                let mut mbs = Vec::new();
                for col in 0..pic.mb_width() {
                    let quant = ((row + col) % 2 == 0).then(|| {
                        n += 1;
                        (n % 31 + 1) as u8
                    });
                    if let Some(code) = quant {
                        q = code;
                    }
                    let mut mb = filler(&mut rng, &pic, q, 6);
                    mb.quant = quant;
                    mb.label = format!("quantiser_scale_code {q}");
                    mbs.push(mb);
                }
                slices.push(Slice {
                    row,
                    col: 0,
                    qcode,
                    mbs,
                });
            }
            pic.slices = slices;
            case(
                if nonlinear {
                    "qscale_nonlinear"
                } else {
                    "qscale_linear"
                },
                pic,
            )
        })
        .collect()
}

fn alternate_scan() -> Vec<Case> {
    let mut pic = Picture::mpeg2(720, 576);
    pic.alternate_scan = true;
    // Every scan position, through the table and through escapes.
    let mut cases = table_cases(&B14);
    for run in 0..=62u8 {
        cases.push((format!("escape ({run}, 3)"), vec![Coef::esc(run, 3)]));
        cases.push((format!("escape ({run}, -5)"), vec![Coef::esc(run, -5)]));
    }
    let b14 = with_cases(pic.clone(), 50, cases);
    pic.intra_vlc_format = true;
    pic.q_scale_type = true;
    let b15 = with_cases(pic, 51, table_cases(&B15));
    vec![case("alternate_b14", b14), case("alternate_b15", b15)]
}

fn field_dct() -> Vec<Case> {
    let mut pic = Picture::mpeg2(720, 576);
    pic.frame_pred_frame_dct = false;
    let mut rng = Rng::new(60);
    layout(&mut pic, &mut rng, |rng, pic, q| filler(rng, pic, q, 12));
    let mut alt = Picture::mpeg2(720, 576);
    alt.frame_pred_frame_dct = false;
    alt.alternate_scan = true;
    alt.intra_vlc_format = true;
    alt.concealment = true;
    alt.f_code = [4, 6];
    layout(&mut alt, &mut rng, |rng, pic, q| filler(rng, pic, q, 12));
    vec![case("field_dct", pic), case("field_dct_alternate_cmv", alt)]
}

fn field_pictures() -> Vec<Case> {
    let mut rng = Rng::new(70);
    [Structure::Top, Structure::Bottom]
        .into_iter()
        .map(|structure| {
            let mut pic = Picture::mpeg2(720, 576);
            pic.structure = structure;
            pic.second_field = true;
            pic.concealment = true;
            pic.f_code = [2, 7];
            pic.dc_precision = 1;
            pic.intra_vlc_format = structure == Structure::Bottom;
            layout(&mut pic, &mut rng, |rng, pic, q| filler(rng, pic, q, 8));
            case(format!("field_{structure:?}").to_lowercase(), pic)
        })
        .collect()
}

fn concealment_vectors() -> Vec<Case> {
    (1..=9u8)
        .map(|f| {
            let mut pic = Picture::mpeg2(352, 64);
            pic.concealment = true;
            pic.f_code = [f, 10 - f];
            let mut rng = Rng::new(80 + u64::from(f));
            let mut i = 0i32;
            layout(&mut pic, &mut rng, |rng, pic, q| {
                let mut mb = filler(rng, pic, q, 3);
                // Horizontal codes walk -16..=16, vertical ones in another order.
                let codes = [i % 33 - 16, (i * 7 + 5) % 33 - 16];
                i += 1;
                let mut mv = Concealment {
                    codes,
                    ..Concealment::default()
                };
                let fields = mv.residuals.iter_mut().zip(codes).zip(pic.f_code);
                for ((residual, code), f) in fields {
                    if f > 1 && code != 0 {
                        *residual = rng.range(0, (1 << (f - 1)) - 1) as u32;
                    }
                }
                mb.label = format!("motion codes {codes:?} residuals {:?}", mv.residuals);
                mb.mv = Some(mv);
                mb
            });
            case(format!("concealment_f{f}"), pic)
        })
        .collect()
}

fn custom_matrices() -> Vec<Case> {
    let mut rng = Rng::new(90);
    let mut seq = Picture::mpeg2(720, 576);
    seq.seq_matrix = Some(random_matrix(&mut rng, 1));
    layout(&mut seq, &mut rng, |rng, pic, q| filler(rng, pic, q, 8));

    // The quant matrix extension replaces the sequence header's matrix.
    let mut ext = Picture::mpeg2(720, 576);
    ext.seq_matrix = Some(random_matrix(&mut rng, 1));
    ext.ext_matrix = Some(random_matrix(&mut rng, 1));
    ext.q_scale_type = true;
    layout(&mut ext, &mut rng, |rng, pic, q| filler(rng, pic, q, 8));

    // MPEG-1 weights stay >= 8 so no product rounds to 0 before
    // oddification (ffmpeg's `(level - 1) | 1` would make that -1, the
    // standard leaves it 0).
    let mut m1 = Picture::mpeg1(720, 576);
    m1.seq_matrix = Some(random_matrix(&mut rng, 8));
    layout(&mut m1, &mut rng, |rng, pic, q| filler(rng, pic, q, 8));
    vec![
        case("matrix_sequence", seq),
        case("matrix_extension", ext),
        case("matrix_mpeg1", m1),
    ]
}

fn mpeg1() -> Vec<Case> {
    let entries = with_cases(Picture::mpeg1(720, 576), 100, table_cases(&B14));
    let mut esc = Picture::mpeg1(720, 576);
    esc.seq_matrix = Some(flat_matrix(16));
    let esc = with_cases(esc, 101, escape_cases(&[1, 40, 127, 128, 200, 255]));
    let dc = dc_walk(Picture::mpeg1(352, 288), 102);

    // Slices of random length that run across macroblock rows, with stuffing.
    let mut long = Picture::mpeg1(720, 576);
    let mut rng = Rng::new(103);
    let total = long.total_macroblocks();
    let mbw = long.mb_width();
    let mut addr = 0u32;
    let mut slices = Vec::new();
    while addr < total {
        let len = (rng.range(1, 120) as u32).min(total - addr);
        let qcode = rng.range(1, 31) as u8;
        let mut q = qcode;
        let mut mbs = Vec::new();
        for _ in 0..len {
            if rng.one_in(4) {
                q = rng.range(1, 31) as u8;
                let mut mb = filler(&mut rng, &long, q, 6);
                mb.quant = Some(q);
                mbs.push(mb);
            } else {
                mbs.push(filler(&mut rng, &long, q, 6));
            }
            if rng.one_in(5) {
                mbs.last_mut().unwrap().stuffing = rng.range(1, 4) as u8;
            }
        }
        slices.push(Slice {
            row: addr / mbw,
            col: addr % mbw,
            qcode,
            mbs,
        });
        addr += len;
    }
    long.slices = slices;
    vec![
        case("mpeg1_b14", entries),
        case("mpeg1_escape", esc),
        case("mpeg1_dc", dc),
        case("mpeg1_long_slices", long),
    ]
}

fn random_pictures() -> Vec<Case> {
    let mut rng = Rng::new(110);
    (0..4)
        .map(|i| {
            let mut pic = Picture::mpeg2(720, 576);
            pic.dc_precision = rng.range(0, 3) as u8;
            pic.q_scale_type = rng.coin();
            pic.intra_vlc_format = rng.coin();
            pic.alternate_scan = rng.coin();
            pic.frame_pred_frame_dct = rng.coin();
            pic.concealment = rng.coin();
            pic.f_code = [rng.range(1, 9) as u8, rng.range(1, 9) as u8];
            if rng.coin() {
                pic.seq_matrix = Some(random_matrix(&mut rng, 4));
            }
            layout(&mut pic, &mut rng, |rng, pic, q| {
                let mut mb = filler(rng, pic, q, 16);
                if rng.one_in(4) {
                    let code = rng.range(1, 31) as u8;
                    let qs = pic.qscale(code);
                    mb.quant = Some(code);
                    for b in mb.blocks.iter_mut() {
                        let n = b.ac.len();
                        *b = random_block(rng, pic, qs, n);
                    }
                }
                mb
            });
            case(format!("random_{i}"), pic)
        })
        .collect()
}

macro_rules! crosscheck {
    ($suite:ident, $ffmpeg:ident, $decoder:ident) => {
        #[test]
        fn $ffmpeg() {
            layer1(stringify!($suite), $suite());
        }

        #[test]
        fn $decoder() {
            layer2(stringify!($suite), $suite());
        }
    };
}

crosscheck!(b14_entries, ffmpeg_b14_entries, decoder_b14_entries);
crosscheck!(b15_entries, ffmpeg_b15_entries, decoder_b15_entries);
crosscheck!(escapes, ffmpeg_escapes, decoder_escapes);
crosscheck!(dc_sizes, ffmpeg_dc_sizes, decoder_dc_sizes);
crosscheck!(
    macroblock_address,
    ffmpeg_macroblock_address,
    decoder_macroblock_address
);
crosscheck!(
    quantiser_scale,
    ffmpeg_quantiser_scale,
    decoder_quantiser_scale
);
crosscheck!(
    alternate_scan,
    ffmpeg_alternate_scan,
    decoder_alternate_scan
);
crosscheck!(field_dct, ffmpeg_field_dct, decoder_field_dct);
crosscheck!(
    field_pictures,
    ffmpeg_field_pictures,
    decoder_field_pictures
);
crosscheck!(
    concealment_vectors,
    ffmpeg_concealment_vectors,
    decoder_concealment_vectors
);
crosscheck!(
    custom_matrices,
    ffmpeg_custom_matrices,
    decoder_custom_matrices
);
crosscheck!(mpeg1, ffmpeg_mpeg1, decoder_mpeg1);
crosscheck!(
    random_pictures,
    ffmpeg_random_pictures,
    decoder_random_pictures
);

/// The generators never produce a coefficient that the inverse quantiser has
/// to saturate: ffmpeg cannot serve as an oracle for that case, so the
/// ffmpeg comparisons must not depend on it.
#[test]
fn table_generated_levels_never_saturate() {
    for suite in [
        b14_entries(),
        escapes(),
        mpeg1(),
        random_pictures(),
        custom_matrices(),
    ] {
        for c in suite {
            let p = &c.pic;
            let scan = p.scan();
            for s in &p.slices {
                let mut q = s.qcode;
                for mb in &s.mbs {
                    q = mb.quant.unwrap_or(q);
                    for b in &mb.blocks {
                        let mut pos = 0;
                        for coef in &b.ac {
                            pos += usize::from(coef.run) + 1;
                            let m = p.dequant_magnitude(coef.level, scan[pos], p.qscale(q));
                            assert!(m <= 2047, "{}: {} {:?} -> {m}", c.name, mb.label, coef);
                            assert!(coef.level.abs() <= max_level(p, scan[pos], p.qscale(q)));
                        }
                    }
                }
            }
        }
    }
}
