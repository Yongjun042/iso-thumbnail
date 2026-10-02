//! Blu-ray transport streams (`BDMV/STREAM/xxxxx.m2ts`): just enough to take
//! the video elementary stream out of a few hundred kilobytes of a clip.
//!
//! An `.m2ts` is a sequence of 192-byte source packets: a 4-byte
//! TP_extra_header (2-bit copy permission indicator, 30-bit arrival time
//! stamp) followed by a 188-byte MPEG transport stream packet. Packets come in
//! aligned units of 32 (6144 bytes). AACS encrypts every aligned unit but its
//! first 16 bytes and marks it in the copy permission bits of its first
//! packet; such units are never decrypted here, the stream is given up.
//!
//! The video PID's packets are reassembled into PES packets (a new one starts
//! at each packet with payload_unit_start_indicator set) and the PES headers
//! are removed. On Blu-ray every H.264, HEVC and VC-1 access unit is one PES
//! packet; MPEG-2 pictures may span several.

use crate::error::{Error, Result};

/// Size of one source packet.
pub const SOURCE_PACKET: usize = 192;
/// Size of one aligned unit (32 source packets), the unit of AACS encryption.
pub const ALIGNED_UNIT: u64 = 6144;

const SYNC_BYTE: u8 = 0x47;

/// Elementary stream data of the video PID found in a span of a clip.
#[derive(Debug, Default)]
pub struct VideoPes {
    /// Payloads of the PES packets that started in the span, in order, PES
    /// headers removed. Every one but the last is complete.
    pub packets: Vec<Vec<u8>>,
    /// Whether the last packet is complete too: another PES packet of the PID
    /// started after it within the span, or it holds the length its header
    /// states, and nothing of it was left out at the byte limit.
    pub last_complete: bool,
    /// Whether bytes were left out at the byte limit.
    pub truncated: bool,
}

/// The PES payload offset in `pes`, or `None` when it is not a video PES
/// packet with a well-formed header.
fn pes_payload_start(pes: &[u8]) -> Option<usize> {
    if pes.get(..3)? != [0, 0, 1] {
        return None;
    }
    let stream_id = *pes.get(3)?;
    // Video streams (0xE0..0xEF) and the extended stream_id used for VC-1.
    if !(0xE0..=0xEF).contains(&stream_id) && stream_id != 0xFD {
        return None;
    }
    // '10' marker bits of the optional header.
    if pes.get(6)? & 0xC0 != 0x80 {
        return None;
    }
    let start = 9 + usize::from(*pes.get(8)?);
    (start <= pes.len()).then_some(start)
}

/// Moves a finished PES packet's payload into `out`.
fn push_payload(out: &mut VideoPes, mut pes: Vec<u8>) {
    if let Some(start) = pes_payload_start(&pes) {
        pes.drain(..start);
        out.packets.push(pes);
    }
}

/// Collects the PES packets of `pid` in `span`, which starts at a source
/// packet boundary at byte `offset` of the clip. A packet is complete where
/// the next one starts, or once it holds the length its header states.
///
/// Stops once `max_packets` packets are complete, or at the next packet start
/// once `max_bytes` of elementary stream data are gathered. Fails with
/// `Unsupported` for AACS-encrypted or transport-scrambled data and with
/// `Corrupt` when the 192-byte packet structure is broken.
pub fn video_pes(
    span: &[u8],
    offset: u64,
    pid: u16,
    max_packets: usize,
    max_bytes: usize,
) -> Result<VideoPes> {
    let mut out = VideoPes::default();
    let mut current: Option<Vec<u8>> = None;
    let mut total = 0usize;
    // Whether bytes were left out at `max_bytes`.
    let mut truncated = false;
    // Whether the last packet in `out` is known to be complete.
    let mut closed = false;
    for (i, packet) in span.chunks_exact(SOURCE_PACKET).enumerate() {
        let at = offset
            .checked_add((i * SOURCE_PACKET) as u64)
            .ok_or(Error::Corrupt("m2ts offset"))?;
        if at % ALIGNED_UNIT == 0 && packet[0] & 0xC0 != 0 {
            return Err(Error::Unsupported("AACS-encrypted stream"));
        }
        let ts = &packet[4..];
        if ts[0] != SYNC_BYTE {
            return Err(Error::Corrupt("m2ts packet sync"));
        }
        let packet_pid = (u16::from(ts[1] & 0x1F) << 8) | u16::from(ts[2]);
        if packet_pid != pid || ts[1] & 0x80 != 0 {
            // Another stream, or a packet flagged with a transport error.
            continue;
        }
        if ts[3] & 0xC0 != 0 {
            return Err(Error::Unsupported("scrambled transport stream"));
        }
        let control = (ts[3] >> 4) & 3;
        let payload_start = match control {
            1 => 4,
            3 => 5 + usize::from(ts[4]),
            _ => continue, // adaptation field only, or reserved
        };
        let Some(payload) = ts.get(payload_start..) else {
            continue;
        };
        if ts[1] & 0x40 != 0 {
            if let Some(done) = current.take() {
                push_payload(&mut out, done);
                closed = true;
            }
            if closed && (total >= max_bytes || out.packets.len() >= max_packets) {
                out.last_complete = !truncated;
                out.truncated = truncated;
                return Ok(out);
            }
            current = Some(Vec::new());
            closed = false;
        }
        let Some(pes) = current.as_mut() else {
            continue;
        };
        let take = payload.len().min(max_bytes - total);
        pes.extend_from_slice(&payload[..take]);
        total += take;
        truncated |= take < payload.len();
        // A packet that states its length (`PES_packet_length`, counted from
        // after that field) is complete once that much is in, without waiting
        // for the next one to start.
        let stated = pes
            .get(4..6)
            .map_or(0, |l| usize::from(u16::from_be_bytes([l[0], l[1]])));
        if stated > 0 && pes.len() >= 6 + stated {
            pes.truncate(6 + stated);
            if let Some(done) = current.take() {
                push_payload(&mut out, done);
            }
            closed = true;
            if out.packets.len() >= max_packets {
                out.last_complete = !truncated;
                out.truncated = truncated;
                return Ok(out);
            }
        }
    }
    match current {
        Some(last) => push_payload(&mut out, last),
        None => out.last_complete = closed && !truncated,
    }
    out.truncated = truncated;
    Ok(out)
}

/// Largest PES packet: `PES_packet_length` is 16 bits.
const MAX_PES: usize = 6 + 0xFFFF;

/// Reassembles the PES packets of one graphics stream (interactive or
/// presentation graphics, carried as private_stream_1) from consecutive
/// spans of a clip.
pub struct GraphicsPes {
    pid: u16,
    current: Option<Vec<u8>>,
}

impl GraphicsPes {
    pub fn new(pid: u16) -> Self {
        Self { pid, current: None }
    }

    /// Passes a finished PES packet's payload and presentation time to `f`.
    fn finish(pes: Vec<u8>, f: &mut dyn FnMut(&[u8], Option<u64>) -> bool) -> bool {
        let start = match pes.get(..9) {
            Some(h) if h[..4] == [0, 0, 1, 0xBD] && h[6] & 0xC0 == 0x80 => 9 + usize::from(h[8]),
            _ => return true,
        };
        // PTS_DTS_flags, then the 33-bit PTS in 5 bytes with marker bits.
        let pts = pes
            .get(9..14)
            .filter(|_| pes[7] & 0x80 != 0 && start >= 14)
            .map(|t| {
                (u64::from(t[0] >> 1 & 7) << 30)
                    | (u64::from(t[1]) << 22)
                    | (u64::from(t[2] >> 1) << 15)
                    | (u64::from(t[3]) << 7)
                    | u64::from(t[4] >> 1)
            });
        match pes.get(start..) {
            Some(payload) => f(payload, pts),
            None => true,
        }
    }

    /// Feeds `span` (whole source packets from byte `offset` of the clip, a
    /// source packet boundary) and calls `f` with the payload and PTS (90 kHz)
    /// of every PES packet completed in it, until `f` returns false. Returns
    /// whether `f` asked to stop. Fails like `video_pes`.
    pub fn push(
        &mut self,
        span: &[u8],
        offset: u64,
        f: &mut dyn FnMut(&[u8], Option<u64>) -> bool,
    ) -> Result<bool> {
        for (i, packet) in span.chunks_exact(SOURCE_PACKET).enumerate() {
            let at = offset
                .checked_add((i * SOURCE_PACKET) as u64)
                .ok_or(Error::Corrupt("m2ts offset"))?;
            if at % ALIGNED_UNIT == 0 && packet[0] & 0xC0 != 0 {
                return Err(Error::Unsupported("AACS-encrypted stream"));
            }
            let ts = &packet[4..];
            if ts[0] != SYNC_BYTE {
                return Err(Error::Corrupt("m2ts packet sync"));
            }
            let packet_pid = (u16::from(ts[1] & 0x1F) << 8) | u16::from(ts[2]);
            if packet_pid != self.pid || ts[1] & 0x80 != 0 {
                continue;
            }
            if ts[3] & 0xC0 != 0 {
                return Err(Error::Unsupported("scrambled transport stream"));
            }
            let payload_start = match (ts[3] >> 4) & 3 {
                1 => 4,
                3 => 5 + usize::from(ts[4]),
                _ => continue,
            };
            let Some(payload) = ts.get(payload_start..) else {
                continue;
            };
            if ts[1] & 0x40 != 0 {
                if let Some(done) = self.current.replace(Vec::new()) {
                    if !Self::finish(done, f) {
                        return Ok(true);
                    }
                }
            }
            let Some(pes) = self.current.as_mut() else {
                continue;
            };
            pes.extend_from_slice(payload);
            let stated = pes
                .get(4..6)
                .map_or(0, |l| usize::from(u16::from_be_bytes([l[0], l[1]])));
            if stated > 0 && pes.len() >= 6 + stated {
                pes.truncate(6 + stated);
                let done = self.current.take().unwrap_or_default();
                if !Self::finish(done, f) {
                    return Ok(true);
                }
            } else if pes.len() > MAX_PES {
                // Longer than any PES packet can be: drop it.
                self.current = None;
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes `es` as one PES packet of `pid` in 192-byte source packets,
    /// padding the last TS packet with an adaptation field.
    pub(crate) fn packetize(pid: u16, stream_id: u8, es: &[u8], out: &mut Vec<u8>) {
        let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        pes.extend_from_slice(es);
        let mut first = true;
        for chunk in pes.chunks(184) {
            let mut ts = vec![
                SYNC_BYTE,
                (if first { 0x40 } else { 0 }) | (pid >> 8) as u8,
                pid as u8,
            ];
            if chunk.len() == 184 {
                ts.push(0x10);
            } else {
                let stuffing = 183 - chunk.len();
                ts.push(0x30);
                ts.push(stuffing as u8);
                if stuffing > 0 {
                    ts.push(0);
                    ts.extend(std::iter::repeat_n(0xFF, stuffing - 1));
                }
            }
            ts.extend_from_slice(chunk);
            assert_eq!(ts.len(), 188);
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&ts);
            first = false;
        }
    }

    fn null_packet(out: &mut Vec<u8>) {
        let mut ts = vec![SYNC_BYTE, 0x1F, 0xFF, 0x10];
        ts.resize(188, 0xFF);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&ts);
    }

    #[test]
    fn reassembles_pes_packets_of_one_pid() {
        let mut s = Vec::new();
        packetize(0x1011, 0xE0, &[1u8; 1000], &mut s);
        packetize(0x1100, 0xC0, &[9u8; 300], &mut s);
        null_packet(&mut s);
        packetize(0x1011, 0xE0, &[2u8; 50], &mut s);
        let got = video_pes(&s, 0, 0x1011, usize::MAX, 1 << 20).unwrap();
        assert_eq!(got.packets.len(), 2);
        assert_eq!(got.packets[0], vec![1u8; 1000]);
        assert_eq!(got.packets[1], vec![2u8; 50]);
        assert!(!got.last_complete);
    }

    #[test]
    fn detects_aacs_units_and_broken_packets() {
        let mut s = Vec::new();
        packetize(0x1011, 0xE0, &[1u8; 100], &mut s);
        let mut enc = s.clone();
        enc[0] |= 0xC0;
        assert_eq!(
            video_pes(&enc, 0, 0x1011, usize::MAX, 1 << 20).unwrap_err(),
            Error::Unsupported("AACS-encrypted stream")
        );
        // Not at an aligned-unit boundary: the copy bits are not looked at.
        assert!(video_pes(&enc, 192, 0x1011, usize::MAX, 1 << 20).is_ok());
        let mut broken = s.clone();
        broken[4] = 0;
        assert!(video_pes(&broken, 0, 0x1011, usize::MAX, 1 << 20).is_err());
    }

    #[test]
    fn stops_at_the_byte_limit() {
        let mut s = Vec::new();
        packetize(0x1011, 0xE0, &[1u8; 5000], &mut s);
        packetize(0x1011, 0xE0, &[2u8; 5000], &mut s);
        let got = video_pes(&s, 0, 0x1011, usize::MAX, 1000).unwrap();
        assert_eq!(got.packets.len(), 1);
        assert!(got.packets[0].len() <= 1000);
        // Cut short, so not complete.
        assert!(!got.last_complete);
        let got = video_pes(&s, 0, 0x1011, 1, 1000).unwrap();
        assert!(!got.last_complete);
    }

    #[test]
    fn a_packet_that_states_its_length_needs_no_successor() {
        let mut s = Vec::new();
        packetize(0x1011, 0xE0, &[5u8; 1000], &mut s);
        // PES_packet_length: 3 flag/length bytes, 5 PTS bytes, the payload.
        s[12..14].copy_from_slice(&1008u16.to_be_bytes());
        for _ in 0..4 {
            packetize(0x1100, 0xC0, &[9u8; 100], &mut s);
        }
        let got = video_pes(&s, 0, 0x1011, 1, 1 << 20).unwrap();
        assert_eq!(got.packets, vec![vec![5u8; 1000]]);
        assert!(got.last_complete);
        let got = video_pes(&s, 0, 0x1011, usize::MAX, 1 << 20).unwrap();
        assert_eq!(got.packets, vec![vec![5u8; 1000]]);
        assert!(got.last_complete);
        // A stated length shorter than the data cuts the packet there.
        s[12..14].copy_from_slice(&508u16.to_be_bytes());
        let got = video_pes(&s, 0, 0x1011, 1, 1 << 20).unwrap();
        assert_eq!(got.packets, vec![vec![5u8; 500]]);
    }

    #[test]
    fn stops_after_the_requested_packets() {
        let mut s = Vec::new();
        for n in 1..=3u8 {
            packetize(0x1011, 0xE0, &[n; 400], &mut s);
            packetize(0x1100, 0xC0, &[9u8; 100], &mut s);
        }
        let got = video_pes(&s, 0, 0x1011, 1, 1 << 20).unwrap();
        assert_eq!(got.packets, vec![vec![1u8; 400]]);
        assert!(got.last_complete);
        // The first packet alone in the span is not known to be complete.
        let first_only = &s[..3 * SOURCE_PACKET];
        let got = video_pes(first_only, 0, 0x1011, 1, 1 << 20).unwrap();
        assert_eq!(got.packets.len(), 1);
        assert!(!got.last_complete);
    }

    /// Collects what a `GraphicsPes` passes on from `spans`, fed in turn.
    fn graphics(pid: u16, spans: &[&[u8]], offset: u64) -> Result<Vec<Vec<u8>>> {
        let mut g = GraphicsPes::new(pid);
        let mut got = Vec::new();
        let mut at = offset;
        for s in spans {
            g.push(s, at, &mut |p, _| {
                got.push(p.to_vec());
                true
            })?;
            at += s.len() as u64;
        }
        Ok(got)
    }

    #[test]
    fn graphics_packets_are_reassembled_across_spans() {
        let mut s = Vec::new();
        packetize(0x1400, 0xBD, &[3u8; 700], &mut s);
        // PES_packet_length of 3 flag/length bytes, 5 PTS bytes, 700 payload.
        s[12..14].copy_from_slice(&708u16.to_be_bytes());
        packetize(0x1011, 0xE0, &[1u8; 300], &mut s);
        packetize(0x1400, 0xBD, &[4u8; 50], &mut s);
        packetize(0x1400, 0xE0, &[5u8; 50], &mut s);
        packetize(0x1400, 0xBD, &[6u8; 10], &mut s);
        // The first packet is complete at its stated length; the second when
        // the (not private) third starts; the last never.
        let cut = 2 * SOURCE_PACKET;
        let got = graphics(0x1400, &[&s[..cut], &s[cut..]], 0).unwrap();
        assert_eq!(got, vec![vec![3u8; 700], vec![4u8; 50]]);
        let mut g = GraphicsPes::new(0x1400);
        let mut n = 0;
        assert!(g
            .push(&s, 0, &mut |_, _| {
                n += 1;
                false
            })
            .unwrap());
        assert_eq!(n, 1);
        let mut enc = s.clone();
        enc[0] |= 0x40;
        assert!(graphics(0x1400, &[&enc], 0).is_err());
        assert!(graphics(0x1400, &[&enc], 192).is_ok());
    }

    #[test]
    fn graphics_packets_carry_their_presentation_time() {
        let mut s = Vec::new();
        packetize(0x1400, 0xBD, &[3u8; 10], &mut s);
        let header = |s: &[u8], from: usize| {
            from + s[from..]
                .windows(4)
                .position(|w| w == [0, 0, 1, 0xBD])
                .unwrap()
        };
        // PTS 0x1_2345_6789 in the marker-bit layout of the header.
        let pts: u64 = 0x1_2345_6789;
        let at = header(&s, 0) + 9;
        s[at..at + 5].copy_from_slice(&[
            0x21 | ((pts >> 29) as u8 & 0x0E),
            (pts >> 22) as u8,
            ((pts >> 14) as u8) | 1,
            (pts >> 7) as u8,
            ((pts << 1) as u8) | 1,
        ]);
        packetize(0x1400, 0xBD, &[4u8; 10], &mut s);
        let second = header(&s, 192);
        s[second + 7] = 0; // no PTS in the second
        packetize(0x1400, 0xBD, &[5u8; 10], &mut s);
        let mut got = Vec::new();
        GraphicsPes::new(0x1400)
            .push(&s, 0, &mut |p, t| {
                got.push((p[0], t));
                true
            })
            .unwrap();
        assert_eq!(got, [(3, Some(pts)), (4, None)]);
    }

    #[test]
    fn a_graphics_packet_that_states_its_length_needs_no_successor() {
        // The last packet of the PID: only its stated length completes it.
        let mut s = Vec::new();
        packetize(0x1400, 0xBD, &[3u8; 700], &mut s);
        s[12..14].copy_from_slice(&708u16.to_be_bytes());
        packetize(0x1011, 0xE0, &[1u8; 300], &mut s);
        assert_eq!(graphics(0x1400, &[&s], 0).unwrap(), vec![vec![3u8; 700]]);
        let mut g = GraphicsPes::new(0x1400);
        assert!(g.push(&s, 0, &mut |_, _| false).unwrap());
        // Without the length it waits for the next packet.
        s[12..14].copy_from_slice(&[0, 0]);
        assert!(graphics(0x1400, &[&s], 0).unwrap().is_empty());
    }

    #[test]
    fn hostile_spans_never_panic() {
        let mut base = Vec::new();
        packetize(0x1011, 0xE0, &[7u8; 3000], &mut base);
        packetize(0x1011, 0xFD, &[8u8; 700], &mut base);
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..3000 {
            let mut s = base.clone();
            for _ in 0..1 + (seed % 8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % s.len();
                s[i] = (seed >> 24) as u8;
            }
            let _ = video_pes(&s, 0, 0x1011, usize::MAX, 4096);
            let _ = video_pes(&s, 0, 0x1011, 1, 4096);
        }
    }
}
