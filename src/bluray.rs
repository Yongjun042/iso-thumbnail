//! Blu-ray video frames: the thumbnail of a Blu-ray without artwork
//! (`BDMV/META/DL` or `TN`) is its top menu, else a key frame of its main
//! feature, chosen the way DVD frames are (`crate::dvd`).
//!
//! The top menu comes first:
//!
//! - `index.bdmv` names the top menu's object. For an HDMV movie object the
//!   playlists it plays are found by following its navigation commands
//!   (`crate::bdnav`); a BD-J object's code cannot be followed, so only the
//!   playlist it starts by itself is used, if any.
//! - The first play item of each such playlist (`crate::mpls`) gives the clip
//!   and its part. An HDMV menu has buttons, so a playlist without an
//!   interactive graphics stream, or with a pop-up one (a film with its
//!   pop-up menu), is passed over: it is an intro, a trailer or the film.
//! - A key frame of the play item playback starts with (`PlayPLatPI` and
//!   `PlayPLatMK` name it) is decoded as below, at its start and halfway (a
//!   motion menu may fade in); a still menu's single picture ends the clip.
//!   The first page of the interactive graphics is drawn over it: the menu
//!   as it settles, with its default button selected (`crate::igs`). Like
//!   libbluray, the graphics presented from the play item's IN time on are
//!   used, looked for a little before the entry point players start from.
//!
//! A presentable menu ends the search; otherwise the main feature's frames
//! follow (until the feature gives one, whatever time the menus took), and
//! the best frame of all is kept:
//!
//! 1. The main feature is the largest `BDMV/STREAM/xxxxx.m2ts`. On real discs
//!    this is the clip of the main playlist's longest play item, also on discs
//!    with hundreds of decoy playlists, without parsing any playlist. Up to
//!    `MAX_CLIPS` clips are tried, largest first; stereoscopic dependent-view
//!    clips (which cannot be decoded alone) are skipped.
//! 2. The clip's `BDMV/CLIPINF/xxxxx.clpi` (or its copy in `BDMV/BACKUP`)
//!    names the video stream and lists its entry points: the source packet of
//!    every I-picture (`crate::clpi`). A few of them are sampled, by position
//!    in the list (25 %, 40 %, 15 %, 55 %, 70 %): the opening of a film is
//!    often logos and titles.
//! 3. At an entry point, the span the EP_map gives for the I-picture (plus one
//!    aligned unit) is read, and the video PID's first complete PES packet is
//!    taken: on Blu-ray it holds the key frame's access unit with its
//!    parameter sets, for an interlaced frame coded as two field pictures both
//!    fields (`crate::m2ts`). The Windows decoder makes a whole frame of the
//!    first field in its thumbnail mode, without combing. When the packet does
//!    not end within the span (audio packets in between), the span is doubled
//!    until it does, up to a limit.
//! 4. MPEG-1/2 video is decoded by the crate's own decoder; H.264, HEVC and
//!    VC-1 by the decoders that come with Windows (`crate::mf`), after their
//!    headers were checked (`crate::nal`). HDR and BT.2020 frames are
//!    tone-mapped to SDR (`crate::yuv`).
//! 5. Frames are judged and the best one kept as for DVDs: nearly black,
//!    washed-out or flat frames are passed over unless nothing better turns
//!    up, and black bars are cut off.
//!
//! AACS-encrypted streams are never decrypted: when an aligned unit is marked
//! encrypted the disc is given up.
//!
//! Work is bounded on every axis, also for crafted images: at most
//! `MAX_STREAMS` stream files are looked at, `MAX_CLIPS` clips tried,
//! `MAX_ATTEMPTS` positions read, `MAX_DECODES` frames decoded and
//! `MAX_VIDEO_BYTES` read for all of it, besides the image's own budgets. The
//! menus take `MENU_PLAYLISTS` playlists, `MENU_ATTEMPTS` positions,
//! `MENU_DECODES` frames and `MENU_BYTES` (graphics included) of their own,
//! the bytes out of `MAX_VIDEO_BYTES`.

use crate::bdnav::{self, Object, MAX_NAV_BYTES};
use crate::clpi::{self, ClipInfo, EntryPoint, APP_DEPENDENT_VIEW, MAX_CLPI_BYTES};
use crate::dvd::{Candidate, Selection};
use crate::finder::{Thumbnail, FULL_SIZE};
use crate::fs::FileSystem;
use crate::igs;
use crate::m2ts::{self, ALIGNED_UNIT, SOURCE_PACKET};
use crate::mpeg2;
use crate::mpls::{self, Start, MAX_MPLS_BYTES};
#[cfg(windows)]
use crate::nal;
use crate::yuv::{Colour, Matrix, Primaries, Transfer};

/// Media Foundation, started on the first frame that needs a Windows decoder
/// (`None` until then, `Some(None)` when it could not be started).
#[cfg(windows)]
type Decoders = Option<Option<crate::mf::Session>>;
#[cfg(not(windows))]
type Decoders = ();

/// Stream files looked at (and sized) in `BDMV/STREAM`. Real discs have at
/// most a few hundred.
const MAX_STREAMS: usize = 512;
/// Clips tried, largest first.
const MAX_CLIPS: usize = 3;
/// Clips smaller than this are not worth a try (menus, short clips).
const MIN_CLIP_BYTES: u64 = 1 << 20;
/// Positions sampled in a clip, in per mille of its entry point list, in the
/// order tried.
const SAMPLE_PERMILLE: [u64; 5] = [250, 400, 150, 550, 700];
/// Positions read for the whole disc.
const MAX_ATTEMPTS: usize = 6;
/// Frames decoded for the whole disc, failed ones included.
const MAX_DECODES: usize = 6;
/// Bytes read from stream files for the whole disc.
const MAX_VIDEO_BYTES: u64 = 20 << 20;
/// Most bytes read at one position: an HD I-picture is well under 1.3 MB, a
/// UHD one under 4.5 MB.
const MAX_SPAN_HD: u64 = 2 << 20;
const MAX_SPAN_UHD: u64 = 6 << 20;
/// Bytes read first at a position whose entry gives no size bound.
const DEFAULT_SPAN_HD: u64 = 1 << 20;
const DEFAULT_SPAN_UHD: u64 = 3 << 20;
/// Once a frame is in hand, no new position is started after this long (a
/// dark UHD disc would otherwise take six decodes of about 250 ms).
const TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(1000);
/// Top menu playlists tried (a playlist without buttons does not count).
const MENU_PLAYLISTS: usize = 3;
/// Positions sampled in a menu's play item, in per mille of its entry
/// points, in the order tried.
const MENU_PERMILLE: [u64; 2] = [0, 500];
/// Positions read and frames decoded in menus, for the whole disc.
const MENU_ATTEMPTS: usize = 4;
const MENU_DECODES: usize = 4;
/// Bytes read for the menus (graphics included), out of `MAX_VIDEO_BYTES`.
const MENU_BYTES: u64 = 8 << 20;
/// Bytes of a menu clip read for its interactive graphics, and how many at
/// a time. A menu's graphics come before its first picture, and real ones
/// take well under a megabyte.
const GRAPHICS_BYTES: u64 = 4 << 20;
const GRAPHICS_CHUNK: u64 = 32 * ALIGNED_UNIT;
/// How far before the entry point a play item starts from its graphics are
/// looked for: a display set is multiplexed ahead of its presentation.
const GRAPHICS_LEAD: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Mpeg,
    #[cfg(windows)]
    Windows(crate::mf::Codec),
}

impl Codec {
    fn of(coding: u8) -> Option<Self> {
        match coding {
            clpi::CODING_MPEG1 | clpi::CODING_MPEG2 => Some(Codec::Mpeg),
            #[cfg(windows)]
            clpi::CODING_H264 => Some(Codec::Windows(crate::mf::Codec::H264)),
            #[cfg(windows)]
            clpi::CODING_HEVC => Some(Codec::Windows(crate::mf::Codec::Hevc)),
            #[cfg(windows)]
            clpi::CODING_VC1 => Some(Codec::Windows(crate::mf::Codec::Vc1)),
            _ => None,
        }
    }
}

struct Clip<N> {
    name: String,
    node: N,
    size: u64,
}

/// `00000.m2ts` .. `99999.m2ts`.
fn is_clip_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 10 && b[..5].iter().all(u8::is_ascii_digit) && b[5..].eq_ignore_ascii_case(b".m2ts")
}

/// The largest clips of `BDMV/STREAM`, largest first.
fn largest_clips<F: FileSystem>(fs: &mut F, stream: &F::Node) -> Vec<Clip<F::Node>> {
    let mut entries = Vec::new();
    let walked = fs.walk(stream, &mut |e| {
        if !e.is_dir && is_clip_name(&e.name) {
            entries.push(e);
        }
        entries.len() < MAX_STREAMS
    });
    if walked.is_err() {
        return Vec::new();
    }
    let mut clips: Vec<Clip<F::Node>> = Vec::new();
    for e in entries {
        if clips.iter().any(|c| c.name.eq_ignore_ascii_case(&e.name)) {
            continue;
        }
        let Ok(size) = fs.file_size(&e.node) else {
            continue;
        };
        if size >= MIN_CLIP_BYTES {
            clips.push(Clip {
                name: e.name,
                node: e.node,
                size,
            });
        }
    }
    clips.sort_by_key(|c| std::cmp::Reverse(c.size));
    clips.truncate(MAX_CLIPS);
    clips
}

/// Where clip information files are: `BDMV/CLIPINF`, and its copy in
/// `BDMV/BACKUP`, looked up only when needed.
struct InfoDirs<N> {
    main: Option<N>,
    backup: Option<Option<N>>,
}

/// Reads and parses one clip information file of `dir`.
fn read_clip_info<F: FileSystem>(fs: &mut F, dir: &F::Node, name: &str) -> Option<ClipInfo> {
    let file = fs.lookup(dir, name, false).ok()??;
    let data = fs.read(&file, MAX_CLPI_BYTES).ok()?;
    clpi::parse(&data).ok()
}

/// The clip information of `stem`, from `CLIPINF` or its backup copy.
fn clip_info<F: FileSystem>(
    fs: &mut F,
    bdmv: &F::Node,
    dirs: &mut InfoDirs<F::Node>,
    stem: &str,
) -> Option<ClipInfo> {
    let name = format!("{stem}.clpi");
    if let Some(info) = dirs
        .main
        .clone()
        .and_then(|d| read_clip_info(fs, &d, &name))
    {
        return Some(info);
    }
    let backup = dirs
        .backup
        .get_or_insert_with(|| {
            let b = fs.lookup(bdmv, "BACKUP", true).ok().flatten()?;
            fs.lookup(&b, "CLIPINF", true).ok().flatten()
        })
        .clone()?;
    read_clip_info(fs, &backup, &name)
}

/// Whether frames of `codec` can be decoded here (asked once per codec, so a
/// system without the decoder reads no video for it).
fn decodable(
    codec: Codec,
    #[cfg_attr(not(windows), allow(unused_variables))] session: &mut Decoders,
    known: &mut Vec<(Codec, bool)>,
) -> bool {
    if let Some(&(_, ok)) = known.iter().find(|(c, _)| *c == codec) {
        return ok;
    }
    let ok = match codec {
        Codec::Mpeg => true,
        #[cfg(windows)]
        Codec::Windows(kind) => session
            .get_or_insert_with(crate::mf::Session::start)
            .as_ref()
            .is_some_and(|s| crate::mf::available(s, kind)),
    };
    known.push((codec, ok));
    ok
}

/// The colour a decoder should assume when the stream does not say.
fn stream_colour(video: &clpi::VideoStream) -> Colour {
    if video.coding != clpi::CODING_HEVC {
        return Colour::BT709;
    }
    let hdr = matches!(
        video.dynamic_range,
        clpi::DYNAMIC_RANGE_HDR10 | clpi::DYNAMIC_RANGE_DOLBY_VISION
    );
    let wide = hdr || video.color_space == 2;
    Colour {
        transfer: if hdr { Transfer::Pq } else { Transfer::Sdr },
        primaries: if wide {
            Primaries::Bt2020
        } else {
            Primaries::Bt709
        },
        matrix: if wide { Matrix::Bt2020 } else { Matrix::Bt709 },
        full_range: false,
    }
}

/// What is left of the disc's allowances.
struct Allowance {
    attempts: usize,
    decodes: usize,
    bytes: u64,
}

impl Allowance {
    fn decode(&mut self) -> bool {
        if self.decodes == 0 {
            return false;
        }
        self.decodes -= 1;
        true
    }
}

enum Read {
    /// Packets from the entry point on (a whole number of source packets).
    Span(Vec<u8>),
    /// The stream is AACS-encrypted: give up the disc.
    Encrypted,
    /// Nothing usable (read error, allowance used up).
    Nothing,
}

/// Reads `len` bytes of the clip from source packet `spn` on, also checking
/// the aligned unit the entry point lies in for AACS encryption.
fn read_span<F: FileSystem>(
    fs: &mut F,
    clip: &Clip<F::Node>,
    spn: u32,
    len: u64,
    allowance: &mut Allowance,
) -> Read {
    let Some(offset) = u64::from(spn).checked_mul(SOURCE_PACKET as u64) else {
        return Read::Nothing;
    };
    let unit = offset - offset % ALIGNED_UNIT;
    let end = offset.saturating_add(len).min(clip.size);
    if end <= offset {
        return Read::Nothing;
    }
    let want = end - unit;
    let want = want - want % SOURCE_PACKET as u64;
    if want == 0 || want > allowance.bytes {
        return Read::Nothing;
    }
    allowance.bytes -= want;
    let Ok(want) = usize::try_from(want) else {
        return Read::Nothing;
    };
    let mut buf = vec![0u8; want];
    let Ok(n) = fs.read_range(&clip.node, unit, &mut buf) else {
        return Read::Nothing;
    };
    let skip = (offset - unit) as usize;
    if n <= skip {
        return Read::Nothing;
    }
    if buf[0] & 0xC0 != 0 {
        return Read::Encrypted;
    }
    buf.truncate(n - (n - skip) % SOURCE_PACKET);
    buf.drain(..skip);
    Read::Span(buf)
}

/// Reads more of the clip onto `span` (which starts at byte `offset`, a source
/// packet boundary) until it is `len` bytes long, in whole source packets.
/// False when nothing could be added.
fn extend_span<F: FileSystem>(
    fs: &mut F,
    clip: &Clip<F::Node>,
    offset: u64,
    span: &mut Vec<u8>,
    len: u64,
    allowance: &mut Allowance,
) -> bool {
    let start = offset.saturating_add(span.len() as u64);
    let end = offset.saturating_add(len).min(clip.size);
    let want = end.saturating_sub(start);
    let want = want - want % SOURCE_PACKET as u64;
    if want == 0 || want > allowance.bytes {
        return false;
    }
    allowance.bytes -= want;
    let old = span.len();
    span.resize(old + want as usize, 0);
    let n = fs
        .read_range(&clip.node, start, &mut span[old..])
        .unwrap_or(0);
    span.truncate(old + n - n % SOURCE_PACKET);
    span.len() > old
}

/// The outcome of one sampling position.
enum Sample {
    Frame(mpeg2::Frame),
    /// Needs more of the stream than was read.
    Incomplete,
    Failed,
}

/// Decodes the key frame at the start of `span` (from an entry point).
/// `at_end`: the span runs to the end of the clip, so its last packet is
/// whole (a still menu's single picture). `max_side`: the thumbnail size, for
/// the Windows decoders' colour conversion (`crate::yuv::to_frame_for`).
#[allow(clippy::too_many_arguments)]
fn decode_span(
    codec: Codec,
    span: &[u8],
    offset: u64,
    at_end: bool,
    info: &ClipInfo,
    allowance: &mut Allowance,
    #[cfg_attr(not(windows), allow(unused_variables))] session: &mut Decoders,
    #[cfg_attr(not(windows), allow(unused_variables))] max_side: u32,
) -> Result<Sample, ()> {
    // MPEG-2 pictures need not start PES packets, so everything is kept; the
    // other codecs only need the first packet, the key frame's access unit.
    let (packets, bytes) = match codec {
        Codec::Mpeg => (usize::MAX, mpeg2_limit()),
        #[cfg(windows)]
        Codec::Windows(_) => (1, crate::mf::MAX_INPUT_BYTES),
    };
    let pes = match m2ts::video_pes(span, offset, info.video.pid, packets, bytes) {
        Ok(p) => p,
        Err(crate::error::Error::Unsupported(_)) => return Err(()),
        Err(_) => return Ok(Sample::Failed),
    };
    match codec {
        Codec::Mpeg => {
            let mut es: Vec<u8> = pes.packets.concat();
            // A picture that ends the clip is terminated.
            let terminated = at_end && !pes.truncated;
            if terminated {
                es.extend_from_slice(&[0, 0, 1, 0xB7]);
            }
            // Without a whole I-picture yet (one ends where the next picture
            // starts) the span is read on, up to the most a position may read.
            let Some(range) = mpeg2::find_intra_picture(&es) else {
                return Ok(Sample::Incomplete);
            };
            if !allowance.decode() {
                return Ok(Sample::Failed);
            }
            // Ended only by the end of the clip: every macroblock must
            // decode, so a picture cut off does not give a grey band (as
            // `dvd::grab_frame` does at the end of a title).
            let by_clip_end = terminated && range.end + 4 == es.len();
            Ok(
                match mpeg2::decode_intra_within(&es[range], mpeg2::MAX_WIDTH, mpeg2::MAX_HEIGHT) {
                    Ok(frame) if !by_clip_end || frame.concealed_macroblocks == 0 => {
                        Sample::Frame(frame)
                    }
                    Ok(_) | Err(_) => Sample::Failed,
                },
            )
        }
        #[cfg(windows)]
        Codec::Windows(kind) => {
            use crate::mf::{Codec as Mf, Request};
            // The first packet is complete once another one started after it,
            // or when it ends the clip.
            let whole = pes.packets.len() > 1 || pes.last_complete || (at_end && !pes.truncated);
            let Some(first) = pes.packets.first().filter(|_| whole) else {
                return Ok(Sample::Incomplete);
            };
            let (format, private) = match kind {
                Mf::H264 => match nal::h264_access_unit(first) {
                    Some(f) => (f, None),
                    None => return Ok(Sample::Failed),
                },
                Mf::Hevc => match nal::hevc_access_unit(first) {
                    Some(f) => (f, None),
                    None => return Ok(Sample::Failed),
                },
                Mf::Vc1 => match nal::vc1_access_unit(first) {
                    Some(u) => (u.format, Some(u.private_data)),
                    None => return Ok(Sample::Failed),
                },
            };
            let started = session.get_or_insert_with(crate::mf::Session::start);
            let Some(started) = started.as_ref() else {
                return Ok(Sample::Failed);
            };
            if !allowance.decode() {
                return Ok(Sample::Failed);
            }
            let request = Request {
                codec: kind,
                unit: first,
                width: format.width,
                height: format.height,
                main10: format.main10,
                private_data: private.as_deref(),
                fallback_colour: stream_colour(&info.video),
                max_side,
            };
            Ok(match crate::mf::decode(started, &request) {
                Some(frame) => Sample::Frame(frame),
                None => Sample::Failed,
            })
        }
    }
}

/// Elementary stream bytes gathered for an MPEG-2 picture.
fn mpeg2_limit() -> usize {
    MAX_SPAN_HD as usize
}

/// The outcome of reading one entry point.
enum Position {
    Frame(mpeg2::Frame),
    Nothing,
    /// The stream is encrypted or scrambled: give up the disc.
    GiveUp,
}

/// Decodes the key frame at `entry` of `clip`, for a thumbnail of
/// `max_side` pixels. `searched` is set once video packets were read.
#[allow(clippy::too_many_arguments)]
fn sample<F: FileSystem>(
    fs: &mut F,
    clip: &Clip<F::Node>,
    info: &ClipInfo,
    codec: Codec,
    entry: EntryPoint,
    allowance: &mut Allowance,
    session: &mut Decoders,
    searched: &mut bool,
    max_side: u32,
) -> Position {
    let uhd = info.video.format == clpi::FORMAT_2160P;
    let (default_span, max_span) = if uhd {
        (DEFAULT_SPAN_UHD, MAX_SPAN_UHD)
    } else {
        (DEFAULT_SPAN_HD, MAX_SPAN_HD)
    };
    let first = clpi::i_picture_bound(entry.i_end, uhd)
        .map_or(default_span, |b| b + ALIGNED_UNIT)
        .min(max_span);
    let offset = u64::from(entry.spn) * SOURCE_PACKET as u64;
    let mut span = match read_span(fs, clip, entry.spn, first, allowance) {
        Read::Span(s) => s,
        Read::Encrypted => {
            // The other view of the disc need not find that again.
            *searched = true;
            return Position::GiveUp;
        }
        Read::Nothing => return Position::Nothing,
    };
    *searched = true;
    let mut target = first;
    loop {
        // No whole source packet of the clip after the span, in a clip of
        // whole aligned units (as Blu-ray clips are; a copy cut off is not).
        let at_end = offset
            .saturating_add(span.len() as u64)
            .saturating_add(SOURCE_PACKET as u64)
            > clip.size
            && clip.size % ALIGNED_UNIT == 0;
        match decode_span(
            codec, &span, offset, at_end, info, allowance, session, max_side,
        ) {
            Err(()) => return Position::GiveUp, // encrypted or scrambled
            Ok(Sample::Frame(f)) => return Position::Frame(f),
            // The access unit runs on past the span (audio packets in
            // between): read on, doubling the span up to the most a position
            // may read.
            Ok(Sample::Incomplete) if target < max_span => {
                target = target.saturating_mul(2).min(max_span);
                if !extend_span(fs, clip, offset, &mut span, target, allowance) {
                    return Position::Nothing;
                }
            }
            Ok(Sample::Incomplete) | Ok(Sample::Failed) => return Position::Nothing,
        }
    }
}

/// Reads and parses `name` in `sub` of the BDMV folder (`None`: in the
/// folder itself) or, when that fails, its copy in `BDMV/BACKUP`.
fn read_nav<F: FileSystem, T>(
    fs: &mut F,
    bdmv: &F::Node,
    sub: Option<&str>,
    name: &str,
    max: usize,
    parse: impl Fn(&[u8]) -> Option<T>,
) -> Option<T> {
    fn read_in<F: FileSystem>(
        fs: &mut F,
        base: &F::Node,
        sub: Option<&str>,
        name: &str,
        max: usize,
    ) -> Option<Vec<u8>> {
        let dir = match sub {
            Some(sub) => fs.lookup(base, sub, true).ok()??,
            None => base.clone(),
        };
        let file = fs.lookup(&dir, name, false).ok()??;
        fs.read(&file, max).ok()
    }
    if let Some(v) = read_in(fs, bdmv, sub, name, max).and_then(|d| parse(&d)) {
        return Some(v);
    }
    let backup = fs.lookup(bdmv, "BACKUP", true).ok()??;
    read_in(fs, &backup, sub, name, max).and_then(|d| parse(&d))
}

/// The playlists the top menu plays, and whether it is an HDMV menu (whose
/// playlists must have buttons).
fn menu_playlists<F: FileSystem>(fs: &mut F, bdmv: &F::Node) -> (Vec<bdnav::Play>, bool) {
    let index = read_nav(fs, bdmv, None, "index.bdmv", MAX_NAV_BYTES, |d| {
        bdnav::parse_index(d).ok()
    });
    match index.as_ref().map(|i| &i.top_menu) {
        Some(Object::Hdmv(_)) => {
            let objects = read_nav(fs, bdmv, None, "MovieObject.bdmv", MAX_NAV_BYTES, |d| {
                bdnav::parse_movie_objects(d).ok()
            });
            let playlists = match (&index, objects) {
                (Some(index), Some(objects)) => bdnav::top_menu_playlists(index, &objects),
                _ => Vec::new(),
            };
            (playlists, true)
        }
        Some(Object::Bdj(name)) => {
            let name = format!("{name}.bdjo");
            let first = read_nav(fs, bdmv, Some("BDJO"), &name, MAX_NAV_BYTES, |d| {
                bdnav::bdjo_autostart_playlist(d).ok().flatten()
            });
            let plays = first.map(|playlist| bdnav::Play {
                playlist,
                start: Start::First,
                button: None,
            });
            (plays.into_iter().collect(), false)
        }
        _ => (Vec::new(), false),
    }
}

enum Graphics {
    /// What the menu draws, if it could be read.
    Menu(Option<igs::Menu>),
    /// A pop-up menu: the playlist is not a top menu.
    PopUp,
    /// The stream is encrypted: give up the disc.
    Encrypted,
}

/// Reads the first display set of the interactive graphics stream `pid` of
/// `clip` presented from `in_pts` (90 kHz) on, from byte `offset` on, with
/// `button` selected before. Like libbluray's `m2ts_filter`, the stream's
/// packets are passed over until the first one presented at or after
/// `in_pts`: earlier display sets belong to what the clip holds before.
fn read_graphics<F: FileSystem>(
    fs: &mut F,
    clip: &Clip<F::Node>,
    (pid, mut offset, in_pts): (u16, u64, u64),
    button: Option<u16>,
    allowance: &mut Allowance,
    searched: &mut bool,
) -> Graphics {
    offset -= offset % ALIGNED_UNIT;
    let end = offset.saturating_add(GRAPHICS_BYTES).min(clip.size);
    let mut started = false;
    let mut pes = m2ts::GraphicsPes::new(pid);
    let mut collector = igs::Collector::default();
    // A pop-up composition settles the matter at once.
    while offset < end && !collector.done() && !collector.is_pop_up() {
        let want = GRAPHICS_CHUNK.min(end - offset);
        let want = want - want % SOURCE_PACKET as u64;
        if want == 0 || want > allowance.bytes {
            break;
        }
        allowance.bytes -= want;
        let mut buf = vec![0u8; want as usize];
        let n = match fs.read_range(&clip.node, offset, &mut buf) {
            Ok(n) => n - n % SOURCE_PACKET,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        *searched = true;
        let push = &mut |p: &[u8], pts: Option<u64>| {
            started |= pts.is_some_and(|t| t >= in_pts);
            !started || (collector.push(p) && !collector.is_pop_up())
        };
        match pes.push(&buf[..n], offset, push) {
            Err(crate::error::Error::Unsupported(_)) => return Graphics::Encrypted,
            // A broken packet: what came before it is kept.
            Err(_) => break,
            Ok(_) => {}
        }
        offset += n as u64;
    }
    if collector.is_pop_up() {
        Graphics::PopUp
    } else {
        Graphics::Menu(collector.into_menu(button))
    }
}

/// The entry point players start a play item from: the last one at or
/// before IN (libbluray `clpi_lookup_spn`), as an index into `info.entries`.
fn start_entry(info: &ClipInfo, item: &mpls::PlayItem) -> Option<usize> {
    info.entries.iter().rposition(|e| e.pts <= item.in_time)
}

/// Where a play item's graphics are looked for: from the start of the clip
/// when the play item starts at its first entry point, else from
/// `GRAPHICS_LEAD` before the entry point players start from.
fn graphics_start(info: &ClipInfo, item: &mpls::PlayItem) -> u64 {
    match start_entry(info, item) {
        Some(i) if i > 0 => {
            let at = u64::from(info.entries[i].spn) * SOURCE_PACKET as u64;
            at.saturating_sub(GRAPHICS_LEAD)
        }
        _ => 0,
    }
}

/// The entry points of `info` within a play item's IN and OUT times; else
/// the one it starts from (a play item within one group of pictures), or all
/// of them when IN comes before every one (a clip with several time bases).
fn entries_within(info: &ClipInfo, item: &mpls::PlayItem) -> Vec<EntryPoint> {
    // Entry point times are rounded down to 256 units.
    let start = item.in_time.saturating_sub(255);
    let within: Vec<EntryPoint> = info
        .entries
        .iter()
        .filter(|e| e.pts >= start && e.pts < item.out_time)
        .copied()
        .collect();
    if !within.is_empty() {
        return within;
    }
    match start_entry(info, item) {
        Some(i) => vec![info.entries[i]],
        None => info.entries.clone(),
    }
}

/// What the menu stage shares with the rest of the search.
struct Search<'a, N> {
    stream: &'a N,
    /// The thumbnail size.
    max_side: u32,
    info_dirs: InfoDirs<N>,
    selection: Selection,
    session: Decoders,
    known: Vec<(Codec, bool)>,
    started: std::time::Instant,
}

impl<N> Search<'_, N> {
    /// Whether no new position should be started: a frame is in hand and
    /// the time is up.
    fn late(&self) -> bool {
        self.started.elapsed() >= TIME_BUDGET && self.selection.best.is_some()
    }
}

/// Offers frames of the top menu to the selection. Returns true when the
/// search is over: a presentable menu, or an encrypted disc.
fn menu_frames<F: FileSystem>(
    fs: &mut F,
    bdmv: &F::Node,
    search: &mut Search<F::Node>,
    allowance: &mut Allowance,
    searched: &mut bool,
) -> bool {
    let (plays, hdmv) = menu_playlists(fs, bdmv);
    let mut tried = 0;
    for play in plays {
        if tried == MENU_PLAYLISTS
            || allowance.attempts == 0
            || allowance.decodes == 0
            || search.late()
        {
            break;
        }
        let name = format!("{:05}.mpls", play.playlist);
        let Some(item) = read_nav(fs, bdmv, Some("PLAYLIST"), &name, MAX_MPLS_BYTES, |d| {
            mpls::play_item(d, play.start).ok()
        }) else {
            continue;
        };
        if hdmv && !item.interactive {
            continue;
        }
        let file = format!("{}.m2ts", item.clip);
        let Some(node) = fs.lookup(search.stream, &file, false).ok().flatten() else {
            continue;
        };
        let Ok(size) = fs.file_size(&node) else {
            continue;
        };
        let clip = Clip {
            name: file,
            node,
            size,
        };
        let Some(info) = clip_info(fs, bdmv, &mut search.info_dirs, &item.clip) else {
            continue;
        };
        if info.application_type == APP_DEPENDENT_VIEW {
            continue;
        }
        let Some(codec) = Codec::of(info.video.coding) else {
            continue;
        };
        if !decodable(codec, &mut search.session, &mut search.known) {
            continue;
        }
        let entries = entries_within(&info, &item);
        let menu = match item.ig_pid.filter(|_| hdmv) {
            Some(pid) => {
                let at = (
                    pid,
                    graphics_start(&info, &item),
                    2 * u64::from(item.in_time),
                );
                match read_graphics(fs, &clip, at, play.button, allowance, searched) {
                    Graphics::Menu(menu) => menu,
                    // A film with its pop-up menu: not a menu at all.
                    Graphics::PopUp => continue,
                    Graphics::Encrypted => return true,
                }
            }
            None => None,
        };
        tried += 1;
        let count = entries.len() as u64;
        let mut sampled: Vec<usize> = Vec::new();
        for permille in MENU_PERMILLE {
            let index = ((count - 1) * permille / 1000) as usize;
            if sampled.contains(&index) {
                continue;
            }
            sampled.push(index);
            if allowance.attempts == 0 || allowance.decodes == 0 || search.late() {
                break;
            }
            allowance.attempts -= 1;
            let frame = match sample(
                fs,
                &clip,
                &info,
                codec,
                entries[index],
                allowance,
                &mut search.session,
                searched,
                // Buttons are drawn over the frame: at full size.
                FULL_SIZE,
            ) {
                Position::Frame(mut f) => {
                    if let Some(menu) = &menu {
                        menu.draw(&mut f);
                    }
                    Some(f)
                }
                Position::Nothing => None,
                Position::GiveUp => return true,
            };
            let source = format!("BDMV/PLAYLIST/{name} (top menu)");
            if search
                .selection
                .offer(frame.and_then(|f| Candidate::new(f, source)))
            {
                return true;
            }
        }
    }
    false
}

/// Finds the best picture of the disc: its top menu, else a frame of its
/// main clips. `searched` is set once video packets were read, so another
/// view of the same disc does not repeat the search.
pub fn video_picture<F: FileSystem>(
    fs: &mut F,
    bdmv: &F::Node,
    searched: &mut bool,
    max_side: u32,
) -> Option<Thumbnail> {
    let stream = fs.lookup(bdmv, "STREAM", true).ok()??;
    let mut search = Search {
        stream: &stream,
        max_side,
        info_dirs: InfoDirs {
            main: fs.lookup(bdmv, "CLIPINF", true).ok().flatten(),
            backup: None,
        },
        selection: Selection::default(),
        session: Default::default(),
        known: Vec::new(),
        started: std::time::Instant::now(),
    };
    let mut menu_allowance = Allowance {
        attempts: MENU_ATTEMPTS,
        decodes: MENU_DECODES,
        bytes: MENU_BYTES,
    };
    if menu_frames(fs, bdmv, &mut search, &mut menu_allowance, searched) {
        return search
            .selection
            .best
            .and_then(|c| c.into_thumbnail(max_side));
    }
    let mut allowance = Allowance {
        attempts: MAX_ATTEMPTS,
        decodes: MAX_DECODES,
        bytes: MAX_VIDEO_BYTES - (MENU_BYTES - menu_allowance.bytes),
    };
    let clips = largest_clips(fs, &stream);
    // The menus may have used up the time with a dark frame: the feature is
    // sampled until it gives a frame of its own, as before menus were looked
    // at.
    let mut feature_found = false;
    'clips: for clip in &clips {
        let stem = &clip.name[..5];
        let Some(info) = clip_info(fs, bdmv, &mut search.info_dirs, stem) else {
            continue;
        };
        if info.application_type == APP_DEPENDENT_VIEW {
            continue;
        }
        let Some(codec) = Codec::of(info.video.coding) else {
            continue;
        };
        if !decodable(codec, &mut search.session, &mut search.known) {
            continue;
        }
        let count = info.entries.len() as u64;
        let mut tried: Vec<usize> = Vec::new();
        for permille in SAMPLE_PERMILLE {
            let index = ((count - 1) * permille / 1000) as usize;
            if tried.contains(&index) {
                continue;
            }
            tried.push(index);
            if allowance.attempts == 0 || allowance.decodes == 0 || (feature_found && search.late())
            {
                break 'clips;
            }
            allowance.attempts -= 1;
            let entry = info.entries[index];
            let frame = match sample(
                fs,
                clip,
                &info,
                codec,
                entry,
                &mut allowance,
                &mut search.session,
                searched,
                search.max_side,
            ) {
                Position::Frame(f) => Some(f),
                Position::Nothing => None,
                Position::GiveUp => break 'clips,
            };
            let source = format!("BDMV/STREAM/{} ({}%)", clip.name, permille / 10);
            let candidate = frame.and_then(|f| Candidate::new(f, source));
            feature_found |= candidate.is_some();
            if search.selection.offer(candidate) {
                break 'clips;
            }
        }
    }
    search
        .selection
        .best
        .and_then(|c| c.into_thumbnail(max_side))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_names() {
        assert!(is_clip_name("00000.m2ts"));
        assert!(is_clip_name("12345.M2TS"));
        assert!(!is_clip_name("0000.m2ts"));
        assert!(!is_clip_name("00000.ssif"));
        assert!(!is_clip_name("abcde.m2ts"));
    }

    #[test]
    fn hdr_streams_get_a_pq_fallback() {
        let mut v = clpi::VideoStream {
            pid: 0x1011,
            coding: clpi::CODING_HEVC,
            format: 8,
            aspect: 3,
            dynamic_range: clpi::DYNAMIC_RANGE_HDR10,
            color_space: 2,
        };
        let c = stream_colour(&v);
        assert_eq!(c.transfer, Transfer::Pq);
        assert_eq!(c.primaries, Primaries::Bt2020);
        v.dynamic_range = clpi::DYNAMIC_RANGE_SDR;
        v.color_space = 1;
        assert_eq!(stream_colour(&v), Colour::BT709);
        v.coding = clpi::CODING_H264;
        assert_eq!(stream_colour(&v), Colour::BT709);
    }
}
