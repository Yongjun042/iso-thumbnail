//! DVD-Video support: jacket pictures and frames from the main title.
//!
//! A DVD carries no thumbnail file the way a Blu-ray does (`BDMV/META/DL`).
//! These sources are used instead, in this order:
//! 1. `JACKET_P/J00___5L.MP2` (or the `5M`/`5S` sizes): the jacket picture, an
//!    MPEG-2 still picture authored for players' disc browsers. Mostly found on
//!    Japanese discs. The finder prefers it over everything but Blu-ray artwork
//!    (`jacket_picture`); the rest is `video_picture`.
//! 2. The disc's menus as its IFO files describe them (see `crate::ifo`): the
//!    root menu (main menu) of the main title set, then the video manager's
//!    title menu. Each is looked at where its first cell starts and, for a
//!    motion menu whose opening animation starts dark, where the cell's last
//!    VOBU starts.
//! 3. An I-frame of the main title, taken as the title set (`VTS_nn_1..9.VOB`)
//!    holding the most data, when no menu gave a presentable picture. A few
//!    positions are sampled.
//! 4. The first picture of the menu VOBs (`VIDEO_TS.VOB`, `VTS_nn_0.VOB`)
//!    without IFO information: the last resort, since a video manager VOB often
//!    starts with a warning or a studio logo.
//!
//! Throughout, frames that are nearly black, washed out or flat (fades, logos,
//! title cards) are passed over unless nothing better turns up, and black
//! letterbox or pillarbox bars are cut off.
//!
//! Scrambled (CSS) video packets are skipped, never decrypted: a title whose
//! video is scrambled yields no frame (its menus may still be unscrambled).
//!
//! Work is bounded on every axis, also for crafted images: all reads go
//! through the image's `CachedReader` budgets; each sampling attempt scans at
//! most `MENU_SCAN_BYTES` (menus at known positions) or `MAX_SCAN_BYTES`; there
//! are at most `MAX_AUTHORED_MENU_ATTEMPTS`, `MAX_TITLE_ATTEMPTS` and
//! `MAX_MENUS` attempts in the three video phases; and every `decode_intra`
//! call counts against `MAX_DECODES_PER_GRAB` and the per-disc allowance of
//! its phase, failed ones included. Pictures larger than DVD-Video allows are
//! not decoded at all.

use crate::finder::{Content, Thumbnail};
use crate::fs::FileSystem;
use crate::ifo::{self, Menu};
use crate::mpeg2::{self, Frame};
use crate::mpegps::{self, Demuxer};
use crate::picture::{self, LumaStats, Rect};

/// DVD packs are 2048 bytes; sampling positions are aligned to them.
const PACK: u64 = 2048;
/// Bytes requested from the image per read while scanning a VOB.
const READ_CHUNK: usize = 256 << 10;
/// Most bytes scanned from one sampling position before giving up on it. A
/// VOBU (which starts with an I-frame) lasts at most about a second, about
/// 1.3 MB at DVD's highest mux rate, so this always reaches the next
/// I-frame and the end of that picture.
const MAX_SCAN_BYTES: u64 = 2 << 20;
/// Most bytes scanned from a menu position the IFO gave: the I-picture starts
/// right there, after the VOBU's navigation pack.
const MENU_SCAN_BYTES: u64 = 1 << 20;
/// Most attempts at the menus the IFO files describe (two menus, two
/// positions each).
const MAX_AUTHORED_MENU_ATTEMPTS: usize = 4;
/// Most `decode_intra` calls for all those attempts.
const AUTHORED_MENU_DECODES: usize = 8;
/// Most elementary stream bytes buffered while waiting for an I-picture to
/// complete. DVD I-pictures are well below 1 MiB.
const MAX_ES_BYTES: usize = 2 << 20;
/// Positions sampled in the main title, in per mille of its size, in the
/// order tried. The beginning of a film is often logos and titles, so it
/// comes last: a title shorter than a few VOBUs has its only sequence
/// headers there.
const SAMPLE_PERMILLE: [u64; 6] = [250, 400, 150, 550, 700, 0];
/// Most sampling attempts in titles: all of the main title's positions and
/// two of the next title set's.
const MAX_TITLE_ATTEMPTS: usize = 8;
/// Title sets tried, largest first, when the largest yields nothing.
const MAX_TITLE_SETS: usize = 2;
/// Menu VOBs tried when no title frame was found. They have their own
/// attempts and decodes, so titles that fail cannot starve them.
const MAX_MENUS: usize = 2;
/// Most `decode_intra` calls per sampling attempt, failed ones included.
const MAX_DECODES_PER_GRAB: usize = 3;
/// Most `decode_intra` calls for all title attempts of a disc: enough for
/// every attempt, so a main title whose pictures all fail still leaves the
/// second title set its share.
const TITLE_DECODES: usize = MAX_TITLE_ATTEMPTS * MAX_DECODES_PER_GRAB;
/// Most `decode_intra` calls for all menu attempts of a disc.
const MENU_DECODES: usize = MAX_MENUS * MAX_DECODES_PER_GRAB;
/// Largest DVD-Video picture (PAL D1); anything larger is not decoded.
const DVD_MAX_WIDTH: u32 = 720;
const DVD_MAX_HEIGHT: u32 = 576;
/// Largest jacket picture file accepted (a 720x576 MPEG-2 still is far smaller).
const MAX_JACKET_BYTES: usize = 2 << 20;
/// Most bytes read from all jacket picture files together.
const MAX_JACKET_TOTAL: u64 = 4 << 20;
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

/// A title VOB part of the disc.
struct Vob<N> {
    node: N,
    size: u64,
}

/// One title set (index 0: the video manager): its IFO file and backup, its
/// menu VOB (sized only when used) and the parts of its title VOBs in order.
struct TitleSet<N> {
    ifo: Option<N>,
    bup: Option<N>,
    menu: Option<(String, N)>,
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

/// Splits `VIDEO_TS.IFO` / `.BUP` into (0, backup) and `VTS_nn_0.IFO` / `.BUP`
/// into (nn, backup).
fn parse_ifo_name(name: &str) -> Option<(u8, bool)> {
    let b = name.as_bytes();
    if b.len() != 12 {
        return None;
    }
    let backup = if b[8..].eq_ignore_ascii_case(b".IFO") {
        false
    } else if b[8..].eq_ignore_ascii_case(b".BUP") {
        true
    } else {
        return None;
    };
    if b[..8].eq_ignore_ascii_case(b"VIDEO_TS") {
        return Some((0, backup));
    }
    let digit = |c: u8| c.is_ascii_digit().then(|| c - b'0');
    if !b[..4].eq_ignore_ascii_case(b"VTS_") || b[6] != b'_' || b[7] != b'0' {
        return None;
    }
    let set = digit(b[4])? * 10 + digit(b[5])?;
    (set >= 1).then_some((set, backup))
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

/// Whether the sequence header at the start of `es` (as returned by
/// `find_intra_picture`) declares a picture no larger than DVD-Video allows: a
/// cheap first filter. The decoder enforces the same limit on the sequence
/// header that actually applies to the I-picture (`decode_intra_within`).
fn dvd_sized(es: &[u8]) -> bool {
    let Some(h) = es.get(..7) else {
        return false;
    };
    if h[..4] != SEQUENCE_HEADER {
        return false;
    }
    let (a, b, c) = (h[4], h[5], h[6]);
    let width = (u32::from(a) << 4) | u32::from(b >> 4);
    let height = (u32::from(b & 0x0F) << 8) | u32::from(c);
    width <= DVD_MAX_WIDTH && height <= DVD_MAX_HEIGHT
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

/// `decode_intra` calls still allowed: per sampling attempt and for a whole
/// group of attempts (the titles or the menus of a disc).
struct Decodes<'a> {
    grab: usize,
    group: &'a mut usize,
}

impl Decodes<'_> {
    fn take(&mut self) -> bool {
        if self.grab == 0 || *self.group == 0 {
            return false;
        }
        self.grab -= 1;
        *self.group -= 1;
        true
    }

    fn exhausted(&self) -> bool {
        self.grab == 0 || *self.group == 0
    }
}

/// Decodes the complete I-pictures of `es` from byte `*from` on, within the
/// decode allowance. `*from` moves past every picture tried, so a failed one
/// is never tried again and nothing is copied.
fn decode_next(es: &[u8], from: &mut usize, decodes: &mut Decodes) -> Option<Frame> {
    while !decodes.exhausted() {
        let range = mpeg2::find_intra_picture(es.get(*from..)?)?;
        let (start, end) = (*from + range.start, *from + range.end);
        *from = end;
        let picture = &es[start..end];
        // Even skipped pictures count: a stream of many tiny pictures must not
        // turn one attempt into thousands of header parses.
        decodes.take();
        if !dvd_sized(picture) {
            continue;
        }
        if let Ok(frame) = mpeg2::decode_intra_within(picture, DVD_MAX_WIDTH, DVD_MAX_HEIGHT) {
            return Some(frame);
        }
    }
    None
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
    /// No complete, decodable I-picture within the scan and decode limits, or
    /// a read failed (for instance because the image's read budget is used up).
    Nothing,
}

/// Demultiplexes from byte `start` of `parts` until the first I-picture that
/// decodes, scanning at most `limit` bytes and decoding at most
/// `MAX_DECODES_PER_GRAB` pictures (and no more than `group` allows).
fn grab_frame<F: FileSystem>(
    fs: &mut F,
    parts: &[&Vob<F::Node>],
    start: u64,
    limit: u64,
    group: &mut usize,
) -> Grab {
    let mut decodes = Decodes {
        grab: MAX_DECODES_PER_GRAB,
        group,
    };
    let mut cursor = Cursor::at(parts, start - start % PACK);
    let mut demux = Demuxer::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    let mut es: Vec<u8> = Vec::new();
    let mut scanned = 0u64;
    while scanned < limit && !decodes.exhausted() {
        let want = (limit - scanned).min(READ_CHUNK as u64) as usize;
        let n = match cursor.read(fs, parts, &mut buf[..want]) {
            Ok(0) => {
                // The end of the title: a picture that ends the stream counts
                // as terminated (players show the last frame too), but only
                // when no packet was cut off and every macroblock decodes, so
                // a truncated VOB does not give a frame with a grey band.
                if pending.is_empty() {
                    es.extend_from_slice(&SEQUENCE_END);
                    let mut from = 0;
                    if let Some(frame) = decode_next(&es, &mut from, &mut decodes) {
                        if frame.concealed_macroblocks == 0 {
                            return Grab::Frame(frame);
                        }
                    }
                }
                break;
            }
            Err(_) => break,
            Ok(n) => n,
        };
        scanned += n as u64;
        pending.extend_from_slice(&buf[..n]);
        // Pack by pack, so that a skipped scrambled packet only costs the
        // data around it.
        let mut off = 0;
        while off < pending.len() {
            let end = (off + PACK as usize).min(pending.len());
            let before = es.len();
            let scrambled_before = demux.scrambled_packets;
            let mut used = demux.push(&pending[off..end], &mut es);
            if used == 0 {
                // A unit longer than a pack (or cut off): give it all we have.
                used = demux.push(&pending[off..], &mut es);
                if used == 0 {
                    break;
                }
            }
            off += used;
            if demux.scrambled_packets != scrambled_before {
                // A dropped packet leaves a hole: pictures completed before it
                // are still whole, the payload around it must not be spliced.
                let mut from = 0;
                if let Some(frame) = decode_next(&es[..before], &mut from, &mut decodes) {
                    return Grab::Frame(frame);
                }
                es.clear();
            }
        }
        pending.drain(..off.min(pending.len()));
        if demux.video_packets == 0 && demux.scrambled_packets >= SCRAMBLED_LIMIT {
            return Grab::Scrambled;
        }
        let mut from = 0;
        if let Some(frame) = decode_next(&es, &mut from, &mut decodes) {
            return Grab::Frame(frame);
        }
        es.drain(..from.min(es.len()));
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

    fn into_thumbnail(self) -> Option<Thumbnail> {
        let picture = picture::to_picture(&self.frame, self.area);
        (picture.width > 0 && picture.height > 0).then_some(Thumbnail {
            path: self.source,
            content: Content::Picture(picture),
        })
    }
}

/// Keeps the best frame seen so far.
#[derive(Default)]
struct Selection {
    best: Option<Candidate>,
}

impl Selection {
    /// Records the outcome of one attempt. Returns true once a presentable
    /// frame was found.
    fn offer(&mut self, candidate: Option<Candidate>) -> bool {
        if let Some(c) = candidate {
            // Once the best is presentable the search stops, so a stored best
            // here is never presentable: a presentable newcomer always wins,
            // otherwise the one with more detail.
            let better = match &self.best {
                None => true,
                Some(b) => presentable(&c.stats) || c.stats.stddev > b.stats.stddev,
            };
            if better {
                self.best = Some(c);
            }
        }
        self.found()
    }

    fn found(&self) -> bool {
        self.best.as_ref().is_some_and(|b| presentable(&b.stats))
    }
}

/// Collects the VOB files of `VIDEO_TS` grouped by title set (index 0 holds
/// `VIDEO_TS.VOB`, the video manager menu). Duplicate names count once, so
/// the walk (bounded by the directory limits) sees every title set a disc can
/// have; only title parts are sized here.
fn title_sets<F: FileSystem>(fs: &mut F, video_ts: &F::Node) -> Vec<TitleSet<F::Node>> {
    let mut seen = [[false; 10]; 100];
    let mut found = Vec::new();
    let mut infos = Vec::new();
    let walked = fs.walk(video_ts, &mut |e| {
        if !e.is_dir {
            if let Some((set, part)) = parse_vob_name(&e.name) {
                let slot = &mut seen[set as usize][part as usize];
                if !*slot {
                    *slot = true;
                    found.push((set, part, e));
                }
            } else if let Some((set, backup)) = parse_ifo_name(&e.name) {
                infos.push((set, backup, e.node));
            }
        }
        true
    });
    if walked.is_err() {
        return Vec::new();
    }
    let mut sets: Vec<TitleSet<F::Node>> = (0..100)
        .map(|_| TitleSet {
            ifo: None,
            bup: None,
            menu: None,
            parts: Vec::new(),
            total: 0,
        })
        .collect();
    for (set, backup, node) in infos {
        let ts = &mut sets[set as usize];
        let slot = if backup { &mut ts.bup } else { &mut ts.ifo };
        if slot.is_none() {
            *slot = Some(node);
        }
    }
    for (set, part, e) in found {
        let ts = &mut sets[set as usize];
        if part == 0 {
            ts.menu = Some((e.name, e.node));
            continue;
        }
        // A size that cannot be read leaves the file out.
        let Ok(size) = fs.file_size(&e.node) else {
            continue;
        };
        if size > 0 {
            ts.total = ts.total.saturating_add(size);
            ts.parts.push((part, Vob { node: e.node, size }));
        }
    }
    for ts in &mut sets {
        ts.parts.sort_by_key(|(p, _)| *p);
    }
    sets
}

/// Where the IFO (or its backup) of title set `ts` says `menu` starts.
fn menu_cell<F: FileSystem>(
    fs: &mut F,
    ts: &TitleSet<F::Node>,
    menu: Menu,
) -> Option<ifo::MenuCell> {
    ts.ifo
        .iter()
        .chain(ts.bup.iter())
        .find_map(|node| ifo::menu_cell(fs, node, menu))
}

/// A picture of the disc's video: a menu the IFO files point at, a frame of
/// the main title, or the first picture of a menu VOB, in that order (see the
/// module documentation). `None` when nothing decodable was found.
///
/// `searched` is set when the disc's title VOB files could be found and
/// sized, so that the caller does not repeat the search on another view of
/// the same disc; a `VIDEO_TS` whose title VOBs cannot be read here leaves it
/// unset.
pub fn video_picture<F: FileSystem>(
    fs: &mut F,
    video_ts: &F::Node,
    searched: &mut bool,
) -> Option<Thumbnail> {
    let sets = title_sets(fs, video_ts);
    if sets.is_empty() {
        // VIDEO_TS could not be read.
        return None;
    }
    // Title VOBs were found and sized: this view of the disc is readable.
    if sets.iter().any(|ts| ts.total > 0) {
        *searched = true;
    }
    // Title sets with title VOBs, largest first (ties: lower number first).
    let mut order: Vec<usize> = (1..sets.len()).filter(|&i| sets[i].total > 0).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sets[i].total));
    let mut selection = Selection::default();

    // 1. The root menu of the main title set, then the title menu.
    let mut decodes = AUTHORED_MENU_DECODES;
    let mut attempts = 0;
    let authored = order
        .first()
        .map(|&i| (i, Menu::Root))
        .into_iter()
        .chain(std::iter::once((0, Menu::Title)));
    'menus: for (i, menu) in authored {
        let ts = &sets[i];
        let Some((name, node)) = ts.menu.as_ref() else {
            continue;
        };
        let Some(cell) = menu_cell(fs, ts, menu) else {
            continue;
        };
        let size = fs.file_size(node).unwrap_or(0);
        let vob = Vob {
            node: node.clone(),
            size,
        };
        let label = match menu {
            Menu::Root => "root menu",
            Menu::Title => "title menu",
        };
        let mut sectors = vec![cell.first_vobu];
        if cell.last_vobu != cell.first_vobu {
            sectors.push(cell.last_vobu);
        }
        for sector in sectors {
            let start = u64::from(sector) * PACK;
            if start >= size {
                continue;
            }
            if attempts >= MAX_AUTHORED_MENU_ATTEMPTS {
                break 'menus;
            }
            attempts += 1;
            let source = format!("VIDEO_TS/{name} ({label})");
            let candidate = match grab_frame(fs, &[&vob], start, MENU_SCAN_BYTES, &mut decodes) {
                Grab::Frame(frame) => Candidate::new(frame, source),
                Grab::Scrambled | Grab::Nothing => None,
            };
            if selection.offer(candidate) {
                break 'menus;
            }
        }
    }

    // 2. Frames of the main title (and of the next title set if needed).
    if !selection.found() {
        let mut decodes = TITLE_DECODES;
        let mut attempts = 0;
        'titles: for &i in order.iter().take(MAX_TITLE_SETS) {
            let parts: Vec<&Vob<F::Node>> = sets[i].parts.iter().map(|(_, v)| v).collect();
            let total = sets[i].total;
            for permille in SAMPLE_PERMILLE {
                if attempts >= MAX_TITLE_ATTEMPTS {
                    break 'titles;
                }
                let start = (u128::from(total) * u128::from(permille) / 1000) as u64;
                let source = format!("VIDEO_TS/VTS_{i:02} title at {}%", permille / 10);
                let candidate = match grab_frame(fs, &parts, start, MAX_SCAN_BYTES, &mut decodes) {
                    Grab::Frame(frame) => Candidate::new(frame, source),
                    // Every part of a scrambled title is scrambled.
                    Grab::Scrambled => continue 'titles,
                    Grab::Nothing => None,
                };
                attempts += 1;
                if selection.offer(candidate) {
                    break 'titles;
                }
            }
        }
    }

    // 3. Nothing presentable yet: the first picture of the menu VOBs, the
    // video manager's first, then the title sets' in size order.
    if !selection.found() {
        let mut decodes = MENU_DECODES;
        let mut attempts = 0;
        let menus = std::iter::once(0)
            .chain(order.iter().copied())
            .filter_map(|i| sets[i].menu.as_ref());
        for (name, node) in menus {
            if attempts >= MAX_MENUS {
                break;
            }
            let size = fs.file_size(node).unwrap_or(0);
            if size == 0 {
                continue;
            }
            attempts += 1;
            let vob = Vob {
                node: node.clone(),
                size,
            };
            let source = format!("VIDEO_TS/{name} (menu)");
            let candidate = match grab_frame(fs, &[&vob], 0, MAX_SCAN_BYTES, &mut decodes) {
                Grab::Frame(frame) => Candidate::new(frame, source),
                Grab::Scrambled | Grab::Nothing => None,
            };
            if selection.offer(candidate) {
                break;
            }
        }
    }
    selection.best.and_then(Candidate::into_thumbnail)
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
    let picture = &es[range];
    if !dvd_sized(picture) {
        return None;
    }
    let frame = mpeg2::decode_intra_within(picture, DVD_MAX_WIDTH, DVD_MAX_HEIGHT).ok()?;
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
    let mut read = 0u64;
    for (_, e) in files {
        let size = fs.file_size(&e.node).unwrap_or(0);
        if size == 0 || size > MAX_JACKET_BYTES as u64 || read + size > MAX_JACKET_TOTAL {
            continue;
        }
        read += size;
        let Ok(data) = fs.read(&e.node, MAX_JACKET_BYTES) else {
            continue;
        };
        if let Some(frame) = decode_jacket(&data) {
            let picture = picture::to_picture(&frame, picture::full_area(&frame));
            if picture.width > 0 && picture.height > 0 {
                return Some(Thumbnail {
                    path: format!("JACKET_P/{}", e.name),
                    content: Content::Picture(picture),
                });
            }
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
    fn ifo_names() {
        assert_eq!(parse_ifo_name("VIDEO_TS.IFO"), Some((0, false)));
        assert_eq!(parse_ifo_name("video_ts.bup"), Some((0, true)));
        assert_eq!(parse_ifo_name("VTS_01_0.IFO"), Some((1, false)));
        assert_eq!(parse_ifo_name("VTS_42_0.BUP"), Some((42, true)));
        assert_eq!(parse_ifo_name("VTS_01_1.IFO"), None);
        assert_eq!(parse_ifo_name("VTS_00_0.IFO"), None);
        assert_eq!(parse_ifo_name("VTS_01_0.VOB"), None);
        assert_eq!(parse_ifo_name("VIDEO_TS.VOB"), None);
        assert_eq!(parse_ifo_name("VTS_1A_0.IFO"), None);
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
    fn dvd_sizes() {
        let header = |w: u32, h: u32| {
            vec![
                0,
                0,
                1,
                0xB3,
                (w >> 4) as u8,
                (((w & 0xF) << 4) | (h >> 8)) as u8,
                h as u8,
                0x23,
            ]
        };
        assert!(dvd_sized(&header(720, 480)));
        assert!(dvd_sized(&header(720, 576)));
        assert!(dvd_sized(&header(352, 240)));
        assert!(!dvd_sized(&header(721, 480)));
        assert!(!dvd_sized(&header(720, 577)));
        assert!(!dvd_sized(&header(1920, 1080)));
        assert!(!dvd_sized(&[0, 0, 1, 0xB3, 0x2D]));
        assert!(!dvd_sized(&[0, 0, 1, 0x00, 0x2D, 0x01, 0xE0]));
    }

    #[test]
    fn decode_allowance_is_shared() {
        let mut group = 4;
        let mut first = Decodes {
            grab: 3,
            group: &mut group,
        };
        assert!(first.take() && first.take() && first.take());
        assert!(!first.take(), "per attempt");
        assert!(first.exhausted());
        let mut second = Decodes {
            grab: 3,
            group: &mut group,
        };
        assert!(second.take());
        assert!(!second.take(), "per group");
        assert_eq!(group, 0);
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
