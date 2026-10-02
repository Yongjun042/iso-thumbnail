//! Blu-ray menus and video frames on synthetic disc images.
//!
//! Clips are built in memory: MPEG-2 pictures from `support/mpeg2_writer.rs`
//! and H.264 pictures of I_PCM macroblocks (written here, so the decoded frame
//! is exactly known) are packed one access unit per PES packet into 192-byte
//! source packets, next to a clip information file listing their entry
//! points, and stored in ISO 9660 and UDF images. Menus add the navigation
//! files and interactive graphics of `support/bd_writer.rs`; libbluray (in
//! ffmpeg's `bluray:` protocol, when available) reads the same discs. The HEVC and VC-1 key frames
//! in `tests/data` were made by the Windows encoders from a synthetic pattern
//! (a horizontal luma ramp with a white box). The codecs Windows decodes are
//! tested on Windows only, and skipped where the decoder is not installed.

extern crate IsoPreview as iso_preview;

#[allow(dead_code)]
#[path = "support/bd_writer.rs"]
mod bd_writer;
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
use iso_preview::reader::{ByteSource, CachedReader, SeekSource};
use iso_preview::udf::Udf;
use iso_preview::Extracted;

use disc_builder::{iso9660, udf102, UdfOptions};
use mpeg2_writer::{intra_picture, predicted_picture, WriterConfig};

const VIDEO_PID: u16 = 0x1011;
const AUDIO_PID: u16 = 0x1100;
const IG_PID: u16 = 0x1400;
/// Time between key frames, in 45 kHz units (a multiple of 256, which the
/// EP_map keeps).
const KEY_INTERVAL: u32 = 176 * 256;
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
    continuity: [u8; 4],
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
            continuity: [0; 4],
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
            IG_PID => 2,
            _ => 3,
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

    /// One PES packet of `es` (PTS 0, unbounded length as Blu-ray video
    /// unless `stated_lengths`).
    fn pes(&mut self, pid: u16, stream_id: u8, es: &[u8]) {
        self.pes_at(pid, stream_id, es, 0);
    }

    /// The same presented at `pts` (45 kHz, the unit of playlist times).
    fn pes_at(&mut self, pid: u16, stream_id: u8, es: &[u8], pts: u32) {
        let t = 2 * u64::from(pts);
        let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5];
        pes.extend([
            0x21 | ((t >> 29) as u8 & 0x0E),
            (t >> 22) as u8,
            ((t >> 14) as u8) | 1,
            (t >> 7) as u8,
            ((t << 1) as u8) | 1,
        ]);
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

    /// Interactive graphics segments, one PES packet each.
    fn graphics(&mut self, segments: &[Vec<u8>]) {
        self.graphics_at(segments, 0);
    }

    /// The same presented at `pts` (45 kHz).
    fn graphics_at(&mut self, segments: &[Vec<u8>], pts: u32) {
        for seg in segments {
            self.pes_at(IG_PID, 0xBD, seg, pts);
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
/// given entry points, the n-th at `n * KEY_INTERVAL`, in coarse groups of
/// up to four with the same high time bits.
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
    // PTS_EP_coarse holds bits 31..18 of the 45 kHz time, PTS_EP_fine bits
    // 18..8; a coarse group shares bits 31..19.
    let pts = |i: usize| i as u32 * KEY_INTERVAL;
    let mut coarse: Vec<usize> = Vec::new();
    for i in 0..points.len() {
        match coarse.last() {
            Some(&c) if i - c < 4 && pts(c) >> 19 == pts(i) >> 19 => {}
            _ => coarse.push(i),
        }
    }
    let mut ep = vec![0u8, 1];
    ep.extend(VIDEO_PID.to_be_bytes());
    let v: u64 = (1u64 << 34) | ((coarse.len() as u64) << 18) | points.len() as u64;
    ep.extend(&(v << 16).to_be_bytes()[..6]);
    ep.extend(14u32.to_be_bytes());
    ep.extend(((4 + 8 * coarse.len()) as u32).to_be_bytes());
    for &first in &coarse {
        let coarse_pts = u64::from(pts(first) >> 18);
        ep.extend(
            (((first as u64) << 46) | (coarse_pts << 32) | u64::from(points[first].0))
                .to_be_bytes(),
        );
    }
    for (i, &(spn, i_end)) in points.iter().enumerate() {
        let fine_pts = (pts(i) >> 8) & 0x7FF;
        ep.extend(((u32::from(i_end) << 28) | (fine_pts << 17) | (spn & 0x1FFFF)).to_be_bytes());
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
fn pictures_come_reduced_for_small_thumbnails() {
    // For a 256-pixel thumbnail the 1280 x 720 frame is converted at half
    // size (still twice the thumbnail); for 1024 pixels, whole.
    let files = bdmv(vec![("00001", mpeg2_files(&[RED; 8]))]);
    let flat: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    let image = udf102(&flat, UdfOptions::default());
    for (max_side, size) in [(256, (640, 360)), (1024, (W, H))] {
        let found =
            iso_preview::extract_thumbnail_for(SeekSource(Cursor::new(&image[..])), max_side)
                .unwrap();
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)");
        let p = picture(&found);
        assert_eq!((p.width, p.height), size, "{max_side}");
        assert!(is_red(p), "{max_side}");
    }
}

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
    assert!(
        bluray::video_picture(&mut fs, &dir, &mut searched, iso_preview::finder::FULL_SIZE)
            .is_none()
    );
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
// Menus
// ----------------------------------------------------------------------------

use bd_writer::{cmd, Button, Ig, Top};

const GREEN: Look = Look::Checker(80, 80);
const YELLOW: Look = Look::Checker(40, 150);
/// Palette entries (index, Y, Cr, Cb, alpha): white, red.
const WHITE_ENTRY: [u8; 5] = [1, 235, 128, 128, 255];
const RED_ENTRY: [u8; 5] = [2, 63, 240, 102, 255];
/// Where the menu's two buttons are, 200 x 100 each.
const BUTTON_1: (u16, u16) = (100, 100);
const BUTTON_2: (u16, u16) = (100, 400);

/// A box of `w` x `h` pixels of palette index `c`.
fn solid(w: usize, h: usize, c: u8) -> Vec<Vec<u8>> {
    vec![vec![c; w]; h]
}

/// The display set of a 1280 x 720 menu with two buttons, white when
/// normal and red when selected; the second is selected first.
fn two_button_menu(popup: bool) -> Vec<Vec<u8>> {
    menu_selecting(popup, 2)
}

/// The same with `default` as the page's default selected button.
fn menu_selecting(popup: bool, default: u16) -> Vec<Vec<u8>> {
    let button = |id, (x, y): (u16, u16)| Button {
        id,
        x,
        y,
        normal: 1,
        selected: 2,
    };
    let page = bd_writer::page(
        0,
        default,
        0,
        &[
            (1, vec![button(1, BUTTON_1)]),
            (2, vec![button(2, BUTTON_2)]),
        ],
    );
    let mut s = vec![bd_writer::ics(W as u16, H as u16, &[page], popup)];
    s.push(bd_writer::pds(0, &[WHITE_ENTRY, RED_ENTRY]));
    s.extend(bd_writer::ods(1, &solid(200, 100, 1), 4000));
    s.extend(bd_writer::ods(2, &solid(200, 100, 2), 4000));
    s.push(bd_writer::end());
    s
}

/// A short clip of one key frame per look, with `graphics` at its start.
fn menu_clip(looks: &[Look], graphics: &[Vec<u8>]) -> ClipFiles {
    menu_clip_filled(looks, graphics, 0)
}

/// The same with `filler` bytes in the picture after each key frame.
fn menu_clip_filled(looks: &[Look], graphics: &[Vec<u8>], filler: usize) -> ClipFiles {
    let mut clip = Clip::new(false);
    clip.graphics(graphics);
    let cfg = mpeg2_cfg();
    for look in looks {
        let es = intra_picture(&cfg, W, H, |x, y| look.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        let mut p = predicted_picture(&cfg, 2);
        p.extend(std::iter::repeat_n(0x55u8, filler));
        clip.unit(0xE0, &p);
    }
    clip_files(clip, MPEG2)
}

/// Graphics that draw nothing: a page without buttons.
fn no_buttons() -> Vec<Vec<u8>> {
    let page = bd_writer::page(0, 0xFFFF, 0, &[]);
    vec![
        bd_writer::ics(W as u16, H as u16, &[page], false),
        bd_writer::pds(0, &[WHITE_ENTRY]),
        bd_writer::end(),
    ]
}

/// Extracts from the UDF image of `files`: the result and the bytes read.
fn extract_counting(files: &[(String, Vec<u8>)]) -> (Extracted, u64) {
    let flat: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_slice()))
        .collect();
    let found = extract(&udf102(&flat, UdfOptions::default())).unwrap();
    let bytes = found.bytes_read;
    (found, bytes)
}

/// Replaces or adds a file.
fn put(files: &mut Vec<(String, Vec<u8>)>, path: &str, data: Vec<u8>) {
    files.retain(|(p, _)| p != path);
    files.push((path.to_string(), data));
}

/// A playlist of the whole of `clip`, said to last 200 s (ffmpeg's
/// `bluray:` protocol only opens discs with a playlist of 3 minutes).
fn playlist(clip: &str, ig: Ig) -> Vec<u8> {
    bd_writer::mpls(clip, CODING_MPEG2, 0, 200 * 45_000, ig)
}

/// A disc with a red main feature (00001, title 1) and an HDMV top menu
/// (movie object 0) running `commands`.
fn hdmv_disc(
    menu_clips: Vec<(&str, ClipFiles)>,
    commands: Vec<[u8; 12]>,
) -> Vec<(String, Vec<u8>)> {
    let mut clips = vec![("00001", mpeg2_files(&[RED; 8]))];
    clips.extend(menu_clips);
    let mut files = bdmv(clips);
    put(
        &mut files,
        "BDMV/index.bdmv",
        bd_writer::index(Top::Hdmv(0), &[Top::Hdmv(1)]),
    );
    let objects = vec![commands, vec![cmd::play_pl(1)]];
    put(
        &mut files,
        "BDMV/MovieObject.bdmv",
        bd_writer::movie_objects(&objects),
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00001.mpls",
        playlist("00001", Ig::None),
    );
    files
}

/// Average (B, G, R) of a `w` x `h` box at (x, y).
fn average_box(p: &Picture, x: u32, y: u32, w: u32, h: u32) -> (u32, u32, u32) {
    let mut sum = [0u64; 3];
    for yy in y..y + h {
        for xx in x..x + w {
            let i = ((yy * p.width + xx) * 4) as usize;
            for (total, &v) in sum.iter_mut().zip(&p.bgra[i..i + 3]) {
                *total += u64::from(v);
            }
        }
    }
    let n = u64::from(w * h);
    (
        (sum[0] / n) as u32,
        (sum[1] / n) as u32,
        (sum[2] / n) as u32,
    )
}

/// Checks the menu: blue, a white first button and a red (selected) second
/// one.
fn assert_two_button_menu(fs: &str, p: &Picture) {
    assert_two_buttons(fs, p);
    let (b, _, r) = average_box(p, 600, 200, 400, 300);
    assert!(b > r + 40, "{fs}: background");
}

/// Checks the buttons of the menu: a white first and a red (selected)
/// second one.
fn assert_two_buttons(fs: &str, p: &Picture) {
    assert_eq!((p.width, p.height), (W, H), "{fs}");
    let (x1, y1) = (u32::from(BUTTON_1.0), u32::from(BUTTON_1.1));
    let (x2, y2) = (u32::from(BUTTON_2.0), u32::from(BUTTON_2.1));
    let (b, g, r) = average_box(p, x1 + 8, y1 + 8, 184, 84);
    assert!(
        b > 225 && g > 225 && r > 225,
        "{fs}: button 1 {:?}",
        (b, g, r)
    );
    let (b, g, r) = average_box(p, x2 + 8, y2 + 8, 184, 84);
    // The palette's BT.709 red, converted for a BT.601 frame.
    assert!(
        r > 245 && g < 20 && b < 20,
        "{fs}: button 2 {:?}",
        (b, g, r)
    );
}

#[test]
fn the_hdmv_top_menu_is_shown_with_its_buttons() {
    // The top menu plays an intro without buttons first, then the menu
    // (playlist number in a register); a compare guards the intro.
    let commands = vec![
        cmd::move_imm(3, 5),
        cmd::equals(0, 1),
        cmd::play_pl(4),
        cmd::play_pl_gpr(3),
    ];
    let mut files = hdmv_disc(
        vec![
            ("00004", menu_clip(&[GREEN; 2], &[])),
            ("00005", menu_clip(&[BLUE; 2], &two_button_menu(false))),
        ],
        commands,
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00004.mpls",
        playlist("00004", Ig::None),
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
}

#[test]
fn menu_navigation_files_come_from_the_backup_too() {
    let mut files = hdmv_disc(
        vec![("00005", menu_clip(&[BLUE; 2], &two_button_menu(false)))],
        vec![cmd::play_pl(5)],
    );
    let index = files
        .iter()
        .find(|(p, _)| p == "BDMV/index.bdmv")
        .unwrap()
        .1
        .clone();
    let objects = files
        .iter()
        .find(|(p, _)| p == "BDMV/MovieObject.bdmv")
        .unwrap()
        .1
        .clone();
    put(&mut files, "BDMV/BACKUP/index.bdmv", index);
    put(&mut files, "BDMV/BACKUP/MovieObject.bdmv", objects);
    put(&mut files, "BDMV/index.bdmv", b"INDX0200 damaged".to_vec());
    files.retain(|(p, _)| p != "BDMV/MovieObject.bdmv");
    put(
        &mut files,
        "BDMV/BACKUP/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
}

#[test]
fn films_with_pop_up_menus_are_not_top_menus() {
    // The top menu plays the film, whose buttons only pop up on request: the
    // film is sampled as usual, not at its first frame.
    let (m2ts, info) = mpeg2_files(&[RED; 8]);
    let mut clip = Clip::new(false);
    clip.graphics(&two_button_menu(true));
    let mut m2ts_with_menu = clip.finish().0;
    m2ts_with_menu.extend(&m2ts[..]);
    let shift = (m2ts_with_menu.len() - m2ts.len()) / SOURCE_PACKET;
    assert_eq!(shift % (ALIGNED_UNIT / SOURCE_PACKET), 0);
    // The graphics fill whole aligned units here, so the entry points just
    // move on by as many packets.
    let mut entries = iso_preview::clpi::parse(&info).unwrap().entries;
    for e in &mut entries {
        e.spn += shift as u32;
    }
    let points: Vec<(u32, u8)> = entries.iter().map(|e| (e.spn, e.i_end)).collect();
    let info = clpi(
        MPEG2,
        (m2ts_with_menu.len() / SOURCE_PACKET) as u32,
        &points,
    );
    let mut files = bdmv(vec![("00001", (m2ts_with_menu, info))]);
    put(
        &mut files,
        "BDMV/index.bdmv",
        bd_writer::index(Top::Hdmv(0), &[Top::Hdmv(0)]),
    );
    put(
        &mut files,
        "BDMV/MovieObject.bdmv",
        bd_writer::movie_objects(&[vec![cmd::play_pl(1)]]),
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00001.mpls",
        playlist("00001", Ig::InClip(IG_PID)),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)", "{fs}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_dark_menu_gives_way_to_the_feature() {
    // A black background with no button shown.
    let page = bd_writer::page(0, 0xFFFF, 0, &[]);
    let graphics = vec![
        bd_writer::ics(W as u16, H as u16, &[page], false),
        bd_writer::pds(0, &[WHITE_ENTRY]),
        bd_writer::end(),
    ];
    let mut files = hdmv_disc(
        vec![("00005", menu_clip(&[Look::Black; 3], &graphics))],
        vec![cmd::play_pl(5)],
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)", "{fs}");
        assert!(is_red(picture(&found)), "{fs}");
    });
}

#[test]
fn a_menu_play_item_starts_at_its_in_time() {
    // One clip holds a green part with pop-up buttons, a yellow key frame,
    // then a blue menu. Its graphics, presented at IN, follow that key frame;
    // IN lies one frame after it. Players read from the key frame, so the
    // graphics are found; frames are sampled from the first key frame after
    // IN.
    let menu_in = 3 * KEY_INTERVAL + 1877;
    let cfg = mpeg2_cfg();
    let mut clip = Clip::new(false);
    clip.graphics(&two_button_menu(true));
    for i in 0..6 {
        let look = match i {
            0..=2 => GREEN,
            3 => YELLOW,
            _ => BLUE,
        };
        let es = intra_picture(&cfg, W, H, |x, y| look.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        if i == 3 {
            clip.graphics_at(&two_button_menu(false), menu_in);
        }
        // Pictures of some size, so the graphics lie well before the next
        // key frame.
        let mut p = predicted_picture(&cfg, 2);
        p.extend(std::iter::repeat_n(0x55u8, 30_000));
        clip.unit(0xE0, &p);
    }
    let mut files = hdmv_disc(
        vec![("00005", clip_files(clip, MPEG2))],
        vec![cmd::play_pl(5), cmd::play_pl(6)],
    );
    let menu = bd_writer::mpls(
        "00005",
        CODING_MPEG2,
        menu_in,
        6 * KEY_INTERVAL,
        Ig::InClip(IG_PID),
    );
    put(&mut files, "BDMV/PLAYLIST/00005.mpls", menu);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
    // Buttons in sub-paths are not drawn but still make a menu.
    let sub_path = bd_writer::mpls(
        "00005",
        CODING_MPEG2,
        menu_in,
        6 * KEY_INTERVAL,
        Ig::SubPath,
    );
    put(&mut files, "BDMV/PLAYLIST/00005.mpls", sub_path);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        let p = picture(&found);
        assert!(is_blue(p), "{fs}");
        let (b, g, r) = average_box(p, 108, 108, 184, 84);
        assert!(g < 225 || r < 225 || b < 225, "{fs}: no buttons");
    });
    // A play item shorter than a group of pictures holds no entry point: it
    // is shown from the one it starts at.
    let short = bd_writer::mpls(
        "00005",
        CODING_MPEG2,
        menu_in,
        menu_in + 7000,
        Ig::InClip(IG_PID),
    );
    put(&mut files, "BDMV/PLAYLIST/00005.mpls", short);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        let p = picture(&found);
        assert_two_buttons(fs, p);
        let (b, g, r) = average_box(p, 600, 200, 400, 300);
        assert!(r > b + 40 && g > b + 40, "{fs}: yellow background");
    });
}

#[test]
fn menu_graphics_may_come_before_the_entry_point() {
    // The menu's graphics are multiplexed a little before the key frame at
    // IN, right after a pop-up display set presented earlier, which belongs
    // to the part of the clip before the menu.
    let menu_in = 3 * KEY_INTERVAL;
    let cfg = mpeg2_cfg();
    let mut clip = Clip::new(false);
    for i in 0..6 {
        let look = if i < 3 { GREEN } else { BLUE };
        if i == 3 {
            clip.graphics_at(&two_button_menu(true), menu_in - 9000);
            clip.graphics_at(&two_button_menu(false), menu_in);
            for _ in 0..64 {
                clip.pes(AUDIO_PID, 0xBD, &[0xAA; 170]);
            }
        }
        let es = intra_picture(&cfg, W, H, |x, y| look.color(x, y));
        clip.key_frame(0xE0, &[&es[..]]);
        clip.unit(0xE0, &predicted_picture(&cfg, 2));
    }
    let mut files = hdmv_disc(
        vec![("00005", clip_files(clip, MPEG2))],
        vec![cmd::play_pl(5)],
    );
    let menu = bd_writer::mpls(
        "00005",
        CODING_MPEG2,
        menu_in,
        6 * KEY_INTERVAL,
        Ig::InClip(IG_PID),
    );
    put(&mut files, "BDMV/PLAYLIST/00005.mpls", menu);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
}

#[test]
fn pop_up_films_do_not_use_up_the_menu_tries() {
    // The walk meets three films with pop-up menus before the top menu.
    let film = |look| menu_clip(&[look; 2], &two_button_menu(true));
    let mut files = hdmv_disc(
        vec![
            ("00002", film(RED)),
            ("00003", film(RED)),
            ("00004", film(RED)),
            ("00005", menu_clip(&[BLUE; 2], &two_button_menu(false))),
        ],
        (2..=5).map(cmd::play_pl).collect(),
    );
    for n in 2..=5 {
        let name = format!("0000{n}");
        let path = format!("BDMV/PLAYLIST/{name}.mpls");
        put(&mut files, &path, playlist(&name, Ig::InClip(IG_PID)));
    }
    let (found, _) = extract_counting(&files);
    assert_eq!(found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)");
    assert_two_button_menu("UDF", picture(&found));
}

#[test]
fn a_bdj_menu_shows_the_playlist_it_starts() {
    let mut files = bdmv(vec![
        ("00001", mpeg2_files(&[RED; 8])),
        ("00007", menu_clip(&[BLUE; 2], &[])),
    ]);
    put(
        &mut files,
        "BDMV/index.bdmv",
        bd_writer::index(Top::Bdj("00000"), &[Top::Bdj("00001")]),
    );
    put(
        &mut files,
        "BDMV/BDJO/00000.bdjo",
        bd_writer::bdjo(&["00007", "00001"], true),
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00007.mpls",
        playlist("00007", Ig::None),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00007.mpls (top menu)",
            "{fs}"
        );
        assert!(is_blue(picture(&found)), "{fs}");
    });
    // Without autostart the Java code picks the playlist: the feature it is.
    put(
        &mut files,
        "BDMV/BDJO/00000.bdjo",
        bd_writer::bdjo(&["00007", "00001"], false),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)", "{fs}");
    });
}

/// Delays every read, like a slow drive or network share.
struct Slow<S>(S, std::time::Duration);

impl<S: ByteSource> ByteSource for Slow<S> {
    fn size(&mut self) -> iso_preview::Result<u64> {
        self.0.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> iso_preview::Result<()> {
        std::thread::sleep(self.1);
        self.0.read_at(offset, buf)
    }
}

#[test]
fn a_slow_dark_menu_still_gives_way_to_the_feature() {
    // The menu stage takes longer than the time budget on this source; the
    // feature is still sampled until it gives a frame, also when its first
    // position is broken.
    for broken in [false, true] {
        let mut files = hdmv_disc(
            vec![("00005", menu_clip(&[Look::Black; 3], &no_buttons()))],
            vec![cmd::play_pl(5)],
        );
        put(
            &mut files,
            "BDMV/PLAYLIST/00005.mpls",
            playlist("00005", Ig::InClip(IG_PID)),
        );
        if broken {
            // The sync byte of the packet at the 25 % entry point.
            let (_, data) = files
                .iter_mut()
                .find(|(p, _)| p == "BDMV/STREAM/00001.m2ts")
                .unwrap();
            let (_, info) = mpeg2_files(&[RED; 8]);
            let entries = iso_preview::clpi::parse(&info).unwrap().entries;
            data[entries[1].spn as usize * SOURCE_PACKET + 4] = 0;
        }
        let flat: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let image = udf102(&flat, UdfOptions::default());
        let t = std::time::Instant::now();
        let slow = Slow(
            SeekSource(Cursor::new(image)),
            std::time::Duration::from_millis(150),
        );
        let found = iso_preview::extract_thumbnail(slow).unwrap();
        assert!(
            t.elapsed() > std::time::Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
        let want = if broken { "40%" } else { "25%" };
        assert_eq!(
            found.thumbnail.path,
            format!("BDMV/STREAM/00001.m2ts ({want})")
        );
        assert!(is_red(picture(&found)));
    }
}

#[test]
fn a_clip_cut_off_inside_a_picture_skips_that_position() {
    // A copy cut off near the end of the key frame at 25 % (the fifth of
    // 20): that picture is not decoded with a grey band, the search goes on.
    // The cut is placed off and on an aligned unit boundary: a clip of whole
    // units reads as ending there, so the picture must decode whole.
    let es = intra_picture(&mpeg2_cfg(), W, H, |x, y| RED.color(x, y));
    // Packets of the picture before the cut.
    let kept = es.len() * 97 / 100 / 184;
    for aligned in [false, true] {
        let cfg = mpeg2_cfg();
        let mut clip = Clip::new(false);
        let mut cut = 0;
        for i in 0..20 {
            if i == 4 {
                let at = clip.packets() as usize + kept;
                let pad = if aligned {
                    (32 - at % 32) % 32
                } else {
                    32 - at % 32 / 2 - 1
                };
                for _ in 0..pad {
                    clip.ts_packet(0x1FFF, false, &[0xFF; 184]);
                }
                cut = (clip.packets() as usize + kept) * SOURCE_PACKET;
            }
            clip.key_frame(0xE0, &[&es[..]]);
            for _ in 0..3 {
                let mut p = predicted_picture(&cfg, 2);
                p.extend(std::iter::repeat_n(0x55u8, 100_000));
                clip.unit(0xE0, &p);
            }
        }
        let (mut m2ts, info) = clip_files(clip, MPEG2);
        assert_eq!(cut % ALIGNED_UNIT == 0, aligned);
        m2ts.truncate(cut);
        let files = bdmv(vec![("00001", (m2ts, info))]);
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            assert_eq!(
                found.thumbnail.path, "BDMV/STREAM/00001.m2ts (15%)",
                "{fs} aligned {aligned}"
            );
            let p = picture(&found);
            let (b, _, r) = average_where(p, |y| y >= H - 32);
            assert!(r > b + 40, "{fs}: bottom rows {b} {r}");
        });
    }
}

#[test]
fn a_menu_entered_at_a_play_item_is_shown() {
    // The menu playlist begins with an intro without buttons; the top menu
    // plays it from its second play item, the menu.
    let mut files = hdmv_disc(
        vec![
            ("00004", menu_clip(&[GREEN; 2], &[])),
            ("00005", menu_clip(&[BLUE; 2], &two_button_menu(false))),
        ],
        vec![cmd::play_pl_at_item(9, 1)],
    );
    let items = [
        ("00004", CODING_MPEG2, 0, 2 * KEY_INTERVAL, Ig::None),
        ("00005", CODING_MPEG2, 0, 200 * 45_000, Ig::InClip(IG_PID)),
    ];
    put(
        &mut files,
        "BDMV/PLAYLIST/00009.mpls",
        bd_writer::mpls_items(&items),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00009.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
    // Played from its start, the playlist is the intro: no menu.
    let objects = vec![vec![cmd::play_pl(9)], vec![cmd::play_pl(1)]];
    put(
        &mut files,
        "BDMV/MovieObject.bdmv",
        bd_writer::movie_objects(&objects),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)", "{fs}");
    });
}

#[test]
fn set_button_page_chooses_the_highlighted_button() {
    // The page names no default: the button the movie object selected is
    // highlighted, else the first.
    for (commands, red) in [
        (vec![cmd::select_button(2), cmd::play_pl(5)], 2),
        (vec![cmd::play_pl(5)], 1),
    ] {
        let mut files = hdmv_disc(
            vec![(
                "00005",
                menu_clip(&[BLUE; 2], &menu_selecting(false, 0xFFFF)),
            )],
            commands,
        );
        put(
            &mut files,
            "BDMV/PLAYLIST/00005.mpls",
            playlist("00005", Ig::InClip(IG_PID)),
        );
        let (found, _) = extract_counting(&files);
        let p = picture(&found);
        let reds: Vec<bool> = [BUTTON_1, BUTTON_2]
            .iter()
            .map(|&(x, y)| {
                let (b, g, r) = average_box(p, u32::from(x) + 8, u32::from(y) + 8, 184, 84);
                r > 245 && g < 20 && b < 20
            })
            .collect();
        assert_eq!(reds, [red == 1, red == 2], "button {red}");
    }
}

#[test]
fn a_still_menu_of_one_picture_is_shown() {
    // The clip holds the graphics and a single picture, which nothing
    // follows.
    let mut clip = Clip::new(false);
    clip.graphics(&two_button_menu(false));
    let es = intra_picture(&mpeg2_cfg(), W, H, |x, y| BLUE.color(x, y));
    clip.key_frame(0xE0, &[&es[..]]);
    let mut files = hdmv_disc(
        vec![("00005", clip_files(clip, MPEG2))],
        vec![cmd::play_pl(5)],
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert_two_button_menu(fs, picture(&found));
    });
}

#[test]
fn menus_are_sampled_halfway_within_their_play_item() {
    // A motion menu that fades in from black: its start is dark, halfway
    // through the play item it is blue. The clip goes on in green past the
    // play item's OUT time.
    let looks = [Look::Black, BLUE, BLUE, GREEN, GREEN, GREEN, GREEN];
    let mut files = hdmv_disc(
        vec![("00005", menu_clip(&looks, &no_buttons()))],
        vec![cmd::play_pl(5)],
    );
    let menu = bd_writer::mpls(
        "00005",
        CODING_MPEG2,
        0,
        3 * KEY_INTERVAL,
        Ig::InClip(IG_PID),
    );
    put(&mut files, "BDMV/PLAYLIST/00005.mpls", menu);
    on_every_file_system(&files, |fs, result| {
        let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
        assert_eq!(
            found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
            "{fs}"
        );
        assert!(is_blue(picture(&found)), "{fs}");
    });
}

#[test]
fn graphics_are_read_only_as_far_as_needed() {
    // A menu clip of 2 MB whose display set ends in its first chunk.
    let mut files = hdmv_disc(
        vec![(
            "00005",
            menu_clip_filled(&[BLUE; 2], &two_button_menu(false), 1 << 20),
        )],
        vec![cmd::play_pl(5)],
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    let (found, bytes) = extract_counting(&files);
    assert_eq!(found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)");
    assert!(bytes < 1 << 20, "read {bytes} bytes");

    // A film whose pop-up composition has no end segment in reach: reading
    // stops at the composition, and the film is sampled as the feature.
    let graphics = two_button_menu(true)[..2].to_vec(); // ICS and PDS
    let mut files = bdmv(vec![(
        "00001",
        menu_clip_filled(&[RED; 4], &graphics, 1 << 20),
    )]);
    put(
        &mut files,
        "BDMV/index.bdmv",
        bd_writer::index(Top::Hdmv(0), &[Top::Hdmv(0)]),
    );
    put(
        &mut files,
        "BDMV/MovieObject.bdmv",
        bd_writer::movie_objects(&[vec![cmd::play_pl(1)]]),
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00001.mpls",
        playlist("00001", Ig::InClip(IG_PID)),
    );
    let (found, bytes) = extract_counting(&files);
    assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (25%)");
    assert!(bytes < 1 << 20, "read {bytes} bytes");
}

#[test]
fn an_encrypted_menu_gives_up_the_disc_early() {
    let (mut m2ts, info) = menu_clip(&[BLUE; 2], &two_button_menu(false));
    encrypt(&mut m2ts);
    let mut files = hdmv_disc(vec![("00005", (m2ts, info))], vec![cmd::play_pl(5)]);
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
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
    assert!(
        bluray::video_picture(&mut fs, &dir, &mut searched, iso_preview::finder::FULL_SIZE)
            .is_none()
    );
    assert!(searched);
    drop(fs);
    assert!(rd.bytes < 400_000, "read {} bytes", rd.bytes);
}

#[test]
fn hostile_menus_never_panic_and_cost_little() {
    let base = hdmv_disc(
        vec![("00005", menu_clip(&[BLUE; 2], &two_button_menu(false)))],
        vec![
            cmd::move_imm(1, 5),
            cmd::equals(0, 0),
            cmd::jump_title(1),
            cmd::play_pl_gpr(1),
        ],
    );
    let mut base = base;
    put(
        &mut base,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    let damaged = [
        "BDMV/index.bdmv",
        "BDMV/MovieObject.bdmv",
        "BDMV/PLAYLIST/00005.mpls",
        "BDMV/STREAM/00005.m2ts",
    ];
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..80 {
        let mut files = base.clone();
        for (path, data) in &mut files {
            if !damaged.contains(&path.as_str()) {
                continue;
            }
            // The stream file: its first units, where the graphics are.
            let span = if path.ends_with("m2ts") {
                3 * ALIGNED_UNIT
            } else {
                data.len()
            };
            for _ in 0..1 + next() % 6 {
                let i = next() as usize % span.min(data.len());
                data[i] = next() as u8;
            }
            if round % 5 == 0 && !path.ends_with("m2ts") {
                let cut = next() as usize % data.len();
                data.truncate(cut);
            }
        }
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

/// libbluray (through ffmpeg's `bluray:` protocol) opens the synthetic disc
/// and plays the menu playlist: its index, playlist and clip information
/// files are what players read. Skipped without ffmpeg or its libbluray.
#[test]
fn libbluray_plays_the_synthetic_menu() {
    let Some(ffmpeg) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            ["ffmpeg.exe", "ffmpeg"]
                .iter()
                .map(|n| dir.join(n))
                .find(|p| p.is_file())
        })
    }) else {
        eprintln!("skipped: ffmpeg not on PATH");
        return;
    };
    let protocols = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-protocols"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&protocols.stdout)
        .lines()
        .any(|l| l.trim() == "bluray")
    {
        eprintln!("skipped: ffmpeg without libbluray");
        return;
    }
    let mut files = hdmv_disc(
        vec![("00005", menu_clip(&[BLUE; 2], &two_button_menu(false)))],
        vec![cmd::play_pl(5)],
    );
    put(
        &mut files,
        "BDMV/PLAYLIST/00005.mpls",
        playlist("00005", Ig::InClip(IG_PID)),
    );
    let dir = std::env::temp_dir().join(format!("isopreview-bd-{}", std::process::id()));
    for (path, data) in &files {
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, data).unwrap();
    }
    for (playlist, blue) in [("5", true), ("1", false)] {
        let out = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-playlist",
                playlist,
                "-i",
            ])
            .arg(format!("bluray:{}", dir.display()))
            .args([
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        // libbluray reports navigation files it cannot use.
        for complaint in [
            "index.bdmv",
            "MovieObject.bdmv",
            "incomplete",
            "No menu support",
        ] {
            assert!(!err.contains(complaint), "playlist {playlist}: {err}");
        }
        let rgb = out.stdout;
        assert_eq!(rgb.len(), (W * H * 3) as usize, "playlist {playlist}");
        let (r, b) = rgb.chunks_exact(3).fold((0u64, 0u64), |(r, b), p| {
            (r + u64::from(p[0]), b + u64::from(p[2]))
        });
        assert_eq!(b > r, blue, "playlist {playlist}: r {r} b {b}");
    }
    let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn h264_clips_cut_off_inside_a_key_frame_skip_it() {
        if !decoder_available(mf::Codec::H264) {
            return;
        }
        // 20 key frames, the copy cut off inside the fifth (the 25 %
        // position): the decoder never gets that partial access unit.
        let mut au = h264_headers(false);
        au.extend(h264_slice(RED, None, true));
        let mut clip = Clip::new(false);
        let mut cut = 0;
        for i in 0..20 {
            if i == 4 {
                cut = (clip.packets() as usize + au.len() * 97 / 100 / 184) * SOURCE_PACKET;
            }
            clip.key_frame(0xE0, &[&au[..]]);
            clip.unit(0xE0, &h264_filler(1000));
        }
        let (mut m2ts, info) = clip_files(clip, h264_stream(FORMAT_480P));
        assert!(cut % ALIGNED_UNIT != 0);
        m2ts.truncate(cut);
        let files = bdmv(vec![("00001", (m2ts, info))]);
        let flat: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, d)| (p.as_str(), d.as_slice()))
            .collect();
        let found = extract(&udf102(&flat, UdfOptions::default())).unwrap();
        assert_eq!(found.thumbnail.path, "BDMV/STREAM/00001.m2ts (15%)");
        assert!(is_red(picture(&found)));
    }

    #[test]
    fn h264_menus_get_their_buttons() {
        if !decoder_available(mf::Codec::H264) {
            return;
        }
        // An SD menu: its graphics plane is 720 x 480 with BT.601 colours.
        let mut clip = Clip::new(false);
        let button = Button {
            id: 1,
            x: 100,
            y: 100,
            normal: 1,
            selected: 1,
        };
        clip.graphics(&[
            bd_writer::ics(
                720,
                480,
                &[bd_writer::page(0, 1, 0, &[(1, vec![button])])],
                false,
            ),
            bd_writer::pds(0, &[[1, 235, 128, 128, 255]]),
            bd_writer::ods(1, &solid(240, 96, 1), 4000).remove(0),
            bd_writer::end(),
        ]);
        // A still menu: one access unit, which ends the clip.
        let mut au = h264_headers(false);
        au.extend(h264_slice(BLUE, None, true));
        clip.key_frame(0xE0, &[&au[..]]);
        let mut files = hdmv_disc(
            vec![("00005", clip_files(clip, h264_stream(FORMAT_480P)))],
            vec![cmd::play_pl(5)],
        );
        put(
            &mut files,
            "BDMV/PLAYLIST/00005.mpls",
            bd_writer::mpls("00005", CODING_H264, 0, 200 * 45_000, Ig::InClip(IG_PID)),
        );
        on_every_file_system(&files, |fs, result| {
            let found = result.unwrap_or_else(|e| panic!("{fs}: {e}"));
            assert_eq!(
                found.thumbnail.path, "BDMV/PLAYLIST/00005.mpls (top menu)",
                "{fs}"
            );
            let p = picture(&found);
            assert_eq!((p.width, p.height), (720, 480), "{fs}");
            let (b, g, r) = average_box(p, 108, 108, 224, 80);
            assert!(
                b > 225 && g > 225 && r > 225,
                "{fs}: button {:?}",
                (b, g, r)
            );
            let (b, _, r) = average_box(p, 400, 250, 300, 200);
            assert!(b > r + 40, "{fs}: background");
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
            let image = udf102(&flat, UdfOptions::default());
            let found = extract(&image).unwrap();
            let p = picture(&found);
            assert!(
                p.width <= 1920 && p.width > 1700,
                "{range:#x}: width {}",
                p.width
            );
            assert_eq!(p.height, 1080, "{range:#x}");
            assert_pattern(&format!("{range:#x}"), p);
            // For a small thumbnail, a frame tone-mapped pixel by pixel is
            // reduced before the tone mapping (by 7, to 308 lines); the SDR
            // one is copied at 1080 lines and reduced at the end (by 3).
            let small =
                iso_preview::extract_thumbnail_for(SeekSource(Cursor::new(&image[..])), 256)
                    .unwrap();
            let s = picture(&small);
            let lines = if range == 0x01 { 360 } else { 2160 / 7 };
            assert_eq!(s.height, lines, "{range:#x}");
            assert_pattern(&format!("{range:#x} small"), s);
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
