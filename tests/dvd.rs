//! DVD-Video thumbnails on synthetic disc images.
//!
//! The images are built in memory: video elementary streams from
//! `support/mpeg2_writer.rs` (flat macroblocks of chosen colours, so the
//! decoded frames are exactly predictable), multiplexed into DVD-like VOBs by
//! `support/ps_writer.rs`, stored in ISO 9660 and UDF 1.02 images by
//! `support/disc_builder.rs`. Every scenario runs on each file system.

extern crate IsoPreview as iso_preview;

#[path = "support/disc_builder.rs"]
mod disc_builder;
#[allow(dead_code)]
#[path = "support/mpeg2_writer.rs"]
mod mpeg2_writer;
#[allow(dead_code)]
#[path = "support/ps_writer.rs"]
mod ps_writer;

use std::io::Cursor;

use iso_preview::error::Error;
use iso_preview::finder::Content;
use iso_preview::fs::FileSystem;
use iso_preview::iso9660::Iso9660;
use iso_preview::picture::Picture;
use iso_preview::reader::{CachedReader, SeekSource};
use iso_preview::udf::Udf;
use iso_preview::Extracted;

use disc_builder::{iso9660, udf102, UdfOptions, SECTOR};
use mpeg2_writer::{intra_picture, predicted_picture, WriterConfig};
use ps_writer::{mux, PsOptions};

const W: u32 = 720;
const H: u32 = 480;

fn reader(image: Vec<u8>) -> CachedReader<SeekSource<Cursor<Vec<u8>>>> {
    CachedReader::new(SeekSource(Cursor::new(image))).unwrap()
}

fn extract(image: &[u8]) -> Result<Extracted, Error> {
    iso_preview::extract_thumbnail(SeekSource(Cursor::new(image.to_vec())))
}

// ----------------------------------------------------------------------------
// Disc image builders
// ----------------------------------------------------------------------------

/// Opens every file of `files` by path and checks its size and contents.
fn open_tree<F: FileSystem>(fs: &mut F, files: &[(&str, &[u8])]) -> Vec<F::Node> {
    let mut nodes = Vec::new();
    for (path, data) in files {
        let mut node = fs.root().unwrap();
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            node = fs
                .lookup(&node, part, !last)
                .unwrap()
                .unwrap_or_else(|| panic!("{path}: {part} not found"));
        }
        assert_eq!(fs.file_size(&node).unwrap(), data.len() as u64, "{path}");
        assert_eq!(&fs.read(&node, 64 << 20).unwrap(), data, "{path}");
        nodes.push(node);
    }
    nodes
}

/// Reads every file back through `read_range` in odd-sized pieces.
fn check_ranges<F: FileSystem>(fs: &mut F, files: &[(&str, &[u8])], nodes: &[F::Node]) {
    for ((path, data), node) in files.iter().zip(nodes) {
        let mut got = Vec::new();
        let mut buf = vec![0u8; 3001];
        loop {
            let n = fs.read_range(node, got.len() as u64, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, data, "{path} via read_range");
    }
}

fn sample_files() -> Vec<(String, Vec<u8>)> {
    let pattern = |len: usize, seed: u8| -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(seed as u32) as u8)
            .collect()
    };
    let mut files = vec![
        ("VIDEO_TS/VIDEO_TS.IFO".to_string(), pattern(6000, 1)),
        ("VIDEO_TS/VIDEO_TS.VOB".to_string(), pattern(10 * 2048, 2)),
        ("VIDEO_TS/VTS_01_0.IFO".to_string(), pattern(4096, 3)),
        ("VIDEO_TS/VTS_01_1.VOB".to_string(), pattern(123_457, 4)),
        ("VIDEO_TS/VTS_01_2.VOB".to_string(), pattern(2048, 5)),
        ("AUDIO_TS/EMPTY.TXT".to_string(), Vec::new()),
        ("JACKET_P/J00___5L.MP2".to_string(), pattern(777, 6)),
        ("README.TXT".to_string(), b"hello".to_vec()),
    ];
    // Enough entries for a directory spanning several sectors.
    for i in 0..90 {
        files.push((
            format!("VIDEO_TS/VTS_{:02}_0.BUP", i + 2),
            pattern(100 + i, i as u8),
        ));
    }
    files
}

#[test]
fn builders_produce_readable_images() {
    let owned = sample_files();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    let fragmented = UdfOptions {
        extent_blocks: Some(3),
    };
    let images = [
        ("ISO 9660", iso9660(&files)),
        ("UDF", udf102(&files, UdfOptions::default())),
        ("UDF, 3-block extents", udf102(&files, fragmented)),
    ];
    for (what, image) in &images {
        let mut rd = reader(image.clone());
        if what.starts_with("UDF") {
            let mut fs = Udf::open(&mut rd).unwrap();
            assert_eq!(fs.description(), "UDF 1.02");
            let nodes = open_tree(&mut fs, &files);
            check_ranges(&mut fs, &files, &nodes);
        } else {
            let mut fs = Iso9660::open(&mut rd).unwrap();
            let nodes = open_tree(&mut fs, &files);
            check_ranges(&mut fs, &files, &nodes);
        }
    }
}

// ----------------------------------------------------------------------------
// DVD content
// ----------------------------------------------------------------------------

/// What one group of pictures shows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Look {
    /// Studio black.
    Black,
    /// A grey checkerboard (luma 60 / 180) tinted by (cb, cr): presentable.
    Checker(u8, u8),
    /// The checkerboard with 64-line black bars at the top and the bottom.
    Letterboxed(u8, u8),
    /// A flat mid-grey frame (a title card without detail).
    Flat,
}

impl Look {
    fn color(self, x: u32, y: u32) -> (u8, u8, u8) {
        let rows = H / 16;
        match self {
            Look::Black => (16, 128, 128),
            Look::Flat => (120, 128, 128),
            Look::Checker(cb, cr) => (if (x + y) % 2 == 0 { 60 } else { 180 }, cb, cr),
            Look::Letterboxed(cb, cr) => {
                if y < 4 || y >= rows - 4 {
                    (16, 128, 128)
                } else {
                    Look::Checker(cb, cr).color(x, y)
                }
            }
        }
    }
}

fn cfg(mpeg1: bool) -> WriterConfig {
    let mut cfg = if mpeg1 {
        WriterConfig::mpeg1()
    } else {
        WriterConfig::mpeg2()
    };
    cfg.sequence_end = false;
    cfg
}

/// A video elementary stream: per look, one GOP of an I-picture followed by
/// predicted pictures carrying `filler` bytes each (bulk, like real P/B
/// pictures; the decoder skips them).
fn title_es(looks: &[Look], filler: usize, mpeg1: bool) -> Vec<u8> {
    let cfg = cfg(mpeg1);
    let mut es = Vec::new();
    for look in looks {
        es.extend(intra_picture(&cfg, W, H, |x, y| look.color(x, y)));
        for _ in 0..3 {
            es.extend(predicted_picture(&cfg, 2));
            // More slice data without start codes.
            es.extend(std::iter::repeat_n(0x55u8, filler));
        }
    }
    es
}

fn vob(looks: &[Look], filler: usize, scrambled: bool) -> Vec<u8> {
    let opts = PsOptions {
        scrambled,
        end_code: false,
        ..PsOptions::default()
    };
    mux(&title_es(looks, filler, false), &opts).data
}

/// Splits a VOB at a pack boundary near `fraction` of its length, like the
/// 1 GiB parts of a real title.
fn split(vob: &[u8], fraction: f64) -> (Vec<u8>, Vec<u8>) {
    let packs = vob.len() / SECTOR;
    let cut = ((packs as f64 * fraction) as usize).max(1) * SECTOR;
    (vob[..cut].to_vec(), vob[cut..].to_vec())
}

/// Builds each disc layout (ISO 9660, UDF, UDF with fragmented files) and
/// runs `check` on its extraction result.
fn on_every_file_system(files: &[(&str, Vec<u8>)], check: impl Fn(&str, Result<Extracted, Error>)) {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(p, d)| (*p, d.as_slice())).collect();
    check("ISO 9660", extract(&iso9660(&files)));
    check("UDF", extract(&udf102(&files, UdfOptions::default())));
    let fragmented = UdfOptions {
        extent_blocks: Some(37),
    };
    check("UDF fragmented", extract(&udf102(&files, fragmented)));
}

fn picture(found: &Extracted) -> &Picture {
    match &found.thumbnail.content {
        Content::Picture(p) => p,
        Content::Encoded(_) => panic!("{}: expected a decoded picture", found.thumbnail.path),
    }
}

/// Average (B, G, R) of a picture.
fn average(p: &Picture) -> (u32, u32, u32) {
    let n = (p.width as u64 * p.height as u64).max(1);
    let mut sum = [0u64; 3];
    for px in p.bgra.chunks_exact(4) {
        for c in 0..3 {
            sum[c] += px[c] as u64;
        }
    }
    (
        (sum[0] / n) as u32,
        (sum[1] / n) as u32,
        (sum[2] / n) as u32,
    )
}

const BLUE: Look = Look::Checker(200, 110);
const RED: Look = Look::Checker(110, 200);

fn is_blue(p: &Picture) -> bool {
    let (b, _, r) = average(p);
    b > r + 40
}

fn is_red(p: &Picture) -> bool {
    let (b, _, r) = average(p);
    r > b + 40
}

fn ifo(kind: &str) -> Vec<u8> {
    let mut b = vec![0u8; 2048];
    b[..9].copy_from_slice(b"DVDVIDEO-");
    b[9..12].copy_from_slice(kind.as_bytes());
    b
}

// ----------------------------------------------------------------------------
// Scenarios
// ----------------------------------------------------------------------------

#[test]
fn main_title_is_the_largest_title_set() {
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.IFO", ifo("VMG")),
        ("VIDEO_TS/VTS_01_0.IFO", ifo("VTS")),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 4], 3000, false)),
        ("VIDEO_TS/VTS_02_0.IFO", ifo("VTS")),
        ("VIDEO_TS/VTS_02_1.VOB", vob(&[RED; 12], 3000, false)),
        ("VIDEO_TS/VTS_03_1.VOB", vob(&[BLUE; 2], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("VTS_02"), "{fs}: {path}");
        let p = picture(&found);
        assert_eq!((p.width, p.height), (W, H), "{fs}");
        assert_eq!(p.pixel_aspect, (8, 9), "{fs}: 4:3 NTSC");
        assert!(is_red(p), "{fs}: {:?}", average(p));
    });
}

#[test]
fn dark_and_flat_frames_are_passed_over() {
    // 20 GOPs; the first sampling point (25 %) and its neighbours are black
    // or flat, the second (40 %) shows the checkerboard. The title is split
    // in two VOBs so the second sample lies in the second part.
    let mut looks = vec![BLUE; 20];
    for (i, look) in looks.iter_mut().enumerate().take(8).skip(3) {
        *look = if i % 2 == 0 { Look::Black } else { Look::Flat };
    }
    let (part1, part2) = split(&vob(&looks, 4000, false), 0.33);
    let files = vec![
        ("VIDEO_TS/VTS_01_1.VOB", part1),
        ("VIDEO_TS/VTS_01_2.VOB", part2),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.ends_with("at 40%"), "{fs}: {path}");
        assert!(is_blue(picture(&found)), "{fs}");
    });
}

#[test]
fn letterbox_bars_are_cut() {
    let files = vec![(
        "VIDEO_TS/VTS_01_1.VOB",
        vob(&[Look::Letterboxed(200, 110); 6], 3000, false),
    )];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let p = picture(&found);
        assert_eq!((p.width, p.height), (W, H - 128), "{fs}");
        assert_eq!(p.display_size(), (640, 352), "{fs}");
        assert!(is_blue(p), "{fs}");
    });
}

#[test]
fn a_short_title_is_read_from_its_start() {
    // One GOP: no I-picture follows any later sampling point.
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", vob(&[RED], 20_000, false))];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.ends_with("at 0%"), "{fs}: {path}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_title_that_is_dark_throughout_still_gives_a_frame() {
    let files = vec![(
        "VIDEO_TS/VTS_01_1.VOB",
        vob(&[Look::Black; 10], 3000, false),
    )];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let p = picture(&found);
        // Nothing to frame: the black picture is kept whole.
        assert_eq!((p.width, p.height), (W, H), "{fs}");
    });
}

#[test]
fn scrambled_titles_fall_back_to_the_menu() {
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.VOB", vob(&[RED; 2], 1000, false)),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 10], 3000, true)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("(menu)"), "{fs}: {path}");
        assert!(is_red(picture(&found)), "{fs}");
    });

    // Everything scrambled: no thumbnail.
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.VOB", vob(&[RED; 2], 1000, true)),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 10], 3000, true)),
    ];
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
}

#[test]
fn jacket_picture_comes_before_root_covers_and_frames() {
    let jacket_cfg = WriterConfig::mpeg2();
    let large = intra_picture(&jacket_cfg, W, H, |x, y| RED.color(x, y));
    let small = intra_picture(&jacket_cfg, 96, 64, |x, y| BLUE.color(x, y));
    let files = vec![
        ("JACKET_P/J00___5S.MP2", small),
        ("JACKET_P/J00___5L.MP2", large.clone()),
        ("COVER.JPG", b"\xFF\xD8\xFF\xE0 not decoded here".to_vec()),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 6], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "JACKET_P/J00___5L.MP2", "{fs}");
        let p = picture(&found);
        assert_eq!((p.width, p.height), (W, H), "{fs}");
        assert!(is_red(p), "{fs}");
    });

    // A jacket wrapped in a program stream, and a broken large jacket: the
    // medium one is used.
    let medium = intra_picture(&jacket_cfg, 176, 112, |x, y| BLUE.color(x, y));
    let files = vec![
        ("JACKET_P/J00___5L.MP2", large[..large.len() / 3].to_vec()),
        (
            "JACKET_P/J00___5M.MP2",
            mux(&medium, &PsOptions::default()).data,
        ),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "JACKET_P/J00___5M.MP2", "{fs}");
        let p = picture(&found);
        assert_eq!((p.width, p.height), (176, 112), "{fs}");
        assert!(is_blue(p), "{fs}");
    });
}

#[test]
fn root_cover_comes_before_frames() {
    let files = vec![
        ("FOLDER.JPG", b"\xFF\xD8\xFF\xE0 cover".to_vec()),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 6], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.eq_ignore_ascii_case("FOLDER.JPG"), "{fs}: {path}");
        assert!(
            matches!(found.thumbnail.content, Content::Encoded(_)),
            "{fs}"
        );
    });
}

#[test]
fn blu_ray_artwork_comes_first() {
    let jacket = intra_picture(&WriterConfig::mpeg2(), W, H, |x, y| RED.color(x, y));
    let files = vec![
        (
            "BDMV/META/DL/DISC_640x360.JPG",
            b"\xFF\xD8\xFF\xE0 bd".to_vec(),
        ),
        ("JACKET_P/J00___5L.MP2", jacket),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 6], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.starts_with("BDMV/META/DL/"), "{fs}: {path}");
    });
}

#[test]
fn mpeg1_titles_work() {
    let es = title_es(&[RED; 8], 2000, true);
    let opts = PsOptions {
        mpeg1: true,
        end_code: false,
        ..PsOptions::default()
    };
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", mux(&es, &opts).data)];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn frame_search_reads_little() {
    // A long title (about 60 MiB): the search reads a few MiB at most, in a
    // few dozen requests, whatever the title length.
    let looks: Vec<Look> = (0..400)
        .map(|i| if i % 3 == 0 { Look::Black } else { BLUE })
        .collect();
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", vob(&looks, 50_000, false))];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(
            found.bytes_read <= 6 << 20,
            "{fs}: {} bytes",
            found.bytes_read
        );
        assert!(found.reads <= 64, "{fs}: {} reads", found.reads);
    });
}

#[test]
fn damaged_dvds_never_panic() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let base_vob = vob(&[BLUE, Look::Black, RED, BLUE], 2000, false);
    let jacket = intra_picture(&WriterConfig::mpeg2(), 176, 112, |x, y| RED.color(x, y));
    for round in 0..120 {
        let mut v = base_vob.clone();
        let mut j = jacket.clone();
        for _ in 0..(1 + round % 16) {
            let r = mpeg2_writer::xorshift(&mut seed) as usize;
            let at = r % v.len();
            v[at] = (r >> 32) as u8;
            let at = (r >> 16) % j.len();
            j[at] ^= (r >> 40) as u8;
        }
        if round % 7 == 0 {
            let keep = mpeg2_writer::xorshift(&mut seed) as usize % v.len();
            v.truncate(keep);
        }
        let files = vec![
            ("JACKET_P/J00___5L.MP2", j),
            ("VIDEO_TS/VIDEO_TS.VOB", v.clone()),
            ("VIDEO_TS/VTS_01_1.VOB", v),
        ];
        // Any outcome is fine as long as there is one.
        on_every_file_system(&files, |_, _| {});
    }
}

// ----------------------------------------------------------------------------
// Hostile and unusual layouts
// ----------------------------------------------------------------------------

/// A tiny I-picture that declares a `width` x `height` sequence and carries
/// one broken slice (quantiser_scale_code 0 is forbidden), so the decoder sets
/// up the picture and then decodes nothing.
fn empty_picture(width: u32, height: u32) -> Vec<u8> {
    let cfg = cfg(false);
    let mut unit = mpeg2_writer::sequence_headers(&cfg, width, height);
    let picture = mpeg2_writer::picture(&cfg, 16, 16, |_, _| {
        mpeg2_writer::MbContent::flat(128, 128, 128)
    });
    let first_slice = picture
        .windows(4)
        .position(|w| w == [0, 0, 1, 1])
        .expect("the writer emits a slice");
    unit.extend_from_slice(&picture[..first_slice]);
    unit.extend_from_slice(&[0, 0, 1, 1, 0x00, 0x00]);
    unit
}

/// Packs an elementary stream as densely as a program stream allows: full
/// 2048-byte packs with one video PES packet each and no regard for picture
/// boundaries, so tiny pictures follow each other within one packet.
fn dense_vob(es: &[u8]) -> Vec<u8> {
    const PACK_HEADER: [u8; 14] = [
        0, 0, 1, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xC3, 0xF8,
    ];
    const PAYLOAD: usize = SECTOR - PACK_HEADER.len() - 9;
    let mut out = Vec::with_capacity(es.len() / PAYLOAD * SECTOR + SECTOR);
    for chunk in es.chunks(PAYLOAD) {
        out.extend_from_slice(&PACK_HEADER);
        let len = (3 + chunk.len()) as u16;
        out.extend_from_slice(&[0, 0, 1, 0xE0]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&[0x80, 0x00, 0x00]);
        out.extend_from_slice(chunk);
    }
    out
}

fn plain_vob(es: &[u8]) -> Vec<u8> {
    let opts = PsOptions {
        end_code: false,
        ..PsOptions::default()
    };
    mux(es, &opts).data
}

#[test]
fn undecodable_pictures_cost_little() {
    // Tens of thousands of pictures that each fail to decode: the search
    // gives up after a few decodes per sampling point instead of trying
    // every one of them.
    for (width, height) in [(720, 576), (1920, 1088)] {
        let unit = empty_picture(width, height);
        let es: Vec<u8> = unit
            .iter()
            .copied()
            .cycle()
            .take(unit.len() * 40_000)
            .collect();
        let files = vec![
            ("VIDEO_TS/VTS_01_1.VOB", dense_vob(&es)),
            ("VIDEO_TS/VTS_02_1.VOB", dense_vob(&es[..es.len() / 2])),
        ];
        let started = std::time::Instant::now();
        on_every_file_system(&files, |fs, result| {
            assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
        });
        let elapsed = started.elapsed();
        // Unbounded, this took minutes; bounded it takes milliseconds (the
        // limit is loose for debug builds and slow machines).
        assert!(elapsed.as_secs() < 20, "{width}x{height}: {elapsed:?}");
    }
}

#[test]
fn titles_that_fail_leave_room_for_the_menus() {
    let unit = empty_picture(720, 480);
    let junk: Vec<u8> = unit
        .iter()
        .copied()
        .cycle()
        .take(unit.len() * 5_000)
        .collect();
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.VOB", vob(&[RED; 2], 1000, false)),
        ("VIDEO_TS/VTS_01_1.VOB", plain_vob(&junk)),
        ("VIDEO_TS/VTS_02_1.VOB", plain_vob(&junk[..junk.len() / 2])),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("(menu)"), "{fs}: {path}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_picture_at_the_very_end_of_a_vob_is_used() {
    // One I-picture and nothing after it: no start code terminates it.
    let mut c = cfg(false);
    c.sequence_end = false;
    let single = plain_vob(&intra_picture(&c, W, H, |x, y| RED.color(x, y)));
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", single.clone())];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_red(picture(&found)), "{fs}");
    });
    // The same as the only usable menu.
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.VOB", single),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 4], 3000, true)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(found.thumbnail.path.contains("(menu)"), "{fs}");
    });
}

#[test]
fn the_main_title_is_found_among_many_title_sets() {
    // 70 small title sets, each with a menu, before the main title set.
    let small = vob(&[BLUE], 500, false);
    let menu = vob(&[BLUE], 200, false);
    let mut owned: Vec<(String, Vec<u8>)> = Vec::new();
    for set in 1..=70 {
        owned.push((format!("VIDEO_TS/VTS_{set:02}_0.VOB"), menu.clone()));
        owned.push((format!("VIDEO_TS/VTS_{set:02}_1.VOB"), small.clone()));
    }
    owned.push((
        "VIDEO_TS/VTS_71_1.VOB".to_string(),
        vob(&[RED; 8], 3000, false),
    ));
    let files: Vec<(&str, Vec<u8>)> = owned.iter().map(|(p, d)| (p.as_str(), d.clone())).collect();
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("VTS_71"), "{fs}: {path}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn partly_scrambled_titles_give_no_frame() {
    // Every other pack scrambled: no picture survives whole, and clear payload
    // on both sides of a dropped packet is never joined into a frame.
    let es = title_es(&[BLUE; 12], 3000, false);
    let clear = plain_vob(&es);
    let scrambled = mux(
        &es,
        &PsOptions {
            scrambled: true,
            end_code: false,
            ..PsOptions::default()
        },
    )
    .data;
    assert_eq!(clear.len(), scrambled.len());
    let mut mixed = clear.clone();
    for (i, pack) in mixed.chunks_exact_mut(SECTOR).enumerate() {
        if i % 2 == 1 {
            pack.copy_from_slice(&scrambled[i * SECTOR..(i + 1) * SECTOR]);
        }
    }
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", mixed)];
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
}

// ----------------------------------------------------------------------------
// Menus described by IFO files
// ----------------------------------------------------------------------------

/// An IFO file (`kind` "VMG" or "VTS") whose menu PGCI unit table holds one
/// language unit with an entry PGC per `(menu id, first VOBU, last VOBU)`,
/// each with one cell. Menu ids: 2 title, 3 root, 5 audio.
fn menu_ifo(kind: &str, menus: &[(u8, u32, u32)]) -> Vec<u8> {
    let mut header = ifo(kind);
    let pointer = if kind == "VMG" { 0xC8 } else { 0xD0 };
    header[pointer..pointer + 4].copy_from_slice(&1u32.to_be_bytes());
    // One PGC per menu: 0xEC bytes of PGC fields, then one 24-byte cell.
    let pgc_len = 0xEC + 24;
    let unit_head = 8 + 8 * menus.len();
    let mut unit = vec![0u8; unit_head];
    unit[0..2].copy_from_slice(&(menus.len() as u16).to_be_bytes());
    for (n, &(id, _, _)) in menus.iter().enumerate() {
        unit[8 + n * 8] = 0x80 | id;
        let at = (unit_head + n * pgc_len) as u32;
        unit[8 + n * 8 + 4..8 + n * 8 + 8].copy_from_slice(&at.to_be_bytes());
    }
    for &(_, first, last) in menus {
        let mut pgc = vec![0u8; pgc_len];
        pgc[2] = 1; // programs
        pgc[3] = 1; // cells
        pgc[0xE8..0xEA].copy_from_slice(&0xECu16.to_be_bytes());
        pgc[0xEC + 8..0xEC + 12].copy_from_slice(&first.to_be_bytes());
        pgc[0xEC + 16..0xEC + 20].copy_from_slice(&last.to_be_bytes());
        unit.extend(pgc);
    }
    let unit_last = (unit.len() - 1) as u32;
    unit[4..8].copy_from_slice(&unit_last.to_be_bytes());
    let mut table = vec![0u8; 16];
    table[0..2].copy_from_slice(&1u16.to_be_bytes());
    table[8..10].copy_from_slice(b"en");
    table[11] = 0x80;
    table[12..16].copy_from_slice(&16u32.to_be_bytes());
    table.extend(unit);
    let table_last = (table.len() - 1) as u32;
    table[4..8].copy_from_slice(&table_last.to_be_bytes());
    header.extend(table);
    header
}

/// Concatenates VOBs (each a whole number of packs) and returns the result
/// with the sector where each part starts.
fn concat(parts: &[Vec<u8>]) -> (Vec<u8>, Vec<u32>) {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    for p in parts {
        assert_eq!(p.len() % SECTOR, 0);
        starts.push((out.len() / SECTOR) as u32);
        out.extend_from_slice(p);
    }
    (out, starts)
}

fn is_green(p: &Picture) -> bool {
    let (b, g, r) = average(p);
    g > b + 20 && g > r + 20
}

const GREEN: Look = Look::Checker(70, 70);

#[test]
fn the_root_menu_comes_before_title_frames() {
    // The menu VOB holds an audio menu (blue) and then the root menu (red);
    // the title is green.
    let (menu_vob, starts) = concat(&[vob(&[BLUE; 2], 500, false), vob(&[RED; 2], 500, false)]);
    let last = (menu_vob.len() / SECTOR - 1) as u32;
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.IFO", ifo("VMG")),
        (
            "VIDEO_TS/VTS_01_0.IFO",
            menu_ifo("VTS", &[(5, starts[0], starts[0]), (3, starts[1], last)]),
        ),
        ("VIDEO_TS/VTS_01_0.VOB", menu_vob),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "VIDEO_TS/VTS_01_0.VOB (root menu)",
            "{fs}"
        );
        assert!(
            is_red(picture(&found)),
            "{fs}: {:?}",
            average(picture(&found))
        );
    });
}

#[test]
fn a_motion_menu_is_looked_at_where_it_settles() {
    // The root menu fades in from black: its first VOBU is black, its last
    // one shows the menu.
    let (menu_vob, starts) = concat(&[
        vob(&[Look::Black; 3], 500, false),
        vob(&[RED; 2], 500, false),
    ]);
    let files = vec![
        (
            "VIDEO_TS/VTS_01_0.IFO",
            menu_ifo("VTS", &[(3, starts[0], starts[1])]),
        ),
        ("VIDEO_TS/VTS_01_0.VOB", menu_vob),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "VIDEO_TS/VTS_01_0.VOB (root menu)",
            "{fs}"
        );
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn the_title_menu_is_used_without_a_root_menu() {
    // The video manager VOB starts with a warning (flat) before its title
    // menu (red); the main title set has no menus at all.
    let (vmg_vob, starts) = concat(&[
        vob(&[Look::Flat; 2], 500, false),
        vob(&[RED; 2], 500, false),
    ]);
    let files = vec![
        (
            "VIDEO_TS/VIDEO_TS.IFO",
            menu_ifo("VMG", &[(2, starts[1], starts[1])]),
        ),
        ("VIDEO_TS/VIDEO_TS.VOB", vmg_vob),
        ("VIDEO_TS/VTS_01_0.IFO", ifo("VTS")),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "VIDEO_TS/VIDEO_TS.VOB (title menu)",
            "{fs}"
        );
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_damaged_ifo_falls_back_to_its_backup() {
    let (menu_vob, starts) = concat(&[vob(&[BLUE; 2], 500, false), vob(&[RED; 2], 500, false)]);
    let good = menu_ifo("VTS", &[(3, starts[1], starts[1])]);
    let mut bad = good.clone();
    bad[..12].copy_from_slice(b"garbage-data");
    let files = vec![
        ("VIDEO_TS/VTS_01_0.IFO", bad),
        ("VIDEO_TS/VTS_01_0.BUP", good),
        ("VIDEO_TS/VTS_01_0.VOB", menu_vob),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(found.thumbnail.path.ends_with("(root menu)"), "{fs}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_dark_menu_gives_way_to_a_title_frame() {
    let (menu_vob, starts) = concat(&[vob(&[Look::Black; 2], 500, false)]);
    let files = vec![
        (
            "VIDEO_TS/VTS_01_0.IFO",
            menu_ifo("VTS", &[(3, starts[0], starts[0])]),
        ),
        ("VIDEO_TS/VTS_01_0.VOB", menu_vob),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("title at"), "{fs}: {path}");
        assert!(is_green(picture(&found)), "{fs}");
    });
}

#[test]
fn menu_positions_outside_the_vob_are_ignored() {
    let files = vec![
        (
            "VIDEO_TS/VTS_01_0.IFO",
            menu_ifo("VTS", &[(3, 1_000_000, 2_000_000)]),
        ),
        ("VIDEO_TS/VTS_01_0.VOB", vob(&[RED; 2], 500, false)),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[GREEN; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_green(picture(&found)), "{fs}: {}", found.thumbnail.path);
    });
}

#[test]
fn damaged_ifos_never_panic() {
    let (menu_vob, starts) = concat(&[vob(&[BLUE; 2], 300, false), vob(&[RED; 2], 300, false)]);
    let base = menu_ifo(
        "VTS",
        &[(5, starts[0], starts[0]), (3, starts[1], starts[1])],
    );
    let title = vob(&[GREEN; 3], 500, false);
    let mut seed = 0xC0FF_EE11_D00D_F00Du64;
    for round in 0..150 {
        let mut ifo_bytes = base.clone();
        for _ in 0..(1 + round % 12) {
            let r = mpeg2_writer::xorshift(&mut seed) as usize;
            // Mostly the table (from 2048 on), sometimes the header.
            let at = if r % 4 == 0 {
                r % 256
            } else {
                2048 + (r >> 8) % (ifo_bytes.len() - 2048)
            };
            ifo_bytes[at] = (r >> 40) as u8;
        }
        if round % 9 == 0 {
            let keep = mpeg2_writer::xorshift(&mut seed) as usize % ifo_bytes.len();
            ifo_bytes.truncate(keep);
        }
        let files = vec![
            ("VIDEO_TS/VTS_01_0.IFO", ifo_bytes),
            ("VIDEO_TS/VTS_01_0.VOB", menu_vob.clone()),
            ("VIDEO_TS/VTS_01_1.VOB", title.clone()),
        ];
        on_every_file_system(&files, |fs, result| {
            // Whatever the IFO says, a picture comes out: a menu or the title.
            assert!(result.is_ok(), "{fs}: {:?}", result.err());
        });
    }
}

// ----------------------------------------------------------------------------
// Second review round
// ----------------------------------------------------------------------------

#[test]
fn an_unreadable_video_ts_is_not_a_crash() {
    // The VIDEO_TS directory record claims 5 MiB, more than a directory may
    // be: the walk fails, and the search must give up quietly.
    let files = [
        ("VIDEO_TS/VIDEO_TS.VOB", vob(&[RED; 2], 500, false)),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[BLUE; 4], 3000, false)),
    ];
    let files: Vec<(&str, &[u8])> = files.iter().map(|(p, d)| (*p, d.as_slice())).collect();
    let mut image = iso9660(&files);
    let root = 18 * SECTOR;
    let at = image[root..root + SECTOR]
        .windows(8)
        .position(|w| w == b"VIDEO_TS")
        .expect("VIDEO_TS record")
        + root
        - 33;
    let size = 5u32 << 20;
    image[at + 10..at + 14].copy_from_slice(&size.to_le_bytes());
    image[at + 14..at + 18].copy_from_slice(&size.to_be_bytes());
    assert_eq!(extract(&image).err(), Some(Error::NotFound));
}

#[test]
fn an_undecodable_main_title_leaves_room_for_the_next_title_set() {
    let unit = empty_picture(720, 480);
    let junk: Vec<u8> = unit
        .iter()
        .copied()
        .cycle()
        .take(unit.len() * 60_000)
        .collect();
    let files = vec![
        ("VIDEO_TS/VTS_01_1.VOB", dense_vob(&junk)),
        ("VIDEO_TS/VTS_02_1.VOB", vob(&[RED; 6], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let path = &found.thumbnail.path;
        assert!(path.contains("VTS_02"), "{fs}: {path}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn an_unreadable_title_vob_does_not_count_as_searched() {
    use iso_preview::finder::find_thumbnail;
    let title = vob(&[RED; 4], 3000, false);
    let files: [(&str, &[u8]); 1] = [("VIDEO_TS/VTS_01_1.VOB", &title)];
    let mut image = udf102(&files, UdfOptions::default());
    // Healthy: the title VOB is read, so the search counts as done.
    let mut rd = reader(image.clone());
    let mut fs = Udf::open(&mut rd).unwrap();
    let mut searched = false;
    assert!(find_thumbnail(&mut fs, &mut searched).is_ok());
    assert!(searched);
    // Break the File Entry of the title VOB (its tag checksum): its size
    // cannot be read, so another view of the disc must still be searched.
    let fe = image
        .chunks_exact(SECTOR)
        .position(|s| {
            s[0..2] == 261u16.to_le_bytes() && s[56..64] == (title.len() as u64).to_le_bytes()
        })
        .expect("file entry of the title VOB");
    image[fe * SECTOR + 4] ^= 0xFF;
    let mut rd = reader(image);
    let mut fs = Udf::open(&mut rd).unwrap();
    let mut searched = false;
    assert_eq!(
        find_thumbnail(&mut fs, &mut searched).err(),
        Some(Error::NotFound)
    );
    assert!(!searched);
}

#[test]
fn sparsely_scrambled_titles_still_give_a_frame() {
    // One pack in 20 is scrambled, so every read chunk has holes: the whole
    // pictures between them are used.
    let es = title_es(&[BLUE; 120], 3000, false);
    let clear = plain_vob(&es);
    let scrambled = mux(
        &es,
        &PsOptions {
            scrambled: true,
            end_code: false,
            ..PsOptions::default()
        },
    )
    .data;
    let mut mixed = clear.clone();
    for (i, pack) in mixed.chunks_exact_mut(SECTOR).enumerate() {
        if i % 20 == 19 {
            pack.copy_from_slice(&scrambled[i * SECTOR..(i + 1) * SECTOR]);
        }
    }
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", mixed)];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_blue(picture(&found)), "{fs}");
    });
}

#[test]
fn pictures_larger_than_dvd_are_not_decoded() {
    // A DVD-sized sequence header followed by an HD one that the I-picture
    // actually uses: the size check must look at the second. The picture is
    // terminated, so it is decoded as found, from the first header on.
    let c = cfg(false);
    let mut es = mpeg2_writer::sequence_headers(&c, 720, 480);
    es.extend(intra_picture(&c, 1280, 720, |x, y| RED.color(x, y)));
    es.extend_from_slice(&mpeg2_writer::SEQUENCE_END);
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", plain_vob(&es))];
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
}

#[test]
fn a_truncated_last_picture_is_not_used() {
    let mut c = cfg(false);
    c.sequence_end = false;
    let picture_es = intra_picture(&c, W, H, |x, y| RED.color(x, y));
    // A few percent missing: few enough to pass the damage limit, so only
    // the end-of-file rule keeps the grey rows out.
    let cut = &picture_es[..picture_es.len() * 97 / 100];
    let files = vec![("VIDEO_TS/VTS_01_1.VOB", plain_vob(cut))];
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
}

#[test]
fn a_dark_title_gives_way_to_a_menu_without_ifo() {
    let files = vec![
        ("VIDEO_TS/VIDEO_TS.VOB", vob(&[RED; 2], 500, false)),
        ("VIDEO_TS/VTS_01_1.VOB", vob(&[Look::Black; 8], 3000, false)),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(
            found.thumbnail.path.contains("(menu)"),
            "{fs}: {}",
            found.thumbnail.path
        );
        assert!(is_red(picture(&found)), "{fs}");
    });
}
