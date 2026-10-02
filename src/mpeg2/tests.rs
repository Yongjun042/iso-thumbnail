//! Unit tests of the MPEG video decoder: code tables, bit reader, headers,
//! `find_intra_picture`, exact decoding of synthetic streams, concealment and
//! robustness against damaged input.

// Binary codes are grouped in fours from the left, as the standard prints them.
#![allow(clippy::unusual_byte_groupings)]

use super::bits::{find_start_code, BitReader};
use super::headers::{PictureCoding, Sequence, DEFAULT_INTRA_MATRIX, ZIGZAG};
use super::vlc::{self, Code, Vlc};
use super::*;

#[allow(dead_code)]
#[path = "../../tests/support/mpeg2_writer.rs"]
mod writer;

use writer::{BitWriter, DisplayExtension, MbContent, Structure, WriterConfig};

// ---------------------------------------------------------------------------
// Code tables

fn all_tables() -> Vec<(&'static str, Vec<Code>, &'static Vlc)> {
    let t = vlc::tables();
    vec![
        (
            "B.1 macroblock_address_increment",
            vlc::MACROBLOCK_ADDRESS_INCREMENT.to_vec(),
            &t.macroblock_address_increment,
        ),
        (
            "B.10 motion_code",
            vlc::MOTION_CODE.to_vec(),
            &t.motion_code,
        ),
        (
            "B.12 dct_dc_size_luminance",
            vlc::DCT_DC_SIZE_LUMINANCE.to_vec(),
            &t.dc_luminance,
        ),
        (
            "B.13 dct_dc_size_chrominance",
            vlc::DCT_DC_SIZE_CHROMINANCE.to_vec(),
            &t.dc_chrominance,
        ),
        (
            "B.14 DCT coefficients zero",
            vlc::dct_codes(false),
            &t.dct_zero,
        ),
        (
            "B.15 DCT coefficients one",
            vlc::dct_codes(true),
            &t.dct_one,
        ),
    ]
}

#[test]
fn vlc_tables_are_prefix_free() {
    for (name, codes, _) in all_tables() {
        let mut kraft = 0f64;
        for (i, &(a, la, sa)) in codes.iter().enumerate() {
            assert!((1..=16).contains(&la), "{name}: bad length {la}");
            assert!(a < 1 << la, "{name}: code {a:b} longer than {la}");
            kraft += 0.5f64.powi(i32::from(la));
            for &(b, lb, sb) in &codes[i + 1..] {
                assert_ne!(sa, sb, "{name}: symbol {sa} twice");
                let l = la.min(lb);
                assert_ne!(
                    a >> (la - l),
                    b >> (lb - l),
                    "{name}: {a:0la$b} and {b:0lb$b} share a prefix",
                    la = usize::from(la),
                    lb = usize::from(lb)
                );
            }
        }
        assert!(kraft <= 1.0, "{name}: Kraft sum {kraft}");
    }
}

#[test]
fn vlc_tables_decode_every_codeword() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for (name, codes, table) in all_tables() {
        for &(code, len, sym) in &codes {
            // Any bits may follow the code.
            for _ in 0..8 {
                let tail = writer::xorshift(&mut seed) as u32;
                let bits = (code << (32 - u32::from(len))) | (tail >> u32::from(len));
                let e = table.decode(bits);
                assert_eq!(
                    (e.sym, e.len),
                    (sym, len),
                    "{name}: code {code:0w$b}",
                    w = usize::from(len)
                );
            }
        }
        // A run of zeros is no code (except '00' as a DC size): decoding the
        // zeros served past the data stops at the next table lookup.
        if !name.contains("dct_dc_size") {
            assert_eq!(table.decode(0).len, 0, "{name}: zeros decode");
        }
    }
}

#[test]
fn dct_tables_have_all_run_level_pairs() {
    for one in [false, true] {
        let codes = vlc::dct_codes(one);
        assert_eq!(codes.len(), 113);
        assert!(codes.iter().any(|c| c.2 == vlc::run_level(0, 40)));
        assert!(codes.iter().any(|c| c.2 == vlc::run_level(1, 18)));
        assert!(codes.iter().any(|c| c.2 == vlc::run_level(31, 1)));
    }
    // Spot checks against Tables B.14 / B.15.
    let find = |one: bool, run, level| {
        vlc::dct_codes(one)
            .into_iter()
            .find(|c| c.2 == vlc::run_level(run, level))
            .map(|c| (c.0, c.1))
    };
    assert_eq!(find(false, 0, 1), Some((0b11, 2)));
    assert_eq!(find(false, 1, 1), Some((0b011, 3)));
    assert_eq!(find(false, 0, 40), Some((0b0000_0000_0010_000, 15)));
    assert_eq!(find(false, 31, 1), Some((0b0000_0000_0001_1011, 16)));
    assert_eq!(find(true, 0, 1), Some((0b10, 2)));
    assert_eq!(find(true, 0, 15), Some((0b1111_1111, 8)));
    assert_eq!(find(true, 9, 1), Some((0b1111_000, 7)));
}

// ---------------------------------------------------------------------------
// Bit reader and start codes

#[test]
fn bit_reader_reads_across_refills_and_past_the_end() {
    let data: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(37) ^ 0x5A).collect();
    let mut w = 0u128;
    let mut r = BitReader::new(&data);
    let mut seed = 77u64;
    let mut pos = 0usize;
    while pos + 32 <= data.len() * 8 {
        let n = 1 + (writer::xorshift(&mut seed) % 32) as u32;
        if pos + n as usize > data.len() * 8 {
            break;
        }
        let want = (0..n as usize).fold(0u32, |v, i| {
            let bit = (data[(pos + i) / 8] >> (7 - (pos + i) % 8)) & 1;
            (v << 1) | u32::from(bit)
        });
        assert_eq!(r.read(n), want, "at bit {pos}");
        pos += n as usize;
        w += 1;
    }
    assert!(w > 10);
    assert!(!r.overrun());
    // Past the end: zeros, and the overrun is reported.
    let rest = data.len() * 8 - pos;
    for _ in 0..rest {
        r.read(1);
    }
    assert!(!r.overrun());
    assert_eq!(r.read(32), 0);
    assert!(r.overrun());
    let mut empty = BitReader::new(&[]);
    assert_eq!(empty.peek32(), 0);
    assert!(!empty.overrun());
    empty.skip(1);
    assert!(empty.overrun());
}

#[test]
fn start_codes_are_found() {
    assert_eq!(find_start_code(&[], 0), None);
    assert_eq!(find_start_code(&[0, 0, 1], 0), Some(0));
    assert_eq!(find_start_code(&[0, 0, 1], 1), None);
    assert_eq!(find_start_code(&[0, 0, 0, 0, 1, 0xB3], 0), Some(2));
    assert_eq!(find_start_code(&[9, 9, 9, 9, 0, 0, 1], 0), Some(4));
    assert_eq!(find_start_code(&[0, 1, 0, 0, 2, 0, 0, 1], 0), Some(5));
    assert_eq!(find_start_code(&[0, 0, 1, 0, 0, 1], 1), Some(3));
    assert_eq!(find_start_code(&[0, 0, 1], usize::MAX), None);
    // Agrees with a naive search on random data.
    let mut seed = 5u64;
    for _ in 0..200 {
        let data: Vec<u8> = (0..64)
            .map(|_| (writer::xorshift(&mut seed) % 3) as u8)
            .collect();
        for from in 0..data.len() {
            let naive = (from..data.len().saturating_sub(2))
                .find(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1);
            assert_eq!(find_start_code(&data, from), naive);
        }
    }
}

// ---------------------------------------------------------------------------
// Headers

/// The payload of the first unit with start code `code`.
fn unit(es: &[u8], code: u8, nth: usize) -> &[u8] {
    let mut seen = 0;
    let mut at = 0;
    while let Some(i) = find_start_code(es, at) {
        if es[i + 3] == code {
            if seen == nth {
                let end = find_start_code(es, i + 4).unwrap_or(es.len());
                return &es[i + 4..end];
            }
            seen += 1;
        }
        at = i + 3;
    }
    panic!("no unit {code:#x}");
}

fn ramp_matrix() -> [u8; 64] {
    let mut m = [0u8; 64];
    for (i, v) in m.iter_mut().enumerate() {
        *v = 8 + i as u8 * 3;
    }
    m
}

#[test]
fn sequence_header_and_extensions_parse() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.aspect_code = 3;
    cfg.progressive_sequence = false;
    cfg.intra_matrix = Some(ramp_matrix());
    cfg.display = Some(DisplayExtension {
        matrix_coefficients: 1,
        display_width: 704,
        display_height: 480,
    });
    let es = writer::sequence_headers(&cfg, 720, 480);
    let mut seq = Sequence::parse(unit(&es, 0xB3, 0)).expect("sequence header");
    assert_eq!((seq.width, seq.height, seq.aspect_code), (720, 480, 3));
    assert_eq!(seq.intra_matrix, ramp_matrix());
    assert!(!seq.mpeg2);
    seq.apply_sequence_extension(unit(&es, 0xB5, 0))
        .expect("sequence extension");
    assert!(seq.mpeg2);
    assert!(!seq.progressive_sequence);
    assert_eq!(seq.chroma_format, 1);
    seq.apply_display_extension(unit(&es, 0xB5, 1))
        .expect("display extension");
    assert!(seq.bt709);
    assert_eq!(seq.display_size, Some((704, 480)));
    // 16:9 with a 704-wide display window: the window does not make the
    // whole picture 16:9, so the aspect applies to the coded 720 x 480.
    assert_eq!(seq.pixel_aspect(), (32, 27));

    // A sequence header without a matrix restores the default one.
    cfg.intra_matrix = None;
    let es = writer::sequence_headers(&cfg, 720, 480);
    let seq = Sequence::parse(unit(&es, 0xB3, 0)).expect("sequence header");
    assert_eq!(seq.intra_matrix, DEFAULT_INTRA_MATRIX);

    // Truncated headers are rejected.
    assert!(Sequence::parse(&[0x2D, 0x01, 0xE0]).is_err());
    let mut cfg1 = WriterConfig::mpeg1();
    cfg1.intra_matrix = Some(ramp_matrix());
    let es = writer::sequence_headers(&cfg1, 352, 240);
    let payload = unit(&es, 0xB3, 0);
    assert!(Sequence::parse(&payload[..payload.len() - 1]).is_err());
}

#[test]
fn quant_matrix_extension_replaces_intra_matrix() {
    let mut seq = Sequence::parse(&[0x2D, 0x01, 0xE0, 0x24, 0xFF, 0xFF, 0xE0, 0x00])
        .expect("sequence header");
    assert_eq!((seq.width, seq.height), (720, 480));
    let mut w = BitWriter::new();
    w.put(3, 4); // quant matrix extension
    w.put(1, 1); // load_intra_quantiser_matrix
    for i in 0..64u32 {
        w.put(10 + i, 8); // zigzag order
    }
    w.put(1, 1); // load_non_intra_quantiser_matrix
    for _ in 0..16 {
        w.put(0, 32);
    }
    w.put(0, 2); // no chroma matrices
    let ext = w.finish();
    seq.apply_quant_matrix_extension(&ext).expect("extension");
    for (i, &pos) in ZIGZAG.iter().enumerate() {
        assert_eq!(seq.intra_matrix[usize::from(pos)], 10 + i as u8);
    }
    // Without load_intra the matrix stays.
    let before = seq.intra_matrix;
    seq.apply_quant_matrix_extension(&[0x30, 0x00])
        .expect("empty");
    assert_eq!(seq.intra_matrix, before);
    assert!(seq.apply_quant_matrix_extension(&ext[..40]).is_err());
}

#[test]
fn picture_coding_extension_parses() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.intra_dc_precision = 2;
    cfg.structure = Structure::BottomField;
    cfg.concealment_motion_vectors = true;
    cfg.f_code = [4, 7];
    cfg.q_scale_type = true;
    cfg.intra_vlc_format = true;
    cfg.alternate_scan = true;
    cfg.progressive_sequence = false;
    let es = writer::picture(&cfg, 64, 64, |_, _| MbContent::flat(1, 2, 3));
    let pce = PictureCoding::parse(unit(&es, 0xB5, 0)).expect("coding extension");
    assert_eq!(
        pce,
        PictureCoding {
            f_code: [[4, 7], [15, 15]],
            intra_dc_precision: 2,
            picture_structure: 2,
            frame_pred_frame_dct: true,
            concealment_motion_vectors: true,
            q_scale_type: true,
            intra_vlc_format: true,
            alternate_scan: true,
        }
    );
    assert!(PictureCoding::parse(&[0x8F, 0xFF]).is_err());
}

#[test]
fn pixel_aspect_ratios() {
    let none = None;
    // MPEG-2: display aspect ratio over the picture (or display) size.
    assert_eq!(pixel_aspect(true, 1, (720, 480), none), (1, 1));
    assert_eq!(pixel_aspect(true, 2, (720, 480), none), (8, 9));
    assert_eq!(pixel_aspect(true, 3, (720, 480), none), (32, 27));
    assert_eq!(pixel_aspect(true, 2, (720, 576), none), (16, 15));
    assert_eq!(pixel_aspect(true, 3, (720, 576), none), (64, 45));
    assert_eq!(pixel_aspect(true, 2, (352, 240), none), (10, 11));
    assert_eq!(pixel_aspect(true, 3, (1920, 1080), none), (1, 1));
    assert_eq!(pixel_aspect(true, 3, (1440, 1080), none), (4, 3));
    assert_eq!(pixel_aspect(true, 4, (720, 480), none), (221, 150));
    // Display rectangles that do not make the whole picture exactly 4:3 or
    // 16:9 are ignored, as ffmpeg does: DVD pan-and-scan windows (540 wide on
    // 720), 704-wide windows, half-D1 pictures with a 720-wide display.
    assert_eq!(pixel_aspect(true, 2, (720, 480), Some((704, 480))), (8, 9));
    assert_eq!(
        pixel_aspect(true, 3, (720, 480), Some((540, 480))),
        (32, 27)
    );
    assert_eq!(
        pixel_aspect(true, 3, (720, 576), Some((540, 576))),
        (64, 45)
    );
    assert_eq!(
        pixel_aspect(true, 2, (352, 480), Some((720, 480))),
        (20, 11)
    );
    // A display rectangle that keeps the whole picture 4:3 is used.
    assert_eq!(pixel_aspect(true, 2, (720, 480), Some((720, 480))), (8, 9));
    assert_eq!(pixel_aspect(true, 2, (720, 480), Some((360, 240))), (8, 9));
    // Any u32 input is safe (no overflow panic in the public helper).
    let _ = pixel_aspect(true, 4, (u32::MAX, u32::MAX), Some((u32::MAX, u32::MAX)));
    let _ = pixel_aspect(true, 3, (u32::MAX, 1), Some((1, u32::MAX)));
    // A zero display size falls back to the picture size.
    assert_eq!(pixel_aspect(true, 2, (720, 480), Some((0, 480))), (8, 9));
    for code in [0, 5, 9, 15] {
        assert_eq!(pixel_aspect(true, code, (720, 480), none), (1, 1));
    }
    // MPEG-1: height/width of a pel.
    assert_eq!(pixel_aspect(false, 1, (352, 240), none), (1, 1));
    assert_eq!(pixel_aspect(false, 12, (352, 240), none), (200, 219));
    assert_eq!(pixel_aspect(false, 8, (352, 288), none), (10000, 9157));
    assert_eq!(pixel_aspect(false, 3, (352, 288), none), (10000, 7031));
    assert_eq!(pixel_aspect(false, 6, (352, 288), none), (10000, 8437));
    assert_eq!(pixel_aspect(false, 14, (352, 288), none), (2000, 2403));
    for code in [0, 15] {
        assert_eq!(pixel_aspect(false, code, (352, 240), none), (1, 1));
    }
}

// ---------------------------------------------------------------------------
// find_intra_picture

fn grey(_: u32, _: u32) -> (u8, u8, u8) {
    (128, 128, 128)
}

#[test]
fn find_intra_picture_boundaries() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.sequence_end = false;
    let (w, h) = (64, 48);
    let headers = writer::sequence_headers(&cfg, w, h);
    let intra = writer::picture(&cfg, w, h, |_, _| MbContent::flat(90, 100, 110));
    let p = writer::predicted_picture(&cfg, 2);
    let b = writer::predicted_picture(&cfg, 3);

    assert_eq!(find_intra_picture(&[]), None);
    assert_eq!(find_intra_picture(&[0, 0, 1, 0xB3]), None);
    assert_eq!(find_intra_picture(&intra), None, "no sequence header");

    // An unterminated I-picture is incomplete; the end code completes it.
    let mut es = headers.clone();
    es.extend_from_slice(&intra);
    assert_eq!(find_intra_picture(&es), None);
    let complete = es.len();
    es.extend_from_slice(&writer::SEQUENCE_END);
    assert_eq!(find_intra_picture(&es), Some(0..complete));
    let frame = decode_intra(&es[0..complete]).expect("decode");
    assert!(frame.y.iter().all(|&v| v == 90));

    // Leading garbage and predicted pictures before the I-picture; the next
    // picture ends it.
    let mut es = vec![0x47, 0, 0, 0, 1, 0xB2, 1, 2, 3];
    let start = es.len();
    es.extend_from_slice(&headers);
    es.extend_from_slice(&p);
    es.extend_from_slice(&b);
    es.extend_from_slice(&intra);
    let end = es.len();
    es.extend_from_slice(&p);
    assert_eq!(find_intra_picture(&es), Some(start..end));
    let frame = decode_intra(&es[start..end]).expect("decode after P/B");
    assert!(frame.y.iter().all(|&v| v == 90));
    assert_eq!(decode_intra(&es).expect("decode whole").y, frame.y);

    // GOP and sequence headers terminate too.
    for code in [0xB8u8, 0xB3, 0xB7, 0x00] {
        let mut es = headers.clone();
        es.extend_from_slice(&intra);
        let end = es.len();
        es.extend_from_slice(&[0, 0, 1, code]);
        assert_eq!(find_intra_picture(&es), Some(0..end), "code {code:#x}");
    }
    // User data and slices do not.
    let mut es = headers.clone();
    es.extend_from_slice(&intra);
    es.extend_from_slice(&[0, 0, 1, 0xB2, 0, 0, 1, 0x05]);
    assert_eq!(find_intra_picture(&es), None);

    // A picture header cut before its coding type: more data needed.
    let mut es = headers.clone();
    es.extend_from_slice(&[0, 0, 1, 0, 0]);
    assert_eq!(find_intra_picture(&es), None);
    // Only predicted pictures.
    let mut es = headers.clone();
    es.extend_from_slice(&p);
    es.extend_from_slice(&b);
    es.extend_from_slice(&writer::SEQUENCE_END);
    assert_eq!(find_intra_picture(&es), None);
    assert_eq!(decode_intra(&es).unwrap_err(), Error::NotFound);
}

// ---------------------------------------------------------------------------
// Exact decoding of synthetic streams

/// Writes a picture, decodes it, and checks every sample and the flags.
fn assert_exact(
    name: &str,
    cfg: &WriterConfig,
    width: u32,
    height: u32,
    mb: impl Fn(u32, u32) -> MbContent + Copy,
) -> Frame {
    let es = writer::intra_picture_blocks(cfg, width, height, mb);
    let frame = decode_intra(&es).unwrap_or_else(|e| panic!("{name}: {e}"));
    let [y, cb, cr] = writer::expected_planes(cfg, width, height, mb);
    assert_eq!((frame.width, frame.height), (width, height), "{name}");
    assert_eq!(
        (frame.chroma_width(), frame.chroma_height()),
        (width.div_ceil(2), height.div_ceil(2))
    );
    assert!(frame.y == y, "{name}: luma differs");
    assert!(frame.cb == cb, "{name}: Cb differs");
    assert!(frame.cr == cr, "{name}: Cr differs");
    assert_eq!(frame.concealed_macroblocks, 0, "{name}");
    assert_eq!(
        frame.total_macroblocks,
        writer::mb_width(width) * writer::mb_rows(cfg, height),
        "{name}"
    );
    assert_eq!(
        frame.field_doubled,
        !cfg.mpeg1 && cfg.structure != Structure::Frame
    );
    assert_eq!(frame.mpeg1, cfg.mpeg1, "{name}");
    frame
}

/// Deterministic per-macroblock content using the whole sample range.
fn pattern(seed: u32) -> impl Fn(u32, u32) -> MbContent + Copy {
    move |x, y| {
        let h = (x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ seed)
            .wrapping_mul(0xC2B2_AE3D);
        let h = h ^ (h >> 15);
        let pick = |v: u32| match v % 7 {
            0 => 0,
            1 => 255,
            _ => (v >> 3) as u8,
        };
        MbContent {
            y: [pick(h), pick(h >> 8), pick(h >> 16), pick(h >> 24)],
            cb: pick(h.rotate_left(5)),
            cr: pick(h.rotate_left(11)),
            field_dct: h & 0x40 != 0,
        }
    }
}

fn flat(f: impl Fn(u32, u32) -> (u8, u8, u8) + Copy) -> impl Fn(u32, u32) -> MbContent + Copy {
    move |x, y| {
        let (l, cb, cr) = f(x, y);
        MbContent::flat(l, cb, cr)
    }
}

#[test]
fn mpeg2_frame_pictures_decode_exactly() {
    for precision in 0..=3 {
        let mut cfg = WriterConfig::mpeg2();
        cfg.intra_dc_precision = precision;
        assert_exact(
            &format!("precision {precision}"),
            &cfg,
            96,
            64,
            pattern(precision.into()),
        );
    }
    let mut cfg = WriterConfig::mpeg2();
    cfg.q_scale_type = true;
    cfg.alternate_scan = true;
    cfg.intra_vlc_format = true;
    cfg.quant_per_macroblock = true;
    cfg.slice_extra_information = true;
    assert_exact("coding options", &cfg, 128, 80, pattern(9));

    let mut cfg = WriterConfig::mpeg2();
    cfg.concealment_motion_vectors = true;
    for f_code in [[1, 1], [2, 9], [7, 3]] {
        cfg.f_code = f_code;
        assert_exact("concealment vectors", &cfg, 160, 96, pattern(11));
    }
}

#[test]
fn signalling_reaches_the_frame() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.aspect_code = 3;
    let frame = assert_exact("16:9", &cfg, 720, 480, flat(grey));
    assert_eq!(frame.pixel_aspect, (32, 27));
    assert_eq!(frame.matrix, ColorMatrix::Bt601);

    cfg.display = Some(DisplayExtension {
        matrix_coefficients: 1,
        display_width: 704,
        display_height: 480,
    });
    cfg.aspect_code = 2;
    let frame = assert_exact("bt709", &cfg, 720, 480, flat(grey));
    // The 704-wide window does not make the picture 4:3: coded size wins.
    assert_eq!(frame.pixel_aspect, (8, 9));
    assert_eq!(frame.matrix, ColorMatrix::Bt709);

    cfg.display = Some(DisplayExtension {
        matrix_coefficients: 6,
        display_width: 0,
        display_height: 0,
    });
    let frame = assert_exact("bt601", &cfg, 720, 576, flat(grey));
    assert_eq!(frame.pixel_aspect, (16, 15));
    assert_eq!(frame.matrix, ColorMatrix::Bt601);

    let mut cfg = WriterConfig::mpeg1();
    cfg.aspect_code = 12;
    let frame = assert_exact("mpeg1 aspect", &cfg, 352, 240, flat(grey));
    assert_eq!(frame.pixel_aspect, (200, 219));
    assert_eq!(frame.matrix, ColorMatrix::Bt601);
}

#[test]
fn field_dct_macroblocks_interleave_lines() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.progressive_sequence = false;
    cfg.frame_pred_frame_dct = false;
    let mb = |x: u32, y: u32| MbContent {
        y: [10, 60, 110, 160],
        cb: 50,
        cr: 200,
        field_dct: (x + y) % 2 == 1,
    };
    let frame = assert_exact("field dct", &cfg, 64, 64, mb);
    // Macroblock (1, 0) is field coded: lines alternate between the top-field
    // blocks and the bottom-field blocks.
    let row = |line: usize| &frame.y[line * 64 + 16..line * 64 + 32];
    assert_eq!(row(0)[..8], [10; 8]);
    assert_eq!(row(1)[..8], [110; 8]);
    assert_eq!(row(2)[8..], [60; 8]);
    assert_eq!(row(3)[8..], [160; 8]);
    // Macroblock (0, 0) is frame coded.
    assert_eq!(
        frame.y[7 * 64..7 * 64 + 16],
        [[10; 8], [60; 8]].concat()[..]
    );
    assert_eq!(
        frame.y[8 * 64..8 * 64 + 16],
        [[110; 8], [160; 8]].concat()[..]
    );
    assert_exact("field dct pattern", &cfg, 208, 96, pattern(3));
}

#[test]
fn field_pictures_are_line_doubled() {
    for structure in [Structure::TopField, Structure::BottomField] {
        let mut cfg = WriterConfig::mpeg2();
        cfg.progressive_sequence = false;
        cfg.structure = structure;
        let frame = assert_exact("field", &cfg, 720, 480, pattern(5));
        assert!(frame.field_doubled);
        assert_eq!(frame.total_macroblocks, 45 * 15);
        // Lines 2n and 2n + 1 are the same field line.
        for line in (0..480).step_by(2) {
            assert_eq!(
                frame.y[line * 720..(line + 1) * 720],
                frame.y[(line + 1) * 720..(line + 2) * 720]
            );
        }
        cfg.concealment_motion_vectors = true;
        cfg.slices_per_row = 4;
        cfg.intra_dc_precision = 2;
        assert_exact("field + concealment vectors", &cfg, 720, 486, pattern(6));
        assert_exact("small field", &cfg, 40, 18, pattern(7));
    }
}

#[test]
fn mpeg1_pictures_decode_exactly() {
    let mut cfg = WriterConfig::mpeg1();
    assert_exact("mpeg1", &cfg, 352, 240, pattern(21));
    cfg.macroblock_stuffing = true;
    cfg.slice_extra_information = true;
    cfg.quant_per_macroblock = true;
    assert_exact("mpeg1 stuffing", &cfg, 176, 120, pattern(22));
    // One slice spanning all rows, as MPEG-1 allows.
    cfg.slices_per_row = 0;
    assert_exact("mpeg1 one slice", &cfg, 200, 100, pattern(23));
    cfg.slices_per_row = 3;
    assert_exact("mpeg1 three slices per row", &cfg, 600, 64, pattern(24));
}

#[test]
fn odd_sizes_and_slice_layouts_decode_exactly() {
    for (w, h) in [
        (8, 8),
        (16, 16),
        (50, 30),
        (350, 200),
        (17, 33),
        (1920, 1152),
    ] {
        for progressive in [true, false] {
            let mut cfg = WriterConfig::mpeg2();
            cfg.progressive_sequence = progressive;
            assert_exact(&format!("{w}x{h}"), &cfg, w, h, pattern(w ^ h));
        }
    }
    let mut cfg = WriterConfig::mpeg2();
    cfg.progressive_sequence = false;
    assert_exact("720x486 interlaced", &cfg, 720, 486, pattern(31));
    // Several slices per row; in wide pictures later slices start with
    // macroblock escapes.
    for slices in [2, 3, 5, 45] {
        cfg.slices_per_row = slices;
        assert_exact("slices", &cfg, 720, 96, pattern(slices));
    }
    cfg.slices_per_row = 3;
    assert_exact("1920 wide, 3 slices", &cfg, 1920, 64, pattern(40));
    cfg.slices_per_row = 0;
    assert_exact("one slice for all rows", &cfg, 96, 64, pattern(41));
}

#[test]
fn later_pictures_and_sequence_end_are_ignored() {
    let cfg = WriterConfig::mpeg2();
    let mut es = writer::intra_picture(&cfg, 64, 32, |_, _| (70, 80, 90));
    es.extend(writer::intra_picture(&cfg, 64, 32, |_, _| (1, 2, 3)));
    let frame = decode_intra(&es).expect("decode");
    assert!(frame.y.iter().all(|&v| v == 70));
    assert!(frame.cb.iter().all(|&v| v == 80));
    assert!(frame.cr.iter().all(|&v| v == 90));

    // A D-picture before the I-picture is skipped (MPEG-1).
    let cfg = WriterConfig::mpeg1();
    let mut es = writer::sequence_headers(&cfg, 64, 32);
    es.extend(writer::predicted_picture(&cfg, 4));
    es.extend(writer::picture(&cfg, 64, 32, |_, _| {
        MbContent::flat(33, 44, 55)
    }));
    let frame = decode_intra(&es).expect("decode");
    assert!(frame.y.iter().all(|&v| v == 33));
}

// ---------------------------------------------------------------------------
// Concealment and errors

/// Byte ranges of the slices (start code included) of a stream.
fn slice_ranges(es: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = find_start_code(es, at) {
        let end = find_start_code(es, i + 4).unwrap_or(es.len());
        if (0x01..=0xAF).contains(&es[i + 3]) {
            out.push((i, end));
        }
        at = i + 3;
    }
    out
}

fn is_grey_mb(frame: &Frame, mbx: usize, mby: usize) -> bool {
    let w = frame.width as usize;
    let cw = frame.chroma_width() as usize;
    (0..16).all(|l| frame.y[(mby * 16 + l) * w + mbx * 16..][..16] == [128; 16])
        && (0..8).all(|l| frame.cb[(mby * 8 + l) * cw + mbx * 8..][..8] == [128; 8])
        && (0..8).all(|l| frame.cr[(mby * 8 + l) * cw + mbx * 8..][..8] == [128; 8])
}

#[test]
fn missing_and_corrupt_slices_are_concealed() {
    let mut cfg = WriterConfig::mpeg2();
    cfg.slices_per_row = 2;
    let (w, h) = (128, 64); // 8 x 4 macroblocks, slices of 4
    let colour = |x: u32, y: u32| (20 + (x * 25) as u8, 30 + (y * 40) as u8, 200);
    let es = writer::intra_picture(&cfg, w, h, colour);
    let slices = slice_ranges(&es);
    assert_eq!(slices.len(), 8);

    // Drop the second slice of row 1: its four macroblocks are grey.
    let (s, e) = slices[3];
    let mut cut = es[..s].to_vec();
    cut.extend_from_slice(&es[e..]);
    let frame = decode_intra(&cut).expect("decode");
    assert_eq!(frame.concealed_macroblocks, 4);
    assert_eq!(frame.total_macroblocks, 32);
    for mby in 0..4 {
        for mbx in 0..8 {
            let lost = mby == 1 && mbx >= 4;
            assert_eq!(is_grey_mb(&frame, mbx, mby), lost, "mb {mbx},{mby}");
            if !lost {
                let (l, _, _) = colour(mbx as u32, mby as u32);
                assert_eq!(frame.y[mby * 16 * 128 + mbx * 16], l);
            }
        }
    }

    // Garbage in the middle of the first slice: the macroblocks before it
    // survive, the rest of that slice is grey, the other slices decode.
    let mut bad = es.clone();
    let (s, e) = slices[0];
    let mid = s + (e - s) / 2;
    bad[mid..e].fill(0xFF);
    let frame = decode_intra(&bad).expect("decode");
    assert!(frame.concealed_macroblocks >= 1 && frame.concealed_macroblocks <= 4);
    assert!(!is_grey_mb(&frame, 0, 0));
    assert!(is_grey_mb(&frame, 3, 0));
    for mbx in 4..8 {
        assert!(!is_grey_mb(&frame, mbx, 0));
    }

    // A slice row beyond the picture is ignored.
    let mut extra = es[..es.len() - 4].to_vec();
    extra.extend_from_slice(&[0, 0, 1, 0x20, 0x45, 0xFF, 0xFF]);
    let frame = decode_intra(&extra).expect("decode");
    assert_eq!(frame.concealed_macroblocks, 0);

    // Row 0 repeated a hundred times (a crafted stream filling megabytes):
    // decoding stops at twice the picture's macroblocks, so the rows after
    // it are not decoded and the cost does not grow with the bytes.
    let mut repeated = es[..slices[0].0].to_vec();
    for _ in 0..100 {
        repeated.extend_from_slice(&es[slices[0].0..slices[1].1]);
    }
    repeated.extend_from_slice(&es[slices[2].0..]);
    let frame = decode_intra(&repeated).expect("decode");
    assert_eq!(frame.concealed_macroblocks, 24);
    assert!(!is_grey_mb(&frame, 0, 0));
    assert!(is_grey_mb(&frame, 0, 1));

    // No slice decodes at all: an error.
    let mut none = es[..slices[0].0].to_vec();
    none.extend_from_slice(&[0, 0, 1, 0x01, 0x00, 0x00, 0x00, 0x00]);
    assert!(matches!(decode_intra(&none), Err(Error::Corrupt(_))));
}

#[test]
fn unusable_headers_fail() {
    let cfg = WriterConfig::mpeg2();
    let es = writer::intra_picture(&cfg, 64, 32, |_, _| (50, 60, 70));
    assert!(decode_intra(&es).is_ok());

    // Sizes: zero, and above the limits.
    let with_size = |w: u32, h: u32| {
        let mut d = es.clone();
        d[4] = (w >> 4) as u8;
        d[5] = ((w & 15) << 4) as u8 | (h >> 8) as u8;
        d[6] = h as u8;
        d
    };
    assert!(matches!(
        decode_intra(&with_size(0, 32)),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(
        decode_intra(&with_size(64, 0)),
        Err(Error::Corrupt(_))
    ));
    assert_eq!(
        decode_intra(&with_size(1921, 32)).unwrap_err(),
        Error::TooLarge
    );
    assert_eq!(
        decode_intra(&with_size(64, 1153)).unwrap_err(),
        Error::TooLarge
    );
    assert!(decode_intra(&with_size(1920, 32)).is_ok());

    // Chroma format 4:2:2 (sequence extension right after the 8-byte
    // sequence header).
    assert_eq!(&es[12..16], &[0, 0, 1, 0xB5]);
    let mut d = es.clone();
    d[17] = (d[17] & !0b0110) | (2 << 1);
    assert!(matches!(decode_intra(&d), Err(Error::Unsupported(_))));

    // No picture coding extension.
    let pce = (0..es.len() - 4)
        .find(|&i| es[i..i + 4] == [0, 0, 1, 0xB5] && es[i + 4] >> 4 == 8)
        .expect("coding extension");
    let next = find_start_code(&es, pce + 4).expect("next");
    let mut d = es[..pce].to_vec();
    d.extend_from_slice(&es[next..]);
    assert!(matches!(decode_intra(&d), Err(Error::Corrupt(_))));

    // picture_structure 0.
    let mut d = es.clone();
    d[pce + 6] &= !0b11;
    assert!(matches!(decode_intra(&d), Err(Error::Corrupt(_))));

    // A sequence scalable extension after the 6-byte sequence extension.
    assert_eq!(&es[22..26], &[0, 0, 1, 0xB8]);
    let mut d = es[..22].to_vec();
    d.extend_from_slice(&[0, 0, 1, 0xB5, 0x50, 0x00]);
    d.extend_from_slice(&es[22..]);
    assert!(matches!(decode_intra(&d), Err(Error::Unsupported(_))));

    // No sequence header, no data.
    assert!(matches!(decode_intra(&es[4..]), Err(Error::Corrupt(_))));
    assert!(decode_intra(&[]).is_err());
}

// ---------------------------------------------------------------------------
// Robustness

fn fuzz_corpus() -> Vec<Vec<u8>> {
    let mut a = WriterConfig::mpeg2();
    a.concealment_motion_vectors = true;
    a.slices_per_row = 3;
    a.intra_dc_precision = 3;
    let mut b = WriterConfig::mpeg2();
    b.progressive_sequence = false;
    b.structure = Structure::TopField;
    b.intra_vlc_format = true;
    b.concealment_motion_vectors = true;
    let mut c = WriterConfig::mpeg1();
    c.macroblock_stuffing = true;
    c.slices_per_row = 0;
    let mut d = WriterConfig::mpeg2();
    d.progressive_sequence = false;
    d.frame_pred_frame_dct = false;
    d.quant_per_macroblock = true;
    d.intra_matrix = Some(ramp_matrix());
    vec![
        writer::intra_picture_blocks(&a, 80, 48, pattern(1)),
        writer::intra_picture_blocks(&b, 64, 64, pattern(2)),
        writer::intra_picture_blocks(&c, 48, 48, pattern(3)),
        writer::intra_picture_blocks(&d, 48, 64, pattern(4)),
    ]
}

#[test]
fn truncated_streams_never_panic() {
    for es in fuzz_corpus() {
        for cut in 0..es.len() {
            let data = &es[..cut];
            if let Ok(frame) = decode_intra(data) {
                assert_eq!(frame.y.len(), (frame.width * frame.height) as usize);
            }
            if let Some(range) = find_intra_picture(data) {
                assert!(range.end <= data.len());
            }
        }
    }
}

#[test]
fn random_mutations_never_panic() {
    let corpus = fuzz_corpus();
    let mut seed = 0xDEAD_BEEF_CAFE_F00Du64;
    let start = std::time::Instant::now();
    let rounds = 5000;
    let mut decoded = 0;
    for round in 0..rounds {
        let data = writer::mutate(&corpus[round % corpus.len()], &mut seed);
        if let Ok(frame) = decode_intra(&data) {
            decoded += 1;
            let planes = frame.width as usize * frame.height as usize;
            assert_eq!(frame.y.len(), planes);
            assert_eq!(
                frame.cb.len(),
                (frame.chroma_width() * frame.chroma_height()) as usize
            );
            assert!(frame.concealed_macroblocks < frame.total_macroblocks);
        }
        let _ = find_intra_picture(&data);
    }
    // Pure noise as well.
    for _ in 0..500 {
        let len = (writer::xorshift(&mut seed) % 4096) as usize;
        let mut data: Vec<u8> = (0..len)
            .map(|_| writer::xorshift(&mut seed) as u8)
            .collect();
        if data.len() > 8 {
            data[..4].copy_from_slice(&[0, 0, 1, 0xB3]);
        }
        let _ = decode_intra(&data);
        let _ = find_intra_picture(&data);
    }
    eprintln!(
        "{rounds} mutated streams in {:.2?}, {decoded} still decoded",
        start.elapsed()
    );
}
