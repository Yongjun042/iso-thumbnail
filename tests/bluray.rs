//! Blu-ray video frames on synthetic disc images.
//!
//! Clips are built in memory: MPEG-2 pictures from `support/mpeg2_writer.rs`
//! and H.264 pictures of I_PCM macroblocks (written here, so the decoded frame
//! is exactly known) are packed one access unit per PES packet into 192-byte
//! source packets, next to a clip information file listing their entry
//! points, and stored in ISO 9660 and UDF images. The HEVC and VC-1 key frames
//! in `tests/data` were made by the Windows encoders from a synthetic pattern
//! (a horizontal luma ramp with a white box). The codecs Windows decodes are
//! tested on Windows only, and skipped where the decoder is not installed.

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

use iso_preview::bluray;
use iso_preview::clpi::{i_picture_bound, CODING_MPEG2};
use iso_preview::error::Error;
use iso_preview::finder::Content;
use iso_preview::fs::FileSystem;
use iso_preview::picture::Picture;
use iso_preview::reader::{CachedReader, SeekSource};
use iso_preview::udf::Udf;
use iso_preview::Extracted;

use disc_builder::{iso9660, udf102, UdfOptions};
use mpeg2_writer::{intra_picture, predicted_picture, WriterConfig};

const VIDEO_PID: u16 = 0x1011;
const AUDIO_PID: u16 = 0x1100;
const SOURCE_PACKET: usize = 192;
const ALIGNED_UNIT: usize = 6144;
/// `video_format` codes.
const FORMAT_480I: u8 = 1;
const FORMAT_480P: u8 = 3;
const FORMAT_720P: u8 = 5;
const FORMAT_1080P: u8 = 6;
const FORMAT_2160P: u8 = 8;
/// `application_type` of a main clip and of a stereoscopic dependent view.
const APP_MAIN: u8 = 1;
const APP_DEPENDENT_VIEW: u8 = 8;

fn extract(image: &[u8]) -> Result<Extracted, Error> {
    iso_preview::extract_thumbnail(SeekSource(Cursor::new(image.to_vec())))
}

// ----------------------------------------------------------------------------
// Pictures
// ----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Look {
    /// Studio black.
    Black,
    /// A grey checkerboard of macroblocks (luma 60 / 180) tinted by (cb, cr).
    Checker(u8, u8),
}

impl Look {
    /// (Y, Cb, Cr) of macroblock (x, y).
    fn color(self, x: u32, y: u32) -> (u8, u8, u8) {
        match self {
            Look::Black => (16, 128, 128),
            Look::Checker(cb, cr) => (if (x + y) % 2 == 0 { 60 } else { 180 }, cb, cr),
        }
    }
}

const BLUE: Look = Look::Checker(200, 110);
const RED: Look = Look::Checker(110, 200);

fn picture(found: &Extracted) -> &Picture {
    match &found.thumbnail.content {
        Content::Picture(p) => p,
        Content::Encoded(_) => panic!("{}: expected a decoded picture", found.thumbnail.path),
    }
}

/// Average (B, G, R) of the rows of `p` selected by `row`.
fn average_where(p: &Picture, row: impl Fn(u32) -> bool) -> (u32, u32, u32) {
    let mut sum = [0u64; 3];
    let mut n = 0u64;
    for (y, line) in p.bgra.chunks_exact(p.width as usize * 4).enumerate() {
        if !row(y as u32) {
            continue;
        }
        for px in line.chunks_exact(4) {
            for c in 0..3 {
                sum[c] += px[c] as u64;
            }
            n += 1;
        }
    }
    let n = n.max(1);
    (
        (sum[0] / n) as u32,
        (sum[1] / n) as u32,
        (sum[2] / n) as u32,
    )
}

fn is_red(p: &Picture) -> bool {
    let (b, _, r) = average_where(p, |_| true);
    r > b + 40
}

fn is_blue(p: &Picture) -> bool {
    let (b, _, r) = average_where(p, |_| true);
    b > r + 40
}

// ----------------------------------------------------------------------------
// Transport streams
// ----------------------------------------------------------------------------

/// A clip under construction: its source packets and entry points.
struct Clip {
    data: Vec<u8>,
    /// (source packet number, I_end_position_offset code) of each key frame.
    entries: Vec<(u32, u8)>,
    continuity: [u8; 3],
    uhd: bool,
    /// `I_end_position_offset` to write instead of the right one (0: unset).
    i_end: Option<u8>,
    /// Write `PES_packet_length` into video packets that fit it.
    stated_lengths: bool,
}

impl Clip {
    fn new(uhd: bool) -> Self {
        Self {
            data: Vec::new(),
            entries: Vec::new(),
            continuity: [0; 3],
            uhd,
            i_end: None,
            stated_lengths: false,
        }
    }

    fn packets(&self) -> u32 {
        (self.data.len() / SOURCE_PACKET) as u32
    }

    /// One source packet: TP_extra_header (copy permission 0, arrival time 0)
    /// and a TS packet carrying `payload`, padded with adaptation stuffing.
    fn ts_packet(&mut self, pid: u16, start: bool, payload: &[u8]) {
        assert!(payload.len() <= 184);
        let slot = match pid {
            VIDEO_PID => 0,
            AUDIO_PID => 1,
            _ => 2,
        };
        let cc = self.continuity[slot];
        self.continuity[slot] = (cc + 1) & 0x0F;
        let mut p = vec![0u8; 4];
        p.push(0x47);
        p.push(((start as u8) << 6) | (pid >> 8) as u8);
        p.push(pid as u8);
        if payload.len() == 184 {
            p.push(0x10 | cc);
        } else {
            p.push(0x30 | cc);
            let stuffing = 183 - payload.len();
            p.push(stuffing as u8);
            if stuffing > 0 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuffing - 1));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), SOURCE_PACKET);
        self.data.extend(p);
    }

    /// One PES packet of `es` (PTS only, unbounded length as Blu-ray video
    /// unless `stated_lengths`).
    fn pes(&mut self, pid: u16, stream_id: u8, es: &[u8]) {
        let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        pes.extend_from_slice(es);
        if let Ok(len) = u16::try_from(pes.len() - 6) {
            if self.stated_lengths || pid != VIDEO_PID {
                pes[4..6].copy_from_slice(&len.to_be_bytes());
            }
        }
        for (i, chunk) in pes.chunks(184).enumerate() {
            self.ts_packet(pid, i == 0, chunk);
        }
    }

    /// A video access unit followed by two audio packets.
    fn unit(&mut self, stream_id: u8, au: &[u8]) {
        self.pes(VIDEO_PID, stream_id, au);
        for _ in 0..2 {
            self.pes(AUDIO_PID, 0xBD, &[0xAA; 170]);
        }
    }

    /// A key frame: an entry point at its first access unit.
    fn key_frame(&mut self, stream_id: u8, units: &[&[u8]]) {
        let spn = self.packets();
        self.unit(stream_id, units[0]);
        let len = u64::from(self.packets() - spn) * SOURCE_PACKET as u64;
        let code = (1..=6)
            .find(|&c| i_picture_bound(c, self.uhd).unwrap() >= len)
            .unwrap_or(7);
        self.entries.push((spn, self.i_end.unwrap_or(code)));
        for au in &units[1..] {
            self.unit(stream_id, au);
        }
    }

    /// Pads the clip to whole aligned units with null packets.
    fn finish(mut self) -> (Vec<u8>, Vec<(u32, u8)>) {
        while self.data.len() % ALIGNED_UNIT != 0 {
            self.ts_packet(0x1FFF, false, &[0xFF; 184]);
        }
        assert!(
            self.packets() < 0x20000,
            "keep entry points in one coarse range"
        );
        (self.data, self.entries)
    }
}

/// Marks every aligned unit as AACS-encrypted (its copy permission bits).
fn encrypt(m2ts: &mut [u8]) {
    for unit in m2ts.chunks_mut(ALIGNED_UNIT) {
        unit[0] |= 0xC0;
    }
}

/// Clip information file attributes of the video stream.
#[derive(Clone, Copy)]
struct Stream {
    coding: u8,
    format: u8,
    app_type: u8,
    /// HEVC `dynamic_range_type << 4 | color_space`.
    hevc_range: u8,
}

/// A clip information file for a clip of `packets` source packets with the
/// given entry points (in coarse groups of four).
fn clpi(s: Stream, packets: u32, points: &[(u32, u8)]) -> Vec<u8> {
    let mut d = vec![0u8; 40];
    d[..8].copy_from_slice(b"HDMV0200");
    let mut clip_info = vec![0u8; 4 + 148];
    clip_info[6] = 1; // Clip_stream_type
    clip_info[7] = s.app_type;
    clip_info[16..20].copy_from_slice(&packets.to_be_bytes());
    let len = (clip_info.len() - 4) as u32;
    clip_info[..4].copy_from_slice(&len.to_be_bytes());
    d.extend(clip_info);
    // ProgramInfo: one sequence, an audio stream, then the video stream.
    let pi = d.len() as u32;
    let mut prog = vec![0, 0, 0, 0, 0, 1];
    prog.extend(0u32.to_be_bytes());
    prog.extend(0x0100u16.to_be_bytes());
    prog.extend([2, 0]);
    prog.extend(AUDIO_PID.to_be_bytes());
    prog.extend([5, 0x80, 0x31, b'e', b'n', b'g']);
    prog.extend(VIDEO_PID.to_be_bytes());
    prog.extend([21, s.coding, (s.format << 4) | 1, 0x30, s.hevc_range]);
    prog.extend([0u8; 17]);
    let plen = (prog.len() - 4) as u32;
    prog[..4].copy_from_slice(&plen.to_be_bytes());
    d.extend(prog);
    // CPI: the EP_map of the video PID.
    let cpi = d.len() as u32;
    let coarse: Vec<usize> = (0..points.len()).step_by(4).collect();
    let mut ep = vec![0u8, 1];
    ep.extend(VIDEO_PID.to_be_bytes());
    let v: u64 = (1u64 << 34) | ((coarse.len() as u64) << 18) | points.len() as u64;
    ep.extend(&(v << 16).to_be_bytes()[..6]);
    ep.extend(14u32.to_be_bytes());
    ep.extend(((4 + 8 * coarse.len()) as u32).to_be_bytes());
    for &first in &coarse {
        ep.extend((((first as u64) << 46) | u64::from(points[first].0)).to_be_bytes());
    }
    for &(spn, i_end) in points {
        ep.extend(((u32::from(i_end) << 28) | (spn & 0x1FFFF)).to_be_bytes());
    }
    let mut block = vec![0u8; 6];
    block[5] = 1; // CPI_type: EP_map
    block.extend(ep);
    let blen = (block.len() - 4) as u32;
    block[..4].copy_from_slice(&blen.to_be_bytes());
    d.extend(block);
    d[12..16].copy_from_slice(&pi.to_be_bytes());
    d[16..20].copy_from_slice(&cpi.to_be_bytes());
    d
}

/// A clip's `.m2ts` and `.clpi`.
type ClipFiles = (Vec<u8>, Vec<u8>);

/// The files of a finished clip.
fn clip_files(clip: Clip, stream: Stream) -> ClipFiles {
    let (m2ts, entries) = clip.finish();
    let info = clpi(stream, (m2ts.len() / SOURCE_PACKET) as u32, &entries);
    (m2ts, info)
}

/// A Blu-ray layout: `BDMV/STREAM/<n>.m2ts` and `BDMV/CLIPINF/<n>.clpi`.
fn bdmv(clips: Vec<(&str, ClipFiles)>) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![("BDMV/index.bdmv".to_string(), b"INDX0200".to_vec())];
    for (name, (m2ts, info)) in clips {
        files.push((format!("BDMV/STREAM/{name}.m2ts"), m2ts));
        files.push((format!("BDMV/CLIPINF/{name}.clpi"), info));
    }
    files
}

/// Builds each disc layout (ISO 9660, UDF, UDF with fragmented files) and
/// runs `check` on its extraction result.
fn on_every_file_system(
    files: &[(String, Vec<u8>)],
    check: impl Fn(&str, Result<Extracted, Error>),
) {
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    check("ISO 9660", extract(&iso9660(&files)));
    check("UDF", extract(&udf102(&files, UdfOptions::default())));
    let fragmented = UdfOptions {
        extent_blocks: Some(37),
    };
    check("UDF fragmented", extract(&udf102(&files, fragmented)));
}

// ----------------------------------------------------------------------------
// MPEG-2 clips
// ----------------------------------------------------------------------------

const W: u32 = 1280;
const H: u32 = 720;

const MPEG2: Stream = Stream {
    coding: CODING_MPEG2,
    format: FORMAT_720P,
    app_type: APP_MAIN,
    hevc_range: 0,
};

fn mpeg2_cfg() -> WriterConfig {
    let mut cfg = WriterConfig::mpeg2();
    cfg.sequence_end = false;
    cfg.aspect_code = 3;
    cfg
}

/// Per look, one GOP: an I-picture, then three predicted pictures carrying
/// `filler` bytes each (the bulk of a real stream; never decoded).
fn mpeg2_clip(looks: &[Look], filler: usize) -> Clip {
    let cfg = mpeg2_cfg();
    let mut clip = Clip::new(false);
    for look in looks {
        let es = intra_picture(&cfg, W, H, |x, y| look.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        for _ in 0..3 {
            let mut p = predicted_picture(&cfg, 2);
            p.extend(std::iter::repeat_n(0x55u8, filler));
            clip.unit(0xE0, &p);
        }
    }
    clip
}

/// An MPEG-2 clip of about 1.5 MB (larger than the 1 MiB minimum).
fn mpeg2_files(looks: &[Look]) -> ClipFiles {
    clip_files(mpeg2_clip(looks, 400_000 / looks.len()), MPEG2)
}

// ----------------------------------------------------------------------------
// Scenarios
// ----------------------------------------------------------------------------

#[test]
fn the_main_feature_is_the_largest_clip() {
    let files = bdmv(vec![
        ("00001", mpeg2_files(&[BLUE; 4])),
        ("00002", clip_files(mpeg2_clip(&[RED; 12], 60_000), MPEG2)),
        // Too small to be worth a try, even though it is first.
        ("00000", clip_files(mpeg2_clip(&[BLUE; 2], 1000), MPEG2)),
    ]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00002.m2ts (25%)", "{fs}");
        let p = picture(&found);
        assert_eq!((p.width, p.height), (W, H), "{fs}");
        assert!(is_red(p), "{fs}");
    });
}

#[test]
fn artwork_comes_before_the_video() {
    let mut files = bdmv(vec![("00001", mpeg2_files(&[RED; 8]))]);
    files.push((
        "BDMV/META/DL/cover_640x360.jpg".to_string(),
        b"\xFF\xD8\xFF\xE0 cover".to_vec(),
    ));
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/META/DL/cover_640x360.jpg",
            "{fs}"
        );
        assert!(
            matches!(found.thumbnail.content, Content::Encoded(_)),
            "{fs}"
        );
    });
}

#[test]
fn dark_entry_points_are_passed_over() {
    // 20 key frames: the first position tried (25 %, the fifth) and those
    // before it are black, the second (40 %, the eighth) is red.
    let mut looks = [RED; 20];
    looks[..6].fill(Look::Black);
    let files = bdmv(vec![("00001", mpeg2_files(&looks))]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (40%)", "{fs}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_clip_that_is_dark_throughout_still_gives_a_frame() {
    let files = bdmv(vec![("00001", mpeg2_files(&[Look::Black; 10]))]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        let p = picture(&found);
        // Nothing to frame: the black picture is kept whole.
        assert_eq!((p.width, p.height), (W, H), "{fs}");
    });
}

#[test]
fn encrypted_clips_are_left_alone() {
    let (plain, info) = mpeg2_files(&[RED; 8]);
    // Only the aligned units holding an entry point marked: the packets read
    // from the entry point on never pass a marked unit start.
    let mut marked = plain.clone();
    for (spn, _) in iso_preview::clpi::parse(&info)
        .unwrap()
        .entries
        .iter()
        .map(|e| (e.spn, ()))
    {
        let at = spn as usize * SOURCE_PACKET;
        marked[at - at % ALIGNED_UNIT] |= 0xC0;
    }
    let files = bdmv(vec![("00001", (marked, info.clone()))]);
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
    let mut m2ts = plain;
    encrypt(&mut m2ts);
    let files = bdmv(vec![("00001", (m2ts, info))]);
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
    });
    // The disc is given up at the first encrypted unit, after one span.
    let flat: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    let image = udf102(&flat, UdfOptions::default());
    let mut rd = CachedReader::new(SeekSource(Cursor::new(image))).unwrap();
    let mut fs = Udf::open(&mut rd).unwrap();
    let root = fs.root().unwrap();
    let dir = fs.lookup(&root, "BDMV", true).unwrap().unwrap();
    let mut searched = false;
    assert!(bluray::video_picture(&mut fs, &dir, &mut searched).is_none());
    drop(fs);
    assert!(rd.bytes < 400_000, "read {} bytes", rd.bytes);
}

#[test]
fn dependent_view_clips_are_skipped() {
    // The largest clip is the dependent (right-eye) view of a 3D feature.
    let dependent = Stream {
        app_type: APP_DEPENDENT_VIEW,
        ..MPEG2
    };
    let files = bdmv(vec![
        ("00001", mpeg2_files(&[BLUE; 8])),
        (
            "00002",
            clip_files(mpeg2_clip(&[RED; 12], 60_000), dependent),
        ),
    ]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)", "{fs}");
        assert!(is_blue(picture(&found)), "{fs}");
    });
}

#[test]
fn clip_information_comes_from_the_backup_too() {
    let (m2ts, info) = mpeg2_files(&[RED; 8]);
    let files = vec![
        ("BDMV/STREAM/00001.m2ts".to_string(), m2ts),
        ("BDMV/BACKUP/CLIPINF/00001.clpi".to_string(), info),
    ];
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn clips_without_usable_clip_information_give_nothing() {
    let (m2ts, info) = mpeg2_files(&[RED; 8]);
    let mut broken = info.clone();
    broken[..4].copy_from_slice(b"XXXX");
    for files in [
        vec![("BDMV/STREAM/00001.m2ts".to_string(), m2ts.clone())],
        vec![
            ("BDMV/STREAM/00001.m2ts".to_string(), m2ts.clone()),
            ("BDMV/CLIPINF/00001.clpi".to_string(), broken),
        ],
    ] {
        on_every_file_system(&files, |fs, result| {
            assert_eq!(result.err(), Some(Error::NotFound), "{fs}");
        });
    }
}

#[test]
fn entry_points_without_a_size_bound_still_work() {
    let mut clip = mpeg2_clip(&[], 0);
    clip.i_end = Some(0);
    let cfg = mpeg2_cfg();
    for _ in 0..8 {
        let es = intra_picture(&cfg, W, H, |x, y| RED.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        let mut p = predicted_picture(&cfg, 2);
        p.extend(std::iter::repeat_n(0x55u8, 150_000));
        clip.unit(0xE0, &p);
    }
    let files = bdmv(vec![("00001", clip_files(clip, MPEG2))]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn mpeg2_spans_are_read_on_after_a_packet_of_stated_length() {
    // Video packets state their length, the entry points understate the
    // I-pictures, and many audio packets follow each one: the first span
    // ends after the I-picture's packet but before the next picture starts,
    // which the MPEG-2 search needs to see.
    let cfg = mpeg2_cfg();
    let mut clip = Clip::new(false);
    clip.stated_lengths = true;
    clip.i_end = Some(1);
    for _ in 0..6 {
        let es = intra_picture(&cfg, W, H, |x, y| RED.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        for _ in 0..800 {
            clip.pes(AUDIO_PID, 0xBD, &[0xAA; 170]);
        }
        let mut p = predicted_picture(&cfg, 2);
        p.extend(std::iter::repeat_n(0x55u8, 30_000));
        clip.unit(0xE0, &p);
    }
    let files = bdmv(vec![("00001", clip_files(clip, MPEG2))]);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_disc_with_dvd_and_bluray_folders_gets_its_bluray_turn() {
    // The DVD title is scrambled, so its video step gives nothing.
    let cfg = {
        let mut c = WriterConfig::mpeg2();
        c.sequence_end = false;
        c
    };
    let mut es = Vec::new();
    for _ in 0..4 {
        es.extend(intra_picture(&cfg, 720, 480, |x, y| BLUE.color(x, y)));
    }
    let opts = ps_writer::PsOptions {
        scrambled: true,
        end_code: false,
        ..ps_writer::PsOptions::default()
    };
    let mut files = bdmv(vec![("00001", mpeg2_files(&[RED; 8]))]);
    files.push((
        "VIDEO_TS/VTS_01_1.VOB".to_string(),
        ps_writer::mux(&es, &opts).data,
    ));
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert!(found.thumbnail.path.starts_with("BDMV/STREAM/"), "{fs}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn hostile_clips_never_panic_and_cost_little() {
    // Entry points that lead nowhere: into audio packets, past key frames,
    // with no size bound, so every position reads the most it may.
    let mut clip = mpeg2_clip(&[], 0);
    clip.i_end = Some(0);
    for _ in 0..6000 {
        clip.pes(AUDIO_PID, 0xBD, &[0xAA; 170]);
    }
    let (m2ts, _) = clip.finish();
    assert!(m2ts.len() > 1 << 20);
    let packets = (m2ts.len() / SOURCE_PACKET) as u32;
    let points: Vec<(u32, u8)> = (0..50).map(|i| (i * 110, 0)).collect();
    let info = clpi(MPEG2, packets, &points);
    let files = bdmv(vec![("00001", (m2ts, info))]);
    on_every_file_system(&files, |fs, result| {
        assert_eq!(result.as_ref().err(), Some(&Error::NotFound), "{fs}");
    });

    // Random damage to a working clip and its clip information.
    let (m2ts, info) = mpeg2_files(&[RED; 6]);
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..60 {
        let mut m = m2ts.clone();
        let mut c = info.clone();
        for _ in 0..20 {
            let i = next() as usize % m.len();
            m[i] = next() as u8;
        }
        for _ in 0..1 + next() % 4 {
            let i = next() as usize % c.len();
            c[i] = next() as u8;
        }
        if round % 3 == 0 {
            let cut = next() as usize % c.len();
            c.truncate(cut);
        }
        let files = bdmv(vec![("00001", (m, c))]);
        let flat: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        if let Ok(found) = extract(&udf102(&flat, UdfOptions::default())) {
            assert!(
                found.bytes_read < 24 << 20,
                "round {round}: {} bytes",
                found.bytes_read
            );
        }
    }
}

// ----------------------------------------------------------------------------
// Codecs decoded by Windows
// ----------------------------------------------------------------------------

#[cfg(windows)]
mod windows_decoders {
    use super::*;
    use iso_preview::clpi::{CODING_H264, CODING_HEVC, CODING_VC1, DYNAMIC_RANGE_HDR10};
    use iso_preview::mf;

    /// Starts COM on the test thread and says whether Windows has `codec`.
    fn decoder_available(codec: mf::Codec) -> bool {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let ok = mf::Session::start().is_some_and(|s| mf::available(&s, codec));
        if !ok {
            eprintln!("skipped: no {codec:?} decoder on this system");
        }
        ok
    }

    // ------------------------------------------------------------------------
    // H.264 of I_PCM macroblocks
    // ------------------------------------------------------------------------

    struct Bits {
        out: Vec<u8>,
        cur: u8,
        n: u8,
    }

    impl Bits {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                cur: 0,
                n: 0,
            }
        }

        fn bit(&mut self, b: bool) {
            self.cur = (self.cur << 1) | b as u8;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.n = 0;
            }
        }

        fn put(&mut self, v: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.bit((v >> i) & 1 == 1);
            }
        }

        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }

        fn align_zero(&mut self) {
            while self.n != 0 {
                self.bit(false);
            }
        }

        fn bytes(&mut self, b: &[u8]) {
            assert_eq!(self.n, 0);
            self.out.extend_from_slice(b);
        }

        /// rbsp_trailing_bits.
        fn finish(mut self) -> Vec<u8> {
            self.bit(true);
            self.align_zero();
            self.out
        }
    }

    /// A NAL unit with its start code and emulation prevention.
    fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    const SD_W_MBS: u32 = 45;
    const SD_H_MBS: u32 = 30;

    /// AUD (I-picture), and SPS + PPS for `interlaced` or progressive 720x480.
    fn h264_headers(interlaced: bool) -> Vec<u8> {
        let mut out = nal(0x09, &[0x10]);
        let mut s = Bits::new();
        s.put(100, 8); // High profile
        s.put(0, 8);
        s.put(40, 8);
        s.ue(0); // seq_parameter_set_id
        s.ue(1); // chroma_format_idc 4:2:0
        s.ue(0);
        s.ue(0); // 8-bit
        s.bit(false);
        s.bit(false); // no scaling matrices
        s.ue(0); // log2_max_frame_num_minus4
        s.ue(2); // pic_order_cnt_type
        s.ue(1); // max_num_ref_frames
        s.bit(false);
        s.ue(SD_W_MBS - 1);
        s.ue(if interlaced { SD_H_MBS / 2 } else { SD_H_MBS } - 1);
        s.bit(!interlaced); // frame_mbs_only_flag
        if interlaced {
            s.bit(false); // mb_adaptive_frame_field_flag
        }
        s.bit(true); // direct_8x8_inference_flag
        s.bit(false); // no cropping
        s.bit(false); // no VUI
        out.extend(nal(0x67, &s.finish()));
        let mut p = Bits::new();
        p.ue(0);
        p.ue(0);
        p.bit(false); // CAVLC
        p.bit(false);
        p.ue(0);
        p.ue(0);
        p.ue(0);
        p.bit(false);
        p.put(0, 2);
        p.ue(0); // pic_init_qp_minus26 (se 0)
        p.ue(0);
        p.ue(0);
        p.bit(true); // deblocking_filter_control_present_flag
        p.bit(false);
        p.bit(false);
        out.extend(nal(0x68, &p.finish()));
        out
    }

    /// An I slice of I_PCM macroblocks: a progressive frame (`field` None) or
    /// the top / bottom field (`Some(false)` / `Some(true)`) of a frame.
    fn h264_slice(look: Look, field: Option<bool>, idr: bool) -> Vec<u8> {
        let mut s = Bits::new();
        s.ue(0); // first_mb_in_slice
        s.ue(7); // slice_type: I, whole picture
        s.ue(0); // pic_parameter_set_id
        s.put(0, 4); // frame_num
        if let Some(bottom) = field {
            s.bit(true); // field_pic_flag
            s.bit(bottom);
        }
        if idr {
            s.ue(0); // idr_pic_id
            s.bit(false);
            s.bit(false);
        } else {
            s.bit(false); // adaptive_ref_pic_marking_mode_flag
        }
        s.ue(0); // slice_qp_delta (se 0)
        s.ue(1); // disable_deblocking_filter_idc
        let rows = if field.is_some() {
            SD_H_MBS / 2
        } else {
            SD_H_MBS
        };
        for y in 0..rows {
            for x in 0..SD_W_MBS {
                let (l, cb, cr) = look.color(x, y);
                s.ue(25); // I_PCM
                s.align_zero();
                s.bytes(&[l; 256]);
                s.bytes(&[cb; 64]);
                s.bytes(&[cr; 64]);
            }
        }
        nal(if idr { 0x65 } else { 0x61 }, &s.finish())
    }

    /// A unit the decoder never sees: AUD of a P-picture and filler.
    fn h264_filler(len: usize) -> Vec<u8> {
        let mut au = nal(0x09, &[0x30]);
        au.extend(nal(0x01, &vec![0x55; len]));
        au
    }

    fn h264_stream(format: u8) -> Stream {
        Stream {
            coding: CODING_H264,
            format,
            app_type: APP_MAIN,
            hevc_range: 0,
        }
    }

    #[test]
    fn h264_key_frames_are_decoded() {
        if !decoder_available(mf::Codec::H264) {
            return;
        }
        let mut clip = Clip::new(false);
        for _ in 0..3 {
            let mut au = h264_headers(false);
            au.extend(h264_slice(RED, None, true));
            clip.key_frame(0xE0, &[&au[..]]);
            clip.unit(0xE0, &h264_filler(100_000));
        }
        let files = bdmv(vec![("00001", clip_files(clip, h264_stream(FORMAT_480P)))]);
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            let p = picture(&found);
            assert_eq!((p.width, p.height), (720, 480), "{fs}");
            assert!(is_red(p), "{fs}");
        });
    }

    #[test]
    fn spans_grow_until_the_access_unit_ends() {
        if !decoder_available(mf::Codec::H264) {
            return;
        }
        // The entry points understate the 0.5 MB I-pictures as 128 KiB: the
        // span is doubled until the next video packet starts in it.
        let mut clip = Clip::new(false);
        clip.i_end = Some(1);
        for _ in 0..3 {
            let mut au = h264_headers(false);
            au.extend(h264_slice(RED, None, true));
            clip.key_frame(0xE0, &[&au[..]]);
            clip.unit(0xE0, &h264_filler(100_000));
        }
        let files = bdmv(vec![("00001", clip_files(clip, h264_stream(FORMAT_480P)))]);
        let flat: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let found = extract(&udf102(&flat, UdfOptions::default())).unwrap();
        assert!(is_red(picture(&found)));
        // 128 KiB + 6 KiB, doubled three times, with no part read twice.
        assert!(
            found.bytes_read < 1_300_000,
            "read {} bytes",
            found.bytes_read
        );
    }

    #[test]
    fn h264_field_pairs_give_a_frame_of_the_first_field() {
        if !decoder_available(mf::Codec::H264) {
            return;
        }
        // Each key frame is two field pictures, an access unit each, of
        // different colours. A frame woven of both would comb (and mix their
        // chroma); the decoder's thumbnail mode makes it of the first alone.
        let mut clip = Clip::new(false);
        for _ in 0..3 {
            let mut top = h264_headers(true);
            top.extend(h264_slice(RED, Some(false), true));
            let mut bottom = nal(0x09, &[0x10]);
            bottom.extend(h264_slice(BLUE, Some(true), false));
            clip.key_frame(0xE0, &[&top[..], &bottom[..]]);
            clip.unit(0xE0, &h264_filler(100_000));
        }
        let files = bdmv(vec![("00001", clip_files(clip, h264_stream(FORMAT_480I)))]);
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            let p = picture(&found);
            assert_eq!((p.width, p.height), (720, 480), "{fs}");
            for parity in [0, 1] {
                let (b, _, r) = average_where(p, |y| y % 2 == parity);
                assert!(r > b + 40, "{fs}: rows {parity}: b {b} r {r}");
            }
        });
    }

    // ------------------------------------------------------------------------
    // HEVC and VC-1 from tests/data
    // ------------------------------------------------------------------------

    const HEVC_1080P: &[u8] = include_bytes!("data/hevc_1080p.bin");
    const HEVC_2160P: &[u8] = include_bytes!("data/hevc_2160p.bin");
    const VC1_1080P: &[u8] = include_bytes!("data/vc1_1080p.bin");

    /// A clip whose key frames are all `au`, with filler units between them.
    fn fixture_clip(au: &[u8], stream_id: u8, filler: &[u8], stream: Stream) -> ClipFiles {
        let mut clip = Clip::new(stream.format == FORMAT_2160P);
        for _ in 0..4 {
            clip.key_frame(stream_id, &[au]);
            for _ in 0..3 {
                let mut f = filler.to_vec();
                f.extend(std::iter::repeat_n(0x55u8, 100_000));
                clip.unit(stream_id, &f);
            }
        }
        clip_files(clip, stream)
    }

    /// Luma (mean of B, G, R) at relative position (fx, fy) of `p`.
    fn luma_at(p: &Picture, fx: f64, fy: f64) -> u32 {
        let x = ((p.width as f64 * fx) as usize).min(p.width as usize - 1);
        let y = ((p.height as f64 * fy) as usize).min(p.height as usize - 1);
        let i = (y * p.width as usize + x) * 4;
        (p.bgra[i] as u32 + p.bgra[i + 1] as u32 + p.bgra[i + 2] as u32) / 3
    }

    /// Brightest of B, G, R at relative position (fx, fy) of `p`.
    fn peak_at(p: &Picture, fx: f64, fy: f64) -> u8 {
        let x = ((p.width as f64 * fx) as usize).min(p.width as usize - 1);
        let y = ((p.height as f64 * fy) as usize).min(p.height as usize - 1);
        let i = (y * p.width as usize + x) * 4;
        p.bgra[i..i + 3].iter().copied().max().unwrap_or(0)
    }

    /// The fixture pattern: white box over a ramp that brightens to the right.
    /// (The box is slightly tinted; read as PQ the tint grows, as tone mapping
    /// keeps the colour ratios of linear light, hence the brightest channel.)
    fn assert_pattern(fs: &str, p: &Picture) {
        assert!(peak_at(p, 0.37, 0.37) > 240, "{fs}: box");
        let (left, right) = (luma_at(p, 0.1, 0.75), luma_at(p, 0.9, 0.75));
        assert!(right > left + 120, "{fs}: ramp {left} → {right}");
    }

    fn hevc_stream(format: u8, hevc_range: u8) -> Stream {
        Stream {
            coding: CODING_HEVC,
            format,
            app_type: APP_MAIN,
            hevc_range,
        }
    }

    /// HEVC access unit delimiter of a P-picture.
    const HEVC_FILLER: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x30];

    #[test]
    fn hevc_key_frames_are_decoded() {
        if !decoder_available(mf::Codec::Hevc) {
            return;
        }
        let stream = hevc_stream(FORMAT_1080P, 0x01);
        let files = bdmv(vec![(
            "00001",
            fixture_clip(HEVC_1080P, 0xE0, &HEVC_FILLER, stream),
        )]);
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            let p = picture(&found);
            assert_eq!(p.height, 1080, "{fs}");
            assert_pattern(fs, p);
        });
    }

    #[test]
    fn uhd_frames_are_scaled_down_and_hdr_is_tone_mapped() {
        if !decoder_available(mf::Codec::Hevc) {
            return;
        }
        let mut mid = Vec::new();
        for range in [0x01, (DYNAMIC_RANGE_HDR10 << 4) | 0x02] {
            let stream = hevc_stream(FORMAT_2160P, range);
            let files = bdmv(vec![(
                "00001",
                fixture_clip(HEVC_2160P, 0xE0, &HEVC_FILLER, stream),
            )]);
            let flat: Vec<(&str, &[u8])> = files
                .iter()
                .map(|(p, d)| (p.as_str(), d.as_slice()))
                .collect();
            let found = extract(&udf102(&flat, UdfOptions::default())).unwrap();
            let p = picture(&found);
            assert!(
                p.width <= 1920 && p.width > 1700,
                "{range:#x}: width {}",
                p.width
            );
            assert_eq!(p.height, 1080, "{range:#x}");
            assert_pattern(&format!("{range:#x}"), p);
            // Just below the box the chroma is nearly neutral (Cb ramps
            // from top to bottom and crosses zero at mid-height).
            mid.push(luma_at(p, 0.6, 0.55));
        }
        // Read as PQ, the 60 % code of the ramp is about 240 cd/m², above
        // SDR reference white: brighter than the same code read as SDR.
        assert!(mid[1] > mid[0] + 20, "SDR {} vs HDR {}", mid[0], mid[1]);
    }

    #[test]
    fn vc1_key_frames_are_decoded() {
        if !decoder_available(mf::Codec::Vc1) {
            return;
        }
        let stream = Stream {
            coding: CODING_VC1,
            format: FORMAT_1080P,
            app_type: APP_MAIN,
            hevc_range: 0,
        };
        let files = bdmv(vec![(
            "00001",
            fixture_clip(VC1_1080P, 0xFD, &[0, 0, 1, 0x0D], stream),
        )]);
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            let p = picture(&found);
            assert_eq!(p.height, 1080, "{fs}");
            assert_pattern(fs, p);
        });
    }
}
