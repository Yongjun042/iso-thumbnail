//! MPEG program stream demultiplexer (ISO/IEC 13818-1 and 11172-1), video only.
//!
//! DVD `.VOB` files are MPEG-2 program streams made of 2048-byte packs; MPEG-1
//! program streams are accepted too. Only the payload of the first video
//! stream (`stream_id` 0xE0–0xEF) is extracted; audio, sub-pictures,
//! navigation packets and padding are skipped. Scrambled PES packets (DVD
//! CSS, `PES_scrambling_control` ≠ 0) are counted and never decoded: this
//! module does not attempt any decryption.
//!
//! The demuxer keeps no parsing state between calls (only the stream it
//! follows and its counters):
//! every call parses complete units (pack headers, system headers, PES
//! packets) from the start of the buffer, so a unit is either consumed whole
//! or left for the next call. No unit is longer than `MAX_UNIT_LEN`, which
//! bounds what a caller ever has to keep.
//!
//! CONTRACT (fixed; other modules are written against it): `Demuxer`, its
//! public fields, `Demuxer::new`, `Demuxer::push` and `is_program_stream` keep
//! these signatures and meanings.

const PROGRAM_END: u8 = 0xB9;
const PACK_START: u8 = 0xBA;
const SYSTEM_HEADER: u8 = 0xBB;
/// First `stream_id` of a packet with a 16-bit length (program stream map);
/// every code from here to 0xFF is such a packet.
const FIRST_PACKET_ID: u8 = 0xBC;

/// Longest unit `push` may wait for: a PES packet with the largest 16-bit
/// `PES_packet_length`. A caller never needs to keep more than this.
pub const MAX_UNIT_LEN: usize = 6 + 0xFFFF;

/// Incremental video demultiplexer. Feed it consecutive chunks of a program
/// stream with `push`.
#[derive(Debug, Default, Clone)]
pub struct Demuxer {
    /// Video stream being followed: the first `stream_id` in 0xE0..=0xEF seen.
    pub stream_id: Option<u8>,
    /// Video PES packets of that stream whose payload was appended.
    pub video_packets: u32,
    /// Video PES packets of that stream skipped because they are scrambled.
    pub scrambled_packets: u32,
}

/// What the unit at the start of a buffer turned out to be.
enum Step {
    /// A complete, valid unit of this many bytes (at least 4) was handled.
    Consumed(usize),
    /// Looks valid so far but is cut off: wait for more data.
    NeedMore,
    /// Not a valid unit: resynchronise.
    Invalid,
}

/// Result of parsing the optional header of a video PES packet.
enum PesHeader {
    /// Header of `len` bytes (counted from the byte after `PES_packet_length`).
    Valid {
        len: usize,
        scrambled: bool,
    },
    /// The available bytes end inside the header.
    Short,
    Invalid,
}

impl Demuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Demultiplexes as many complete packs and packets at the start of `ps`
    /// as possible and appends the payload of unscrambled video PES packets to
    /// `es`.
    ///
    /// Returns how many bytes of `ps` were consumed. A packet cut off at the
    /// end of `ps` is not consumed: the caller keeps `ps[consumed..]`, appends
    /// the next chunk of the stream and calls `push` again. Bytes that do not
    /// start a valid pack or packet are skipped until the next start code
    /// (resynchronisation), so garbage never stalls the demuxer.
    pub fn push(&mut self, ps: &[u8], es: &mut Vec<u8>) -> usize {
        let mut pos = 0usize;
        while let Some(rest) = ps.get(pos..).filter(|r| !r.is_empty()) {
            match self.unit(rest, es) {
                Step::Consumed(n) => pos += n,
                Step::NeedMore => break,
                Step::Invalid => pos += 1 + resync(&rest[1..]),
            }
        }
        pos
    }

    /// Handles the unit at the start of `b` (which is not empty).
    fn unit(&mut self, b: &[u8], es: &mut Vec<u8>) -> Step {
        if b.len() < 4 {
            return if is_start_code_prefix(b) {
                Step::NeedMore
            } else {
                Step::Invalid
            };
        }
        if b[..3] != [0, 0, 1] {
            return Step::Invalid;
        }
        match b[3] {
            PACK_START => pack_header(b),
            PROGRAM_END => Step::Consumed(4),
            SYSTEM_HEADER => {
                let Some(len) = length_field(b) else {
                    return Step::NeedMore;
                };
                // Six fixed bytes (rate bound, bounds and flags) are mandatory.
                if len < 6 {
                    return Step::Invalid;
                }
                complete(b, 6 + len)
            }
            id @ FIRST_PACKET_ID..=0xFF => {
                let Some(len) = length_field(b) else {
                    return Step::NeedMore;
                };
                // A zero length ("unbounded") is only allowed for video in a
                // transport stream; in a program stream it is corruption.
                if len == 0 {
                    return Step::Invalid;
                }
                let total = 6 + len;
                let followed = self.stream_id.is_none_or(|s| s == id);
                if (0xE0..=0xEF).contains(&id) && followed {
                    self.video_packet(id, b, total, es)
                } else {
                    complete(b, total)
                }
            }
            _ => Step::Invalid,
        }
    }

    /// A PES packet of the followed (or first) video stream; `total` is the
    /// whole packet length.
    fn video_packet(&mut self, id: u8, b: &[u8], total: usize, es: &mut Vec<u8>) -> Step {
        let avail = &b[6..b.len().min(total)];
        let (header_len, scrambled) = match pes_header(avail) {
            PesHeader::Valid { len, scrambled } => (len, scrambled),
            PesHeader::Short if b.len() < total => return Step::NeedMore,
            PesHeader::Short | PesHeader::Invalid => return Step::Invalid,
        };
        if b.len() < total {
            return Step::NeedMore;
        }
        self.stream_id = Some(id);
        if scrambled {
            self.scrambled_packets = self.scrambled_packets.saturating_add(1);
        } else {
            es.extend_from_slice(&b[6 + header_len..total]);
            self.video_packets = self.video_packets.saturating_add(1);
        }
        Step::Consumed(total)
    }
}

/// Whether `b` (shorter than 4 bytes) could still grow into `00 00 01 xx`.
fn is_start_code_prefix(b: &[u8]) -> bool {
    [0u8, 0, 1].starts_with(b)
}

/// Whether `code` can follow `00 00 01` at the program stream level.
fn is_system_code(code: u8) -> bool {
    code >= PROGRAM_END
}

/// Offset in `b` of the next `00 00 01` followed by a program stream code.
/// Without one, returns how much of `b` can be dropped: everything except a
/// trailing partial start code, which may be completed by the next chunk.
fn resync(b: &[u8]) -> usize {
    let mut i = 0usize;
    while let Some(w) = b.get(i..i + 3) {
        if w[2] > 1 {
            // No start code can begin at i, i + 1 or i + 2: each needs this
            // byte to be 0 (or 1 as the last byte of the prefix at i).
            i += 3;
        } else if w == [0, 0, 1] {
            match b.get(i + 3) {
                Some(&code) if is_system_code(code) => return i,
                Some(_) => i += 3,
                None => return i,
            }
        } else {
            i += 1;
        }
    }
    // Fewer than three bytes left from `i`: keep them if they can begin a start code.
    let tail = &b[i.min(b.len())..];
    let keep = (0..=tail.len())
        .find(|&k| is_start_code_prefix(&tail[k..]))
        .unwrap_or(tail.len());
    i + keep
}

/// The 16-bit length after a 4-byte start code, if available.
fn length_field(b: &[u8]) -> Option<usize> {
    let s = b.get(4..6)?;
    Some(u16::from_be_bytes([s[0], s[1]]) as usize)
}

/// A unit of `total` bytes that needs no further checks.
fn complete(b: &[u8], total: usize) -> Step {
    if b.len() < total {
        Step::NeedMore
    } else {
        Step::Consumed(total)
    }
}

/// Validates a pack header (MPEG-2: 14 bytes plus up to 7 stuffing bytes;
/// MPEG-1: 12 bytes) by its marker bits, so a stray `00 00 01 BA` in garbage
/// is not taken for one.
fn pack_header(b: &[u8]) -> Step {
    let Some(&kind) = b.get(4) else {
        return Step::NeedMore;
    };
    if kind & 0xC0 == 0x40 {
        let Some(h) = b.get(..14) else {
            return Step::NeedMore;
        };
        let markers = h[4] & 0x04 != 0
            && h[6] & 0x04 != 0
            && h[8] & 0x04 != 0
            && h[9] & 0x01 != 0
            && h[12] & 0x03 == 0x03;
        if !markers {
            return Step::Invalid;
        }
        complete(b, 14 + (h[13] & 7) as usize)
    } else if kind & 0xF0 == 0x20 {
        let Some(h) = b.get(..12) else {
            return Step::NeedMore;
        };
        let markers = h[4] & 0x01 != 0
            && h[6] & 0x01 != 0
            && h[8] & 0x01 != 0
            && h[9] & 0x80 != 0
            && h[11] & 0x01 != 0;
        if markers {
            Step::Consumed(12)
        } else {
            Step::Invalid
        }
    } else {
        Step::Invalid
    }
}

/// Parses the header that follows `PES_packet_length` in a video packet.
/// `p` holds the available bytes of the packet body (never more than the
/// body), so a header running past it is `Short` while the packet is
/// incomplete and invalid once it is complete.
fn pes_header(p: &[u8]) -> PesHeader {
    let Some(&first) = p.first() else {
        return PesHeader::Short;
    };
    if first & 0xC0 == 0x80 {
        // MPEG-2: '10', PES_scrambling_control, flags; flags; PES_header_data_length.
        if p.len() < 3 {
            return PesHeader::Short;
        }
        let (flags, data_len) = (p[1], p[2]);
        // PTS_DTS_flags '01' is forbidden.
        if flags & 0xC0 == 0x40 {
            return PesHeader::Invalid;
        }
        let len = 3 + data_len as usize;
        if p.len() < len {
            return PesHeader::Short;
        }
        return PesHeader::Valid {
            len,
            scrambled: first & 0x30 != 0,
        };
    }
    // MPEG-1: up to 16 stuffing bytes, an optional STD buffer field ('01'),
    // then PTS ('0010'), PTS + DTS ('0011') or the 0x0F marker.
    let mut i = 0usize;
    while p.get(i) == Some(&0xFF) {
        i += 1;
        if i > 16 {
            return PesHeader::Invalid;
        }
    }
    let Some(&b) = p.get(i) else {
        return PesHeader::Short;
    };
    if b & 0xC0 == 0x40 {
        i += 2;
    }
    let Some(&b) = p.get(i) else {
        return PesHeader::Short;
    };
    let field = match b >> 4 {
        _ if b == 0x0F => 1,
        0x2 => 5,
        0x3 => 10,
        _ => return PesHeader::Invalid,
    };
    let len = i + field;
    if p.len() < len {
        return PesHeader::Short;
    }
    PesHeader::Valid {
        len,
        scrambled: false,
    }
}

/// Whether `data` starts with an MPEG program stream pack header (`00 00 01 BA`).
pub fn is_program_stream(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1, 0xBA])
}

#[cfg(test)]
mod tests {
    use super::resync;

    #[test]
    fn resync_finds_codes_and_keeps_partial_tails() {
        assert_eq!(resync(b"abc\0\0\x01\xBAxyz"), 3);
        // A video start code is not a program stream code.
        assert_eq!(resync(b"\0\0\x01\xB3\0\0\x01\xE0"), 4);
        assert_eq!(resync(b"abcdef"), 6);
        assert_eq!(resync(b"abc\0"), 3);
        assert_eq!(resync(b"abc\0\0"), 3);
        assert_eq!(resync(b"abc\0\0\x01"), 3);
        assert_eq!(resync(b"abc\x01"), 4);
        assert_eq!(resync(b"\0\0\0\x01\xBA"), 1);
        assert_eq!(resync(b""), 0);
    }
}
