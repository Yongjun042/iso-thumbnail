//! Minimal read-only ISO 9660 reader with Joliet support.
//!
//! Used as a fallback for images that have no UDF structures.

use crate::bytes::{slice, u32le, u8_at};
use crate::error::{Error, Result};
use crate::fs::{DirEntry, FileSystem, MAX_DIR_ENTRIES};
use crate::reader::{ByteSource, CachedReader};

const SECTOR: u64 = 2048;
/// Largest directory we are willing to parse.
const MAX_DIR_BYTES: usize = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// First data sector (extended attribute record already skipped).
    pub extent: u32,
    pub size: u32,
    pub is_dir: bool,
}

pub struct Iso9660<'a, S: ByteSource> {
    rd: &'a mut CachedReader<S>,
    root: Node,
    joliet: bool,
}

fn parse_record(r: &[u8]) -> Result<(Node, &[u8])> {
    let len = u8_at(r, 0)? as usize;
    if len < 34 || len > r.len() {
        return Err(Error::Corrupt("directory record"));
    }
    let xar = u8_at(r, 1)? as u32;
    let extent = u32le(r, 2)?
        .checked_add(xar)
        .ok_or(Error::Corrupt("extent overflow"))?;
    let size = u32le(r, 10)?;
    let flags = u8_at(r, 25)?;
    let name_len = u8_at(r, 32)? as usize;
    let name = slice(r, 33, name_len)?;
    Ok((
        Node {
            extent,
            size,
            is_dir: flags & 0x02 != 0,
        },
        name,
    ))
}

fn is_joliet(escape: &[u8]) -> bool {
    escape.starts_with(b"%/@") || escape.starts_with(b"%/C") || escape.starts_with(b"%/E")
}

/// Strips the ";1" version suffix and a trailing dot left by 8.3 mangling.
fn clean_name(name: &str) -> String {
    let base = name.split(';').next().unwrap_or("");
    base.trim_end_matches('.').to_string()
}

impl<'a, S: ByteSource> Iso9660<'a, S> {
    pub fn open(rd: &'a mut CachedReader<S>) -> Result<Self> {
        let mut primary = None;
        let mut joliet = None;
        for s in 16u64..48 {
            let off = s * SECTOR;
            if off + SECTOR > rd.size() {
                break;
            }
            let blk = rd.read_vec(off, SECTOR as usize)?;
            if &blk[1..6] != b"CD001" {
                break;
            }
            match blk[0] {
                1 if primary.is_none() => primary = Some(parse_record(&blk[156..190])?.0),
                2 if joliet.is_none() && is_joliet(&blk[88..120]) => {
                    joliet = Some(parse_record(&blk[156..190])?.0)
                }
                255 => break,
                _ => {}
            }
        }
        let (root, joliet) = match (joliet, primary) {
            (Some(r), _) => (r, true),
            (None, Some(r)) => (r, false),
            (None, None) => return Err(Error::NoVolume),
        };
        Ok(Self { rd, root, joliet })
    }
}

impl<S: ByteSource> FileSystem for Iso9660<'_, S> {
    type Node = Node;

    fn description(&self) -> String {
        if self.joliet {
            "ISO 9660 (Joliet)".to_string()
        } else {
            "ISO 9660".to_string()
        }
    }

    fn root(&mut self) -> Result<Node> {
        Ok(self.root)
    }

    fn walk(&mut self, dir: &Node, visit: &mut dyn FnMut(DirEntry<Node>) -> bool) -> Result<()> {
        if !dir.is_dir {
            return Err(Error::Corrupt("not a directory"));
        }
        if dir.size as usize > MAX_DIR_BYTES {
            return Err(Error::TooLarge);
        }
        let data = self
            .rd
            .read_vec(dir.extent as u64 * SECTOR, dir.size as usize)?;
        let mut pos = 0usize;
        let mut seen = 0usize;
        while pos < data.len() {
            let len = data[pos] as usize;
            if len == 0 {
                // Records never straddle a sector; zero padding means "next sector".
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if pos + len > data.len() {
                break;
            }
            let Ok((node, raw)) = parse_record(&data[pos..pos + len]) else {
                break;
            };
            pos += len;
            seen += 1;
            if seen > MAX_DIR_ENTRIES {
                break;
            }
            if raw == [0] || raw == [1] {
                continue; // "." and ".."
            }
            let name = if self.joliet {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            } else {
                raw.iter().map(|&b| b as char).collect()
            };
            let name = clean_name(&name);
            if name.is_empty() {
                continue;
            }
            let entry = DirEntry {
                name,
                is_dir: node.is_dir,
                node,
            };
            if !visit(entry) {
                break;
            }
        }
        Ok(())
    }

    fn file_size(&mut self, file: &Node) -> Result<u64> {
        Ok(file.size as u64)
    }

    fn read(&mut self, file: &Node, max_len: usize) -> Result<Vec<u8>> {
        if file.is_dir {
            return Err(Error::Corrupt("is a directory"));
        }
        if file.size as usize > max_len {
            return Err(Error::TooLarge);
        }
        self.rd
            .read_vec(file.extent as u64 * SECTOR, file.size as usize)
    }

    fn read_range(&mut self, file: &Node, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let _ = (file, offset, buf);
        Err(Error::Unsupported("read_range is not implemented yet"))
    }
}
