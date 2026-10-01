//! Checks the intra MPEG-1/2 decoder against ffmpeg.
//!
//! Every case encodes a picture with ffmpeg, decodes its first frame with
//! both ffmpeg (`-f rawvideo -pix_fmt yuv420p`) and `decode_intra`, and
//! compares all three planes. The IDCTs differ (ffmpeg uses an integer one),
//! so samples may be off by a little; the limits are PSNR >= 48 dB and a
//! largest difference of 4 in every plane. Each case also checks that the
//! encoded stream really carries the coding tool it is named after. Run with
//! `--nocapture` to see the table of observed values.
//!
//! The streams of the test writer (`support/mpeg2_writer.rs`) are decoded by
//! ffmpeg too, which checks the writer's code tables independently of the
//! decoder's.
//!
//! The tests skip (with a message) when ffmpeg is not on PATH.

#[allow(dead_code)]
#[path = "support/mpeg2_writer.rs"]
mod mpeg2_writer;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use mpeg2_writer::{MbContent, Structure, WriterConfig};
use IsoPreview::mpeg2::{decode_intra, find_intra_picture, ColorMatrix, Frame};

const PSNR_LIMIT: f64 = 48.0;
const MAX_DIFF_LIMIT: u8 = 4;
const PHOTO: &str = r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg";

fn ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        ["ffmpeg.exe", "ffmpeg"]
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// A scratch directory under the system temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "isopreview-mpeg2-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug)]
enum Source {
    TestSrc2,
    Mandelbrot,
    Noise,
    Photo,
    /// testsrc2 with the two fields 80 levels apart: an interlaced encoder
    /// picks field DCT for most macroblocks.
    Combed,
    /// Two moving testsrc2 frames woven into one interlaced frame.
    Woven,
}

struct Case {
    name: String,
    codec: &'static str,
    source: Source,
    width: u32,
    height: u32,
    args: Vec<String>,
    /// Header fields the encoded stream must carry, so that every case is
    /// known to exercise what it is named after.
    expect: Vec<(&'static str, u32)>,
}

fn case(name: &str, codec: &'static str, source: Source, size: (u32, u32), args: &[&str]) -> Case {
    Case {
        name: name.to_string(),
        codec,
        source,
        width: size.0,
        height: size.1,
        args: args.iter().map(|s| s.to_string()).collect(),
        expect: Vec::new(),
    }
}

impl Case {
    fn expect(mut self, fields: &[(&'static str, u32)]) -> Case {
        self.expect.extend_from_slice(fields);
        self
    }
}

/// Reads `n` bits at bit offset `at` of `data` (zeros past the end).
fn bits_at(data: &[u8], at: usize, n: usize) -> u32 {
    (at..at + n).fold(0, |v, i| {
        let bit = data.get(i / 8).map_or(0, |b| (b >> (7 - i % 8)) & 1);
        (v << 1) | u32::from(bit)
    })
}

/// Header fields of the first sequence header, its extensions and the first
/// picture coding extension, read independently of the decoder.
fn stream_fields(es: &[u8]) -> Vec<(&'static str, u32)> {
    let mut fields = Vec::new();
    let mut seen_sequence = false;
    let mut seen_picture_coding = false;
    let mut i = 0;
    while let Some(found) = es[i..].windows(3).position(|w| w == [0, 0, 1]) {
        let at = i + found;
        let Some(&code) = es.get(at + 3) else { break };
        let p = &es[(at + 4).min(es.len())..];
        match code {
            0xB3 if !seen_sequence => {
                seen_sequence = true;
                fields.push(("aspect", bits_at(p, 24, 4)));
                fields.push(("load_intra_matrix", bits_at(p, 62, 1)));
            }
            0xB5 => match bits_at(p, 0, 4) {
                1 => fields.push(("progressive_sequence", bits_at(p, 12, 1))),
                2 => {
                    fields.push(("display_extension", 1));
                    if bits_at(p, 7, 1) == 1 {
                        fields.push(("matrix_coefficients", bits_at(p, 24, 8)));
                    }
                }
                8 if !seen_picture_coding => {
                    seen_picture_coding = true;
                    for (name, at, n) in [
                        ("intra_dc_precision", 20, 2),
                        ("picture_structure", 22, 2),
                        ("frame_pred_frame_dct", 25, 1),
                        ("q_scale_type", 27, 1),
                        ("intra_vlc_format", 28, 1),
                        ("alternate_scan", 29, 1),
                    ] {
                        fields.push((name, bits_at(p, at, n)));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        i = at + 3;
    }
    fields
}

fn input_args(source: Source, width: u32, height: u32) -> Option<Vec<String>> {
    let size = format!("{width}x{height}");
    let lavfi = |graph: String| vec!["-f".into(), "lavfi".into(), "-i".into(), graph];
    Some(match source {
        Source::TestSrc2 => lavfi(format!("testsrc2=size={size}:rate=25")),
        Source::Mandelbrot => lavfi(format!("mandelbrot=size={size}:rate=25")),
        Source::Noise => lavfi(format!(
            "nullsrc=size={size}:rate=25,format=yuv444p,\
             geq=lum=random(1)*255:cb=random(2)*255:cr=random(3)*255"
        )),
        Source::Combed => lavfi(format!(
            "testsrc2=size={size}:rate=25,format=yuv444p,\
             geq=lum='p(X,Y)+if(mod(Y,2),40,-40)':cb='p(X,Y)':cr='p(X,Y)'"
        )),
        Source::Woven => lavfi(format!(
            "testsrc2=size={size}:rate=50,tinterlace=mode=interleave_top"
        )),
        Source::Photo => {
            if !Path::new(PHOTO).is_file() {
                return None;
            }
            vec![
                "-i".into(),
                PHOTO.into(),
                "-vf".into(),
                format!("scale={width}:{height}"),
            ]
        }
    })
}

fn run(ffmpeg: &Path, args: &[String]) {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Encodes `frames` frames of the case; returns the elementary stream and its
/// path, or None when the source is not available.
fn encode(ffmpeg: &Path, dir: &TempDir, c: &Case, frames: u32) -> Option<(Vec<u8>, PathBuf)> {
    let es_path = dir.path(&format!("{}.m2v", sanitize(&c.name)));
    let mut args = input_args(c.source, c.width, c.height)?;
    args.extend(["-frames:v".into(), frames.to_string()]);
    args.extend(["-pix_fmt".into(), "yuv420p".into()]);
    args.extend(["-c:v".into(), c.codec.into()]);
    args.extend(c.args.iter().cloned());
    args.extend(["-f".into(), c.codec.into()]);
    args.push(es_path.to_string_lossy().into_owned());
    run(ffmpeg, &args);
    Some((std::fs::read(&es_path).expect("read es"), es_path))
}

/// ffmpeg's decoding of the first frame of `es_path` as planar 4:2:0.
fn reference(ffmpeg: &Path, dir: &TempDir, es_path: &Path) -> Vec<u8> {
    let raw = dir.path("reference.yuv");
    run(
        ffmpeg,
        &[
            "-i".into(),
            es_path.to_string_lossy().into_owned(),
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            raw.to_string_lossy().into_owned(),
        ],
    );
    std::fs::read(&raw).expect("read reference")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// PSNR and largest absolute difference of one plane.
fn plane_stats(a: &[u8], b: &[u8]) -> (f64, u8) {
    assert_eq!(a.len(), b.len(), "plane sizes differ");
    let mut sq = 0u64;
    let mut max = 0u8;
    for (x, y) in a.iter().zip(b) {
        let d = x.abs_diff(*y);
        max = max.max(d);
        sq += u64::from(d) * u64::from(d);
    }
    let mse = sq as f64 / a.len() as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    (psnr, max)
}

/// Splits a raw yuv420p frame into its planes.
fn split_planes(raw: &[u8], width: u32, height: u32) -> [&[u8]; 3] {
    let ylen = (width * height) as usize;
    let clen = (width.div_ceil(2) * height.div_ceil(2)) as usize;
    assert!(
        raw.len() >= ylen + 2 * clen,
        "reference too short: {} < {}",
        raw.len(),
        ylen + 2 * clen
    );
    [
        &raw[..ylen],
        &raw[ylen..ylen + clen],
        &raw[ylen + clen..ylen + 2 * clen],
    ]
}

/// Worst PSNR and largest difference over the three planes of `frame`
/// against a raw yuv420p reference frame.
fn compare(frame: &Frame, raw: &[u8]) -> (f64, u8) {
    let [y, cb, cr] = split_planes(raw, frame.width, frame.height);
    [
        plane_stats(&frame.y, y),
        plane_stats(&frame.cb, cb),
        plane_stats(&frame.cr, cr),
    ]
    .iter()
    .fold((f64::INFINITY, 0u8), |(p, m), &(pp, mm)| {
        (p.min(pp), m.max(mm))
    })
}

struct Outcome {
    name: String,
    psnr: f64,
    max_diff: u8,
    frame: Frame,
}

/// Runs a case end to end and checks the limits. None when skipped.
fn check(ffmpeg: &Path, dir: &TempDir, c: &Case) -> Option<Outcome> {
    let Some((es, es_path)) = encode(ffmpeg, dir, c, 1) else {
        eprintln!("{}: source not available, skipped", c.name);
        return None;
    };
    let fields = stream_fields(&es);
    for want in &c.expect {
        assert!(
            fields.contains(want),
            "{}: stream lacks {want:?}; has {fields:?}",
            c.name
        );
    }
    let raw = reference(ffmpeg, dir, &es_path);
    let frame = decode_intra(&es).unwrap_or_else(|e| panic!("{}: decode failed: {e}", c.name));
    assert_eq!(
        (frame.width, frame.height),
        (c.width, c.height),
        "{}",
        c.name
    );
    assert_eq!(frame.concealed_macroblocks, 0, "{}: concealed", c.name);
    assert_eq!(frame.mpeg1, c.codec == MPEG1, "{}", c.name);
    assert!(!frame.field_doubled, "{}", c.name);

    // The whole-file path: the range found must decode the same.
    let mut whole = es.clone();
    whole.extend_from_slice(&mpeg2_writer::SEQUENCE_END);
    let range = find_intra_picture(&whole).unwrap_or_else(|| panic!("{}: no I-picture", c.name));
    let again = decode_intra(&whole[range]).expect("decode range");
    assert_eq!(again.y, frame.y, "{}", c.name);

    let (psnr, max_diff) = compare(&frame, &raw);
    println!("{:<46} {:>8.2} dB  max diff {}", c.name, psnr, max_diff);
    assert!(
        psnr >= PSNR_LIMIT && max_diff <= MAX_DIFF_LIMIT,
        "{}: PSNR {psnr:.2} dB, max diff {max_diff}",
        c.name
    );
    Some(Outcome {
        name: c.name.clone(),
        psnr,
        max_diff,
        frame,
    })
}

fn run_cases(tag: &str, cases: &[Case]) -> Vec<Outcome> {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the MPEG oracle tests ({tag})");
        return Vec::new();
    };
    let dir = TempDir::new(tag);
    let outcomes: Vec<Outcome> = cases
        .iter()
        .filter_map(|c| check(&ffmpeg, &dir, c))
        .collect();
    if let Some(worst) = outcomes.iter().min_by(|a, b| a.psnr.total_cmp(&b.psnr)) {
        println!(
            "[{tag}] worst PSNR {:.2} dB ({}), largest diff {}",
            worst.psnr,
            worst.name,
            outcomes.iter().map(|o| o.max_diff).max().unwrap_or(0)
        );
    }
    outcomes
}

const MPEG2: &str = "mpeg2video";
const MPEG1: &str = "mpeg1video";
const SD: (u32, u32) = (720, 480);
const INTERLACED: [&str; 4] = ["-flags", "+ildct+ilme", "-top", "1"];

fn with(base: &[&'static str], more: &[&'static str]) -> Vec<&'static str> {
    base.iter().chain(more).copied().collect()
}

#[test]
fn mpeg2_coding_options() {
    let matrix = custom_matrix();
    let cases = [
        case("mpeg2 default", MPEG2, Source::TestSrc2, SD, &["-g", "1"]).expect(&[
            ("progressive_sequence", 1),
            ("intra_vlc_format", 0),
            ("alternate_scan", 0),
            ("q_scale_type", 0),
            ("intra_dc_precision", 0),
            ("picture_structure", 3),
        ]),
        case(
            "mpeg2 intra_vlc",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &["-g", "1", "-intra_vlc", "1", "-qscale:v", "3"],
        )
        .expect(&[("intra_vlc_format", 1)]),
        case(
            "mpeg2 alternate_scan",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &["-g", "1", "-alternate_scan", "1", "-qscale:v", "3"],
        )
        .expect(&[("alternate_scan", 1)]),
        case(
            "mpeg2 non_linear_quant",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &[
                "-g",
                "1",
                "-non_linear_quant",
                "1",
                "-qmax",
                "28",
                "-qscale:v",
                "6",
            ],
        )
        .expect(&[("q_scale_type", 1)]),
        case(
            "mpeg2 intra_dc_precision 1",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-intra_dc_precision", "1"],
        )
        .expect(&[("intra_dc_precision", 1)]),
        case(
            "mpeg2 intra_dc_precision 2",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &["-g", "1", "-intra_dc_precision", "2", "-qscale:v", "2"],
        )
        .expect(&[("intra_dc_precision", 2)]),
        case(
            "mpeg2 intra_dc_precision 3",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &[
                "-g",
                "1",
                "-intra_dc_precision",
                "3",
                "-qscale:v",
                "2",
                "-strict",
                "-1",
            ],
        )
        .expect(&[("intra_dc_precision", 3)]),
        case(
            "mpeg2 interlaced (combed)",
            MPEG2,
            Source::Combed,
            SD,
            &with(&["-g", "1"], &INTERLACED),
        )
        .expect(&[("progressive_sequence", 0), ("frame_pred_frame_dct", 0)]),
        case(
            "mpeg2 interlaced (woven)",
            MPEG2,
            Source::Woven,
            SD,
            &with(&["-g", "1", "-qscale:v", "2"], &INTERLACED),
        )
        .expect(&[("progressive_sequence", 0), ("frame_pred_frame_dct", 0)]),
        case(
            "mpeg2 interlaced + alt scan + vlc + nonlinear",
            MPEG2,
            Source::Combed,
            (720, 576),
            &with(
                &[
                    "-g",
                    "1",
                    "-alternate_scan",
                    "1",
                    "-intra_vlc",
                    "1",
                    "-non_linear_quant",
                    "1",
                    "-qmax",
                    "28",
                ],
                &INTERLACED,
            ),
        )
        .expect(&[
            ("frame_pred_frame_dct", 0),
            ("alternate_scan", 1),
            ("intra_vlc_format", 1),
            ("q_scale_type", 1),
        ]),
        case(
            "mpeg2 qscale 1 (escapes, saturation)",
            MPEG2,
            Source::Noise,
            SD,
            &["-g", "1", "-qscale:v", "1"],
        ),
        case(
            "mpeg2 qscale 1 testsrc2",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-qscale:v", "1"],
        ),
        case(
            "mpeg2 qscale 1 intra_vlc",
            MPEG2,
            Source::Noise,
            SD,
            &["-g", "1", "-qscale:v", "1", "-intra_vlc", "1"],
        )
        .expect(&[("intra_vlc_format", 1)]),
        case(
            "mpeg2 qscale 31",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-qscale:v", "31"],
        ),
        case(
            "mpeg2 qscale 28 nonlinear",
            MPEG2,
            Source::Noise,
            SD,
            &[
                "-g",
                "1",
                "-qmax",
                "28",
                "-qscale:v",
                "28",
                "-non_linear_quant",
                "1",
            ],
        )
        .expect(&[("q_scale_type", 1)]),
        case(
            "mpeg2 custom intra_matrix",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &["-g", "1", "-qscale:v", "3", "-intra_matrix", &matrix],
        )
        .expect(&[("load_intra_matrix", 1)]),
    ];
    run_cases("options", &cases);
}

/// A valid custom intra matrix: 8 for DC, then varied weights.
fn custom_matrix() -> String {
    (0..64)
        .map(|i| if i == 0 { 8 } else { 9 + (i * 37) % 70 })
        .map(|v: i32| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The interlaced cases really use field DCT: the same picture coded with
/// frame DCT only reconstructs differently.
#[test]
fn field_dct_is_exercised() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the field DCT check");
        return;
    };
    let dir = TempDir::new("fielddct");
    for source in [Source::Combed, Source::Woven] {
        let interlaced = case(
            "field dct interlaced",
            MPEG2,
            source,
            SD,
            &with(&["-g", "1", "-qscale:v", "4"], &INTERLACED),
        );
        let progressive = case(
            "field dct progressive",
            MPEG2,
            source,
            SD,
            &["-g", "1", "-qscale:v", "4"],
        );
        let (_, a) = encode(&ffmpeg, &dir, &interlaced, 1).expect("encode");
        let ra = reference(&ffmpeg, &dir, &a);
        let (_, b) = encode(&ffmpeg, &dir, &progressive, 1).expect("encode");
        let rb = reference(&ffmpeg, &dir, &b);
        assert_ne!(ra, rb, "{source:?}: no macroblock used field DCT");
    }
}

#[test]
fn mpeg2_signalling() {
    let cases = [
        case(
            "mpeg2 bt709 display ext",
            MPEG2,
            Source::TestSrc2,
            (1920, 1080),
            &[
                "-g",
                "1",
                "-seq_disp_ext",
                "1",
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
            ],
        )
        .expect(&[("display_extension", 1), ("matrix_coefficients", 1)]),
        case(
            "mpeg2 aspect 16:9",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-aspect", "16:9"],
        )
        .expect(&[("aspect", 3)]),
        case(
            "mpeg2 aspect 16:9 display ext",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-aspect", "16:9", "-seq_disp_ext", "1"],
        )
        .expect(&[("aspect", 3), ("display_extension", 1)]),
        case(
            "mpeg2 aspect 4:3",
            MPEG2,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-aspect", "4:3"],
        )
        .expect(&[("aspect", 2)]),
        case(
            "mpeg2 aspect 4:3 PAL",
            MPEG2,
            Source::TestSrc2,
            (720, 576),
            &["-g", "1", "-aspect", "4:3"],
        )
        .expect(&[("aspect", 2)]),
    ];
    let outcomes = run_cases("signalling", &cases);
    for o in &outcomes {
        let (aspect, matrix) = match o.name.as_str() {
            "mpeg2 bt709 display ext" => ((1, 1), ColorMatrix::Bt709),
            "mpeg2 aspect 16:9" | "mpeg2 aspect 16:9 display ext" => ((32, 27), ColorMatrix::Bt601),
            "mpeg2 aspect 4:3" => ((8, 9), ColorMatrix::Bt601),
            "mpeg2 aspect 4:3 PAL" => ((16, 15), ColorMatrix::Bt601),
            other => panic!("unexpected case {other}"),
        };
        assert_eq!(o.frame.pixel_aspect, aspect, "{}", o.name);
        assert_eq!(o.frame.matrix, matrix, "{}", o.name);
    }
}

#[test]
fn mpeg2_picture_sizes() {
    let sizes = [
        (720, 480),
        (720, 576),
        (352, 240),
        (704, 480),
        (1920, 1080),
        (720, 486),
        (350, 200),
    ];
    let mut cases = Vec::new();
    for &(w, h) in &sizes {
        cases.push(case(
            &format!("mpeg2 {w}x{h}"),
            MPEG2,
            Source::TestSrc2,
            (w, h),
            &["-g", "1"],
        ));
        cases.push(
            case(
                &format!("mpeg2 {w}x{h} interlaced"),
                MPEG2,
                Source::Combed,
                (w, h),
                &with(&["-g", "1"], &INTERLACED),
            )
            .expect(&[("progressive_sequence", 0)]),
        );
    }
    run_cases("sizes", &cases);
}

#[test]
fn mpeg1_streams() {
    let cases = [
        case(
            "mpeg1 default",
            MPEG1,
            Source::TestSrc2,
            (352, 240),
            &["-g", "1"],
        ),
        case(
            "mpeg1 qscale 1 (long escapes)",
            MPEG1,
            Source::Noise,
            (352, 240),
            &["-g", "1", "-qscale:v", "1"],
        ),
        case(
            "mpeg1 qscale 1 mandelbrot",
            MPEG1,
            Source::Mandelbrot,
            (352, 288),
            &["-g", "1", "-qscale:v", "1"],
        ),
        case(
            "mpeg1 qscale 31",
            MPEG1,
            Source::TestSrc2,
            (352, 240),
            &["-g", "1", "-qscale:v", "31"],
        ),
        case(
            "mpeg1 720x480",
            MPEG1,
            Source::TestSrc2,
            SD,
            &["-g", "1", "-qscale:v", "4"],
        ),
        case(
            "mpeg1 350x200",
            MPEG1,
            Source::Mandelbrot,
            (350, 200),
            &["-g", "1"],
        ),
        case(
            "mpeg1 custom intra_matrix",
            MPEG1,
            Source::Mandelbrot,
            (352, 240),
            &[
                "-g",
                "1",
                "-qscale:v",
                "2",
                "-intra_matrix",
                &custom_matrix(),
            ],
        )
        .expect(&[("load_intra_matrix", 1)]),
    ];
    run_cases("mpeg1", &cases);
}

#[test]
fn picture_sources() {
    let cases = [
        case(
            "mandelbrot 720x480",
            MPEG2,
            Source::Mandelbrot,
            SD,
            &["-g", "1"],
        ),
        case(
            "noise 720x480",
            MPEG2,
            Source::Noise,
            SD,
            &["-g", "1", "-qscale:v", "4"],
        ),
        case(
            "noise 720x480 intra_vlc",
            MPEG2,
            Source::Noise,
            SD,
            &["-g", "1", "-intra_vlc", "1"],
        ),
        case(
            "photo 720x480",
            MPEG2,
            Source::Photo,
            SD,
            &["-g", "1", "-qscale:v", "2"],
        ),
        case(
            "photo 720x576 rate control",
            MPEG2,
            Source::Photo,
            (720, 576),
            &["-g", "1", "-b:v", "6M"],
        ),
        case(
            "photo 1920x1080 interlaced",
            MPEG2,
            Source::Photo,
            (1920, 1080),
            &with(&["-g", "1", "-qscale:v", "3"], &INTERLACED),
        ),
        case(
            "photo 352x240 mpeg1",
            MPEG1,
            Source::Photo,
            (352, 240),
            &["-g", "1", "-qscale:v", "1"],
        ),
    ];
    run_cases("sources", &cases);
}

/// A GOP with P and B pictures: the I-picture found first is frame 0.
#[test]
fn gop_with_predicted_pictures() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the MPEG GOP oracle test");
        return;
    };
    let dir = TempDir::new("gop");
    for (codec, args) in [
        (MPEG2, vec!["-g", "6", "-bf", "2"]),
        (MPEG2, with(&["-g", "12", "-bf", "2"], &INTERLACED)),
        (MPEG1, vec!["-g", "5", "-bf", "1"]),
    ] {
        let c = case(
            &format!("{codec} gop {}", args.join(" ")),
            codec,
            Source::TestSrc2,
            (352, 288),
            &args,
        );
        let (es, es_path) = encode(&ffmpeg, &dir, &c, 15).expect("encode");
        let raw = reference(&ffmpeg, &dir, &es_path);

        // The stream's last picture is never complete without an end code,
        // but the first I-picture is followed by other pictures.
        let range = find_intra_picture(&es).expect("I-picture");
        assert_eq!(range.start, 0);
        assert!(range.end < es.len());
        assert_eq!(&es[range.end..range.end + 4], &[0, 0, 1, 0]);
        let frame = decode_intra(&es[range.clone()]).expect("decode");
        let (psnr, max_diff) = compare(&frame, &raw);
        println!("{:<46} {:>8.2} dB  max diff {}", c.name, psnr, max_diff);
        assert!(psnr >= PSNR_LIMIT && max_diff <= MAX_DIFF_LIMIT);

        // The whole stream decodes to the same picture.
        let whole = decode_intra(&es).expect("decode whole");
        assert_eq!(whole.y, frame.y);
    }
}

/// Streams of the test writer, decoded by ffmpeg, give the samples the writer
/// promises (to within the rounding of ffmpeg's integer IDCT).
#[test]
fn writer_streams_match_ffmpeg() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the writer cross-check");
        return;
    };
    let dir = TempDir::new("writer");
    let colour = |seed: u32| {
        move |x: u32, y: u32| {
            let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed)
                .wrapping_mul(2_654_435_761);
            MbContent {
                y: [(h >> 24) as u8, (h >> 16) as u8, (h >> 8) as u8, h as u8],
                cb: (h >> 5) as u8,
                cr: (h >> 13) as u8,
                field_dct: h & 0x100 != 0,
            }
        }
    };

    let mut wide = WriterConfig::mpeg2();
    wide.slices_per_row = 3;
    wide.concealment_motion_vectors = true;
    wide.f_code = [9, 5];
    wide.quant_per_macroblock = true;
    wide.intra_vlc_format = true;
    wide.intra_dc_precision = 3;

    let mut interlaced = WriterConfig::mpeg2();
    interlaced.progressive_sequence = false;
    interlaced.frame_pred_frame_dct = false;
    interlaced.alternate_scan = true;
    interlaced.q_scale_type = true;
    interlaced.slice_extra_information = true;
    interlaced.slices_per_row = 5;
    interlaced.intra_dc_precision = 1;

    let mut mpeg1 = WriterConfig::mpeg1();
    mpeg1.macroblock_stuffing = true;
    mpeg1.slices_per_row = 0;
    mpeg1.slice_extra_information = true;
    mpeg1.quant_per_macroblock = true;

    let mut odd = WriterConfig::mpeg2();
    odd.intra_dc_precision = 2;
    odd.slices_per_row = 2;

    let cases: [(&str, WriterConfig, (u32, u32)); 4] = [
        ("writer 1920x1080 escapes+cmv+quant", wide, (1920, 1080)),
        ("writer interlaced field dct", interlaced, (720, 480)),
        ("writer mpeg1 stuffing one slice", mpeg1, (352, 240)),
        ("writer 350x200 precision 2", odd, (350, 200)),
    ];
    for (i, (name, cfg, (w, h))) in cases.into_iter().enumerate() {
        let mb = colour(i as u32);
        let es = mpeg2_writer::intra_picture_blocks(&cfg, w, h, mb);
        let path = dir.path(&format!("{}.m2v", sanitize(name)));
        std::fs::write(&path, &es).expect("write");
        let raw = reference(&ffmpeg, &dir, &path);
        let want = mpeg2_writer::expected_planes(&cfg, w, h, mb);
        let got = split_planes(&raw, w, h);
        let mut max = 0;
        for (a, b) in want.iter().zip(got) {
            max = max.max(plane_stats(a, b).1);
        }
        println!("{name:<46} ffmpeg vs writer max diff {max}");
        assert!(max <= 1, "{name}: ffmpeg disagrees with the writer");

        let frame = decode_intra(&es).expect("decode");
        assert_eq!(
            [frame.y, frame.cb, frame.cr],
            want,
            "{name}: decoder disagrees"
        );
    }

    // Field pictures: an I top field and an I bottom field. ffmpeg weaves
    // both; the decoder line-doubles the first.
    let (w, h) = (720, 480);
    let mut top = WriterConfig::mpeg2();
    top.progressive_sequence = false;
    top.structure = Structure::TopField;
    top.concealment_motion_vectors = true;
    top.slices_per_row = 2;
    let mut bottom = top.clone();
    bottom.structure = Structure::BottomField;
    let mut es = mpeg2_writer::sequence_headers(&top, w, h);
    es.extend(mpeg2_writer::picture(&top, w, h, colour(10)));
    es.extend(mpeg2_writer::picture(&bottom, w, h, colour(11)));
    es.extend_from_slice(&mpeg2_writer::SEQUENCE_END);
    let path = dir.path("fields.m2v");
    std::fs::write(&path, &es).expect("write");
    let raw = reference(&ffmpeg, &dir, &path);
    let got = split_planes(&raw, w, h);
    let want_top = mpeg2_writer::expected_planes(&top, w, h, colour(10));
    let want_bottom = mpeg2_writer::expected_planes(&bottom, w, h, colour(11));
    let mut max = 0;
    for p in 0..3 {
        let width = if p == 0 { w } else { w / 2 } as usize;
        for (line, row) in got[p].chunks(width).enumerate() {
            let want = if line % 2 == 0 {
                &want_top[p]
            } else {
                &want_bottom[p]
            };
            max = max.max(plane_stats(&want[line * width..(line + 1) * width], row).1);
        }
    }
    println!(
        "{:<46} ffmpeg vs writer max diff {max}",
        "writer field pictures"
    );
    assert!(max <= 1, "field pictures: ffmpeg disagrees with the writer");
    let frame = decode_intra(&es).expect("decode");
    assert!(frame.field_doubled);
    assert_eq!([frame.y, frame.cb, frame.cr], want_top);
}

/// Times the decoding of 720x480 I-frames (meaningful in release builds).
#[test]
fn decode_speed() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the MPEG timing test");
        return;
    };
    let dir = TempDir::new("speed");
    for c in [
        case(
            "timing photo qscale 2",
            MPEG2,
            Source::Photo,
            SD,
            &["-g", "1", "-qscale:v", "2"],
        ),
        case(
            "timing photo 8 Mbit/s",
            MPEG2,
            Source::Photo,
            SD,
            &["-g", "1", "-b:v", "8M"],
        ),
        case("timing testsrc2", MPEG2, Source::TestSrc2, SD, &["-g", "1"]),
        case(
            "timing noise qscale 1 (worst case)",
            MPEG2,
            Source::Noise,
            SD,
            &["-g", "1", "-qscale:v", "1"],
        ),
    ] {
        let Some((es, _)) = encode(&ffmpeg, &dir, &c, 1) else {
            continue;
        };
        decode_intra(&es).expect("decode");
        let runs = 20;
        let start = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(decode_intra(std::hint::black_box(&es)).expect("decode"));
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
        println!(
            "{:<46} {:>7.3} ms per 720x480 I-frame ({} bytes)",
            c.name,
            per,
            es.len()
        );
    }
}

/// Random corruption of real streams never panics and stays fast.
#[test]
fn mutated_real_streams_do_not_panic() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("ffmpeg not found on PATH: skipping the MPEG mutation test");
        return;
    };
    let dir = TempDir::new("fuzz");
    let mut streams = Vec::new();
    for c in [
        case(
            "fuzz mpeg2",
            MPEG2,
            Source::Mandelbrot,
            (176, 144),
            &["-g", "1", "-qscale:v", "2"],
        ),
        case(
            "fuzz mpeg2 vlc interlaced",
            MPEG2,
            Source::Noise,
            (176, 144),
            &with(
                &["-g", "1", "-intra_vlc", "1", "-qscale:v", "1"],
                &INTERLACED,
            ),
        ),
        case(
            "fuzz mpeg1",
            MPEG1,
            Source::Noise,
            (176, 144),
            &["-g", "1", "-qscale:v", "1"],
        ),
        case(
            "fuzz mpeg2 gop",
            MPEG2,
            Source::TestSrc2,
            (176, 144),
            &["-g", "3", "-bf", "1"],
        ),
    ] {
        streams.push(encode(&ffmpeg, &dir, &c, 4).expect("encode").0);
    }
    let start = Instant::now();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut decoded = 0;
    let rounds = 4000;
    for round in 0..rounds {
        let base = &streams[round % streams.len()];
        let data = mpeg2_writer::mutate(base, &mut seed);
        if let Ok(frame) = decode_intra(&data) {
            decoded += 1;
            assert_eq!(frame.y.len(), (frame.width * frame.height) as usize);
            assert!(frame.concealed_macroblocks < frame.total_macroblocks);
        }
        if let Some(range) = find_intra_picture(&data) {
            assert!(range.start < range.end && range.end <= data.len());
        }
    }
    println!(
        "{rounds} mutated streams in {:.2?} ({decoded} still decoded)",
        start.elapsed()
    );
}
