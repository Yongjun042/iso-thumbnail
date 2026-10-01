//! DVD-Video IFO files: where a disc's title menu and root menu start.
//!
//! Only what is needed to find a menu's video is read: the IFO header (its
//! first sector) and the menu program chain information unit table
//! (`VMGM_PGCI_UT` in `VIDEO_TS.IFO`, `VTSM_PGCI_UT` in `VTS_nn_0.IFO`). In
//! that table every language unit lists its menu program chains (PGCs) by
//! menu type; the first cell of the wanted menu's PGC gives the sector, in
//! the menu VOB, where the menu's first VOBU starts and where its last VOBU
//! starts.
//!
//! Layout (all numbers big-endian, offsets in bytes):
//! - IFO header: identifier `DVDVIDEO-VMG` / `DVDVIDEO-VTS` at 0; the sector
//!   of the menu PGCI unit table within the IFO at 0xC8 (VMG) / 0xD0 (VTS).
//! - PGCI unit table: language unit count (u16) at 0, offset of its last
//!   byte (u32) at 4, then 8-byte language unit entries from 8: language code
//!   (u16), reserved, menu existence flags, offset of the unit (u32, from the
//!   start of the table).
//! - Language unit: PGC count (u16) at 0, offset of its last byte (u32) at 4,
//!   then 8-byte search pointers from 8: category byte (bit 7 = entry PGC,
//!   low four bits = menu type: 2 title, 3 root, 4 sub-picture, 5 audio,
//!   6 angle, 7 chapter), three more category bytes, offset of the PGC (u32,
//!   from the start of the unit).
//! - PGC: cell count (u8) at 3, offset of the cell playback table (u16, from
//!   the start of the PGC) at 0xE8. A cell playback entry is 24 bytes: the
//!   first VOBU's start sector (u32) at 8 and the last VOBU's start sector
//!   (u32) at 16, both relative to the start of the menu VOB.
//!
//! IFO files are never scrambled. Everything is bounds-checked; anything
//! unexpected gives `None`, and the caller falls back to other sources.

use crate::fs::FileSystem;

const SECTOR: u64 = 2048;
/// Largest menu PGCI unit table read (real ones are a few KiB).
const MAX_PGCI_UT: u32 = 256 << 10;
/// Language units and PGC search pointers examined (the format allows 99).
const MAX_UNITS: usize = 100;

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

/// Where a menu's video lies in its menu VOB, in 2048-byte sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuCell {
    /// Start of the first VOBU of the menu's first cell.
    pub first_vobu: u32,
    /// Start of the last VOBU of that cell (the same for a still menu). For a
    /// motion menu this is where an opening animation has settled.
    pub last_vobu: u32,
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

/// Finds the first cell of `menu` in the menu PGCI unit table `table`.
/// Language units are tried in order; the first that has the menu wins.
pub fn find_menu_cell(table: &[u8], menu: Menu) -> Option<MenuCell> {
    let units = usize::from(be16(table, 0)?).min(MAX_UNITS);
    for u in 0..units {
        let entry = 8 + u * 8;
        let unit_at = be32(table, entry + 4)? as usize;
        let Some(unit) = table.get(unit_at..) else {
            continue;
        };
        if let Some(cell) = cell_in_unit(unit, menu) {
            return Some(cell);
        }
    }
    None
}

/// The first cell of the entry PGC for `menu` in one language unit.
fn cell_in_unit(unit: &[u8], menu: Menu) -> Option<MenuCell> {
    let pgcs = usize::from(be16(unit, 0)?).min(MAX_UNITS);
    for p in 0..pgcs {
        let srp = 8 + p * 8;
        let category = *unit.get(srp)?;
        if category & 0x80 == 0 || category & 0x0F != menu.id() {
            continue;
        }
        let pgc_at = be32(unit, srp + 4)? as usize;
        let Some(pgc) = unit.get(pgc_at..) else {
            continue;
        };
        if let Some(cell) = first_cell(pgc) {
            return Some(cell);
        }
    }
    None
}

/// The first cell of a PGC; `None` for a PGC without cells (a menu that only
/// runs navigation commands).
fn first_cell(pgc: &[u8]) -> Option<MenuCell> {
    let cells = *pgc.get(3)?;
    let table = usize::from(be16(pgc, 0xE8)?);
    if cells == 0 || table == 0 {
        return None;
    }
    let first_vobu = be32(pgc, table + 8)?;
    let last_vobu = be32(pgc, table + 16)?;
    Some(MenuCell {
        first_vobu,
        last_vobu: last_vobu.max(first_vobu),
    })
}

/// Reads `ifo` (a `VIDEO_TS.IFO` for `Menu::Title`, a `VTS_nn_0.IFO` for
/// `Menu::Root`) and returns where that menu starts in its menu VOB.
pub fn menu_cell<F: FileSystem>(fs: &mut F, ifo: &F::Node, menu: Menu) -> Option<MenuCell> {
    let header = read_exact(fs, ifo, 0, SECTOR as usize)?;
    if &header[..12] != menu.identifier() {
        return None;
    }
    let table_sector = be32(&header, menu.table_pointer())?;
    if table_sector == 0 {
        return None;
    }
    let table_at = u64::from(table_sector) * SECTOR;
    let head = read_exact(fs, ifo, table_at, 8)?;
    let len = be32(&head, 4)?.checked_add(1)?.min(MAX_PGCI_UT);
    let table = read_exact(fs, ifo, table_at, len as usize)?;
    find_menu_cell(&table, menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One PGC: its category byte and its cells as (first, last) VOBU sectors.
    type Pgc<'a> = (u8, &'a [(u32, u32)]);

    /// A PGCI unit table with the given language units, each a list of PGCs.
    fn table(units: &[&[Pgc]]) -> Vec<u8> {
        let mut unit_blobs = Vec::new();
        for pgcs in units {
            let mut pgc_blobs = Vec::new();
            for (_, cells) in pgcs.iter() {
                let mut pgc = vec![0u8; 0xEC];
                pgc[3] = cells.len() as u8;
                pgc[0xE8..0xEA].copy_from_slice(&0xECu16.to_be_bytes());
                for (first, last) in cells.iter() {
                    let mut cell = [0u8; 24];
                    cell[8..12].copy_from_slice(&first.to_be_bytes());
                    cell[16..20].copy_from_slice(&last.to_be_bytes());
                    pgc.extend_from_slice(&cell);
                }
                pgc_blobs.push(pgc);
            }
            let mut unit = vec![0u8; 8 + 8 * pgcs.len()];
            unit[0..2].copy_from_slice(&(pgcs.len() as u16).to_be_bytes());
            let mut at = unit.len();
            for (i, ((category, _), blob)) in pgcs.iter().zip(&pgc_blobs).enumerate() {
                unit[8 + i * 8] = *category;
                unit[8 + i * 8 + 4..8 + i * 8 + 8].copy_from_slice(&(at as u32).to_be_bytes());
                at += blob.len();
            }
            for blob in pgc_blobs {
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

    #[test]
    fn finds_the_wanted_menu() {
        let t = table(&[&[
            (0x85, &[(0, 10)]),            // audio menu
            (0x83, &[(40, 52), (60, 70)]), // root menu, two cells
            (0x87, &[(80, 80)]),           // chapter menu
        ]]);
        assert_eq!(
            find_menu_cell(&t, Menu::Root),
            Some(MenuCell {
                first_vobu: 40,
                last_vobu: 52
            })
        );
        assert_eq!(find_menu_cell(&t, Menu::Title), None);
    }

    #[test]
    fn needs_an_entry_pgc_with_cells() {
        // Not an entry PGC, then an entry PGC without cells (commands only),
        // then the language unit that has a usable one.
        let t = table(&[&[(0x03, &[(5, 5)]), (0x83, &[])], &[(0x82, &[(7, 9)])]]);
        assert_eq!(find_menu_cell(&t, Menu::Root), None);
        assert_eq!(
            find_menu_cell(&t, Menu::Title),
            Some(MenuCell {
                first_vobu: 7,
                last_vobu: 9
            })
        );
    }

    #[test]
    fn a_last_vobu_before_the_first_is_ignored() {
        let t = table(&[&[(0x83, &[(40, 3)])]]);
        assert_eq!(
            find_menu_cell(&t, Menu::Root),
            Some(MenuCell {
                first_vobu: 40,
                last_vobu: 40
            })
        );
    }

    #[test]
    fn damaged_tables_never_panic() {
        let base = table(&[&[(0x85, &[(0, 10)]), (0x83, &[(40, 52), (60, 70)])]]);
        let mut seed = 0x9E37_79B9u32;
        for len in 0..base.len() {
            let _ = find_menu_cell(&base[..len], Menu::Root);
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
            let _ = find_menu_cell(&t, Menu::Root);
            let _ = find_menu_cell(&t, Menu::Title);
        }
    }
}
