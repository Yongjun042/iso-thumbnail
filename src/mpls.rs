//! Blu-ray playlists (`BDMV/PLAYLIST/xxxxx.mpls`): what a playlist starts
//! with, enough to show the picture a menu playlist puts on screen.
//!
//! A playlist is a sequence of play items, each a part of one clip (from its
//! IN time to its OUT time, 45 kHz units) with a table of the streams that may
//! be selected while it plays (STN_table). Only the play item playback starts
//! with is read: its clip, times and first interactive graphics stream (the
//! menu's buttons). Playback starts at the first play item, or where the
//! navigation command says: at a play item (`PlayPLatPlayItem`) or a mark
//! (`PlayPLatMark`); like libbluray, a start that does not exist is ignored.
//!
//! Layouts follow libbluray (`mpls_parse.c`). All fields are big-endian; every
//! offset and length is checked against the file, which comes from an
//! untrusted image.

use crate::error::{Error, Result};

/// Largest playlist file read. Real ones are a few KB; a playlist of a
/// thousand play items with full stream tables stays under 1 MiB.
pub const MAX_MPLS_BYTES: usize = 1 << 20;

/// Where playback of a playlist starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    First,
    /// At play item n.
    Item(u32),
    /// At mark n of the PlayListMark table.
    Mark(u32),
}

/// `stream_coding_type` of an interactive graphics stream.
const CODING_IG: u8 = 0x91;
/// `stream_type` of a stream multiplexed into the play item's own clip.
const STREAM_IN_MAIN_CLIP: u8 = 1;

/// The first play item of a playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayItem {
    /// `xxxxx` of the clip's `.m2ts` and `.clpi` (five ASCII digits).
    pub clip: String,
    /// IN and OUT time in 45 kHz units.
    pub in_time: u32,
    pub out_time: u32,
    /// Whether the STN_table lists an interactive graphics stream (menu
    /// buttons), in the clip or in a sub-path.
    pub interactive: bool,
    /// PID of the first interactive graphics stream multiplexed into the clip.
    pub ig_pid: Option<u16>,
}

fn be16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o.checked_add(2).ok_or(Error::Corrupt("mpls offset"))?)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or(Error::Corrupt("mpls truncated"))
}

fn be32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o.checked_add(4).ok_or(Error::Corrupt("mpls offset"))?)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(Error::Corrupt("mpls truncated"))
}

fn byte(d: &[u8], o: usize) -> Result<u8> {
    d.get(o).copied().ok_or(Error::Corrupt("mpls truncated"))
}

fn at(base: usize, rel: usize) -> Result<usize> {
    base.checked_add(rel).ok_or(Error::Corrupt("mpls offset"))
}

/// Whether `d` starts with the header of a BDMV file of kind `magic` in one
/// of the versions players accept.
pub(crate) fn has_header(d: &[u8], magic: &[u8; 4]) -> bool {
    d.get(..4) == Some(magic)
        && matches!(
            d.get(4..8),
            Some(b"0100") | Some(b"0200") | Some(b"0240") | Some(b"0300")
        )
}

/// Whether the STN_table at `stn` (its 16-bit length first), which must end
/// by `end`, lists interactive graphics streams, and the PID of the first one
/// multiplexed into the main clip.
fn interactive_graphics(d: &[u8], stn: usize, end: usize) -> Result<(bool, Option<u16>)> {
    let len = usize::from(be16(d, stn)?);
    let stn_end = at(at(stn, 2)?, len)?;
    if stn_end > end {
        return Err(Error::Corrupt("mpls STN_table length"));
    }
    let d = &d[..stn_end];
    // reserved 16, then the stream counts: video, audio, PG, IG, secondary
    // audio, secondary video, PiP PG, Dolby Vision; 32 bits reserved.
    let count = |i: usize| -> Result<usize> { Ok(usize::from(byte(d, at(stn, 4 + i)?)?)) };
    let (video, audio, pg, ig, pip_pg) = (count(0)?, count(1)?, count(2)?, count(3)?, count(6)?);
    let mut o = at(stn, 16)?;
    // Each stream: stream_entry and stream_attributes, both led by their
    // 8-bit length.
    let skip = |o: usize| -> Result<usize> {
        let entry = at(at(o, 1)?, usize::from(byte(d, o)?))?;
        at(at(entry, 1)?, usize::from(byte(d, entry)?))
    };
    for _ in 0..video + audio + pg + pip_pg {
        o = skip(o)?;
    }
    for _ in 0..ig {
        let entry_len = byte(d, o)?;
        let stream_type = byte(d, at(o, 1)?)?;
        let attributes = at(at(o, 1)?, usize::from(entry_len))?;
        let coding = byte(d, at(attributes, 1)?)?;
        if stream_type == STREAM_IN_MAIN_CLIP && entry_len >= 3 && coding == CODING_IG {
            return Ok((true, Some(be16(d, at(o, 2)?)?)));
        }
        o = skip(o)?;
    }
    Ok((ig > 0, None))
}

/// The play item and time a mark points at: (ref_to_PlayItem_id,
/// mark_time_stamp), if the mark exists.
fn mark(d: &[u8], n: u32) -> Option<(u32, u32)> {
    // PlayListMark: length 32, number_of_PlayList_marks 16, then 14 bytes
    // per mark: reserved 8, mark_type 8, ref_to_PlayItem_id 16,
    // mark_time_stamp 32, entry_ES_PID 16, duration 32.
    let marks = usize::try_from(be32(d, 12).ok()?).ok()?;
    if n >= u32::from(be16(d, at(marks, 4).ok()?).ok()?) {
        return None;
    }
    let m = at(marks, 6 + 14 * n as usize).ok()?;
    Some((
        u32::from(be16(d, at(m, 2).ok()?).ok()?),
        be32(d, at(m, 4).ok()?).ok()?,
    ))
}

/// Parses the play item playback of a playlist begins with, from `start`.
pub fn play_item(d: &[u8], start: Start) -> Result<PlayItem> {
    if !has_header(d, b"MPLS") {
        return Err(Error::Corrupt("not a playlist"));
    }
    let list = usize::try_from(be32(d, 8)?).map_err(|_| Error::Corrupt("mpls offset"))?;
    // PlayList: length 32, reserved 16, number_of_PlayItems 16,
    // number_of_SubPaths 16, then the play items.
    let items = u32::from(be16(d, at(list, 6)?)?);
    if items == 0 {
        return Err(Error::NotFound);
    }
    let (index, time) = match start {
        Start::First => (0, None),
        Start::Item(n) => (n, None),
        Start::Mark(n) => mark(d, n).map_or((0, None), |(i, t)| (i, Some(t))),
    };
    let (index, time) = if index < items {
        (index, time)
    } else {
        (0, None)
    };
    let mut item = at(list, 10)?;
    for _ in 0..index {
        item = at(at(item, 2)?, usize::from(be16(d, item)?))?;
    }
    let len = usize::from(be16(d, item)?);
    let body = at(item, 2)?;
    let end = at(body, len)?;
    if len < 18 || end > d.len() {
        return Err(Error::Corrupt("mpls play item length"));
    }
    let clip = &d[body..body + 5];
    if !clip.iter().all(u8::is_ascii_digit) {
        return Err(Error::Corrupt("mpls clip name"));
    }
    // Clip_codec_identifier 4 bytes, reserved 11 | is_multi_angle 1 |
    // connection_condition 4, ref_to_STC_id 8, IN_time 32, OUT_time 32,
    // UO_mask_table 64, random access 1 | reserved 7, still_mode 8,
    // still_time 16.
    let multi_angle = byte(d, at(body, 10)?)? & 0x10 != 0;
    let mut in_time = be32(d, at(body, 12)?)?;
    let out_time = be32(d, at(body, 16)?)?;
    // Playback from a mark starts at its time.
    if let Some(t) = time.filter(|t| (in_time..out_time).contains(t)) {
        in_time = t;
    }
    let mut stn = at(body, 32)?;
    if multi_angle {
        // number_of_angles 8, flags 8, then each further angle's clip name
        // (5), codec identifier (4) and STC id (1).
        let angles = usize::from(byte(d, stn)?);
        stn = at(stn, 2 + 10 * angles.saturating_sub(1))?;
    }
    // Every play item has an STN_table; an unreadable one only costs the
    // buttons.
    let (interactive, ig_pid) = interactive_graphics(d, stn, end).unwrap_or((false, None));
    Ok(PlayItem {
        clip: String::from_utf8_lossy(clip).into_owned(),
        in_time,
        out_time,
        interactive,
        ig_pid,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One stream of an STN_table: an entry in the main clip with `pid`, and
    /// attributes of `coding`.
    fn stream(pid: u16, coding: u8) -> Vec<u8> {
        let mut s = vec![9, STREAM_IN_MAIN_CLIP];
        s.extend(pid.to_be_bytes());
        s.extend([0u8; 6]);
        s.extend([5, coding, b'e', b'n', b'g', 0]);
        s
    }

    /// The same in a sub-path's clip (stream_type 3: sub-path id, PID).
    fn sub_path_stream(pid: u16, coding: u8) -> Vec<u8> {
        let mut s = vec![9, 3, 0];
        s.extend(pid.to_be_bytes());
        s.extend([0u8; 5]);
        s.extend([5, coding, b'e', b'n', b'g', 0]);
        s
    }

    /// A playlist of one play item of `clip` with a video, an audio, a PG
    /// and, when given, an IG stream; `angles` > 1 makes it multi-angle.
    pub(crate) fn build(
        clip: &str,
        in_time: u32,
        out_time: u32,
        ig: Option<u16>,
        angles: u8,
    ) -> Vec<u8> {
        playlist(&[item(clip, in_time, out_time, ig, angles)], &[])
    }

    /// A playlist of the given play items (each with its 16-bit length) and
    /// marks of (play item, time).
    fn playlist(items: &[Vec<u8>], marks: &[(u16, u32)]) -> Vec<u8> {
        let mut d = b"MPLS0200".to_vec();
        d.extend(58u32.to_be_bytes()); // PlayList_start_address
        d.extend([0u8; 8]); // PlayListMark, ExtensionData
        d.extend([0u8; 20]);
        // AppInfoPlayList: its length (14) and fields.
        d.extend([0u8; 18]);
        d[40..44].copy_from_slice(&14u32.to_be_bytes());
        let mut list = vec![0, 0];
        list.extend((items.len() as u16).to_be_bytes());
        list.extend([0, 0]); // no sub-paths
        for i in items {
            list.extend(i);
        }
        d.extend(((list.len()) as u32).to_be_bytes());
        d.extend(list);
        let at = d.len() as u32;
        d[12..16].copy_from_slice(&at.to_be_bytes());
        let mut m = (marks.len() as u16).to_be_bytes().to_vec();
        for &(item, time) in marks {
            m.extend([0, 1]); // entry mark
            m.extend(item.to_be_bytes());
            m.extend(time.to_be_bytes());
            m.extend([0xFF, 0xFF, 0, 0, 0, 0]);
        }
        d.extend((m.len() as u32).to_be_bytes());
        d.extend(m);
        d
    }

    /// One play item, led by its length.
    fn item(clip: &str, in_time: u32, out_time: u32, ig: Option<u16>, angles: u8) -> Vec<u8> {
        let mut item = clip.as_bytes().to_vec();
        item.extend(b"M2TS");
        item.extend([0, if angles > 1 { 0x11 } else { 0x01 }, 0]);
        item.extend(in_time.to_be_bytes());
        item.extend(out_time.to_be_bytes());
        item.extend([0u8; 8]);
        item.extend([0x80, 0, 0, 0]);
        if angles > 1 {
            item.extend([angles, 0]);
            for _ in 1..angles {
                item.extend(b"99999M2TS");
                item.push(0);
            }
        }
        let mut streams = Vec::new();
        streams.extend(stream(0x1011, 0x1B));
        streams.extend(stream(0x1100, 0x81));
        streams.extend(stream(0x1200, 0x90));
        let mut counts = [1u8, 1, 1, 0, 0, 0, 0, 0];
        if let Some(pid) = ig {
            // A PG-coded entry first, which must not be taken for the IG.
            streams.extend(stream(0x1401, 0x90));
            streams.extend(stream(pid, CODING_IG));
            counts[3] = 2;
        }
        let mut stn = vec![0u8, 0];
        stn.extend(counts);
        stn.extend([0u8; 4]);
        stn.extend(streams);
        item.extend((stn.len() as u16).to_be_bytes());
        item.extend(stn);
        let mut led = (item.len() as u16).to_be_bytes().to_vec();
        led.extend(item);
        led
    }

    #[test]
    fn reads_the_first_play_item() {
        let p = play_item(
            &build("00042", 90_000, 1_440_000, Some(0x1400), 1),
            Start::First,
        )
        .unwrap();
        assert_eq!(
            p,
            PlayItem {
                clip: "00042".into(),
                in_time: 90_000,
                out_time: 1_440_000,
                interactive: true,
                ig_pid: Some(0x1400),
            }
        );
        let p = play_item(&build("00007", 0, 45_000, None, 1), Start::First).unwrap();
        assert_eq!((p.interactive, p.ig_pid), (false, None));
    }

    #[test]
    fn playback_starts_at_the_play_item_or_mark_given() {
        let d = playlist(
            &[
                item("00010", 0, 9000, None, 1),
                item("00011", 1000, 9000, Some(0x1400), 2),
                item("00012", 0, 9000, None, 1),
            ],
            &[(1, 5000), (7, 0), (2, 99_999)],
        );
        let at = |start| {
            let p = play_item(&d, start).unwrap();
            (p.clip, p.in_time, p.ig_pid)
        };
        let first = ("00010".to_string(), 0, None);
        assert_eq!(at(Start::First), first);
        assert_eq!(at(Start::Item(1)), ("00011".into(), 1000, Some(0x1400)));
        assert_eq!(at(Start::Item(2)).0, "00012");
        // A mark starts at its time; a mark beyond the item's OUT keeps IN.
        assert_eq!(at(Start::Mark(0)), ("00011".into(), 5000, Some(0x1400)));
        assert_eq!(at(Start::Mark(2)), ("00012".into(), 0, None));
        // Starts that do not exist are ignored, as libbluray does.
        assert_eq!(at(Start::Item(3)), first);
        assert_eq!(at(Start::Mark(1)), first);
        assert_eq!(at(Start::Mark(3)), first);
        assert_eq!(at(Start::Item(u32::MAX)), first);
        assert_eq!(at(Start::Mark(u32::MAX)), first);
    }

    #[test]
    fn graphics_in_sub_paths_have_no_pid_here() {
        let d = build("00042", 0, 45_000, Some(0x1400), 1);
        // Turn the last stream (the IG in the main clip) into one in a
        // sub-path, of the same length.
        let main = stream(0x1400, CODING_IG);
        let at = d.windows(main.len()).rposition(|w| w == main).unwrap();
        let mut sub = d.clone();
        sub[at..at + main.len()].copy_from_slice(&sub_path_stream(0x1400, CODING_IG));
        let p = play_item(&sub, Start::First).unwrap();
        assert_eq!((p.interactive, p.ig_pid), (true, None));
    }

    #[test]
    fn multi_angle_items_skip_their_angles() {
        let p = play_item(&build("00003", 0, 45_000, Some(0x1402), 3), Start::First).unwrap();
        assert_eq!((p.clip.as_str(), p.ig_pid), ("00003", Some(0x1402)));
    }

    #[test]
    fn rejects_other_files() {
        let mut d = build("00001", 0, 1, None, 1);
        assert!(play_item(b"HDMV0200", Start::First).is_err());
        d[4..8].copy_from_slice(b"0999");
        assert!(play_item(&d, Start::First).is_err());
        let mut d = build("0000x", 0, 1, None, 1);
        assert!(play_item(&d, Start::First).is_err());
        d[..8].copy_from_slice(b"MPLS0300");
        assert!(play_item(&d, Start::First).is_err());
    }

    #[test]
    fn corrupted_files_never_panic() {
        let base = playlist(
            &[
                item("00041", 0, 9000, None, 1),
                item("00042", 0, 45_000, Some(0x1400), 2),
            ],
            &[(1, 100), (0, 5)],
        );
        let mut seed: u64 = 0x5851_F42D_4C95_7F2D;
        for _ in 0..4000 {
            let mut d = base.clone();
            for _ in 0..1 + (seed % 5) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % d.len();
                d[i] = (seed >> 32) as u8;
            }
            let cut = (seed as usize >> 8) % d.len();
            for start in [Start::First, Start::Item(1), Start::Mark(0), Start::Mark(1)] {
                let _ = play_item(&d, start);
                let _ = play_item(&d[..cut], start);
            }
        }
    }
}
