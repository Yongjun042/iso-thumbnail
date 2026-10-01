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
                return Ok(out);
            }
        }
    }
    match current {
        Some(last) => push_payload(&mut out, last),
        None => out.last_complete = closed && !truncated,
    }
    Ok(out)
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
