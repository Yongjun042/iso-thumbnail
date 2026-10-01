//! DVD-Video IFO files: where a disc's title menu and root menu are.
//!
//! Only what is needed to find a menu's video is read: the IFO header (its
//! first sector) and the menu program chain information unit table
//! (`VMGM_PGCI_UT` in `VIDEO_TS.IFO`, `VTSM_PGCI_UT` in `VTS_nn_0.IFO`). In
//! that table every language unit lists its menu program chains (PGCs) by
//! menu type; the cells of the wanted menu's PGC give the sectors, in the
//! menu VOB, where its video is.
//!
//! Authored discs often make the entry PGC of a menu a "router" without cells
//! whose pre-commands link to the PGC that shows the menu (`LinkPGCN`), and
//! the video manager's title menu often jumps to the root menu of a title set
//! (`JumpSS VTSM`). Both are followed, a few links deep.
//!
//! Layout (all numbers big-endian, offsets in bytes, as in libdvdread's
//! `ifo_types.h`):
//! - IFO header: identifier `DVDVIDEO-VMG` / `DVDVIDEO-VTS` at 0; the sector
//!   of the menu PGCI unit table within the IFO at 0xC8 (VMG) / 0xD0 (VTS).
//! - PGCI unit table: language unit count (u16) at 0, offset of its last
//!   byte (u32) at 4, then 8-byte language unit entries from 8: language code
//!   (u16), extension, menu existence flags, offset of the unit (u32, from the
//!   start of the table).
//! - Language unit: PGC count (u16) at 0, offset of its last byte (u32) at 4,
//!   then 8-byte search pointers from 8: category byte (bit 7 = entry PGC,
//!   low four bits = menu type: 2 title, 3 root, 4 sub-picture, 5 audio,
//!   6 angle, 7 chapter), three more category bytes, offset of the PGC (u32,
//!   from the start of the unit).
//! - PGC: cell count (u8) at 3; offsets (u16, from the start of the PGC) of
//!   the command table at 0xE4 and of the cell playback table at 0xE8. The
//!   command table holds the pre-command count (u16) at 0 and the 8-byte
//!   commands from 8. A cell playback entry is 24 bytes: the first VOBU's
//!   start sector (u32) at 8 and the last VOBU's start sector (u32) at 16,
//!   both relative to the start of the menu VOB.
//! - Commands (libdvdnav's `vmcmd.c`): `LinkPGCN n` has the top three bits of
//!   byte 0 = 001, bit 4 of byte 0 clear, low nibble of byte 1 = 4, and n in
//!   the low 15 bits of bytes 6-7. `JumpSS` has byte 0 & 0xF0 = 0x30 and low
//!   nibble of byte 1 = 6; the top two bits of byte 5 give the domain: 2 =
//!   the menus of title set (byte 4 & 0x7F), menu type (byte 5 & 0x0F); 3 =
//!   video manager PGC (low 15 bits of bytes 2-3).
//!
//! IFO files are never scrambled. Everything is bounds-checked; anything
//! unexpected gives `None`, and the caller falls back to other sources.

use crate::fs::FileSystem;

const SECTOR: u64 = 2048;
/// Largest menu PGCI unit table read (real ones are a few KiB).
const MAX_PGCI_UT: u64 = 256 << 10;
/// Language units and PGC search pointers examined (the format allows 99).
const MAX_UNITS: usize = 100;
/// Pre-commands examined per PGC (the format allows 128 commands in all).
const MAX_COMMANDS: usize = 128;
/// Links followed from an entry PGC before giving up.
const MAX_LINK_DEPTH: usize = 4;
/// Positions returned per menu.
const MAX_POSITIONS: usize = 3;

/// Which menu to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    /// The title menu of the video manager (`VIDEO_TS.IFO`, `VIDEO_TS.VOB`).
    Title,
    /// The root menu, the main menu of a title set (`VTS_nn_0.IFO`, `VTS_nn_0.VOB`).
    Root,
}

impl Menu {
    fn id(self) -> u8 {
        match self {
            Menu::Title => 2,
            Menu::Root => 3,
        }
    }

    fn identifier(self) -> &'static [u8; 12] {
        match self {
            Menu::Title => b"DVDVIDEO-VMG",
            Menu::Root => b"DVDVIDEO-VTS",
        }
    }

    /// Offset in the IFO header of the menu PGCI unit table's sector.
    fn table_pointer(self) -> usize {
        match self {
            Menu::Title => 0xC8,
            Menu::Root => 0xD0,
        }
    }
}

/// Where a menu is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuLocation {
    /// Sectors of this IFO's menu VOB to look at, best first: the start of the
    /// menu's first cell, the start of its last cell (the loop of a menu that
    /// opens with an intro cell), and the last VOBU of the first cell (where a
    /// motion menu's opening animation has settled). Duplicates removed.
    Sectors(Vec<u32>),
    /// The title menu jumps to the root menu of this title set (1..=99).
    TitleSetRoot(u8),
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Reads exactly `len` bytes of `file` at `offset`, or `None`.
fn read_exact<F: FileSystem>(
    fs: &mut F,
    file: &F::Node,
    offset: u64,
    len: usize,
) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = fs
            .read_range(file, offset + done as u64, &mut buf[done..])
            .ok()?;
        if n == 0 {
            return None;
        }
        done += n;
    }
    Some(buf)
}

/// A navigation command this module follows.
enum Command {
    LinkPgcn(u16),
    JumpTitleSetMenu { title_set: u8, menu: u8 },
    JumpManagerPgc(u16),
}

fn command(c: &[u8]) -> Option<Command> {
    let (b0, b1) = (*c.first()?, *c.get(1)?);
    if b0 >> 5 != 1 {
        return None;
    }
    if b0 & 0x10 == 0 && b1 & 0x0F == 4 {
        return Some(Command::LinkPgcn(be16(c, 6)? & 0x7FFF));
    }
    if b0 & 0xF0 == 0x30 && b1 & 0x0F == 6 {
        let (b4, b5) = (*c.get(4)?, *c.get(5)?);
        return match b5 >> 6 {
            2 => Some(Command::JumpTitleSetMenu {
                title_set: b4 & 0x7F,
                menu: b5 & 0x0F,
            }),
            3 => Some(Command::JumpManagerPgc(be16(c, 2)? & 0x7FFF)),
            _ => None,
        };
    }
    None
}

/// One language unit of the table: its bytes and PGC count.
struct Unit<'a> {
    bytes: &'a [u8],
    pgcs: usize,
}

impl<'a> Unit<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        let pgcs = usize::from(be16(bytes, 0)?).min(MAX_UNITS);
        Some(Self { bytes, pgcs })
    }

    /// Category byte and bytes of PGC `index` (0-based).
    fn pgc(&self, index: usize) -> Option<(u8, &'a [u8])> {
        if index >= self.pgcs {
            return None;
        }
        let srp = 8 + index * 8;
        let category = *self.bytes.get(srp)?;
        let at = be32(self.bytes, srp + 4)? as usize;
        Some((category, self.bytes.get(at..)?))
    }
}

/// The positions to look at in a PGC with cells inside the menu VOB
/// (`sectors` long); `None` when it has no usable cell.
fn positions(pgc: &[u8], sectors: u32) -> Option<Vec<u32>> {
    let cells = usize::from(*pgc.get(3)?);
    let table = usize::from(be16(pgc, 0xE8)?);
    if cells == 0 || table == 0 {
        return None;
    }
    let cell = |i: usize| -> Option<(u32, u32)> {
        let at = table.checked_add(i.checked_mul(24)?)?;
        Some((be32(pgc, at + 8)?, be32(pgc, at + 16)?))
    };
    let (first, first_last) = cell(0)?;
    let mut out = Vec::with_capacity(MAX_POSITIONS);
    let mut add = |s: u32| {
        if s < sectors && !out.contains(&s) && out.len() < MAX_POSITIONS {
            out.push(s);
        }
    };
    add(first);
    if cells > 1 {
        if let Some((last_cell, _)) = cell(cells - 1) {
            add(last_cell);
        }
    }
    if first_last > first {
        add(first_last);
    }
    (!out.is_empty()).then_some(out)
}

/// Where PGC `index` of `unit` leads: its own cells, or (for a router without
/// usable cells) wherever its pre-commands link or jump to.
fn resolve(
    unit: &Unit,
    index: usize,
    menu: Menu,
    sectors: u32,
    depth: usize,
    visited: &mut [bool; MAX_UNITS],
) -> Option<MenuLocation> {
    if depth > MAX_LINK_DEPTH || std::mem::replace(visited.get_mut(index)?, true) {
        return None;
    }
    let (_, pgc) = unit.pgc(index)?;
    if let Some(found) = positions(pgc, sectors) {
        return Some(MenuLocation::Sectors(found));
    }
    let table = usize::from(be16(pgc, 0xE4)?);
    if table == 0 {
        return None;
    }
    let pre = usize::from(be16(pgc, table)?).min(MAX_COMMANDS);
    for i in 0..pre {
        let at = table + 8 + i * 8;
        let Some(c) = pgc.get(at..at + 8) else {
            break;
        };
        let target = match command(c) {
            Some(Command::LinkPgcn(n)) => n,
            // Only the video manager's PGCs are numbered in this table.
            Some(Command::JumpManagerPgc(n)) if menu == Menu::Title => n,
            Some(Command::JumpTitleSetMenu { title_set, menu: 3 })
                if menu == Menu::Title && (1..=99).contains(&title_set) =>
            {
                return Some(MenuLocation::TitleSetRoot(title_set));
            }
            _ => continue,
        };
        if target == 0 {
            continue;
        }
        if let Some(found) = resolve(
            unit,
            usize::from(target) - 1,
            menu,
            sectors,
            depth + 1,
            visited,
        ) {
            return Some(found);
        }
    }
    None
}

/// Finds `menu` in the menu PGCI unit table `table` whose menu VOB is
/// `sectors` long. Language units are tried in order; within one, the entry
/// PGC of the wanted menu type and whatever it links to.
pub fn find_menu(table: &[u8], menu: Menu, sectors: u32) -> Option<MenuLocation> {
    let units = usize::from(be16(table, 0)?).min(MAX_UNITS);
    for u in 0..units {
        let unit_at = be32(table, 8 + u * 8 + 4)? as usize;
        let Some(unit) = table.get(unit_at..).and_then(Unit::new) else {
            continue;
        };
        for index in 0..unit.pgcs {
            let Some((category, _)) = unit.pgc(index) else {
                break;
            };
            if category & 0x80 == 0 || category & 0x0F != menu.id() {
                continue;
            }
            let mut visited = [false; MAX_UNITS];
            if let Some(found) = resolve(&unit, index, menu, sectors, 0, &mut visited) {
                return Some(found);
            }
        }
    }
    None
}

/// Reads `ifo` (a `VIDEO_TS.IFO` for `Menu::Title`, a `VTS_nn_0.IFO` for
/// `Menu::Root`) and returns where that menu is in a menu VOB of `sectors`
/// sectors.
pub fn menu_location<F: FileSystem>(
    fs: &mut F,
    ifo: &F::Node,
    menu: Menu,
    sectors: u32,
) -> Option<MenuLocation> {
    let header = read_exact(fs, ifo, 0, SECTOR as usize)?;
    if header[..12] != menu.identifier()[..] {
        return None;
    }
    let table_sector = be32(&header, menu.table_pointer())?;
    if table_sector == 0 {
        return None;
    }
    let table_at = u64::from(table_sector) * SECTOR;
    // The table's own length field is only a hint (players do not bound the
    // language units by it): read what the file holds, up to the limit.
    let size = fs.file_size(ifo).ok()?;
    let len = size.checked_sub(table_at)?.min(MAX_PGCI_UT);
    if len < 8 {
        return None;
    }
    let table = read_exact(fs, ifo, table_at, len as usize)?;
    find_menu(&table, menu, sectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One PGC: category byte, cells as (first, last) VOBU sectors, pre-commands.
    type Pgc<'a> = (u8, &'a [(u32, u32)], &'a [[u8; 8]]);

    fn link_pgcn(n: u16) -> [u8; 8] {
        [0x20, 0x04, 0, 0, 0, 0, (n >> 8) as u8 & 0x7F, n as u8]
    }

    fn jump_title_set_root(vts: u8) -> [u8; 8] {
        [0x30, 0x06, 0, 0, vts & 0x7F, 0x80 | 3, 0, 0]
    }

    /// A PGC laid out like an authored one: header, command table, program
    /// map, cell playback table, cell position table.
    fn pgc(cells: &[(u32, u32)], pre: &[[u8; 8]]) -> Vec<u8> {
        let mut p = vec![0u8; 0xEC];
        p[2] = cells.len().min(1) as u8;
        p[3] = cells.len() as u8;
        let commands = p.len();
        let mut table = vec![0u8; 8];
        table[0..2].copy_from_slice(&(pre.len() as u16).to_be_bytes());
        for c in pre {
            table.extend_from_slice(c);
        }
        let last = (table.len() - 1) as u16;
        table[6..8].copy_from_slice(&last.to_be_bytes());
        p.extend(table);
        let program_map = p.len();
        p.extend_from_slice(&[1, 0]);
        let cell_table = p.len();
        for (first, last) in cells {
            let mut c = [0u8; 24];
            c[8..12].copy_from_slice(&first.to_be_bytes());
            c[16..20].copy_from_slice(&last.to_be_bytes());
            p.extend_from_slice(&c);
        }
        let cell_positions = p.len();
        for i in 0..cells.len() {
            p.extend_from_slice(&[0, 1, 0, i as u8 + 1]);
        }
        p[0xE4..0xE6].copy_from_slice(&(commands as u16).to_be_bytes());
        if !cells.is_empty() {
            p[0xE6..0xE8].copy_from_slice(&(program_map as u16).to_be_bytes());
            p[0xE8..0xEA].copy_from_slice(&(cell_table as u16).to_be_bytes());
            p[0xEA..0xEC].copy_from_slice(&(cell_positions as u16).to_be_bytes());
        }
        p
    }

    /// A PGCI unit table with the given language units.
    fn table(units: &[&[Pgc]]) -> Vec<u8> {
        let mut unit_blobs = Vec::new();
        for pgcs in units {
            let blobs: Vec<Vec<u8>> = pgcs.iter().map(|(_, cells, pre)| pgc(cells, pre)).collect();
            let mut unit = vec![0u8; 8 + 8 * pgcs.len()];
            unit[0..2].copy_from_slice(&(pgcs.len() as u16).to_be_bytes());
            let mut at = unit.len();
            for (i, ((category, _, _), blob)) in pgcs.iter().zip(&blobs).enumerate() {
                unit[8 + i * 8] = *category;
                unit[8 + i * 8 + 4..8 + i * 8 + 8].copy_from_slice(&(at as u32).to_be_bytes());
                at += blob.len();
            }
            for blob in blobs {
                unit.extend(blob);
            }
            let last = (unit.len() - 1) as u32;
            unit[4..8].copy_from_slice(&last.to_be_bytes());
            unit_blobs.push(unit);
        }
        let mut t = vec![0u8; 8 + 8 * units.len()];
        t[0..2].copy_from_slice(&(units.len() as u16).to_be_bytes());
        let mut at = t.len();
        for (i, blob) in unit_blobs.iter().enumerate() {
            t[8 + i * 8..8 + i * 8 + 2].copy_from_slice(b"en");
            t[8 + i * 8 + 3] = 0x80;
            t[8 + i * 8 + 4..8 + i * 8 + 8].copy_from_slice(&(at as u32).to_be_bytes());
            at += blob.len();
        }
        for blob in unit_blobs {
            t.extend(blob);
        }
        let last = (t.len() - 1) as u32;
        t[4..8].copy_from_slice(&last.to_be_bytes());
        t
    }

    fn sectors(v: &[u32]) -> Option<MenuLocation> {
        Some(MenuLocation::Sectors(v.to_vec()))
    }

    const BIG: u32 = 1_000_000;

    #[test]
    fn finds_the_wanted_menu() {
        let t = table(&[&[
            (0x85, &[(0, 10)], &[]),            // audio menu
            (0x83, &[(40, 52), (60, 70)], &[]), // root menu: intro cell, loop cell
            (0x87, &[(80, 80)], &[]),           // chapter menu
        ]]);
        assert_eq!(find_menu(&t, Menu::Root, BIG), sectors(&[40, 60, 52]));
        assert_eq!(find_menu(&t, Menu::Title, BIG), None);
    }

    #[test]
    fn follows_router_pgcs() {
        // The entry root PGC has no cells; its pre-commands link to PGC 3
        // (a broken link to PGC 9 first), which holds the menu.
        let pre = [link_pgcn(9), link_pgcn(3)];
        let t = table(&[&[
            (0x83, &[], &pre),
            (0x05, &[(1, 1)], &[]),
            (0x03, &[(30, 30)], &[]),
        ]]);
        assert_eq!(find_menu(&t, Menu::Root, BIG), sectors(&[30]));
    }

    #[test]
    fn link_loops_end() {
        let t = table(&[&[(0x83, &[], &[link_pgcn(2)]), (0x03, &[], &[link_pgcn(1)])]]);
        assert_eq!(find_menu(&t, Menu::Root, BIG), None);
        let t = table(&[&[(0x83, &[], &[link_pgcn(1)])]]);
        assert_eq!(find_menu(&t, Menu::Root, BIG), None);
    }

    #[test]
    fn the_title_menu_may_jump_to_a_title_set() {
        let t = table(&[&[(0x82, &[], &[jump_title_set_root(4)])]]);
        assert_eq!(
            find_menu(&t, Menu::Title, BIG),
            Some(MenuLocation::TitleSetRoot(4))
        );
        // A root menu cannot jump to another title set this way.
        let t = table(&[&[(0x83, &[], &[jump_title_set_root(4)])]]);
        assert_eq!(find_menu(&t, Menu::Root, BIG), None);
    }

    #[test]
    fn needs_an_entry_pgc_and_cells_inside_the_vob() {
        // Not an entry PGC; then an entry PGC whose cell lies past the end of
        // the VOB; the next language unit has a usable one.
        let t = table(&[
            &[(0x03, &[(5, 5)], &[]), (0x83, &[(900, 900)], &[])],
            &[(0x83, &[(7, 9)], &[])],
        ]);
        assert_eq!(find_menu(&t, Menu::Root, 100), sectors(&[7, 9]));
    }

    #[test]
    fn damaged_tables_never_panic() {
        let pre = [link_pgcn(2), jump_title_set_root(1)];
        let base = table(&[&[
            (0x85, &[(0, 10)], &[]),
            (0x83, &[], &pre),
            (0x03, &[(40, 52), (60, 70)], &[]),
        ]]);
        let mut seed = 0x9E37_79B9u32;
        for len in 0..base.len() {
            let _ = find_menu(&base[..len], Menu::Root, BIG);
        }
        for _ in 0..20_000 {
            let mut t = base.clone();
            for _ in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let at = seed as usize % t.len();
                t[at] = (seed >> 8) as u8;
            }
            let _ = find_menu(&t, Menu::Root, BIG);
            let _ = find_menu(&t, Menu::Title, BIG);
        }
    }
}
