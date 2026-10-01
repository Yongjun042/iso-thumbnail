//! Builds small disc images with an arbitrary directory tree, for tests.
//!
//! `iso9660` writes a plain ISO 9660 image (no Joliet); `udf102` writes a UDF
//! 1.02 image with one type 1 partition, short allocation descriptors and
//! 8-bit names, laid out like a DVD's UDF side. Both take a list of
//! `(path, contents)` pairs with `/`-separated paths; directories are created
//! as needed. Files can be split into several allocation extents in the UDF
//! image (`UdfOptions::extent_blocks`) to exercise extent mapping.
//!
//! Include from a test file with
//! `#[path = "support/disc_builder.rs"] mod disc_builder;`.

#![allow(dead_code)]

use std::collections::BTreeMap;

pub const SECTOR: usize = 2048;

/// A directory tree: subdirectories and files by name, in sorted order.
#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<String, Dir<'a>>,
    files: BTreeMap<String, &'a [u8]>,
}

fn tree<'a>(files: &[(&str, &'a [u8])]) -> Dir<'a> {
    let mut root = Dir::default();
    for (path, data) in files {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let name = parts.pop().expect("file path has a name");
        let mut dir = &mut root;
        for p in parts {
            dir = dir.dirs.entry(p.to_string()).or_default();
        }
        dir.files.insert(name.to_string(), data);
    }
    root
}

fn put(img: &mut Vec<u8>, offset: usize, bytes: &[u8]) {
    if img.len() < offset + bytes.len() {
        img.resize(offset + bytes.len(), 0);
    }
    img[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn sectors(len: usize) -> usize {
    len.div_ceil(SECTOR).max(1)
}

// ----------------------------------------------------------------------------
// ISO 9660
// ----------------------------------------------------------------------------

fn both16(v: u16) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..].copy_from_slice(&v.to_be_bytes());
    b
}

fn both32(v: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..].copy_from_slice(&v.to_be_bytes());
    b
}

fn iso_record(name: &[u8], extent: u32, size: u32, dir: bool) -> Vec<u8> {
    let mut r = vec![0u8; 33 + name.len()];
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r[2..10].copy_from_slice(&both32(extent));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]); // 2026-01-01
    r[25] = if dir { 2 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    r
}

/// Appends records to a directory, never letting one straddle a sector.
fn push_record(dir: &mut Vec<u8>, record: &[u8]) {
    let room = SECTOR - dir.len() % SECTOR;
    if record.len() > room {
        dir.resize(dir.len() + room, 0);
    }
    dir.extend_from_slice(record);
}

struct IsoLayout {
    next: u32,
}

impl IsoLayout {
    fn alloc(&mut self, len: usize) -> u32 {
        let at = self.next;
        self.next += sectors(len) as u32;
        at
    }
}

/// Lays out `dir` (whose own extent is `own`, parent `parent`) and returns
/// the directory's bytes; file and subdirectory contents go into `img`.
fn iso_dir(
    img: &mut Vec<u8>,
    layout: &mut IsoLayout,
    dir: &Dir,
    own: (u32, u32),
    parent: (u32, u32),
) {
    // Sizes of the subdirectories are needed for their records, so lay them
    // out (recursively) first: their extents are allocated as we go.
    let mut records = Vec::new();
    for (name, sub) in &dir.dirs {
        let size = iso_dir_size(sub);
        let extent = layout.alloc(size);
        iso_dir(img, layout, sub, (extent, size as u32), own);
        records.push(iso_record(name.as_bytes(), extent, size as u32, true));
    }
    for (name, data) in &dir.files {
        let extent = layout.alloc(data.len());
        put(img, extent as usize * SECTOR, data);
        let id = format!("{name};1");
        records.push(iso_record(id.as_bytes(), extent, data.len() as u32, false));
    }
    let mut bytes = Vec::new();
    push_record(&mut bytes, &iso_record(&[0], own.0, own.1, true));
    push_record(&mut bytes, &iso_record(&[1], parent.0, parent.1, true));
    for r in &records {
        push_record(&mut bytes, r);
    }
    bytes.resize(own.1 as usize, 0);
    put(img, own.0 as usize * SECTOR, &bytes);
}

/// Size in bytes (whole sectors) of the directory's record area.
fn iso_dir_size(dir: &Dir) -> usize {
    let mut bytes = Vec::new();
    push_record(&mut bytes, &iso_record(&[0], 0, 0, true));
    push_record(&mut bytes, &iso_record(&[1], 0, 0, true));
    for name in dir.dirs.keys() {
        push_record(&mut bytes, &iso_record(name.as_bytes(), 0, 0, true));
    }
    for name in dir.files.keys() {
        push_record(
            &mut bytes,
            &iso_record(format!("{name};1").as_bytes(), 0, 0, false),
        );
    }
    sectors(bytes.len()) * SECTOR
}

/// A plain ISO 9660 image holding `files`.
pub fn iso9660(files: &[(&str, &[u8])]) -> Vec<u8> {
    let root = tree(files);
    let mut img = vec![0u8; 18 * SECTOR];
    let mut layout = IsoLayout { next: 18 };
    let root_size = iso_dir_size(&root);
    let root_extent = layout.alloc(root_size);
    iso_dir(
        &mut img,
        &mut layout,
        &root,
        (root_extent, root_size as u32),
        (root_extent, root_size as u32),
    );
    img.resize(layout.next as usize * SECTOR, 0);

    let mut pvd = vec![0u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[40..48].copy_from_slice(b"TESTDISC");
    pvd[80..88].copy_from_slice(&both32(layout.next));
    pvd[120..124].copy_from_slice(&both16(1));
    pvd[124..128].copy_from_slice(&both16(1));
    pvd[128..132].copy_from_slice(&both16(SECTOR as u16));
    pvd[156..190].copy_from_slice(&iso_record(&[0], root_extent, root_size as u32, true));
    pvd[881] = 1;
    put(&mut img, 16 * SECTOR, &pvd);
    let mut term = vec![0u8; SECTOR];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    put(&mut img, 17 * SECTOR, &term);
    img
}

// ----------------------------------------------------------------------------
// UDF 1.02
// ----------------------------------------------------------------------------

/// First sector of the UDF partition (after the anchor at 256).
pub const UDF_PART_START: u32 = 272;

#[derive(Clone, Copy, Default)]
pub struct UdfOptions {
    /// Split file data into allocation extents of at most this many blocks
    /// (the extents stay contiguous on disc). `None`: one extent per file.
    pub extent_blocks: Option<u32>,
}

fn tag(id: u16, location: u32, body: &[u8]) -> [u8; 16] {
    let mut t = [0u8; 16];
    t[0..2].copy_from_slice(&id.to_le_bytes());
    t[2..4].copy_from_slice(&2u16.to_le_bytes());
    // Descriptor CRC over the body (CRC-ITU-T, polynomial 0x1021, init 0).
    let crc_len = body.len().min(u16::MAX as usize);
    let mut crc: u16 = 0;
    for &b in &body[..crc_len] {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    t[8..10].copy_from_slice(&crc.to_le_bytes());
    t[10..12].copy_from_slice(&(crc_len as u16).to_le_bytes());
    t[12..16].copy_from_slice(&location.to_le_bytes());
    let mut sum = 0u8;
    for (i, b) in t.iter().enumerate() {
        if i != 4 {
            sum = sum.wrapping_add(*b);
        }
    }
    t[4] = sum;
    t
}

/// Writes a descriptor: fills in its tag (over `desc[16..]`) and stores it.
fn descriptor(img: &mut Vec<u8>, sector: u32, id: u16, location: u32, mut desc: Vec<u8>) {
    let t = tag(id, location, &desc[16..]);
    desc[..16].copy_from_slice(&t);
    put(img, sector as usize * SECTOR, &desc);
}

fn long_ad(len: u32, block: u32, part: u16) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0..4].copy_from_slice(&len.to_le_bytes());
    a[4..8].copy_from_slice(&block.to_le_bytes());
    a[8..10].copy_from_slice(&part.to_le_bytes());
    a
}

fn short_ad(len: u32, pos: u32) -> [u8; 8] {
    let mut a = [0u8; 8];
    a[0..4].copy_from_slice(&len.to_le_bytes());
    a[4..8].copy_from_slice(&pos.to_le_bytes());
    a
}

struct UdfLayout {
    /// Next free logical block of the partition.
    next: u32,
    opts: UdfOptions,
}

impl UdfLayout {
    fn alloc(&mut self, blocks: u32) -> u32 {
        let at = self.next;
        self.next += blocks.max(1);
        at
    }
}

/// File Entry (file type 4 = directory, 5 = file) with short ADs.
fn file_entry(file_type: u8, size: u64, ads: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; SECTOR];
    b[20..22].copy_from_slice(&4u16.to_le_bytes()); // strategy 4
    b[24..26].copy_from_slice(&1u16.to_le_bytes()); // maximum entries
    b[27] = file_type;
    b[34..36].copy_from_slice(&0u16.to_le_bytes()); // short_ad
    b[48..50].copy_from_slice(&1u16.to_le_bytes()); // link count
    b[56..64].copy_from_slice(&size.to_le_bytes());
    b[64..72].copy_from_slice(&size.div_ceil(SECTOR as u64).to_le_bytes());
    b[168..172].copy_from_slice(&0u32.to_le_bytes());
    b[172..176].copy_from_slice(&(ads.len() as u32).to_le_bytes());
    b[176..176 + ads.len()].copy_from_slice(ads);
    b.truncate(176 + ads.len());
    b
}

fn fid(name: Option<&str>, chars: u8, icb_block: u32) -> Vec<u8> {
    let raw: Vec<u8> = match name {
        None => Vec::new(),
        Some(n) => std::iter::once(8u8).chain(n.bytes()).collect(),
    };
    let total = (38 + raw.len() + 3) & !3;
    let mut f = vec![0u8; total];
    f[16..18].copy_from_slice(&1u16.to_le_bytes());
    f[18] = chars;
    f[19] = raw.len() as u8;
    f[20..36].copy_from_slice(&long_ad(SECTOR as u32, icb_block, 0));
    f[38..38 + raw.len()].copy_from_slice(&raw);
    f
}

fn part_sector(block: u32) -> u32 {
    UDF_PART_START + block
}

/// Writes a regular file and returns its File Entry block.
fn udf_file(img: &mut Vec<u8>, layout: &mut UdfLayout, data: &[u8]) -> u32 {
    let fe_block = layout.alloc(1);
    let blocks = sectors(data.len()) as u32;
    let first = layout.alloc(blocks);
    put(img, part_sector(first) as usize * SECTOR, data);
    // The descriptors must fit in the File Entry's block (this builder writes
    // no Allocation Extent Descriptors), so very long files get longer extents.
    let max_ads = ((SECTOR - 176) / 8) as u32;
    let per = layout
        .opts
        .extent_blocks
        .unwrap_or(u32::MAX)
        .max(blocks.div_ceil(max_ads))
        .max(1);
    let mut ads = Vec::new();
    if !data.is_empty() {
        // Only the last extent may end inside a block.
        let mut done = 0u32;
        while done < blocks {
            let n = per.min(blocks - done);
            let start = done as usize * SECTOR;
            let len = (data.len() - start).min(n as usize * SECTOR);
            ads.extend_from_slice(&short_ad(len as u32, first + done));
            done += n;
        }
    }
    let fe = file_entry(5, data.len() as u64, &ads);
    descriptor(img, part_sector(fe_block), 261, fe_block, fe);
    fe_block
}

/// Writes a directory (and everything below it); returns its FE block.
fn udf_dir(img: &mut Vec<u8>, layout: &mut UdfLayout, dir: &Dir, parent_fe: Option<u32>) -> u32 {
    let fe_block = layout.alloc(1);
    let parent = parent_fe.unwrap_or(fe_block);
    let mut children = Vec::new();
    for (name, sub) in &dir.dirs {
        children.push((
            name.clone(),
            0x02u8,
            udf_dir(img, layout, sub, Some(fe_block)),
        ));
    }
    for (name, data) in &dir.files {
        children.push((name.clone(), 0u8, udf_file(img, layout, data)));
    }
    let mut fids = fid(None, 0x0A, parent);
    for (name, chars, block) in &children {
        fids.extend(fid(Some(name), *chars, *block));
    }
    let blocks = sectors(fids.len()) as u32;
    let data_block = layout.alloc(blocks);
    // Tag every FID with its own location (the block it starts in).
    let mut pos = 0usize;
    let mut tagged = Vec::with_capacity(fids.len());
    let mut rest: &[u8] = &fids;
    while !rest.is_empty() {
        let l_fi = rest[19] as usize;
        let len = (38 + l_fi + 3) & !3;
        let mut one = rest[..len].to_vec();
        let location = data_block + (pos / SECTOR) as u32;
        let t = tag(257, location, &one[16..]);
        one[..16].copy_from_slice(&t);
        tagged.extend_from_slice(&one);
        pos += len;
        rest = &rest[len..];
    }
    put(img, part_sector(data_block) as usize * SECTOR, &tagged);
    let fe = file_entry(
        4,
        tagged.len() as u64,
        &short_ad(tagged.len() as u32, data_block),
    );
    descriptor(img, part_sector(fe_block), 261, fe_block, fe);
    fe_block
}

/// A UDF 1.02 image holding `files`.
pub fn udf102(files: &[(&str, &[u8])], opts: UdfOptions) -> Vec<u8> {
    let root = tree(files);
    let mut img = vec![0u8; UDF_PART_START as usize * SECTOR];
    let mut layout = UdfLayout { next: 0, opts };
    let fsd_block = layout.alloc(1);
    let root_fe = udf_dir(&mut img, &mut layout, &root, None);
    let part_len = layout.next;
    let total = UDF_PART_START + part_len + 1;
    img.resize(total as usize * SECTOR, 0);

    // Volume recognition sequence: BEA01, NSR02, TEA01 at sectors 16..18.
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let mut d = vec![0u8; 7];
        d[1..6].copy_from_slice(*id);
        d[6] = 1;
        put(&mut img, (16 + i) * SECTOR, &d);
    }
    // Anchor at 256 and at the last sector -> main and reserve VDS at 32.
    let mut avdp = vec![0u8; 512];
    avdp[16..20].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[20..24].copy_from_slice(&32u32.to_le_bytes());
    avdp[24..28].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[28..32].copy_from_slice(&32u32.to_le_bytes());
    descriptor(&mut img, 256, 2, 256, avdp.clone());
    descriptor(&mut img, total - 1, 2, total - 1, avdp);

    // Partition descriptor: partition 0 at UDF_PART_START.
    let mut pd = vec![0u8; 512];
    pd[16..20].copy_from_slice(&1u32.to_le_bytes());
    pd[20..22].copy_from_slice(&1u16.to_le_bytes()); // allocated
    pd[22..24].copy_from_slice(&0u16.to_le_bytes()); // partition number
    pd[25..31].copy_from_slice(b"+NSR02");
    pd[188..192].copy_from_slice(&UDF_PART_START.to_le_bytes());
    pd[192..196].copy_from_slice(&part_len.to_le_bytes());
    descriptor(&mut img, 32, 5, 32, pd);

    // Logical volume descriptor with one type 1 partition map.
    let mut lvd = vec![0u8; 446];
    lvd[16..20].copy_from_slice(&2u32.to_le_bytes());
    lvd[212..216].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    lvd[217..236].copy_from_slice(b"*OSTA UDF Compliant");
    lvd[240..242].copy_from_slice(&0x0102u16.to_le_bytes());
    lvd[248..264].copy_from_slice(&long_ad(SECTOR as u32, fsd_block, 0));
    lvd[264..268].copy_from_slice(&6u32.to_le_bytes());
    lvd[268..272].copy_from_slice(&1u32.to_le_bytes());
    lvd[440..446].copy_from_slice(&[1, 6, 1, 0, 0, 0]);
    descriptor(&mut img, 33, 6, 33, lvd);
    descriptor(&mut img, 34, 8, 34, vec![0u8; 512]);

    // File set descriptor -> root directory.
    let mut fsd = vec![0u8; 512];
    fsd[400..416].copy_from_slice(&long_ad(SECTOR as u32, root_fe, 0));
    descriptor(&mut img, part_sector(fsd_block), 256, fsd_block, fsd);
    img
}
