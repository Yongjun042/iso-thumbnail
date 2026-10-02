//! Blu-ray navigation: which playlists the disc's top menu plays.
//!
//! `BDMV/index.bdmv` names the object run for the top menu (and for first
//! playback and every title). An HDMV movie object (`BDMV/MovieObject.bdmv`)
//! is a short program of 12-byte navigation commands; the menu's background
//! video is the playlist its `PlayPL` command starts, and its buttons come
//! with that playlist's interactive graphics stream. A BD-J object
//! (`BDMV/BDJO/xxxxx.bdjo`) runs Java code, which cannot be followed; its
//! table of accessible playlists can only say which playlist starts by itself.
//!
//! The commands are not executed: a compare command, which skips the next
//! command when false, is taken both ways (true first), so every branch is
//! looked at. `GoTo`, `JumpObject`, `CallObject`, `JumpTitle` and `CallTitle`
//! are followed (title 0 is the top menu, 0xFFFF first playback), and
//! registers set from constants (`Move`) are tracked, so a playlist number
//! held in a general purpose register is found too. Each `PlayPL`,
//! `PlayPLatPI` and `PlayPLatMK` gives a playlist with the play item or mark
//! it starts at, in the order met, and the button `SetButtonPage` last chose
//! (PSR 10), which the menu selects when its page names none. The walk is
//! bounded (`MAX_STEPS` commands, `MAX_DEPTH` nested objects, `MAX_PLAYLISTS`
//! playlists) and never visits a command twice, so crafted loops end.
//!
//! Layouts follow libbluray (`index_parse.c`, `mobj_parse.c`,
//! `bdjo_parse.c`, `hdmv_insn.h`). All fields are big-endian; every offset and
//! count is checked against the file, which comes from an untrusted image.

use crate::error::{Error, Result};
use crate::mpls::{has_header, Start};

/// Largest navigation file read (`index.bdmv`, `MovieObject.bdmv`, a
/// `.bdjo`). Real ones are well under 100 KB.
pub const MAX_NAV_BYTES: usize = 1 << 20;
/// Commands looked at in one walk.
const MAX_STEPS: usize = 4096;
/// Objects entered within one another (jumps and calls).
const MAX_DEPTH: usize = 8;
/// Playlists collected in one walk.
pub const MAX_PLAYLISTS: usize = 8;
/// Highest playlist number (`xxxxx.mpls`).
const MAX_PLAYLIST: u32 = 99_999;
/// No object (an `id_ref` of 0xFFFF).
const NONE: u16 = 0xFFFF;

/// The object run for a menu or title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    /// An HDMV movie object, by index in `MovieObject.bdmv`.
    Hdmv(u16),
    /// A BD-J object: the `xxxxx` of `BDMV/BDJO/xxxxx.bdjo`.
    Bdj(String),
    /// None or of an unknown kind.
    Missing,
}

/// What `index.bdmv` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub first_play: Object,
    pub top_menu: Object,
    /// Titles 1, 2, ... in order.
    pub titles: Vec<Object>,
}

fn be16(d: &[u8], o: usize) -> Result<u16> {
    d.get(
        o..o.checked_add(2)
            .ok_or(Error::Corrupt("navigation offset"))?,
    )
    .map(|s| u16::from_be_bytes([s[0], s[1]]))
    .ok_or(Error::Corrupt("navigation file truncated"))
}

fn be32(d: &[u8], o: usize) -> Result<u32> {
    d.get(
        o..o.checked_add(4)
            .ok_or(Error::Corrupt("navigation offset"))?,
    )
    .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    .ok_or(Error::Corrupt("navigation file truncated"))
}

fn slice(d: &[u8], o: usize, len: usize) -> Result<&[u8]> {
    o.checked_add(len)
        .and_then(|end| d.get(o..end))
        .ok_or(Error::Corrupt("navigation file truncated"))
}

/// One 12-byte object reference of `index.bdmv`: object_type 2 | 30 bits
/// (access_type and reserved), then playback_type 2 | reserved 14 and either
/// an HDMV `id_ref` (16) or a BD-J object name (5 characters).
fn object(entry: &[u8]) -> Object {
    match entry[0] >> 6 {
        1 => match u16::from_be_bytes([entry[6], entry[7]]) {
            NONE => Object::Missing,
            id => Object::Hdmv(id),
        },
        2 => {
            let name = &entry[6..11];
            if name.iter().all(u8::is_ascii_digit) {
                Object::Bdj(String::from_utf8_lossy(name).into_owned())
            } else {
                Object::Missing
            }
        }
        _ => Object::Missing,
    }
}

/// Parses `index.bdmv`.
pub fn parse_index(d: &[u8]) -> Result<Index> {
    if !has_header(d, b"INDX") {
        return Err(Error::Corrupt("not an index file"));
    }
    let start = usize::try_from(be32(d, 8)?).map_err(|_| Error::Corrupt("index offset"))?;
    // Indexes: length 32, FirstPlayback (12), TopMenu (12), number_of_Titles
    // 16, then a 12-byte entry per title.
    let first_play = object(slice(d, start.saturating_add(4), 12)?);
    let top_menu = object(slice(d, start.saturating_add(16), 12)?);
    let count = usize::from(be16(d, start.saturating_add(28))?);
    let titles = slice(d, start.saturating_add(30), count * 12)?
        .chunks_exact(12)
        .map(object)
        .collect();
    Ok(Index {
        first_play,
        top_menu,
        titles,
    })
}

/// One navigation command (libbluray `mobj_parse_cmd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// operand_count 3 | command_group 2 | command_sub_group 3.
    group: u8,
    sub_group: u8,
    /// Whether operand 1 (`dst`) and 2 (`src`) are constants rather than
    /// register numbers.
    imm_dst: bool,
    imm_src: bool,
    branch_option: u8,
    set_option: u8,
    dst: u32,
    src: u32,
}

impl Command {
    fn parse(b: &[u8]) -> Self {
        Self {
            group: (b[0] >> 3) & 3,
            sub_group: b[0] & 7,
            imm_dst: b[1] & 0x80 != 0,
            imm_src: b[1] & 0x40 != 0,
            branch_option: b[1] & 0x0F,
            set_option: b[3] & 0x1F,
            dst: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            src: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        }
    }
}

/// Parses `MovieObject.bdmv` into the commands of each movie object.
pub fn parse_movie_objects(d: &[u8]) -> Result<Vec<Vec<Command>>> {
    if !has_header(d, b"MOBJ") {
        return Err(Error::Corrupt("not a movie object file"));
    }
    // MovieObjects at 40: length 32, reserved 32, number_of_mobjs 16; each
    // object: flags 16, number_of_navigation_commands 16, the commands.
    let count = be16(d, 48)?;
    let mut o = 50usize;
    let mut objects = Vec::new();
    for _ in 0..count {
        let n = usize::from(be16(d, o + 2)?);
        let commands = slice(d, o + 4, n * 12)?;
        objects.push(commands.chunks_exact(12).map(Command::parse).collect());
        o += 4 + n * 12;
    }
    Ok(objects)
}

/// The playlist a BD-J object starts by itself: the first of its table of
/// accessible playlists when `autostart_first_playlist_flag` is set.
pub fn bdjo_autostart_playlist(d: &[u8]) -> Result<Option<u32>> {
    if !has_header(d, b"BDJO") {
        return Err(Error::Corrupt("not a BD-J object"));
    }
    // The header and its 40 bytes of addresses, then TerminalInfo (length
    // 32, 10 bytes), AppCacheInfo (length 32, number_of_entries 8, reserved 8,
    // 12 bytes per entry), TableOfAccessiblePlayLists (length 32,
    // number_of_acc_PlayLists 11 | access_to_all 1 | autostart_first 1 |
    // reserved 19, then 5 characters and a reserved byte per playlist).
    let cache = 48 + 14;
    let entries = usize::from(*d.get(cache + 4).ok_or(Error::Corrupt("bdjo truncated"))?);
    let table = cache + 6 + 12 * entries;
    let flags = be32(d, table + 4)?;
    let count = flags >> 21;
    let autostart = flags & (1 << 19) != 0;
    if !autostart || count == 0 {
        return Ok(None);
    }
    Ok(playlist_number(slice(d, table + 8, 5)?))
}

/// The number of a playlist named by five ASCII digits.
fn playlist_number(name: &[u8]) -> Option<u32> {
    if name.len() == 5 && name.iter().all(u8::is_ascii_digit) {
        std::str::from_utf8(name).ok()?.parse().ok()
    } else {
        None
    }
}

/// Command groups, sub-groups and options (libbluray `hdmv_insn.h`).
const GROUP_BRANCH: u8 = 0;
const GROUP_COMPARE: u8 = 1;
const GROUP_SET: u8 = 2;
const BRANCH_GOTO: u8 = 0;
const BRANCH_JUMP: u8 = 1;
const BRANCH_PLAY: u8 = 2;
const GOTO: u8 = 1;
const BREAK: u8 = 2;
const JUMP_OBJECT: u8 = 0;
const JUMP_TITLE: u8 = 1;
const CALL_OBJECT: u8 = 2;
const CALL_TITLE: u8 = 3;
const RESUME: u8 = 4;
const PLAY_PL: u8 = 0;
const PLAY_PL_PI: u8 = 1;
const PLAY_PL_PM: u8 = 2;
const SET_SET: u8 = 0;
const SET_SYSTEM: u8 = 1;
const SET_MOVE: u8 = 1;
const SET_SWAP: u8 = 2;
const SET_BUTTON_PAGE: u8 = 3;
/// Title numbers of the top menu and of first playback.
const TITLE_TOP_MENU: u32 = 0;
const TITLE_FIRST_PLAY: u32 = 0xFFFF;
/// Bit of an operand that names a player status register (not a general
/// purpose one).
const PSR_FLAG: u32 = 0x8000_0000;
/// General purpose registers.
const GPRS: u32 = 4096;

/// A playlist a menu plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Play {
    pub playlist: u32,
    pub start: Start,
    /// The button selected before (PSR 10), when known.
    pub button: Option<u16>,
}

/// What the walk tracks.
struct Walk<'a> {
    index: &'a Index,
    objects: &'a [Vec<Command>],
    /// General purpose registers set from constants (register, value).
    registers: Vec<(u32, u32)>,
    /// The selected button register (PSR 10), when known.
    button: Option<u16>,
    /// Commands looked at, as (object, command), so none is looked at twice.
    visited: std::collections::HashSet<(u16, u16)>,
    steps: usize,
    plays: Vec<Play>,
}

impl Walk<'_> {
    fn value(&self, imm: bool, operand: u32) -> Option<u32> {
        if imm {
            return Some(operand);
        }
        if operand & PSR_FLAG != 0 || operand >= GPRS {
            return None;
        }
        self.registers
            .iter()
            .find(|(r, _)| *r == operand)
            .map(|&(_, v)| v)
    }

    fn set(&mut self, register: u32, value: Option<u32>) {
        self.registers.retain(|(r, _)| *r != register);
        if let Some(v) = value {
            self.registers.push((register, v));
        }
    }

    fn done(&self) -> bool {
        self.steps >= MAX_STEPS || self.plays.len() >= MAX_PLAYLISTS
    }

    /// The object of title `number`.
    fn title(&self, number: u32) -> Option<u16> {
        let object = match number {
            TITLE_TOP_MENU => &self.index.top_menu,
            TITLE_FIRST_PLAY => &self.index.first_play,
            n => self.index.titles.get(usize::try_from(n - 1).ok()?)?,
        };
        match object {
            Object::Hdmv(id) => Some(*id),
            _ => None,
        }
    }

    /// Looks at movie object `id` from command 0 on.
    fn object(&mut self, id: u16, depth: usize) {
        let objects = self.objects;
        let Some(commands) = objects.get(usize::from(id)) else {
            return;
        };
        // Paths still to look at: a compare skips the next command when
        // false, so both the command after it and the one after that go on.
        let mut paths = vec![0usize];
        while let Some(mut pc) = paths.pop() {
            while let Some(c) = commands.get(pc) {
                if self.done() || !self.visited.insert((id, pc as u16)) {
                    break;
                }
                self.steps += 1;
                pc += 1;
                match (c.group, c.sub_group) {
                    (GROUP_BRANCH, BRANCH_GOTO) => match c.branch_option {
                        GOTO => {
                            // A target outside the object is not followed.
                            if let Some(to) = self.value(c.imm_dst, c.dst) {
                                if (to as usize) < commands.len() {
                                    pc = to as usize;
                                }
                            }
                        }
                        BREAK => break,
                        _ => {}
                    },
                    (GROUP_BRANCH, BRANCH_JUMP) => {
                        let (target, returns) = match c.branch_option {
                            JUMP_OBJECT | CALL_OBJECT => (
                                self.value(c.imm_dst, c.dst)
                                    .and_then(|v| u16::try_from(v).ok()),
                                c.branch_option == CALL_OBJECT,
                            ),
                            JUMP_TITLE | CALL_TITLE => (
                                self.value(c.imm_dst, c.dst).and_then(|t| self.title(t)),
                                c.branch_option == CALL_TITLE,
                            ),
                            RESUME => break,
                            _ => (None, true),
                        };
                        // A target that is not known or does not exist is
                        // passed over, as if the command were skipped.
                        let Some(target) = target.filter(|&t| usize::from(t) < objects.len())
                        else {
                            continue;
                        };
                        if depth < MAX_DEPTH {
                            // A call saves PSR 10 and Resume restores it
                            // (libbluray `_suspend_object`, `_resume_object`).
                            let button = self.button;
                            self.object(target, depth + 1);
                            if returns {
                                self.button = button;
                            }
                        }
                        // A jump never comes back.
                        if !returns {
                            break;
                        }
                    }
                    (GROUP_BRANCH, BRANCH_PLAY) => {
                        // The play item or mark of PlayPLatPI and PlayPLatMK;
                        // when its register is not known, the first item.
                        let at = || self.value(c.imm_src, c.src);
                        let start = match c.branch_option {
                            PLAY_PL => Start::First,
                            PLAY_PL_PI => at().map_or(Start::First, Start::Item),
                            PLAY_PL_PM => at().map_or(Start::First, Start::Mark),
                            _ => continue,
                        };
                        let Some(playlist) = self.value(c.imm_dst, c.dst) else {
                            continue;
                        };
                        let seen = self
                            .plays
                            .iter()
                            .any(|p| p.playlist == playlist && p.start == start);
                        if playlist <= MAX_PLAYLIST && !seen {
                            self.plays.push(Play {
                                playlist,
                                start,
                                button: self.button,
                            });
                        }
                    }
                    (GROUP_COMPARE, _) => paths.push(pc + 1),
                    (GROUP_SET, SET_SET) => {
                        // The destination is always a register.
                        if c.imm_dst || c.dst & PSR_FLAG != 0 || c.dst >= GPRS {
                            continue;
                        }
                        match c.set_option {
                            SET_MOVE => {
                                let v = self.value(c.imm_src, c.src);
                                self.set(c.dst, v);
                            }
                            SET_SWAP if !c.imm_src => {
                                let (a, b) = (self.value(false, c.dst), self.value(false, c.src));
                                self.set(c.dst, b);
                                if c.src & PSR_FLAG == 0 && c.src < GPRS {
                                    self.set(c.src, a);
                                }
                            }
                            // Arithmetic: the result is not followed.
                            _ => self.set(c.dst, None),
                        }
                    }
                    (GROUP_SET, SET_SYSTEM) if c.set_option == SET_BUTTON_PAGE => {
                        // Button 15..0 and page 31..16, flagged in bits 31
                        // and 30. A register operand keeps the flags and
                        // reads the button from GPR (operand & 0xFFF)
                        // (libbluray `_fetch_operand`, `_set_button_page`).
                        let operand = if c.imm_dst {
                            Some(c.dst)
                        } else {
                            self.value(false, c.dst & 0xFFF)
                                .map(|v| (c.dst & 0xC000_C000) | (v & 0x3FFF))
                        };
                        match operand {
                            Some(v) if v & 0x8000_0000 != 0 => self.button = Some(v as u16),
                            Some(_) => {}
                            None if c.dst & 0x8000_0000 != 0 => self.button = None,
                            None => {}
                        }
                    }
                    // Other SetSystem commands.
                    _ => {}
                }
            }
            if self.done() {
                return;
            }
        }
    }
}

/// The playlists the top menu's movie object plays, in the order met.
pub fn top_menu_playlists(index: &Index, objects: &[Vec<Command>]) -> Vec<Play> {
    let Object::Hdmv(top) = index.top_menu else {
        return Vec::new();
    };
    let mut walk = Walk {
        index,
        objects,
        registers: Vec::new(),
        button: None,
        visited: Default::default(),
        steps: 0,
        plays: Vec::new(),
    };
    walk.object(top, 0);
    walk.plays
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 12-byte object reference of `index.bdmv`.
    fn entry(object: &Object) -> Vec<u8> {
        let mut e = vec![0u8; 12];
        match object {
            Object::Hdmv(id) => {
                e[0] = 0x40;
                e[6..8].copy_from_slice(&id.to_be_bytes());
            }
            Object::Bdj(name) => {
                e[0] = 0x80;
                e[4] = 0x40; // playback type: interactive
                e[6..11].copy_from_slice(name.as_bytes());
            }
            Object::Missing => {
                e[0] = 0x40;
                e[6..8].copy_from_slice(&NONE.to_be_bytes());
            }
        }
        e
    }

    /// An `index.bdmv` (first playback runs title 1's object).
    pub(crate) fn index(top_menu: &Object, titles: &[Object]) -> Vec<u8> {
        index_with(titles.first().unwrap_or(&Object::Missing), top_menu, titles)
    }

    /// An `index.bdmv` with its first playback object.
    fn index_with(first_play: &Object, top_menu: &Object, titles: &[Object]) -> Vec<u8> {
        let mut d = b"INDX0200".to_vec();
        d.extend(78u32.to_be_bytes()); // Indexes_start_address
        d.extend([0u8; 28]);
        d.extend(34u32.to_be_bytes()); // AppInfoBDMV
        d.extend([0u8; 34]);
        assert_eq!(d.len(), 78);
        let mut indexes = entry(first_play);
        indexes.extend(entry(top_menu));
        indexes.extend((titles.len() as u16).to_be_bytes());
        for t in titles {
            indexes.extend(entry(t));
        }
        d.extend((indexes.len() as u32).to_be_bytes());
        d.extend(indexes);
        d
    }

    /// Navigation commands for tests.
    pub(crate) mod cmd {
        fn command(b0: u8, b1: u8, set: u8, dst: u32, src: u32) -> [u8; 12] {
            let mut c = [0u8; 12];
            c[0] = b0;
            c[1] = b1;
            c[3] = set;
            c[4..8].copy_from_slice(&dst.to_be_bytes());
            c[8..12].copy_from_slice(&src.to_be_bytes());
            c
        }
        pub fn play_pl(pl: u32) -> [u8; 12] {
            command(0x22 | 0x20, 0x80, 0, pl, 0)
        }
        pub fn play_pl_register(gpr: u32) -> [u8; 12] {
            command(0x22 | 0x20, 0x00, 0, gpr, 0)
        }
        pub fn play_pl_at_item(pl: u32, item: u32) -> [u8; 12] {
            command(0x42 | 0x20, 0xC1, 0, pl, item)
        }
        /// `PlayPLatPlayItem pl, GPR[gpr]`.
        pub fn play_pl_at_item_gpr(pl: u32, gpr: u32) -> [u8; 12] {
            command(0x42, 0x81, 0, pl, gpr)
        }
        pub fn play_pl_at_mark(pl: u32, mark: u32) -> [u8; 12] {
            command(0x42, 0xC2, 0, pl, mark)
        }
        /// `SetButtonPage` with a constant: button | page, flagged in bits
        /// 31 and 30.
        pub fn set_button_page(v: u32) -> [u8; 12] {
            command(0x31, 0x80, 3, v, 0)
        }
        /// `SetButtonPage` of the button in GPR[gpr].
        pub fn set_button_page_gpr(gpr: u32) -> [u8; 12] {
            command(0x31, 0x00, 3, 0x8000_0000 | gpr, 0)
        }
        pub fn jump_object(id: u32) -> [u8; 12] {
            command(0x21 | 0x20, 0x80, 0, id, 0)
        }
        pub fn call_object(id: u32) -> [u8; 12] {
            command(0x21 | 0x20, 0x82, 0, id, 0)
        }
        pub fn jump_title(title: u32) -> [u8; 12] {
            command(0x21 | 0x20, 0x81, 0, title, 0)
        }
        pub fn call_title(title: u32) -> [u8; 12] {
            command(0x21 | 0x20, 0x83, 0, title, 0)
        }
        pub fn goto(to: u32) -> [u8; 12] {
            command(0x20, 0x81, 0, to, 0)
        }
        pub fn brk() -> [u8; 12] {
            command(0x00, 0x02, 0, 0, 0)
        }
        pub fn resume() -> [u8; 12] {
            command(0x01, 0x04, 0, 0, 0)
        }
        /// `Move GPR[gpr] = value`.
        pub fn move_imm(gpr: u32, value: u32) -> [u8; 12] {
            command(0x50, 0x40, 1, gpr, value)
        }
        /// `Add GPR[gpr] += value`.
        pub fn add_imm(gpr: u32, value: u32) -> [u8; 12] {
            command(0x50, 0x40, 3, gpr, value)
        }
        /// `GPR[a] == value` (skips the next command when false).
        pub fn equals(gpr: u32, value: u32) -> [u8; 12] {
            command(0x48, 0x40, 0, gpr, value) // cmp_opt in byte 2 is not read
        }
    }

    /// A `MovieObject.bdmv` of the given objects' commands.
    pub(crate) fn movie_objects(objects: &[Vec<[u8; 12]>]) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend((objects.len() as u16).to_be_bytes());
        for o in objects {
            body.extend([0x80, 0]);
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

    /// A BD-J object listing `playlists`, the first started by itself when
    /// `autostart`, with `cache_entries` application cache entries.
    pub(crate) fn bdjo(playlists: &[&str], autostart: bool, cache_entries: u8) -> Vec<u8> {
        let mut d = b"BDJO0200".to_vec();
        d.extend([0u8; 40]);
        d.extend(10u32.to_be_bytes());
        d.extend(b"*****");
        d.extend([0x10, 0, 0, 0, 0]);
        let mut cache = vec![cache_entries, 0];
        for _ in 0..cache_entries {
            cache.extend([1, b'0', b'0', b'0', b'0', b'1', b'*', b'*', b'*', 0, 0, 0]);
        }
        d.extend((cache.len() as u32).to_be_bytes());
        d.extend(cache);
        let flags = ((playlists.len() as u32) << 21) | ((autostart as u32) << 19);
        let mut table = flags.to_be_bytes().to_vec();
        for p in playlists {
            table.extend(p.as_bytes());
            table.push(0);
        }
        d.extend((table.len() as u32).to_be_bytes());
        d.extend(table);
        d.extend([0u8; 16]); // the application management table and the rest
        d
    }

    fn objects(objects: &[Vec<[u8; 12]>]) -> Vec<Vec<Command>> {
        parse_movie_objects(&movie_objects(objects)).unwrap()
    }

    fn plays(top: u16, titles: &[Object], objs: &[Vec<[u8; 12]>]) -> Vec<Play> {
        let index = parse_index(&index(&Object::Hdmv(top), titles)).unwrap();
        top_menu_playlists(&index, &objects(objs))
    }

    fn walk(top: u16, titles: &[Object], objs: &[Vec<[u8; 12]>]) -> Vec<u32> {
        plays(top, titles, objs)
            .iter()
            .map(|p| p.playlist)
            .collect()
    }

    use cmd::*;

    #[test]
    fn parses_index_entries() {
        let titles = [
            Object::Hdmv(3),
            Object::Bdj("00002".into()),
            Object::Missing,
        ];
        let i = parse_index(&index(&Object::Hdmv(1), &titles)).unwrap();
        assert_eq!(i.first_play, Object::Hdmv(3));
        assert_eq!(i.top_menu, Object::Hdmv(1));
        assert_eq!(i.titles, titles);
        let i = parse_index(&index(&Object::Bdj("00000".into()), &[])).unwrap();
        assert_eq!(i.top_menu, Object::Bdj("00000".into()));
        let i = parse_index(&index(&Object::Missing, &[])).unwrap();
        assert_eq!(i.top_menu, Object::Missing);
        assert!(parse_index(b"INDX0200").is_err());
        assert!(parse_index(b"MOBJ0200").is_err());
    }

    #[test]
    fn the_top_menu_plays_its_playlist() {
        let objs = vec![vec![play_pl(1)], vec![move_imm(0, 1), play_pl(10)]];
        assert_eq!(walk(1, &[], &objs), [10]);
        // PlayPLatPlayItem, with both operands constants.
        let objs = vec![vec![play_pl_at_item(12, 0)]];
        assert_eq!(walk(0, &[], &objs), [12]);
    }

    #[test]
    fn jumps_calls_and_titles_are_followed_in_order() {
        let objs = vec![
            vec![
                call_object(2),
                call_title(1),
                play_pl(30),
                jump_object(3),
                play_pl(98),
            ],
            vec![play_pl(20)],
            vec![play_pl(10), resume(), play_pl(99)],
            vec![play_pl(40)],
        ];
        let titles = [Object::Hdmv(1)];
        // Calls come back, jumps do not.
        assert_eq!(walk(0, &titles, &objs), [10, 20, 30, 40]);
        let objs = vec![vec![jump_title(1), play_pl(98)], vec![play_pl(20)]];
        assert_eq!(walk(0, &titles, &objs), [20]);
    }

    #[test]
    fn title_0xffff_is_first_playback() {
        let d = index_with(&Object::Hdmv(1), &Object::Hdmv(0), &[Object::Hdmv(2)]);
        let index = parse_index(&d).unwrap();
        let objs = objects(&[
            vec![jump_title(0xFFFF)],
            vec![play_pl(800)],
            vec![play_pl(1)],
        ]);
        let got: Vec<u32> = top_menu_playlists(&index, &objs)
            .iter()
            .map(|p| p.playlist)
            .collect();
        assert_eq!(got, [800]);
    }

    #[test]
    fn plays_start_at_their_play_item_or_mark() {
        let objs = vec![vec![
            play_pl_at_item(12, 1),
            play_pl_at_mark(12, 2),
            play_pl(12),
            // The same as the first, and one whose item is not known.
            play_pl_at_item(12, 1),
            play_pl_at_item_gpr(13, 7),
        ]];
        let starts: Vec<(u32, Start)> = plays(0, &[], &objs)
            .iter()
            .map(|p| (p.playlist, p.start))
            .collect();
        assert_eq!(
            starts,
            [
                (12, Start::Item(1)),
                (12, Start::Mark(2)),
                (12, Start::First),
                (13, Start::First)
            ]
        );
    }

    #[test]
    fn set_button_page_selects_the_button_of_later_plays() {
        let objs = vec![vec![
            play_pl(1),
            set_button_page(0x8000_0003),
            play_pl(2),
            // A page alone leaves the button.
            set_button_page(0x4001_0000),
            play_pl(3),
            move_imm(2, 0x4005),
            set_button_page_gpr(2),
            play_pl(4),
            set_button_page_gpr(9),
            play_pl(5),
        ]];
        let buttons: Vec<Option<u16>> = plays(0, &[], &objs).iter().map(|p| p.button).collect();
        // The register's value is masked to the 14 bits of a button id.
        assert_eq!(buttons, [None, Some(3), Some(3), Some(5), None]);
        // A call saves the button and Resume restores it; a jump keeps it.
        let objs = vec![
            vec![call_object(1), play_pl(5), jump_object(2)],
            vec![set_button_page(0x8000_0002), play_pl(4), resume()],
            vec![set_button_page(0x8000_0007), play_pl(6)],
        ];
        let got: Vec<(u32, Option<u16>)> = plays(0, &[], &objs)
            .iter()
            .map(|p| (p.playlist, p.button))
            .collect();
        assert_eq!(got, [(4, Some(2)), (5, None), (6, Some(7))]);
    }

    #[test]
    fn nesting_is_bounded() {
        // Calls nested MAX_DEPTH deep are followed, one more is not.
        let chain = |n: u32| -> Vec<Vec<[u8; 12]>> {
            (0..=n)
                .map(|i| {
                    if i < n {
                        vec![call_object(i + 1)]
                    } else {
                        vec![play_pl(1)]
                    }
                })
                .collect()
        };
        assert_eq!(walk(0, &[], &chain(MAX_DEPTH as u32)), [1]);
        assert!(walk(0, &[], &chain(MAX_DEPTH as u32 + 1)).is_empty());
        // A long chain ends there too, on a small stack.
        let long = chain(5000);
        let got = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || walk(0, &[], &long))
            .unwrap()
            .join()
            .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn every_branch_is_looked_at() {
        // if GPR0 == 1 { PlayPL 5 } else { PlayPL 6 }, written with GoTo.
        let objs = vec![vec![
            equals(0, 1),
            goto(3),
            play_pl(6),
            play_pl(5),
            brk(),
            play_pl(7),
        ]];
        assert_eq!(walk(0, &[], &objs), [5, 6]);
    }

    #[test]
    fn playlists_in_registers_are_found() {
        let objs = vec![
            vec![
                move_imm(7, 40),
                call_object(1),
                move_imm(7, 41),
                add_imm(7, 1),
                play_pl_register(7),
            ],
            vec![play_pl_register(7), play_pl_register(8)],
        ];
        // After Add the register's value is not known.
        assert_eq!(walk(0, &[], &objs), [40]);
    }

    #[test]
    fn walks_end_in_loops_and_bad_references() {
        let objs = vec![
            vec![goto(0)],
            vec![jump_object(1)],
            vec![
                jump_object(500),
                jump_title(9),
                goto(70_000),
                play_pl(100_000),
                play_pl(3),
            ],
        ];
        assert_eq!(walk(0, &[], &objs), Vec::<u32>::new());
        assert_eq!(walk(1, &[], &objs), Vec::<u32>::new());
        assert_eq!(walk(2, &[], &objs), [3]);
        // Objects calling each other endlessly, each playing a playlist.
        let objs: Vec<Vec<[u8; 12]>> = (0..100u32)
            .map(|i| vec![play_pl(i), call_object((i + 1) % 100), call_object(i)])
            .collect();
        let got = walk(0, &[], &objs);
        assert_eq!(got, (0..MAX_PLAYLISTS as u32).collect::<Vec<_>>());
        assert!(walk(5, &[], &[vec![goto(0)]]).is_empty());
    }

    #[test]
    fn bdj_menus_start_their_first_playlist_when_flagged() {
        assert_eq!(
            bdjo_autostart_playlist(&bdjo(&["00010", "00011"], true, 2)),
            Ok(Some(10))
        );
        assert_eq!(
            bdjo_autostart_playlist(&bdjo(&["00010"], false, 0)),
            Ok(None)
        );
        assert_eq!(bdjo_autostart_playlist(&bdjo(&[], true, 1)), Ok(None));
        assert_eq!(
            bdjo_autostart_playlist(&bdjo(&["0001x"], true, 0)),
            Ok(None)
        );
        assert!(bdjo_autostart_playlist(b"BDJO0200").is_err());
    }

    #[test]
    fn corrupted_files_never_panic() {
        let objs = vec![
            vec![move_imm(1, 4), call_object(1), play_pl_register(1)],
            vec![equals(0, 0), goto(0), jump_title(1), play_pl(2)],
        ];
        let files = [
            index(
                &Object::Hdmv(0),
                &[Object::Hdmv(1), Object::Bdj("00001".into())],
            ),
            movie_objects(&objs),
            bdjo(&["00010", "00011"], true, 3),
        ];
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..3000 {
            for base in &files {
                let mut d = base.clone();
                for _ in 0..1 + (seed % 5) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let i = (seed as usize) % d.len();
                    d[i] = (seed >> 32) as u8;
                }
                let cut = (seed as usize >> 8) % d.len();
                for d in [&d[..], &d[..cut]] {
                    let _ = bdjo_autostart_playlist(d);
                    if let (Ok(i), Ok(o)) = (parse_index(d), parse_movie_objects(&files[1])) {
                        let _ = top_menu_playlists(&i, &o);
                    }
                    if let Ok(o) = parse_movie_objects(d) {
                        let i = parse_index(&files[0]).unwrap();
                        assert!(top_menu_playlists(&i, &o).len() <= MAX_PLAYLISTS);
                    }
                }
            }
        }
    }
}
