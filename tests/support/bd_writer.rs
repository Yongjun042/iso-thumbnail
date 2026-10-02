//! Writes Blu-ray navigation files and interactive graphics for tests:
//! `index.bdmv`, `MovieObject.bdmv`, BD-J objects, playlists, and the
//! segments of an interactive graphics display set. Layouts follow libbluray
//! (`index_parse.c`, `mobj_parse.c`, `bdjo_parse.c`, `mpls_parse.c`,
//! `ig_decode.c`, `pg_decode.c`), written independently of the parsers.

/// The object of the top menu or of a title.
#[derive(Clone, Copy)]
pub enum Top<'a> {
    Hdmv(u16),
    Bdj(&'a str),
    None,
}

fn object_entry(o: Top) -> Vec<u8> {
    let mut e = vec![0u8; 12];
    match o {
        Top::Hdmv(id) => {
            e[0] = 0x40; // object_type 1
            e[4] = 0x40; // HDMV playback type: interactive
            e[6..8].copy_from_slice(&id.to_be_bytes());
        }
        Top::Bdj(name) => {
            e[0] = 0x80; // object_type 2
            e[4] = 0xC0; // BD-J playback type: interactive
            e[6..11].copy_from_slice(name.as_bytes());
        }
        Top::None => {
            e[0] = 0x40;
            e[6..8].copy_from_slice(&0xFFFFu16.to_be_bytes());
        }
    }
    e
}

/// `index.bdmv`: first playback runs the first title's object.
pub fn index(top_menu: Top, titles: &[Top]) -> Vec<u8> {
    let mut d = b"INDX0200".to_vec();
    d.extend(78u32.to_be_bytes()); // Indexes_start_address
    d.extend(0u32.to_be_bytes()); // ExtensionData_start_address
    d.extend([0u8; 24]);
    d.extend(34u32.to_be_bytes()); // AppInfoBDMV length
    d.extend([0u8; 34]);
    let mut body = object_entry(*titles.first().unwrap_or(&Top::None));
    body.extend(object_entry(top_menu));
    body.extend((titles.len() as u16).to_be_bytes());
    for t in titles {
        body.extend(object_entry(*t));
    }
    d.extend((body.len() as u32).to_be_bytes());
    d.extend(body);
    d
}

/// Navigation commands (libbluray `mobj_parse_cmd`): operand count 3 |
/// group 2 | sub-group 3, immediate flags and branch option, compare option,
/// set option, destination and source.
pub mod cmd {
    #[allow(clippy::too_many_arguments)]
    fn command(
        ops: u8,
        group: u8,
        sub: u8,
        imm: u8,
        branch: u8,
        set: u8,
        dst: u32,
        src: u32,
    ) -> [u8; 12] {
        let mut c = [0u8; 12];
        c[0] = (ops << 5) | (group << 3) | sub;
        c[1] = (imm << 6) | branch;
        c[3] = set;
        c[4..8].copy_from_slice(&dst.to_be_bytes());
        c[8..12].copy_from_slice(&src.to_be_bytes());
        c
    }
    /// `PlayPL pl`.
    pub fn play_pl(pl: u32) -> [u8; 12] {
        command(1, 0, 2, 0b10, 0, 0, pl, 0)
    }
    /// `PlayPL GPR[gpr]`.
    pub fn play_pl_gpr(gpr: u32) -> [u8; 12] {
        command(1, 0, 2, 0b00, 0, 0, gpr, 0)
    }
    /// `PlayPLatPlayItem pl, item`.
    pub fn play_pl_at_item(pl: u32, item: u32) -> [u8; 12] {
        command(2, 0, 2, 0b11, 1, 0, pl, item)
    }
    /// `SetButtonPage` selecting `button`.
    pub fn select_button(button: u16) -> [u8; 12] {
        command(1, 2, 1, 0b10, 0, 3, 0x8000_0000 | u32::from(button), 0)
    }
    pub fn call_object(id: u32) -> [u8; 12] {
        command(1, 0, 1, 0b10, 2, 0, id, 0)
    }
    pub fn jump_title(title: u32) -> [u8; 12] {
        command(1, 0, 1, 0b10, 1, 0, title, 0)
    }
    /// `GPR[gpr] = value`.
    pub fn move_imm(gpr: u32, value: u32) -> [u8; 12] {
        command(2, 2, 0, 0b01, 0, 1, gpr, value)
    }
    /// `if GPR[gpr] == value` (skips the next command when false).
    pub fn equals(gpr: u32, value: u32) -> [u8; 12] {
        let mut c = command(2, 1, 0, 0b01, 0, 0, gpr, value);
        c[2] = 2; // EQ
        c
    }
}

/// `MovieObject.bdmv` with the given objects.
pub fn movie_objects(objects: &[Vec<[u8; 12]>]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend((objects.len() as u16).to_be_bytes());
    for o in objects {
        body.extend([0, 0]);
        body.extend((o.len() as u16).to_be_bytes());
        for c in o {
            body.extend(c);
        }
    }
    let mut d = b"MOBJ0200".to_vec();
    d.extend([0u8; 32]);
    d.extend((body.len() as u32).to_be_bytes());
    d.extend(body);
    d
}

/// A BD-J object whose table of accessible playlists lists `playlists`, the
/// first started by itself when `autostart`.
pub fn bdjo(playlists: &[&str], autostart: bool) -> Vec<u8> {
    let mut d = b"BDJO0200".to_vec();
    d.extend([0u8; 40]);
    // TerminalInfo: default font, HAVi configuration 1 (1920x1080).
    d.extend(10u32.to_be_bytes());
    d.extend(b"00000");
    d.extend([0x10, 0, 0, 0, 0]);
    // AppCacheInfo: one JAR file.
    d.extend(14u32.to_be_bytes());
    d.extend([1, 0, 1]);
    d.extend(b"00000");
    d.extend(b"*** ");
    d.extend([0, 0]);
    let flags = ((playlists.len() as u32) << 21) | ((autostart as u32) << 19);
    let mut table = flags.to_be_bytes().to_vec();
    for p in playlists {
        table.extend(p.as_bytes());
        table.push(0);
    }
    d.extend((table.len() as u32).to_be_bytes());
    d.extend(table);
    // An empty application management table, key interest table and file
    // access info.
    d.extend(2u32.to_be_bytes());
    d.extend([0, 0]);
    d.extend([0u8; 4]);
    d.extend(0u32.to_be_bytes());
    d
}

/// The interactive graphics stream of a play item.
#[derive(Clone, Copy)]
pub enum Ig {
    None,
    /// Multiplexed into the play item's clip, with this PID.
    InClip(u16),
    /// In a sub-path's clip.
    SubPath,
}

fn stream_entry(kind: u8, pid: u16, coding: u8) -> Vec<u8> {
    let mut s = match kind {
        1 => vec![9, 1],
        _ => vec![9, 3, 0],
    };
    s.extend(pid.to_be_bytes());
    s.resize(10, 0);
    match coding {
        0x02 | 0x1B => s.extend([5, coding, 0x61, 0, 0, 0]),
        0x91 => s.extend([5, coding, b'k', b'o', b'r', 0]),
        _ => s.extend([5, coding, 0x31, b'k', b'o', b'r']),
    }
    s
}

/// A playlist of one play item: `clip` from `in_time` to `out_time` (45 kHz),
/// with a video stream of `coding`, an audio stream and `ig`.
pub fn mpls(clip: &str, coding: u8, in_time: u32, out_time: u32, ig: Ig) -> Vec<u8> {
    mpls_items(&[(clip, coding, in_time, out_time, ig)])
}

/// A playlist of the given play items: (clip, video coding, IN, OUT, IG).
pub fn mpls_items(items: &[(&str, u8, u32, u32, Ig)]) -> Vec<u8> {
    let mut d = b"MPLS0200".to_vec();
    d.extend(58u32.to_be_bytes()); // PlayList_start_address
    d.extend([0u8; 8]); // PlayListMark and ExtensionData start addresses
    d.extend([0u8; 20]);
    d.extend(14u32.to_be_bytes()); // AppInfoPlayList
    d.extend([0, 1, 0, 0]);
    d.extend([0u8; 10]);
    let mut list = vec![0, 0];
    list.extend((items.len() as u16).to_be_bytes());
    list.extend([0, 0]); // no sub-paths
    for &(clip, coding, in_time, out_time, ig) in items {
        list.extend(play_item(clip, coding, in_time, out_time, ig));
    }
    d.extend((list.len() as u32).to_be_bytes());
    d.extend(list);
    // PlayListMark: none.
    let marks = d.len() as u32;
    d[12..16].copy_from_slice(&marks.to_be_bytes());
    d.extend(2u32.to_be_bytes());
    d.extend([0, 0]);
    d
}

/// One play item, led by its length.
fn play_item(clip: &str, coding: u8, in_time: u32, out_time: u32, ig: Ig) -> Vec<u8> {
    let mut item = clip.as_bytes().to_vec();
    item.extend(b"M2TS");
    item.extend([0x00, 0x01, 0x00]); // not multi-angle, connection 1, STC 0
    item.extend(in_time.to_be_bytes());
    item.extend(out_time.to_be_bytes());
    item.extend([0u8; 8]); // UO mask
    item.extend([0x00, 0x00, 0x00, 0x00]); // random access, still mode, still time
    let mut streams = stream_entry(1, 0x1011, coding);
    streams.extend(stream_entry(1, 0x1100, 0x81));
    let mut ig_count = 0;
    match ig {
        Ig::None => {}
        Ig::InClip(pid) => {
            streams.extend(stream_entry(1, pid, 0x91));
            ig_count = 1;
        }
        Ig::SubPath => {
            streams.extend(stream_entry(3, 0x1400, 0x91));
            ig_count = 1;
        }
    }
    let mut stn = vec![0, 0, 1, 1, 0, ig_count, 0, 0, 0, 0, 0, 0, 0, 0];
    stn.extend(streams);
    item.extend((stn.len() as u16).to_be_bytes());
    item.extend(stn);
    let mut led = (item.len() as u16).to_be_bytes().to_vec();
    led.extend(item);
    led
}

// ----------------------------------------------------------------------------
// Interactive graphics segments
// ----------------------------------------------------------------------------

fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut s = vec![kind];
    s.extend((body.len() as u16).to_be_bytes());
    s.extend(body);
    s
}

/// Run-length codes rows of palette indices (every row whole).
pub fn rle(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        let mut i = 0;
        while i < row.len() {
            let c = row[i];
            let mut n = 1;
            while i + n < row.len() && row[i + n] == c && n < 0x3FFF {
                n += 1;
            }
            if c != 0 && n < 3 {
                out.extend(std::iter::repeat_n(c, n));
            } else {
                let long = if n >= 64 { 0x40 } else { 0 };
                let colour = if c != 0 { 0x80 } else { 0 };
                out.push(0);
                if long != 0 {
                    out.extend([colour | long | (n >> 8) as u8, n as u8]);
                } else {
                    out.push(colour | n as u8);
                }
                if c != 0 {
                    out.push(c);
                }
            }
            i += n;
        }
        out.extend([0, 0]);
    }
    out
}

/// ODS segments of object `id`: `rows` of palette indices, split into parts
/// of at most `part` bytes of object data.
pub fn ods(id: u16, rows: &[Vec<u8>], part: usize) -> Vec<Vec<u8>> {
    let coded = rle(rows);
    let mut data = ((coded.len() + 4) as u32).to_be_bytes()[1..].to_vec();
    data.extend((rows[0].len() as u16).to_be_bytes());
    data.extend((rows.len() as u16).to_be_bytes());
    data.extend(coded);
    let parts: Vec<&[u8]> = data.chunks(part).collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut body = id.to_be_bytes().to_vec();
            body.push(0); // version
            body.push(if i == 0 { 0x80 } else { 0 } | if i + 1 == parts.len() { 0x40 } else { 0 });
            body.extend(*p);
            segment(0x15, &body)
        })
        .collect()
}

/// A PDS: entries of (index, Y, Cr, Cb, alpha).
pub fn pds(id: u8, entries: &[[u8; 5]]) -> Vec<u8> {
    let mut body = vec![id, 0];
    for e in entries {
        body.extend(e);
    }
    segment(0x14, &body)
}

pub fn end() -> Vec<u8> {
    segment(0x80, &[])
}

/// A button: id, position, and its normal and selected objects.
#[derive(Clone, Copy)]
pub struct Button {
    pub id: u16,
    pub x: u16,
    pub y: u16,
    pub normal: u16,
    pub selected: u16,
}

fn button(b: &Button) -> Vec<u8> {
    let mut d = b.id.to_be_bytes().to_vec();
    d.extend([0xFF, 0xFF, 0]); // numeric_select_value, auto action
    d.extend(b.x.to_be_bytes());
    d.extend(b.y.to_be_bytes());
    d.extend([0xFFu8; 8]); // neighbours
    d.extend(b.normal.to_be_bytes());
    d.extend(b.normal.to_be_bytes());
    d.extend([0x00, 0xFF]); // repeat, selected sound
    d.extend(b.selected.to_be_bytes());
    d.extend(b.selected.to_be_bytes());
    d.extend([0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]); // repeat, sound, activated
                                                    // One command: PlayPL 1.
    d.extend(1u16.to_be_bytes());
    d.extend(cmd::play_pl(1));
    d
}

/// A page: its id, default selected button, palette, and BOGs of (button
/// shown, buttons).
pub fn page(id: u8, selected: u16, palette: u8, bogs: &[(u16, Vec<Button>)]) -> Vec<u8> {
    let mut d = vec![id, 0];
    d.extend([0u8; 8]); // UO mask
    d.extend([0, 0, 0, 0]); // in and out effects: none
    d.push(0x20); // animation frame rate
    d.extend(selected.to_be_bytes());
    d.extend(0xFFFFu16.to_be_bytes());
    d.push(palette);
    d.push(bogs.len() as u8);
    for (shown, buttons) in bogs {
        d.extend(shown.to_be_bytes());
        d.push(buttons.len() as u8);
        for b in buttons {
            d.extend(button(b));
        }
    }
    d
}

/// The ICS segment of a display set for a `width` x `height` plane,
/// multiplexed with the video, always shown or a pop-up.
pub fn ics(width: u16, height: u16, pages: &[Vec<u8>], popup: bool) -> Vec<u8> {
    let mut comp = vec![if popup { 0x40 } else { 0x00 }];
    comp.extend([0u8; 10]); // time-outs
    comp.extend([0, 0, 0]); // user time-out
    comp.push(pages.len() as u8);
    for p in pages {
        comp.extend(p);
    }
    let mut body = width.to_be_bytes().to_vec();
    body.extend(height.to_be_bytes());
    body.push(0x60); // 50 Hz
    body.extend([0, 0, 0x80]); // composition 0, epoch start
    body.push(0xC0); // first and last part
    body.extend(&(comp.len() as u32).to_be_bytes()[1..]);
    body.extend(comp);
    segment(0x18, &body)
}
