//! DVD-like MPEG program stream writer for tests.
//!
//! `mux` wraps a video elementary stream into 2048-byte packs the way a DVD
//! authoring tool lays out a `.VOB`:
//!
//! - every pack starts with a pack header carrying an increasing SCR
//!   (MPEG-2: 14 bytes at 10.08 Mbit/s; MPEG-1: 12 bytes);
//! - a NAV pack (system header + PCI and DSI `private_stream_2` packets) opens
//!   the stream and every group of pictures, i.e. precedes each sequence
//!   header or a GOP header that is not already behind one;
//! - video packs hold one video PES packet each; the first packet of every
//!   picture carries a PTS (and a DTS for I and P pictures), a new packet is
//!   started at every picture, and a pack that is not full is completed with
//!   PES header stuffing (gaps under 6 bytes) or a padding packet;
//! - dummy audio packs are interleaved: AC-3 in `private_stream_1`
//!   (substream 0x80) and MPEG audio (0xC0). Their payload deliberately
//!   contains fake start codes (`00 00 01 E0`, `00 00 01 BA`) so a demuxer that
//!   does not skip packets by their length is caught, and in MPEG-2 mode they
//!   use non-zero `pack_stuffing_length`;
//! - optionally the video PES packets are marked scrambled
//!   (`PES_scrambling_control` = 01, as on a CSS protected disc) or MPEG-1
//!   pack and PES headers are used.
//!
//! Every pack is exactly `PACK_SIZE` bytes; an optional program end code
//! follows the last one. The writer does not validate the elementary stream:
//! any bytes are accepted, start codes are only used to place NAV packs and
//! PTS.

/// Size of every pack written.
pub const PACK_SIZE: usize = 2048;

/// Program mux rate in units of 50 bytes/s (10.08 Mbit/s, the DVD maximum).
const MUX_RATE: u32 = 25_200;
/// One frame at 29.97 Hz in 90 kHz units.
const FRAME_TICKS: u64 = 3003;

#[derive(Debug, Clone)]
pub struct PsOptions {
    /// MPEG-1 pack headers (12 bytes) and MPEG-1 PES headers.
    pub mpeg1: bool,
    /// Mark every video PES packet as scrambled (MPEG-2 only).
    pub scrambled: bool,
    /// Insert an audio pack after every this many video packs (0 = no audio).
    pub audio_every: usize,
    /// `stream_id` of the video packets.
    pub video_id: u8,
    /// Append a program end code (`00 00 01 B9`) after the last pack.
    pub end_code: bool,
}

impl Default for PsOptions {
    fn default() -> Self {
        Self {
            mpeg1: false,
            scrambled: false,
            audio_every: 3,
            video_id: 0xE0,
            end_code: true,
        }
    }
}

/// A written stream and what it contains.
#[derive(Debug, Clone, Default)]
pub struct Muxed {
    pub data: Vec<u8>,
    /// Video PES packets written (one per video pack).
    pub video_packets: u32,
    pub nav_packs: u32,
    pub audio_packs: u32,
}

impl Muxed {
    /// Number of whole packs (the program end code excluded).
    pub fn packs(&self) -> usize {
        self.data.len() / PACK_SIZE
    }
}

/// Writes `es` as a DVD-like program stream; see the module documentation.
pub fn mux(es: &[u8], opts: &PsOptions) -> Muxed {
    assert!(
        !(opts.mpeg1 && opts.scrambled),
        "MPEG-1 has no scrambling control"
    );
    let mut w = Writer {
        opts: opts.clone(),
        out: Muxed::default(),
        pack_index: 0,
        video_since_audio: 0,
    };
    let segments = segments(es);
    if segments.first().is_none_or(|s| !s.nav) {
        w.nav_pack();
    }
    for (n, seg) in segments.iter().enumerate() {
        if seg.nav {
            w.nav_pack();
        }
        let pts = 0x1_0000 + n as u64 * FRAME_TICKS;
        let stamps = seg.picture_type.map(|t| {
            // I and P pictures are decoded one frame before they are shown.
            let dts = matches!(t, 1 | 2).then_some(pts - FRAME_TICKS);
            (pts, dts)
        });
        w.video(&es[seg.range.clone()], stamps);
    }
    if opts.end_code {
        w.out.data.extend_from_slice(&[0, 0, 1, 0xB9]);
    }
    w.out
}

/// A part of the elementary stream that starts a new video PES packet.
struct Segment {
    range: std::ops::Range<usize>,
    /// Starts a group of pictures: a NAV pack goes in front.
    nav: bool,
    /// `picture_coding_type` of the picture inside (1 = I, 2 = P, 3 = B).
    picture_type: Option<u8>,
}

/// Splits `es` before every picture, keeping sequence and GOP headers with
/// the picture that follows them.
fn segments(es: &[u8]) -> Vec<Segment> {
    let mut starts: Vec<(usize, bool)> = Vec::new();
    let mut in_headers = false;
    let mut i = 0;
    while i + 4 <= es.len() {
        if es[i..i + 3] != [0, 0, 1] {
            i += 1;
            continue;
        }
        match es[i + 3] {
            0xB3 | 0xB8 => {
                if !in_headers {
                    starts.push((i, true));
                    in_headers = true;
                }
            }
            0x00 => {
                if !in_headers {
                    starts.push((i, false));
                }
                in_headers = false;
            }
            _ => {}
        }
        i += 4;
    }
    if starts.first().is_none_or(|&(p, _)| p != 0) {
        starts.insert(0, (0, false));
    }
    let mut out = Vec::new();
    for (k, &(start, nav)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(es.len(), |&(p, _)| p);
        if start == end {
            continue;
        }
        let range = start..end;
        let picture_type = find_picture(&es[range.clone()])
            .and_then(|p| es.get(start + p + 5))
            .map(|b| (b >> 3) & 7);
        out.push(Segment {
            range,
            nav,
            picture_type,
        });
    }
    out
}

/// Offset of the first picture start code in `seg`.
fn find_picture(seg: &[u8]) -> Option<usize> {
    seg.windows(4).position(|w| w == [0, 0, 1, 0])
}

struct Writer {
    opts: PsOptions,
    out: Muxed,
    pack_index: u64,
    video_since_audio: usize,
}

impl Writer {
    /// Starts a pack: the pack header with the SCR of this pack's first byte.
    fn pack_header(&mut self, stuffing: usize) {
        // 27 MHz system clock; the stream advances PACK_SIZE bytes per pack.
        let scr27 = self.pack_index * PACK_SIZE as u64 * 27_000_000 / (MUX_RATE as u64 * 50);
        self.pack_index += 1;
        let d = &mut self.out.data;
        d.extend_from_slice(&[0, 0, 1, 0xBA]);
        if self.opts.mpeg1 {
            let scr = scr27 / 300;
            d.push(0x20 | (((scr >> 30) & 7) as u8) << 1 | 1);
            d.push((scr >> 22) as u8);
            d.push((((scr >> 15) & 0x7F) as u8) << 1 | 1);
            d.push((scr >> 7) as u8);
            d.push(((scr & 0x7F) as u8) << 1 | 1);
            d.push(0x80 | ((MUX_RATE >> 15) & 0x7F) as u8);
            d.push((MUX_RATE >> 7) as u8);
            d.push(((MUX_RATE & 0x7F) as u8) << 1 | 1);
        } else {
            let (base, ext) = (scr27 / 300, scr27 % 300);
            d.push(0x40 | (((base >> 30) & 7) as u8) << 3 | 0x04 | ((base >> 28) & 3) as u8);
            d.push((base >> 20) as u8);
            d.push((((base >> 15) & 0x1F) as u8) << 3 | 0x04 | ((base >> 13) & 3) as u8);
            d.push((base >> 5) as u8);
            d.push(((base & 0x1F) as u8) << 3 | 0x04 | ((ext >> 7) & 3) as u8);
            d.push(((ext & 0x7F) as u8) << 1 | 1);
            d.push((MUX_RATE >> 14) as u8);
            d.push((MUX_RATE >> 6) as u8);
            d.push(((MUX_RATE & 0x3F) as u8) << 2 | 3);
            d.push(0xF8 | stuffing as u8);
            d.extend(std::iter::repeat_n(0xFF, stuffing));
        }
    }

    /// Bytes left in the pack being written.
    fn room(&self) -> usize {
        PACK_SIZE - self.out.data.len() % PACK_SIZE
    }

    /// NAV pack: system header, PCI and DSI.
    fn nav_pack(&mut self) {
        self.pack_header(0);
        let d = &mut self.out.data;
        // System header with four stream entries (video, MPEG audio, private 1 and 2).
        d.extend_from_slice(&[0, 0, 1, 0xBB, 0, 18]);
        let rate_bound = MUX_RATE;
        d.push(0x80 | ((rate_bound >> 15) & 0x7F) as u8);
        d.push((rate_bound >> 7) as u8);
        d.push(((rate_bound & 0x7F) as u8) << 1 | 1);
        d.push(1 << 2); // audio_bound 1, fixed_flag 0, CSPS_flag 0
        d.push(0xE0 | 1); // audio and video lock, marker, video_bound 1
        d.push(0x7F); // packet_rate_restriction 0, reserved
        for (id, scale, size) in [
            (self.opts.video_id, 1u8, 232u16),
            (0xC0, 0, 32),
            (0xBD, 1, 58),
            (0xBF, 1, 2),
        ] {
            d.push(id);
            d.push(0xC0 | scale << 5 | (size >> 8) as u8);
            d.push(size as u8);
        }
        // PCI: 980 bytes of body starting with substream 0x00.
        let mut pci = vec![0u8; 980];
        pci[0] = 0x00;
        pci[1..9].copy_from_slice(&self.pack_index.to_be_bytes());
        self.packet(0xBF, &pci);
        // DSI fills the rest of the pack.
        let body = self.room() - 6;
        let mut dsi = vec![0u8; body];
        dsi[0] = 0x01;
        self.packet(0xBF, &dsi);
        assert_eq!(self.room(), PACK_SIZE);
        self.out.nav_packs += 1;
    }

    /// Appends a PES packet with the given body (header included).
    fn packet(&mut self, id: u8, body: &[u8]) {
        let d = &mut self.out.data;
        d.extend_from_slice(&[0, 0, 1, id]);
        d.extend_from_slice(&(body.len() as u16).to_be_bytes());
        d.extend_from_slice(body);
    }

    /// PES header (after `PES_packet_length`) with optional PTS/DTS and stuffing.
    fn pes_header(
        &self,
        stamps: Option<(u64, Option<u64>)>,
        stuffing: usize,
        video: bool,
    ) -> Vec<u8> {
        let mut h = Vec::new();
        if self.opts.mpeg1 {
            h.extend(std::iter::repeat_n(0xFF, stuffing));
            // STD buffer on stamped packets, as muxers do: '01', scale 1
            // (KiB units), size 46 (13 bits, the high five of them zero).
            if stamps.is_some() {
                h.extend_from_slice(&[0x40 | 0x20, 46]);
            }
            match stamps {
                Some((pts, Some(dts))) => {
                    h.extend(timestamp(0x3, pts));
                    h.extend(timestamp(0x1, dts));
                }
                Some((pts, None)) => h.extend(timestamp(0x2, pts)),
                None => h.push(0x0F),
            }
        } else {
            let scrambling = if video && self.opts.scrambled {
                0x10
            } else {
                0
            };
            // '10', scrambling control, priority 0, data_alignment 1, copyright 0, original 0.
            h.push(0x80 | scrambling | 0x04);
            let (flags, mut data) = match stamps {
                Some((pts, Some(dts))) => {
                    (0xC0, [timestamp(0x3, pts), timestamp(0x1, dts)].concat())
                }
                Some((pts, None)) => (0x80, timestamp(0x2, pts).to_vec()),
                None => (0x00, Vec::new()),
            };
            data.extend(std::iter::repeat_n(0xFF, stuffing));
            h.push(flags);
            h.push(data.len() as u8);
            h.extend(data);
        }
        h
    }

    /// Writes `seg` in video packs; the first packet carries `stamps`.
    fn video(&mut self, seg: &[u8], mut stamps: Option<(u64, Option<u64>)>) {
        let mut rest = seg;
        while !rest.is_empty() {
            if self.opts.audio_every > 0 && self.video_since_audio == self.opts.audio_every {
                self.audio_pack();
                self.video_since_audio = 0;
            }
            self.pack_header(0);
            let header = self.pes_header(stamps, 0, true);
            let room = self.room() - 6 - header.len();
            let take = room.min(rest.len());
            let gap = room - take;
            let header = if (1..6).contains(&gap) {
                self.pes_header(stamps, gap, true)
            } else {
                header
            };
            let mut body = header;
            body.extend_from_slice(&rest[..take]);
            self.packet(self.opts.video_id, &body);
            if gap >= 6 {
                self.packet(0xBE, &vec![0xFF; gap - 6]);
            }
            assert_eq!(self.room(), PACK_SIZE);
            rest = &rest[take..];
            stamps = None;
            self.out.video_packets += 1;
            self.video_since_audio += 1;
        }
    }

    /// A pack of dummy audio, alternating between AC-3 and MPEG audio.
    fn audio_pack(&mut self) {
        let n = self.out.audio_packs;
        let stuffing = if self.opts.mpeg1 { 0 } else { (n % 4) as usize };
        self.pack_header(stuffing);
        let pts = 0x1_0000 + n as u64 * 2880;
        let mut body = self.pes_header(Some((pts, None)), 0, false);
        let ac3 = n % 2 == 0;
        if ac3 {
            // Substream 0x80 (AC-3 track 1), one frame, first access unit at 1.
            body.extend_from_slice(&[0x80, 0x01, 0x00, 0x01]);
        }
        let fill = self.room() - 6 - body.len();
        body.extend((0..fill).map(|i| match i % 97 {
            // Fake start codes inside the payload.
            10..=15 => [0, 0, 1, 0xE0, 0x07, 0xEC][i % 97 - 10],
            50..=53 => [0, 0, 1, 0xBA][i % 97 - 50],
            _ => (i * 7 + 3) as u8 | 0x10,
        }));
        self.packet(if ac3 { 0xBD } else { 0xC0 }, &body);
        assert_eq!(self.room(), PACK_SIZE);
        self.out.audio_packs += 1;
    }
}

/// A 5-byte PTS or DTS field with the given 4-bit prefix.
fn timestamp(prefix: u8, t: u64) -> [u8; 5] {
    let t = t & ((1 << 33) - 1);
    [
        prefix << 4 | (((t >> 30) & 7) as u8) << 1 | 1,
        (t >> 22) as u8,
        (((t >> 15) & 0x7F) as u8) << 1 | 1,
        (t >> 7) as u8,
        ((t & 0x7F) as u8) << 1 | 1,
    ]
}
