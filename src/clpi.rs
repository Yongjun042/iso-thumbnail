//! Blu-ray clip information files (`BDMV/CLIPINF/xxxxx.clpi`).
//!
//! A clip information file describes one `BDMV/STREAM/xxxxx.m2ts` transport
//! stream. Two parts matter for a thumbnail:
//! - ProgramInfo names the primary video stream: its PID, coding type (MPEG-2,
//!   H.264, HEVC or VC-1), video format (1080i, 2160p, ...) and dynamic range.
//! - The EP_map (in CPI) lists every entry point of that stream: the source
//!   packet number (SPN) where an I-picture starts, and a coarse bound on that
//!   picture's size (`I_end_position_offset`). Reading at `SPN * 192` gives one
//!   key frame without scanning the stream.
//!
//! Layouts follow libbluray (`clpi_parse.c`) and were checked against real
//! discs. All fields are big-endian; every offset and count is checked against
//! the file, which comes from an untrusted image.

use crate::error::{Error, Result};

/// Largest clip information file read. Real ones are under 100 KB; the EP_map
/// fine table alone may legally reach about 1 MiB.
pub const MAX_CLPI_BYTES: usize = 4 << 20;

/// Stream coding types (`stream_coding_type`).
pub const CODING_MPEG1: u8 = 0x01;
pub const CODING_MPEG2: u8 = 0x02;
pub const CODING_H264: u8 = 0x1B;
pub const CODING_HEVC: u8 = 0x24;
pub const CODING_VC1: u8 = 0xEA;

/// `video_format` of 2160p (UHD) streams.
pub const FORMAT_2160P: u8 = 8;
/// `application_type` of a stereoscopic dependent-view clip: it has no
/// program info or EP_map of its own and cannot be decoded alone.
pub const APP_DEPENDENT_VIEW: u8 = 8;
/// `dynamic_range_type` values of HEVC streams.
pub const DYNAMIC_RANGE_SDR: u8 = 0;
pub const DYNAMIC_RANGE_HDR10: u8 = 1;
pub const DYNAMIC_RANGE_DOLBY_VISION: u8 = 2;

/// The clip's primary video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStream {
    pub pid: u16,
    /// `stream_coding_type` (`CODING_*`).
    pub coding: u8,
    /// `video_format`: 1 480i, 2 576i, 3 480p, 4 1080i, 5 720p, 6 1080p,
    /// 7 576p, 8 2160p.
    pub format: u8,
    /// `aspect_ratio`: 2 = 4:3, 3 = 16:9.
    pub aspect: u8,
    /// `dynamic_range_type` (HEVC only, else 0).
    pub dynamic_range: u8,
    /// `color_space` (HEVC only): 1 = BT.709, 2 = BT.2020.
    pub color_space: u8,
}

/// One entry point of the video stream: an I-picture starts at source packet
/// `spn` of the clip's `.m2ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryPoint {
    pub spn: u32,
    /// `I_end_position_offset`: a coarse bound on the I-picture's size (see
    /// `i_picture_bound`); 0 when not set.
    pub i_end: u8,
    /// Presentation time of the I-picture in 45 kHz units (the unit of
    /// playlist times), as the EP_map gives it: to 256 units (5.7 ms).
    pub pts: u32,
}

#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub application_type: u8,
    /// Size of the `.m2ts` in 192-byte source packets.
    pub source_packets: u32,
    pub video: VideoStream,
    /// Entry points of the video stream, in stream order (SPN strictly increasing).
    pub entries: Vec<EntryPoint>,
}

fn be16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o.checked_add(2).ok_or(Error::Corrupt("clpi offset"))?)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or(Error::Corrupt("clpi truncated"))
}

fn be32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o.checked_add(4).ok_or(Error::Corrupt("clpi offset"))?)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(Error::Corrupt("clpi truncated"))
}

fn be64(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o.checked_add(8).ok_or(Error::Corrupt("clpi offset"))?)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or(Error::Corrupt("clpi truncated"))
}

fn byte(d: &[u8], o: usize) -> Result<u8> {
    d.get(o).copied().ok_or(Error::Corrupt("clpi truncated"))
}

fn at(base: usize, rel: u64) -> Result<usize> {
    usize::try_from(rel)
        .ok()
        .and_then(|r| base.checked_add(r))
        .ok_or(Error::Corrupt("clpi offset"))
}

fn is_video(coding: u8) -> bool {
    matches!(
        coding,
        CODING_MPEG1 | CODING_MPEG2 | CODING_H264 | CODING_HEVC | CODING_VC1
    )
}

/// Upper bound in bytes, from the entry point's first source packet, of the
/// span holding its I-picture, for an `I_end_position_offset` code; `None`
/// when the code gives no bound (0 = unset, 7 = open-ended).
pub fn i_picture_bound(code: u8, uhd: bool) -> Option<u64> {
    const HD: [u64; 6] = [131_072, 262_144, 393_216, 589_824, 917_504, 1_310_720];
    const UHD: [u64; 6] = [
        786_432, 1_572_864, 2_359_296, 3_145_728, 3_932_160, 4_718_592,
    ];
    let table = if uhd { &UHD } else { &HD };
    match code {
        1..=6 => Some(table[usize::from(code - 1)]),
        _ => None,
    }
}

/// Finds the first video stream of the first program sequence.
fn parse_program_info(d: &[u8], pi: usize) -> Result<VideoStream> {
    let num_sequences = byte(d, at(pi, 5)?)?;
    if num_sequences == 0 {
        return Err(Error::NotFound);
    }
    // First program sequence: SPN start (4), PMT PID (2), stream count (1),
    // group count (1), then the streams.
    let seq = at(pi, 6)?;
    let num_streams = byte(d, at(seq, 6)?)?;
    let mut o = at(seq, 8)?;
    for _ in 0..num_streams {
        let pid = be16(d, o)?;
        let len = usize::from(byte(d, at(o, 2)?)?);
        let info = at(o, 3)?;
        let coding = byte(d, info)?;
        if is_video(coding) && len >= 3 {
            let format_rate = byte(d, at(info, 1)?)?;
            let aspect = byte(d, at(info, 2)?)? >> 4;
            let (dynamic_range, color_space) = if coding == CODING_HEVC && len >= 4 {
                // After aspect 4 | reserved 2 | oc_flag 1 | cr_flag 1 comes
                // dynamic_range_type 4 | color_space 4 (libbluray _parse_stream_attr).
                let v = byte(d, at(info, 3)?)?;
                (v >> 4, v & 0x0F)
            } else {
                (DYNAMIC_RANGE_SDR, 0)
            };
            return Ok(VideoStream {
                pid,
                coding,
                format: format_rate >> 4,
                aspect,
                dynamic_range,
                color_space,
            });
        }
        o = at(o, 3 + len as u64)?;
    }
    Err(Error::NotFound)
}

/// Reads the EP_map entries of `pid` (or of the first stream listed when
/// `pid` has none).
fn parse_ep_map(d: &[u8], cpi: usize, pid: u16, source_packets: u32) -> Result<Vec<EntryPoint>> {
    if be32(d, cpi)? == 0 {
        return Err(Error::NotFound);
    }
    if be16(d, at(cpi, 4)?)? & 0x0F != 1 {
        return Err(Error::Unsupported("CPI type"));
    }
    let ep_map = at(cpi, 6)?;
    let num_pids = byte(d, at(ep_map, 1)?)?;
    let mut chosen = None;
    for k in 0..u64::from(num_pids) {
        let e = at(ep_map, 2 + 12 * k)?;
        let stream_pid = be16(d, e)?;
        // reserved 10 | EP_stream_type 4 | coarse 16 | fine 18, then the start address.
        let v = be64(d, at(e, 2)?)? >> 16;
        let num_coarse = ((v >> 18) & 0xFFFF) as usize;
        let num_fine = (v & 0x3FFFF) as usize;
        let start = be32(d, at(e, 8)?)?;
        if chosen.is_none() || stream_pid == pid {
            chosen = Some((num_coarse, num_fine, start));
        }
        if stream_pid == pid {
            break;
        }
    }
    let (num_coarse, num_fine, start) = chosen.ok_or(Error::NotFound)?;
    if num_coarse == 0 || num_fine == 0 {
        return Err(Error::NotFound);
    }
    let base = at(ep_map, u64::from(start))?;
    let fine_table = at(base, u64::from(be32(d, base)?))?;
    let coarse_table = at(base, 4)?;
    // Both tables must lie inside the file before anything is allocated.
    let coarse_end = coarse_table
        .checked_add(num_coarse.checked_mul(8).ok_or(Error::TooLarge)?)
        .ok_or(Error::TooLarge)?;
    let fine_end = fine_table
        .checked_add(num_fine.checked_mul(4).ok_or(Error::TooLarge)?)
        .ok_or(Error::TooLarge)?;
    if coarse_end > d.len() || fine_end > d.len() {
        return Err(Error::Corrupt("EP_map outside clpi"));
    }
    let mut entries = Vec::with_capacity(num_fine);
    let mut last_spn = None;
    for i in 0..num_coarse {
        let c = be64(d, coarse_table + 8 * i)?;
        // ref_to_EP_fine_id 18 | PTS_EP_coarse 14 | SPN_EP_coarse 32.
        let first = (c >> 46) as usize;
        let coarse_pts = ((c >> 32) & 0x3FFF) as u32;
        let coarse_spn = c as u32;
        let end = if i + 1 < num_coarse {
            (be64(d, coarse_table + 8 * (i + 1))? >> 46) as usize
        } else {
            num_fine
        };
        if first > end || end > num_fine {
            return Err(Error::Corrupt("EP_map coarse references"));
        }
        for j in first..end {
            // is_angle_change_point 1 | I_end_position_offset 3 |
            // PTS_EP_fine 11 | SPN_EP_fine 17.
            let f = be32(d, fine_table + 4 * j)?;
            let i_end = ((f >> 28) & 7) as u8;
            // In 45 kHz units the coarse part holds bits 31..18 and the fine
            // part bits 18..8; they share bit 18 (libbluray clpi_lookup_spn).
            let pts = ((coarse_pts & !1) << 18) + (((f >> 17) & 0x7FF) << 8);
            let spn = (coarse_spn & !0x1FFFF).checked_add(f & 0x1FFFF);
            let Some(spn) = spn else { continue };
            // Keep the map strictly increasing and inside the clip.
            if spn >= source_packets || last_spn.is_some_and(|l| spn <= l) {
                continue;
            }
            last_spn = Some(spn);
            entries.push(EntryPoint { spn, i_end, pts });
        }
    }
    if entries.is_empty() {
        return Err(Error::NotFound);
    }
    Ok(entries)
}

/// Parses a clip information file.
pub fn parse(d: &[u8]) -> Result<ClipInfo> {
    if d.get(..4) != Some(b"HDMV") {
        return Err(Error::Corrupt("not a clip information file"));
    }
    match d.get(4..8) {
        Some(b"0100") | Some(b"0200") | Some(b"0240") | Some(b"0300") => {}
        _ => return Err(Error::Unsupported("clip information version")),
    }
    let program_info = at(0, u64::from(be32(d, 12)?))?;
    let cpi = at(0, u64::from(be32(d, 16)?))?;
    // ClipInfo sits at a fixed offset after the 40-byte header.
    let application_type = byte(d, 47)?;
    let source_packets = be32(d, 56)?;
    let video = parse_program_info(d, program_info)?;
    let entries = parse_ep_map(d, cpi, video.pid, source_packets)?;
    Ok(ClipInfo {
        application_type,
        source_packets,
        video,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal clip information file with one video stream and the
    /// given (SPN, I_end) entry points, split into coarse groups of `group`.
    pub(crate) fn build(coding: u8, format: u8, points: &[(u32, u8)], group: usize) -> Vec<u8> {
        let timed: Vec<(u32, u8, u32)> = points.iter().map(|&(s, i)| (s, i, 0)).collect();
        build_timed(coding, format, &timed, group)
    }

    /// As `build`, with the presentation time (45 kHz, a multiple of 256) of
    /// each entry point. As on real discs, PTS_EP_coarse is the time's bits
    /// 31..18, so its last bit repeats the first of PTS_EP_fine.
    pub(crate) fn build_timed(
        coding: u8,
        format: u8,
        points: &[(u32, u8, u32)],
        group: usize,
    ) -> Vec<u8> {
        let mut d = vec![0u8; 40];
        d[..8].copy_from_slice(b"HDMV0200");
        // ClipInfo at 40.
        let mut clip_info = vec![0u8; 4 + 148];
        clip_info[6] = 1; // Clip_stream_type
        clip_info[7] = 1; // application_type: main TS
        let packets = points.last().map_or(0, |p| p.0) + 1000;
        clip_info[16..20].copy_from_slice(&packets.to_be_bytes());
        let len = (clip_info.len() - 4) as u32;
        clip_info[..4].copy_from_slice(&len.to_be_bytes());
        d.extend(clip_info);
        // ProgramInfo.
        let pi = d.len() as u32;
        let mut prog = vec![0, 0, 0, 0, 0, 1];
        prog.extend(0u32.to_be_bytes()); // SPN_program_sequence_start
        prog.extend(0x0100u16.to_be_bytes()); // PMT PID
        prog.extend([2, 0]); // streams, groups
                             // An audio stream first, then the video stream.
        prog.extend(0x1100u16.to_be_bytes());
        prog.extend([5, 0x80, 0x31, b'e', b'n', b'g']);
        prog.extend(0x1011u16.to_be_bytes());
        prog.extend([21, coding, (format << 4) | 1, 0x30]);
        prog.extend([0u8; 18]);
        let plen = (prog.len() - 4) as u32;
        prog[..4].copy_from_slice(&plen.to_be_bytes());
        d.extend(prog);
        // CPI / EP_map.
        let cpi = d.len() as u32;
        let coarse: Vec<usize> = (0..points.len()).step_by(group.max(1)).collect();
        let mut ep = vec![0u8, 1]; // reserved, one stream PID entry
        let mut entry = 0x1011u16.to_be_bytes().to_vec();
        let v: u64 = (1u64 << 34) | ((coarse.len() as u64) << 18) | points.len() as u64;
        entry.extend(&(v << 16).to_be_bytes()[..6]);
        let start = (ep.len() + 12) as u32;
        entry.extend(start.to_be_bytes());
        ep.extend(entry);
        let fine_start = (4 + 8 * coarse.len()) as u32;
        ep.extend(fine_start.to_be_bytes());
        for &first in &coarse {
            let (spn, _, pts) = points[first];
            let pts_coarse = u64::from(pts >> 18);
            let c: u64 = ((first as u64) << 46) | (pts_coarse << 32) | u64::from(spn);
            ep.extend(c.to_be_bytes());
        }
        for &(spn, i_end, pts) in points {
            let pts_fine = (pts >> 8) & 0x7FF;
            let f: u32 = (u32::from(i_end) << 28) | (pts_fine << 17) | (spn & 0x1FFFF);
            ep.extend(f.to_be_bytes());
        }
        let mut cpi_block = vec![0u8; 6];
        cpi_block[5] = 1; // CPI_type: EP_map
        cpi_block.extend(ep);
        let clen = (cpi_block.len() - 4) as u32;
        cpi_block[..4].copy_from_slice(&clen.to_be_bytes());
        d.extend(cpi_block);
        d[12..16].copy_from_slice(&pi.to_be_bytes());
        d[16..20].copy_from_slice(&cpi.to_be_bytes());
        d
    }

    #[test]
    fn parses_video_stream_and_entry_points() {
        let points = [(0, 3), (500, 2), (140_000, 5), (140_100, 1), (300_000, 4)];
        let info = parse(&build(CODING_H264, 6, &points, 2)).unwrap();
        assert_eq!(info.application_type, 1);
        assert_eq!(info.video.pid, 0x1011);
        assert_eq!(info.video.coding, CODING_H264);
        assert_eq!(info.video.format, 6);
        let got: Vec<(u32, u8)> = info.entries.iter().map(|e| (e.spn, e.i_end)).collect();
        assert_eq!(got, points);
    }

    #[test]
    fn entry_points_carry_their_presentation_times() {
        // Times on both sides of a coarse boundary (bit 18 is shared by the
        // coarse and the fine part), and past 2^31 / 45 kHz = 13 hours.
        let points = [
            (0, 1, 0),
            (100, 1, 0x3_FF00),
            (200, 1, 0x4_0000),
            (300, 1, 0x4_0100),
            (400, 1, 0x1234_5600),
            (500, 1, 0xFFFF_FF00),
        ];
        // A coarse group shares bits 31..19, as on real discs: the first four
        // times fit in one group, the others each need their own.
        for (group, points) in [(1, &points[..]), (4, &points[..4])] {
            let info = parse(&build_timed(CODING_H264, 6, points, group)).unwrap();
            let got: Vec<u32> = info.entries.iter().map(|e| e.pts).collect();
            let want: Vec<u32> = points.iter().map(|p| p.2).collect();
            assert_eq!(got, want, "group {group}");
        }
    }

    #[test]
    fn i_picture_bounds_follow_the_tables() {
        assert_eq!(i_picture_bound(1, false), Some(131_072));
        assert_eq!(i_picture_bound(6, false), Some(1_310_720));
        assert_eq!(i_picture_bound(2, true), Some(1_572_864));
        assert_eq!(i_picture_bound(0, false), None);
        assert_eq!(i_picture_bound(7, true), None);
    }

    #[test]
    fn rejects_wrong_magic_and_versions() {
        let mut d = build(CODING_H264, 6, &[(0, 1)], 1);
        d[4..8].copy_from_slice(b"0999");
        assert!(parse(&d).is_err());
        assert!(parse(b"MPLS0200").is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn skips_non_increasing_and_out_of_clip_entries() {
        let mut d = build(CODING_H264, 6, &[(10, 1), (5, 1), (20, 1)], 3);
        let info = parse(&d).unwrap();
        let spns: Vec<u32> = info.entries.iter().map(|e| e.spn).collect();
        assert_eq!(spns, [10, 20]);
        // Shrink the clip so that every entry lies outside it.
        d[56..60].copy_from_slice(&3u32.to_be_bytes());
        assert!(parse(&d).is_err());
    }

    #[test]
    fn corrupted_files_never_panic() {
        let base = build(CODING_HEVC, 8, &[(0, 1), (9000, 2), (200_000, 3)], 2);
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..4000 {
            let mut d = base.clone();
            for _ in 0..1 + (seed % 6) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % d.len();
                d[i] = (seed >> 32) as u8;
            }
            let _ = parse(&d);
            let cut = (seed as usize >> 8) % d.len();
            let _ = parse(&d[..cut]);
        }
    }
}
