//! DVD-Video support: jacket pictures and frames from the main title.
//!
//! A DVD carries no thumbnail file the way a Blu-ray does (`BDMV/META/DL`).
//! Two sources are used instead:
//! 1. `JACKET_P/J00___5L.MP2` (or the `5M`/`5S` sizes): the jacket picture, an
//!    MPEG-2 still picture authored for players' disc browsers. Mostly found on
//!    Japanese discs. The finder prefers it over everything but Blu-ray artwork.
//! 2. An I-frame of the main title, taken as the title set (`VTS_nn_1..9.VOB`)
//!    holding the most data. A few positions are sampled; frames that are
//!    nearly black, washed out or flat (fades, logos, title cards) are passed
//!    over, and black letterbox or pillarbox bars are cut off. The menu VOBs
//!    (`VIDEO_TS.VOB`, `VTS_nn_0.VOB`) are the last resort.
//!
//! Scrambled (CSS) video packets are skipped, never decrypted: a title whose
//! video is scrambled yields no frame.
//!
//! Work is bounded: every read goes through the image's `CachedReader`
//! budgets, each sampling attempt scans at most `MAX_SCAN_BYTES`, and at most
//! `MAX_ATTEMPTS` frames are decoded.

use crate::finder::{Content, Thumbnail};
use crate::fs::FileSystem;
use crate::mpeg2::{self, Frame};
use crate::mpegps::{self, Demuxer};
use crate::picture::{self, LumaStats, Rect};

/// DVD packs are 2048 bytes; sampling positions are aligned to them.
const PACK: u64 = 2048;
/// Bytes requested from the image per read while scanning a VOB.
const READ_CHUNK: usize = 256 << 10;
/// Most bytes scanned from one sampling position before giving up on it. A
/// VOBU (which starts with an I-frame) is at most about 1.2 MiB at the
/// highest DVD bit rate, so this always reaches the next I-frame.
const MAX_SCAN_BYTES: u64 = 3 << 20;
/// Most elementary stream bytes buffered while waiting for an I-picture to
/// complete. DVD I-pictures are well below 1 MiB.
const MAX_ES_BYTES: usize = 2 << 20;
/// Positions sampled in the main title, in per mille of its size, in the
/// order tried. The beginning of a film is often logos and titles.
const SAMPLE_PERMILLE: [u64; 5] = [250, 400, 150, 550, 700];
/// Most frames decoded per disc (titles and menus together).
const MAX_ATTEMPTS: usize = 6;
/// Title sets tried, largest first, when the largest yields nothing.
const MAX_TITLE_SETS: usize = 2;
/// Menu VOBs tried when no title frame was found.
const MAX_MENUS: usize = 2;
/// Most VOB files considered in `VIDEO_TS` (a real disc has a few dozen).
const MAX_VOB_FILES: usize = 64;
/// Largest jacket picture file accepted (a 720x576 MPEG-2 still is far smaller).
const MAX_JACKET_BYTES: usize = 2 << 20;
/// Jacket picture files considered in `JACKET_P`.
const MAX_JACKET_FILES: usize = 16;
/// Scrambled video packets seen without any clear one before a title is
/// treated as CSS-protected.
const SCRAMBLED_LIMIT: u32 = 8;
/// A frame with more than one concealed macroblock in this many is rejected.
const MAX_CONCEALED_RATIO: u32 = 10;
/// Frame acceptance on studio-range luma (16 = black, 235 = white).
const MIN_MEAN: f32 = 30.0;
const MAX_MEAN: f32 = 225.0;
const MIN_STDDEV: f32 = 10.0;

const SEQUENCE_HEADER: [u8; 4] = [0, 0, 1, 0xB3];
const SEQUENCE_END: [u8; 4] = [0, 0, 1, 0xB7];

/// A VOB file of the disc.
struct Vob<N> {
    name: String,
    node: N,
    size: u64,
}

/// One title set: its menu VOB and the parts of its title VOBs in order.
struct TitleSet<N> {
    menu: Option<Vob<N>>,
    parts: Vec<(u8, Vob<N>)>,
    total: u64,
}

/// Splits `VIDEO_TS.VOB` into (0, 0) and `VTS_nn_k.VOB` into (nn, k), with
/// nn in 1..=99 and k in 0..=9. Anything else is not a DVD-Video VOB.
fn parse_vob_name(name: &str) -> Option<(u8, u8)> {
    if name.eq_ignore_ascii_case("VIDEO_TS.VOB") {
        return Some((0, 0));
    }
    let b = name.as_bytes();
    if b.len() != 12
        || !b[..4].eq_ignore_ascii_case(b"VTS_")
        || b[6] != b'_'
        || !b[8..].eq_ignore_ascii_case(b".VOB")
    {
        return None;
    }
    let digit = |c: u8| c.is_ascii_digit().then(|| c - b'0');
    let set = digit(b[4])? * 10 + digit(b[5])?;
    let part = digit(b[7])?;
    (set >= 1).then_some((set, part))
}

/// Whether the decoded frame is too damaged to show.
fn too_damaged(frame: &Frame) -> bool {
    frame.total_macroblocks == 0
        || frame
            .concealed_macroblocks
            .saturating_mul(MAX_CONCEALED_RATIO)
            > frame.total_macroblocks
}

fn presentable(stats: &LumaStats) -> bool {
    stats.mean >= MIN_MEAN && stats.mean <= MAX_MEAN && stats.stddev >= MIN_STDDEV
}

/// Last index of `needle` in `hay`.
fn rfind(hay: &[u8], needle: &[u8; 4]) -> Option<usize> {
    hay.windows(4).rposition(|w| w == needle)
}

/// Drops elementary stream bytes that can no longer start a picture: keeps
/// everything from the last sequence header, or the last three bytes (a start
/// code may be split across chunks) when there is none.
fn trim_es(es: &mut Vec<u8>) {
    match rfind(es, &SEQUENCE_HEADER) {
        Some(0) => {}
        Some(i) => {
            es.drain(..i);
        }
        None => {
            let keep = es.len().min(3);
            es.drain(..es.len() - keep);
        }
    }
}

/// Reads the concatenated parts of a title (or a single VOB) in sequence.
struct Cursor {
    part: usize,
    offset: u64,
}

impl Cursor {
    /// Positions the cursor at byte `pos` of the concatenation of `parts`.
    fn at<N>(parts: &[&Vob<N>], mut pos: u64) -> Self {
        for (i, vob) in parts.iter().enumerate() {
            if pos < vob.size {
                return Self {
                    part: i,
                    offset: pos,
                };
            }
            pos -= vob.size;
        }
        Self {
            part: parts.len(),
            offset: 0,
        }
    }

    /// Reads the next bytes; 0 at the end of the last part.
    fn read<F: FileSystem>(
        &mut self,
        fs: &mut F,
        parts: &[&Vob<F::Node>],
        buf: &mut [u8],
    ) -> crate::error::Result<usize> {
        while let Some(vob) = parts.get(self.part) {
            let n = fs.read_range(&vob.node, self.offset, buf)?;
            if n > 0 {
                self.offset += n as u64;
                return Ok(n);
            }
            self.part += 1;
            self.offset = 0;
        }
        Ok(0)
    }
}

enum Grab {
    Frame(Frame),
    /// The video packets are scrambled (CSS).
    Scrambled,
    /// No complete, decodable I-picture within the scan limit, or a read
    /// failed (for instance because the image's read budget is used up).
    Nothing,
}

/// Demultiplexes from byte `start` of `parts` until the first I-picture that
/// decodes, scanning at most `MAX_SCAN_BYTES`.
fn grab_frame<F: FileSystem>(fs: &mut F, parts: &[&Vob<F::Node>], start: u64) -> Grab {
    let mut cursor = Cursor::at(parts, start - start % PACK);
    let mut demux = Demuxer::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    let mut es: Vec<u8> = Vec::new();
    let mut scanned = 0u64;
    while scanned < MAX_SCAN_BYTES {
        let want = (MAX_SCAN_BYTES - scanned).min(READ_CHUNK as u64) as usize;
        let n = match cursor.read(fs, parts, &mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        scanned += n as u64;
        pending.extend_from_slice(&buf[..n]);
        let used = demux.push(&pending, &mut es).min(pending.len());
        pending.drain(..used);
        if demux.video_packets == 0 && demux.scrambled_packets >= SCRAMBLED_LIMIT {
            return Grab::Scrambled;
        }
        while let Some(range) = mpeg2::find_intra_picture(&es) {
            let end = range.end;
            if let Ok(frame) = mpeg2::decode_intra(&es[range]) {
                return Grab::Frame(frame);
            }
            // Undecodable picture: look for the next one after it.
            es.drain(..end.min(es.len()));
        }
        trim_es(&mut es);
        if es.len() > MAX_ES_BYTES {
            break;
        }
    }
    if demux.video_packets == 0 && demux.scrambled_packets > 0 {
        Grab::Scrambled
    } else {
        Grab::Nothing
    }
}

/// A decoded frame with what the selection needs.
struct Candidate {
    frame: Frame,
    area: Rect,
    stats: LumaStats,
    source: String,
}

impl Candidate {
    fn new(frame: Frame, source: String) -> Option<Self> {
        if too_damaged(&frame) {
            return None;
        }
        let area = picture::active_area(&frame);
        let stats = picture::luma_stats(&frame, area);
        Some(Self {
            frame,
            area,
            stats,
            source,
        })
    }

    fn into_thumbnail(self) -> Thumbnail {
        Thumbnail {
            path: self.source,
            content: Content::Picture(picture::to_picture(&self.frame, self.area)),
        }
    }
}

/// Keeps the best frame seen so far and says when to stop looking.
#[derive(Default)]
struct Selection {
    best: Option<Candidate>,
    attempts: usize,
}

impl Selection {
    /// Returns true once a presentable frame was found or attempts ran out.
    fn offer(&mut self, candidate: Option<Candidate>) -> bool {
        self.attempts += 1;
        if let Some(c) = candidate {
            // Once the best is presentable `done` stops the search, so a
            // stored best here is never presentable: a presentable newcomer
            // always wins, otherwise the one with more detail.
            let better = match &self.best {
                None => true,
                Some(b) => presentable(&c.stats) || c.stats.stddev > b.stats.stddev,
            };
            if better {
                self.best = Some(c);
            }
        }
        self.done()
    }

    fn done(&self) -> bool {
        self.best.as_ref().is_some_and(|b| presentable(&b.stats)) || self.attempts >= MAX_ATTEMPTS
    }
}

/// Collects the VOB files of `VIDEO_TS` grouped by title set (index 0 holds
/// `VIDEO_TS.VOB`, the video manager menu).
fn title_sets<F: FileSystem>(fs: &mut F, video_ts: &F::Node) -> Vec<TitleSet<F::Node>> {
    let mut found = Vec::new();
    let walked = fs.walk(video_ts, &mut |e| {
        if !e.is_dir {
            if let Some((set, part)) = parse_vob_name(&e.name) {
                found.push((set, part, e));
            }
        }
        found.len() < MAX_VOB_FILES
    });
    if walked.is_err() {
        return Vec::new();
    }
    let mut sets: Vec<TitleSet<F::Node>> = (0..100)
        .map(|_| TitleSet {
            menu: None,
            parts: Vec::new(),
            total: 0,
        })
        .collect();
    for (set, part, e) in found {
        // A size that cannot be read leaves the file out.
        let Ok(size) = fs.file_size(&e.node) else {
            continue;
        };
        if size == 0 {
            continue;
        }
        let vob = Vob {
            name: e.name,
            node: e.node,
            size,
        };
        let ts = &mut sets[set as usize];
        if part == 0 {
            if ts.menu.is_none() {
                ts.menu = Some(vob);
            }
        } else if !ts.parts.iter().any(|(p, _)| *p == part) {
            ts.total = ts.total.saturating_add(size);
            ts.parts.push((part, vob));
        }
    }
    for ts in &mut sets {
        ts.parts.sort_by_key(|(p, _)| *p);
    }
    sets
}

/// A frame of the disc's main title, or of its menus when the titles yield
/// none. `None` when nothing decodable was found.
pub fn title_frame<F: FileSystem>(fs: &mut F, video_ts: &F::Node) -> Option<Thumbnail> {
    let sets = title_sets(fs, video_ts);
    // Title sets with title VOBs, largest first (ties: lower number first).
    let mut order: Vec<usize> = (1..sets.len()).filter(|&i| sets[i].total > 0).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sets[i].total));
    let mut selection = Selection::default();
    'titles: for &i in order.iter().take(MAX_TITLE_SETS) {
        let parts: Vec<&Vob<F::Node>> = sets[i].parts.iter().map(|(_, v)| v).collect();
        let total = sets[i].total;
        for permille in SAMPLE_PERMILLE {
            let start = (u128::from(total) * u128::from(permille) / 1000) as u64;
            let source = format!("VIDEO_TS/VTS_{i:02} title at {}%", permille / 10);
            match grab_frame(fs, &parts, start) {
                Grab::Frame(frame) => {
                    if selection.offer(Candidate::new(frame, source)) {
                        break 'titles;
                    }
                }
                // Every part of a scrambled title is scrambled.
                Grab::Scrambled => continue 'titles,
                Grab::Nothing => {
                    if selection.offer(None) {
                        break 'titles;
                    }
                }
            }
        }
    }
    if selection.best.is_none() {
        // Menus: the video manager's first, then the title sets' in size order.
        let menus = std::iter::once(0)
            .chain(order.iter().copied())
            .filter_map(|i| sets[i].menu.as_ref())
            .take(MAX_MENUS);
        for menu in menus {
            if selection.attempts >= MAX_ATTEMPTS {
                break;
            }
            let source = format!("VIDEO_TS/{} (menu)", menu.name);
            let candidate = match grab_frame(fs, &[menu], 0) {
                Grab::Frame(frame) => Candidate::new(frame, source),
                Grab::Scrambled | Grab::Nothing => None,
            };
            if selection.offer(candidate) {
                break;
            }
        }
    }
    selection.best.map(Candidate::into_thumbnail)
}

/// Rank of a jacket picture file name: `J00___5L.MP2` (large) first, then
/// medium and small. `None` for files that are not jacket pictures.
fn jacket_rank(name: &str) -> Option<u8> {
    let b = name.as_bytes();
    if b.len() < 6
        || !b[0].eq_ignore_ascii_case(&b'J')
        || !b[b.len() - 4..].eq_ignore_ascii_case(b".MP2")
    {
        return None;
    }
    match b[b.len() - 5].to_ascii_uppercase() {
        b'L' => Some(0),
        b'M' => Some(1),
        b'S' => Some(2),
        _ => Some(3),
    }
}

/// Decodes a jacket picture file: an MPEG-2 video elementary stream holding
/// one I-picture, or the same wrapped in a program stream.
fn decode_jacket(data: &[u8]) -> Option<Frame> {
    let mut es = if mpegps::is_program_stream(data) {
        let mut es = Vec::new();
        Demuxer::new().push(data, &mut es);
        es
    } else {
        data.to_vec()
    };
    // The file ends with the picture: make sure it counts as terminated.
    es.extend_from_slice(&SEQUENCE_END);
    let range = mpeg2::find_intra_picture(&es)?;
    let frame = mpeg2::decode_intra(&es[range]).ok()?;
    (!too_damaged(&frame)).then_some(frame)
}

/// The largest readable jacket picture in `JACKET_P`.
pub fn jacket_picture<F: FileSystem>(fs: &mut F, jacket_p: &F::Node) -> Option<Thumbnail> {
    let mut files = Vec::new();
    fs.walk(jacket_p, &mut |e| {
        if !e.is_dir {
            if let Some(rank) = jacket_rank(&e.name) {
                files.push((rank, e));
            }
        }
        files.len() < MAX_JACKET_FILES
    })
    .ok()?;
    files.sort_by_key(|(rank, _)| *rank);
    for (_, e) in files {
        let size = fs.file_size(&e.node).unwrap_or(0);
        if size == 0 || size > MAX_JACKET_BYTES as u64 {
            continue;
        }
        let Ok(data) = fs.read(&e.node, MAX_JACKET_BYTES) else {
            continue;
        };
        if let Some(frame) = decode_jacket(&data) {
            let area = picture::full_area(&frame);
            return Some(Thumbnail {
                path: format!("JACKET_P/{}", e.name),
                content: Content::Picture(picture::to_picture(&frame, area)),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vob_names() {
        assert_eq!(parse_vob_name("VIDEO_TS.VOB"), Some((0, 0)));
        assert_eq!(parse_vob_name("video_ts.vob"), Some((0, 0)));
        assert_eq!(parse_vob_name("VTS_01_0.VOB"), Some((1, 0)));
        assert_eq!(parse_vob_name("VTS_12_9.VOB"), Some((12, 9)));
        assert_eq!(parse_vob_name("vts_99_1.vob"), Some((99, 1)));
        assert_eq!(parse_vob_name("VTS_00_1.VOB"), None);
        assert_eq!(parse_vob_name("VTS_01_1.IFO"), None);
        assert_eq!(parse_vob_name("VTS_1_1.VOB"), None);
        assert_eq!(parse_vob_name("VTS_0A_1.VOB"), None);
        assert_eq!(parse_vob_name("VTS_01_10.VOB"), None);
        assert_eq!(parse_vob_name("VTS_01_1.VOBX"), None);
        assert_eq!(parse_vob_name("é_01_1.VOB"), None);
    }

    #[test]
    fn jacket_names() {
        assert_eq!(jacket_rank("J00___5L.MP2"), Some(0));
        assert_eq!(jacket_rank("j00___5m.mp2"), Some(1));
        assert_eq!(jacket_rank("J00___5S.MP2"), Some(2));
        assert_eq!(jacket_rank("J01___5X.MP2"), Some(3));
        assert_eq!(jacket_rank("J00___5L.JPG"), None);
        assert_eq!(jacket_rank("X00___5L.MP2"), None);
        assert_eq!(jacket_rank(".MP2"), None);
    }

    #[test]
    fn es_trimming_keeps_the_last_sequence_header() {
        let mut es = vec![9, 9, 0, 0, 1, 0xB3, 1, 2, 0, 0, 1, 0xB3, 7];
        trim_es(&mut es);
        assert_eq!(es, vec![0, 0, 1, 0xB3, 7]);
        let mut es = vec![1, 2, 3, 4, 5, 0, 0];
        trim_es(&mut es);
        assert_eq!(es, vec![5, 0, 0]);
        let mut es = vec![0, 0, 1, 0xB3];
        trim_es(&mut es);
        assert_eq!(es, vec![0, 0, 1, 0xB3]);
    }

    #[test]
    fn presentable_frames() {
        let s = |mean, stddev| LumaStats { mean, stddev };
        assert!(presentable(&s(90.0, 40.0)));
        assert!(!presentable(&s(17.0, 2.0)), "black");
        assert!(!presentable(&s(25.0, 30.0)), "credits on black");
        assert!(!presentable(&s(234.0, 3.0)), "white flash");
        assert!(!presentable(&s(120.0, 4.0)), "flat title card");
    }
}
