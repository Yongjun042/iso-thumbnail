//! Minimal read-only UDF reader (ECMA-167 3rd edition, OSTA UDF 1.02 – 2.60).
//!
//! Only what is needed to walk directories and read small files:
//! anchor → volume descriptor sequence → partition maps (physical, sparable,
//! metadata, virtual/VAT) → file set descriptor → file entries → file
//! identifier descriptors. Every access is bounds-checked and capped, since
//! the handler runs on arbitrary files.

use crate::bytes::{slice, u16le, u32le, u64le, u8_at};
use crate::error::{Error, Result};
use crate::fs::{DirEntry, FileSystem, MAX_DIR_ENTRIES};
use crate::reader::{ByteSource, CachedReader};

const TAG_AVDP: u16 = 2;
const TAG_VDP: u16 = 3;
const TAG_PD: u16 = 5;
const TAG_LVD: u16 = 6;
const TAG_TD: u16 = 8;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_AED: u16 = 258;
const TAG_IE: u16 = 259;
const TAG_FE: u16 = 261;
const TAG_EFE: u16 = 266;

const FT_DIRECTORY: u8 = 4;
const FT_VAT: u8 = 248;

/// Largest directory we are willing to parse.
const MAX_DIR_BYTES: usize = 4 << 20;
/// Largest virtual allocation table we are willing to load.
const MAX_VAT_BYTES: usize = 8 << 20;
/// Most extents one file may consist of. A stereoscopic Blu-ray's main clip
/// is interleaved with its dependent view in a few thousand extents.
const MAX_EXTENTS: usize = 8192;
/// Most Allocation Extent Descriptors followed for one file; each is one block.
const MAX_AED_DEPTH: u32 = 128;
/// Most blocks scanned in one volume descriptor sequence extent.
const MAX_VDS_BLOCKS: u32 = 64;

/// Offsets that differ between a File Entry and an Extended File Entry:
/// (length of extended attributes, length of allocation descriptors, start of EA area).
struct FeLayout {
    ea_len: usize,
    ad_len: usize,
    base: usize,
}
const FE_LAYOUT: FeLayout = FeLayout {
    ea_len: 168,
    ad_len: 172,
    base: 176,
};
const EFE_LAYOUT: FeLayout = FeLayout {
    ea_len: 208,
    ad_len: 212,
    base: 216,
};

/// ICB address (logical block + partition reference number); the node handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icb {
    pub block: u32,
    pub part: u16,
}

struct Partition {
    number: u16,
    start: u32,
}

enum MapKind {
    /// Type 1 map, or a sparable map treated as one (sparing tables are ignored).
    Physical,
    /// UDF 2.50+ metadata partition: logical blocks live in the extents of the
    /// metadata file. Each entry: (first logical block of the extent, number of
    /// blocks, absolute sector of the first block or `None` when the extent is
    /// not recorded). The first blocks are strictly increasing.
    Metadata(Vec<(u64, u32, Option<u64>)>),
    /// Virtual partition on write-once media: the VAT maps virtual to physical blocks.
    Virtual(Vec<u32>),
}

struct PartMap {
    part_idx: usize,
    kind: MapKind,
}

enum RawMap {
    Physical {
        number: u16,
    },
    Metadata {
        number: u16,
        file_loc: u32,
        mirror_loc: u32,
    },
    Virtual {
        number: u16,
    },
    Unknown,
}

struct Lvd {
    seq: u32,
    block_size: u32,
    fsd: Icb,
    revision: u16,
    raw_maps: Vec<RawMap>,
}

#[derive(Default)]
struct Vds {
    parts: Vec<Partition>,
    lvd: Option<Lvd>,
}

struct Extent {
    /// Absolute sector of the first byte (meaningless when `recorded` is false).
    sector: u64,
    len: u32,
    recorded: bool,
}

struct Inode {
    file_type: u8,
    size: u64,
    extents: Vec<Extent>,
    /// Data stored inline in the file entry (allocation type 3).
    embedded: Option<Vec<u8>>,
}

pub struct Udf<'a, S: ByteSource> {
    rd: &'a mut CachedReader<S>,
    bs: u32,
    parts: Vec<Partition>,
    maps: Vec<Option<PartMap>>,
    root: Icb,
    revision: u16,
    has_metadata: bool,
    /// The file `read_range` read last, with its parsed extents: ranged reads
    /// of one large file come one after the other, and parsing a fragmented
    /// file's descriptors again for each would read its AED blocks again.
    last_file: Option<(Icb, Inode)>,
}

/// Returns the tag identifier if the 16-byte descriptor tag checksum is valid.
fn tag_id(block: &[u8]) -> Option<u16> {
    let head = block.get(..16)?;
    let mut sum = 0u8;
    for (i, &v) in head.iter().enumerate() {
        if i != 4 {
            sum = sum.wrapping_add(v);
        }
    }
    if sum != head[4] {
        return None;
    }
    Some(u16::from_le_bytes([head[0], head[1]]))
}

/// Decodes an OSTA compressed Unicode d-string (compression id 8 or 16).
fn decode_name(raw: &[u8]) -> Option<String> {
    let (&comp, rest) = raw.split_first()?;
    match comp {
        8 | 254 => Some(rest.iter().map(|&b| b as char).collect()),
        16 | 255 => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

fn find_anchor<S: ByteSource>(rd: &mut CachedReader<S>) -> Result<(u32, Vec<u8>)> {
    let size = rd.size();
    let mut candidates: Vec<(u32, u64)> = vec![(2048, 256), (512, 256), (1024, 256), (4096, 256)];
    let sectors = size / 2048;
    if sectors > 257 {
        candidates.push((2048, sectors - 1));
        candidates.push((2048, sectors - 257));
    }
    for (bs, sector) in candidates {
        let off = sector * bs as u64;
        if off + bs as u64 > size {
            continue;
        }
        let blk = rd.read_vec(off, bs as usize)?;
        if tag_id(&blk) == Some(TAG_AVDP) {
            return Ok((bs, blk));
        }
    }
    Err(Error::NoVolume)
}

fn parse_lvd(b: &[u8]) -> Result<Lvd> {
    let seq = u32le(b, 16)?;
    let block_size = u32le(b, 212)?;
    // Domain identifier regid at 216: flags(1) + identifier(23) + suffix(8);
    // the suffix starts with the UDF revision in BCD.
    let revision = u16le(b, 240)?;
    let fsd = Icb {
        block: u32le(b, 252)?,
        part: u16le(b, 256)?,
    };
    let map_len = u32le(b, 264)? as usize;
    let n_maps = u32le(b, 268)? as usize;
    let end = 440usize.saturating_add(map_len).min(b.len());
    let mut off = 440usize;
    let mut raw_maps = Vec::new();
    for _ in 0..n_maps.min(16) {
        if off + 2 > end {
            break;
        }
        let ty = u8_at(b, off)?;
        let len = u8_at(b, off + 1)? as usize;
        if len < 2 || off + len > end {
            break;
        }
        let map = match ty {
            1 if len >= 6 => RawMap::Physical {
                number: u16le(b, off + 4)?,
            },
            2 if len >= 64 => {
                let ident = slice(b, off + 5, 23)?;
                let number = u16le(b, off + 38)?;
                if ident.starts_with(b"*UDF Metadata Partition") {
                    RawMap::Metadata {
                        number,
                        file_loc: u32le(b, off + 40)?,
                        mirror_loc: u32le(b, off + 44)?,
                    }
                } else if ident.starts_with(b"*UDF Sparable Partition") {
                    RawMap::Physical { number }
                } else if ident.starts_with(b"*UDF Virtual Partition") {
                    RawMap::Virtual { number }
                } else {
                    RawMap::Unknown
                }
            }
            _ => RawMap::Unknown,
        };
        raw_maps.push(map);
        off += len;
    }
    Ok(Lvd {
        seq,
        block_size,
        fsd,
        revision,
        raw_maps,
    })
}

fn read_vds<S: ByteSource>(
    rd: &mut CachedReader<S>,
    bs: u32,
    mut loc: u32,
    len: u32,
) -> Result<Vds> {
    let mut vds = Vds::default();
    let mut count = (len / bs).min(MAX_VDS_BLOCKS);
    let mut hops = 0;
    let mut i = 0;
    while i < count {
        let blk = rd.read_vec((loc as u64 + i as u64) * bs as u64, bs as usize)?;
        i += 1;
        match tag_id(&blk) {
            Some(TAG_PD) => vds.parts.push(Partition {
                number: u16le(&blk, 22)?,
                start: u32le(&blk, 188)?,
            }),
            Some(TAG_LVD) => {
                let lvd = parse_lvd(&blk)?;
                if vds.lvd.as_ref().is_none_or(|old| lvd.seq >= old.seq) {
                    vds.lvd = Some(lvd);
                }
            }
            Some(TAG_VDP) => {
                // Volume Descriptor Pointer: the sequence continues in another extent.
                hops += 1;
                if hops > 4 {
                    break;
                }
                count = (u32le(&blk, 20)? / bs).min(MAX_VDS_BLOCKS);
                loc = u32le(&blk, 24)?;
                i = 0;
            }
            Some(TAG_TD) | None => break,
            Some(_) => {}
        }
    }
    Ok(vds)
}

fn decode_vat(data: &[u8]) -> Result<Vec<u32>> {
    let n = data.len();
    let entries = if n >= 36 && data[n - 35..].starts_with(b"*UDF Virtual Alloc Tbl") {
        // UDF 1.50 layout: entries, then a regid and the previous VAT location.
        &data[..n - 36]
    } else {
        // UDF 2.00+ layout: header (its length in the first two bytes), then entries.
        let header = u16le(data, 0)? as usize;
        data.get(header..).ok_or(Error::Corrupt("VAT header"))?
    };
    Ok(entries
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

impl<'a, S: ByteSource> Udf<'a, S> {
    pub fn open(rd: &'a mut CachedReader<S>) -> Result<Self> {
        let (bs, avdp) = find_anchor(rd)?;
        let main = (u32le(&avdp, 16)?, u32le(&avdp, 20)?);
        let reserve = (u32le(&avdp, 24)?, u32le(&avdp, 28)?);
        let mut vds = read_vds(rd, bs, main.1, main.0).unwrap_or_default();
        if vds.lvd.is_none() || vds.parts.is_empty() {
            vds = read_vds(rd, bs, reserve.1, reserve.0)?;
        }
        let lvd = vds
            .lvd
            .ok_or(Error::Corrupt("no logical volume descriptor"))?;
        if lvd.block_size != bs {
            return Err(Error::Unsupported(
                "logical block size differs from sector size",
            ));
        }
        let mut udf = Udf {
            rd,
            bs,
            parts: vds.parts,
            maps: Vec::new(),
            root: Icb { block: 0, part: 0 },
            revision: lvd.revision,
            has_metadata: false,
            last_file: None,
        };

        // Pass 1: maps that address a physical partition directly.
        let mut pending = Vec::new();
        for (i, raw) in lvd.raw_maps.into_iter().enumerate() {
            match raw {
                RawMap::Physical { number } => {
                    let map = udf.part_index(number).map(|part_idx| PartMap {
                        part_idx,
                        kind: MapKind::Physical,
                    });
                    udf.maps.push(map);
                }
                other => {
                    udf.maps.push(None);
                    pending.push((i, other));
                }
            }
        }
        // Pass 2: maps layered on top of a physical partition.
        for (i, raw) in pending {
            let map = match raw {
                RawMap::Metadata {
                    number,
                    file_loc,
                    mirror_loc,
                } => {
                    let part_idx = udf
                        .part_index(number)
                        .ok_or(Error::Corrupt("metadata map references unknown partition"))?;
                    let phys_ref = udf
                        .physical_ref(number)
                        .ok_or(Error::Corrupt("metadata map without physical map"))?;
                    let extents = udf
                        .load_metadata_extents(part_idx, phys_ref, file_loc)
                        .or_else(|_| udf.load_metadata_extents(part_idx, phys_ref, mirror_loc))?;
                    udf.has_metadata = true;
                    Some(PartMap {
                        part_idx,
                        kind: MapKind::Metadata(extents),
                    })
                }
                RawMap::Virtual { number } => {
                    let part_idx = udf
                        .part_index(number)
                        .ok_or(Error::Corrupt("virtual map references unknown partition"))?;
                    let phys_ref = udf
                        .physical_ref(number)
                        .ok_or(Error::Corrupt("virtual map without physical map"))?;
                    let vat = udf.load_vat(phys_ref)?;
                    Some(PartMap {
                        part_idx,
                        kind: MapKind::Virtual(vat),
                    })
                }
                _ => None,
            };
            udf.maps[i] = map;
        }

        let fsd_sector = udf.sector_of(lvd.fsd.block, lvd.fsd.part)?;
        let fsd = udf.read_block(fsd_sector)?;
        if tag_id(&fsd) != Some(TAG_FSD) {
            return Err(Error::Corrupt("file set descriptor"));
        }
        udf.root = Icb {
            block: u32le(&fsd, 404)?,
            part: u16le(&fsd, 408)?,
        };
        Ok(udf)
    }

    fn part_index(&self, number: u16) -> Option<usize> {
        self.parts.iter().position(|p| p.number == number)
    }

    /// Partition reference number of the type 1 map for physical partition `number`.
    fn physical_ref(&self, number: u16) -> Option<u16> {
        self.maps
            .iter()
            .position(|m| {
                matches!(m, Some(PartMap { part_idx, kind: MapKind::Physical })
                if self.parts.get(*part_idx).map(|p| p.number) == Some(number))
            })
            .map(|i| i as u16)
    }

    fn sector_of(&self, block: u32, part_ref: u16) -> Result<u64> {
        let map = self
            .maps
            .get(part_ref as usize)
            .and_then(|m| m.as_ref())
            .ok_or(Error::Unsupported("partition map"))?;
        let part = self
            .parts
            .get(map.part_idx)
            .ok_or(Error::Corrupt("partition"))?;
        match &map.kind {
            MapKind::Physical => Ok(part.start as u64 + block as u64),
            MapKind::Metadata(extents) => {
                // Binary search: this runs for every descriptor of every entry.
                let block = block as u64;
                let i = extents.partition_point(|&(first, _, _)| first <= block);
                let &(first, blocks, sector) = i
                    .checked_sub(1)
                    .and_then(|i| extents.get(i))
                    .ok_or(Error::Corrupt("metadata block outside metadata file"))?;
                let rel = block - first;
                if rel >= blocks as u64 {
                    return Err(Error::Corrupt("metadata block outside metadata file"));
                }
                sector
                    .map(|s| s + rel)
                    .ok_or(Error::Corrupt("metadata block not recorded"))
            }
            MapKind::Virtual(vat) => {
                let phys = *vat
                    .get(block as usize)
                    .ok_or(Error::Corrupt("virtual block outside VAT"))?;
                if phys == u32::MAX {
                    return Err(Error::Corrupt("unallocated virtual block"));
                }
                Ok(part.start as u64 + phys as u64)
            }
        }
    }

    fn read_block(&mut self, sector: u64) -> Result<Vec<u8>> {
        let bs = self.bs as u64;
        let off = sector
            .checked_mul(bs)
            .ok_or(Error::Corrupt("sector overflow"))?;
        self.rd.read_vec(off, self.bs as usize)
    }

    fn load_metadata_extents(
        &mut self,
        part_idx: usize,
        phys_ref: u16,
        file_loc: u32,
    ) -> Result<Vec<(u64, u32, Option<u64>)>> {
        let sector = self.parts[part_idx].start as u64 + file_loc as u64;
        let blk = self.read_block(sector)?;
        let inode = match tag_id(&blk) {
            Some(TAG_FE) => self.parse_fe(&blk, phys_ref, &FE_LAYOUT)?,
            Some(TAG_EFE) => self.parse_fe(&blk, phys_ref, &EFE_LAYOUT)?,
            _ => return Err(Error::Corrupt("metadata file entry")),
        };
        let bs = self.bs;
        // Every extent is at least one block long (parse_ads stops at length 0),
        // so the first blocks are strictly increasing.
        let mut first = 0u64;
        let extents: Vec<(u64, u32, Option<u64>)> = inode
            .extents
            .iter()
            .map(|e| {
                let blocks = e.len.div_ceil(bs);
                let item = (first, blocks, e.recorded.then_some(e.sector));
                first += blocks as u64;
                item
            })
            .collect();
        if extents.is_empty() {
            return Err(Error::Corrupt("empty metadata file"));
        }
        Ok(extents)
    }

    /// The VAT ICB lives in the last recorded sector of write-once media.
    fn load_vat(&mut self, phys_ref: u16) -> Result<Vec<u32>> {
        let total = self.rd.size() / self.bs as u64;
        for back in 0..16u64 {
            let Some(sector) = total.checked_sub(1 + back) else {
                break;
            };
            let Ok(blk) = self.read_block(sector) else {
                continue;
            };
            let layout = match tag_id(&blk) {
                Some(TAG_FE) => &FE_LAYOUT,
                Some(TAG_EFE) => &EFE_LAYOUT,
                _ => continue,
            };
            if u8_at(&blk, 27)? != FT_VAT {
                continue;
            }
            let inode = self.parse_fe(&blk, phys_ref, layout)?;
            let data = self.read_data(&inode, MAX_VAT_BYTES)?;
            return decode_vat(&data);
        }
        Err(Error::Unsupported("virtual partition without VAT"))
    }

    /// Reads the File Entry or Extended File Entry at `icb`, following
    /// strategy-4096 Indirect Entries. Returns the block, the ICB it was found
    /// at, and its layout.
    fn file_entry(&mut self, icb: Icb) -> Result<(Vec<u8>, Icb, &'static FeLayout)> {
        let mut icb = icb;
        for _ in 0..4 {
            let sector = self.sector_of(icb.block, icb.part)?;
            let blk = self.read_block(sector)?;
            match tag_id(&blk) {
                Some(TAG_FE) => return Ok((blk, icb, &FE_LAYOUT)),
                Some(TAG_EFE) => return Ok((blk, icb, &EFE_LAYOUT)),
                Some(TAG_IE) => {
                    // Indirect entry: the Indirect ICB long_ad at byte 36 points
                    // at the real entry (its lb_addr starts at byte 40).
                    icb = Icb {
                        block: u32le(&blk, 40)?,
                        part: u16le(&blk, 44)?,
                    };
                }
                _ => return Err(Error::Corrupt("expected file entry")),
            }
        }
        Err(Error::Corrupt("indirect entry chain too long"))
    }

    fn read_inode(&mut self, icb: Icb) -> Result<Inode> {
        let (blk, icb, layout) = self.file_entry(icb)?;
        self.parse_fe(&blk, icb.part, layout)
    }

    fn parse_fe(&mut self, blk: &[u8], part: u16, layout: &FeLayout) -> Result<Inode> {
        let file_type = u8_at(blk, 27)?;
        let flags = u16le(blk, 34)?;
        let size = u64le(blk, 56)?;
        let l_ea = u32le(blk, layout.ea_len)? as usize;
        let l_ad = u32le(blk, layout.ad_len)? as usize;
        let ads = slice(blk, layout.base.saturating_add(l_ea), l_ad)?;
        let ad_type = (flags & 7) as u8;
        if ad_type == 3 {
            let n = (size.min(l_ad as u64)) as usize;
            return Ok(Inode {
                file_type,
                size,
                extents: Vec::new(),
                embedded: Some(ads[..n].to_vec()),
            });
        }
        let mut extents = Vec::new();
        self.parse_ads(ads, ad_type, part, &mut extents)?;
        Ok(Inode {
            file_type,
            size,
            extents,
            embedded: None,
        })
    }

    /// Collects the extents of a file from its allocation descriptors,
    /// following Allocation Extent Descriptor continuations iteratively (a
    /// stereoscopic Blu-ray's interleaved main clip needs a few dozen).
    fn parse_ads(
        &mut self,
        ads: &[u8],
        ad_type: u8,
        part: u16,
        out: &mut Vec<Extent>,
    ) -> Result<()> {
        let step = match ad_type {
            0 => 8,
            1 => 16,
            2 => 20,
            _ => return Err(Error::Unsupported("allocation descriptor type")),
        };
        let mut ads = ads.to_vec();
        let mut part = part;
        for depth in 0..=MAX_AED_DEPTH {
            let mut continuation = None;
            let mut off = 0usize;
            while off + step <= ads.len() {
                let raw = u32le(&ads, off)?;
                let len = raw & 0x3FFF_FFFF;
                let kind = raw >> 30;
                if len == 0 {
                    break;
                }
                let (block, p) = match ad_type {
                    0 => (u32le(&ads, off + 4)?, part),
                    1 => (u32le(&ads, off + 4)?, u16le(&ads, off + 8)?),
                    _ => (u32le(&ads, off + 12)?, u16le(&ads, off + 16)?),
                };
                off += step;
                if kind == 3 {
                    // Continuation: the extent holds an Allocation Extent Descriptor.
                    continuation = Some((block, p));
                    break;
                }
                if out.len() >= MAX_EXTENTS {
                    return Err(Error::TooLarge);
                }
                let recorded = kind == 0;
                let sector = if recorded {
                    self.sector_of(block, p)?
                } else {
                    0
                };
                out.push(Extent {
                    sector,
                    len,
                    recorded,
                });
            }
            let Some((block, p)) = continuation else {
                return Ok(());
            };
            if depth == MAX_AED_DEPTH {
                break;
            }
            let sector = self.sector_of(block, p)?;
            let blk = self.read_block(sector)?;
            if tag_id(&blk) != Some(TAG_AED) {
                return Err(Error::Corrupt("expected allocation extent descriptor"));
            }
            let l_ad = u32le(&blk, 20)? as usize;
            ads = slice(&blk, 24, l_ad)?.to_vec();
            part = p;
        }
        Err(Error::Corrupt("allocation extent chain too deep"))
    }

    fn read_data(&mut self, inode: &Inode, max_len: usize) -> Result<Vec<u8>> {
        if inode.size > max_len as u64 {
            return Err(Error::TooLarge);
        }
        let want = inode.size as usize;
        if let Some(embedded) = &inode.embedded {
            return Ok(embedded[..want.min(embedded.len())].to_vec());
        }
        // Never allocate more than the allocation descriptors can deliver.
        let described: u64 = inode.extents.iter().map(|e| e.len as u64).sum();
        let want = want.min(usize::try_from(described).unwrap_or(usize::MAX));
        let bs = self.bs as u64;
        let mut out = vec![0u8; want];
        let mut pos = 0usize;
        for ext in &inode.extents {
            if pos >= want {
                break;
            }
            let n = (ext.len as usize).min(want - pos);
            if ext.recorded {
                let off = ext
                    .sector
                    .checked_mul(bs)
                    .ok_or(Error::Corrupt("sector overflow"))?;
                self.rd.read_exact(off, &mut out[pos..pos + n])?;
            }
            pos += n;
        }
        out.truncate(pos);
        Ok(out)
    }

    /// Fills `buf` with the bytes of `inode` from `offset` on, clipped to the
    /// Information Length, and returns how many bytes that is. Extents are
    /// consecutive in the file; unrecorded extents and whatever lies past the
    /// last extent (or past the embedded data) read as zeros. Only the
    /// requested range is touched, so the cost does not grow with the file.
    fn read_inode_range(&mut self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= inode.size {
            return Ok(0);
        }
        let left = inode.size - offset;
        let n = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let out = &mut buf[..n];
        if let Some(embedded) = &inode.embedded {
            let tail = usize::try_from(offset)
                .ok()
                .and_then(|start| embedded.get(start..))
                .unwrap_or(&[]);
            let k = tail.len().min(n);
            out[..k].copy_from_slice(&tail[..k]);
            out[k..].fill(0);
            return Ok(n);
        }
        let bs = self.bs as u64;
        let end = offset.saturating_add(n as u64);
        // File offset of the current extent; at most MAX_EXTENTS * 1 GiB.
        let mut ext_start = 0u64;
        let mut pos = offset;
        for ext in &inode.extents {
            if pos >= end {
                break;
            }
            let ext_end = ext_start.saturating_add(ext.len as u64);
            if pos < ext_end {
                let within = pos.saturating_sub(ext_start);
                let k = (ext_end.min(end) - pos) as usize;
                let done = (pos - offset) as usize;
                let dst = &mut out[done..done + k];
                if ext.recorded {
                    let at = ext
                        .sector
                        .checked_mul(bs)
                        .and_then(|o| o.checked_add(within))
                        .ok_or(Error::Corrupt("sector overflow"))?;
                    self.rd.read_exact(at, dst)?;
                } else {
                    dst.fill(0);
                }
                pos += k as u64;
            }
            ext_start = ext_end;
        }
        let done = (pos - offset) as usize;
        out[done..].fill(0);
        Ok(n)
    }
}

impl<S: ByteSource> FileSystem for Udf<'_, S> {
    type Node = Icb;

    fn description(&self) -> String {
        format!(
            "UDF {:x}.{:02x}{}",
            self.revision >> 8,
            self.revision & 0xff,
            if self.has_metadata {
                " (metadata partition)"
            } else {
                ""
            }
        )
    }

    fn root(&mut self) -> Result<Icb> {
        Ok(self.root)
    }

    fn walk(&mut self, dir: &Icb, visit: &mut dyn FnMut(DirEntry<Icb>) -> bool) -> Result<()> {
        let inode = self.read_inode(*dir)?;
        if inode.file_type != FT_DIRECTORY {
            return Err(Error::Corrupt("not a directory"));
        }
        let data = self.read_data(&inode, MAX_DIR_BYTES)?;
        let mut pos = 0usize;
        let mut seen = 0usize;
        while pos + 38 <= data.len() {
            let fid = &data[pos..];
            if tag_id(fid) != Some(TAG_FID) {
                break;
            }
            let chars = u8_at(fid, 18)?;
            let l_fi = u8_at(fid, 19)? as usize;
            let l_iu = u16le(fid, 36)? as usize;
            let total = (38 + l_iu + l_fi + 3) & !3;
            if total > fid.len() {
                break;
            }
            let icb = Icb {
                block: u32le(fid, 24)?,
                part: u16le(fid, 28)?,
            };
            let name_raw = &fid[38 + l_iu..38 + l_iu + l_fi];
            pos += total;
            seen += 1;
            if seen > MAX_DIR_ENTRIES {
                break;
            }
            // Skip deleted entries (bit 2) and the parent link (bit 3).
            if chars & 0x0C != 0 || l_fi == 0 {
                continue;
            }
            if let Some(name) = decode_name(name_raw) {
                let entry = DirEntry {
                    name,
                    is_dir: chars & 0x02 != 0,
                    node: icb,
                };
                if !visit(entry) {
                    break;
                }
            }
        }
        Ok(())
    }

    fn file_size(&mut self, file: &Icb) -> Result<u64> {
        // Information Length only (byte 56 in both FE and EFE): allocation
        // descriptors are parsed just for the file that is actually read.
        let (blk, _, _) = self.file_entry(*file)?;
        u64le(&blk, 56)
    }

    fn read(&mut self, file: &Icb, max_len: usize) -> Result<Vec<u8>> {
        let inode = self.read_inode(*file)?;
        if inode.file_type == FT_DIRECTORY {
            return Err(Error::Corrupt("is a directory"));
        }
        self.read_data(&inode, max_len)
    }

    fn read_range(&mut self, file: &Icb, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // The file entry and its allocation descriptors are parsed once per
        // file and kept for the following calls; the file data itself is only
        // read for the requested range, so a 1 GiB VOB costs no more than a
        // small file.
        let inode = match self.last_file.take() {
            Some((icb, inode)) if icb == *file => inode,
            _ => self.read_inode(*file)?,
        };
        if inode.file_type == FT_DIRECTORY {
            return Err(Error::Corrupt("is a directory"));
        }
        let result = self.read_inode_range(&inode, offset, buf);
        self.last_file = Some((*file, inode));
        result
    }
}
