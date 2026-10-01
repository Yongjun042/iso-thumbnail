//! Parser tests on synthetic in-memory images: no IMAPI, no admin rights.
//!
//! Two tiny builders produce ISO 9660 (plain / Joliet) and UDF (type 1
//! partition / metadata partition) images with a Blu-ray layout, and the
//! extractor is run against intact, truncated and randomly corrupted copies.

extern crate IsoPreview as iso_preview;

use std::io::Cursor;

use iso_preview::error::Error;
use iso_preview::fs::FileSystem;
use iso_preview::iso9660::Iso9660;
use iso_preview::reader::{ByteSource, CachedReader, SeekSource};
use iso_preview::udf::{Icb, Udf};
use iso_preview::Extracted;

const SECTOR: usize = 2048;
const JPEG_BIG: &[u8] = b"\xFF\xD8\xFF\xE0 big 640x360 cover bytes";
const JPEG_SMALL: &[u8] = b"\xFF\xD8\xFF\xE0 small";
const XML: &[u8] = b"<disclib/>";

fn extract(image: &[u8]) -> Result<Extracted, Error> {
    iso_preview::extract_thumbnail(SeekSource(Cursor::new(image.to_vec())))
}

struct Image {
    data: Vec<u8>,
}

impl Image {
    fn new(sectors: usize) -> Self {
        Self {
            data: vec![0u8; sectors * SECTOR],
        }
    }

    fn put(&mut self, sector: usize, off: usize, bytes: &[u8]) {
        let start = sector * SECTOR + off;
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

// ----------------------------------------------------------------------------
// ISO 9660
// ----------------------------------------------------------------------------

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
    r[25] = if dir { 2 } else { 0 };
    r[28] = 1;
    r[31] = 1;
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    r
}

fn iso_dir(own: u32, parent: u32, entries: &[(Vec<u8>, u32, u32, bool)]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend(iso_record(&[0], own, SECTOR as u32, true));
    d.extend(iso_record(&[1], parent, SECTOR as u32, true));
    for (name, extent, size, dir) in entries {
        d.extend(iso_record(name, *extent, *size, *dir));
    }
    d
}

/// Sectors: 16 PVD, 17 SVD (Joliet only), 18 terminator, 19 root, 20 BDMV,
/// 21 META, 22 DL, 23 big JPEG, 24 small JPEG, 25 XML.
fn build_iso9660(joliet: bool) -> Vec<u8> {
    let name = |s: &str| -> Vec<u8> {
        if joliet {
            s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
        } else {
            s.as_bytes().to_vec()
        }
    };
    let mut img = Image::new(26);
    let mut pvd = vec![0u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[156..190].copy_from_slice(&iso_record(&[0], 19, SECTOR as u32, true));
    img.put(16, 0, &pvd);
    if joliet {
        let mut svd = pvd.clone();
        svd[0] = 2;
        svd[88..91].copy_from_slice(b"%/E");
        img.put(17, 0, &svd);
    }
    let mut term = vec![0u8; 7];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    img.put(18, 0, &term);
    let d = SECTOR as u32;
    img.put(19, 0, &iso_dir(19, 19, &[(name("BDMV"), 20, d, true)]));
    img.put(20, 0, &iso_dir(20, 19, &[(name("META"), 21, d, true)]));
    img.put(21, 0, &iso_dir(21, 20, &[(name("DL"), 22, d, true)]));
    img.put(
        22,
        0,
        &iso_dir(
            22,
            21,
            &[
                (name("BDMT_ENG.XML;1"), 25, XML.len() as u32, false),
                (
                    name("COVER_416X240.JPG;1"),
                    24,
                    JPEG_SMALL.len() as u32,
                    false,
                ),
                (
                    name("COVER_640X360.JPG;1"),
                    23,
                    JPEG_BIG.len() as u32,
                    false,
                ),
            ],
        ),
    );
    img.put(23, 0, JPEG_BIG);
    img.put(24, 0, JPEG_SMALL);
    img.put(25, 0, XML);
    img.data
}

// ----------------------------------------------------------------------------
// UDF
// ----------------------------------------------------------------------------

fn tag(id: u16, location: u32) -> [u8; 16] {
    let mut t = [0u8; 16];
    t[0..2].copy_from_slice(&id.to_le_bytes());
    t[2..4].copy_from_slice(&2u16.to_le_bytes());
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

/// File Entry with one allocation descriptor (`ad_type` 0 = short, 1 = long).
fn fe(location: u32, file_type: u8, size: u64, ad_type: u16, ad: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; SECTOR];
    b[..16].copy_from_slice(&tag(261, location));
    b[20..22].copy_from_slice(&4u16.to_le_bytes()); // strategy 4
    b[24..26].copy_from_slice(&1u16.to_le_bytes()); // max entries
    b[27] = file_type;
    b[34..36].copy_from_slice(&ad_type.to_le_bytes());
    b[56..64].copy_from_slice(&size.to_le_bytes());
    b[172..176].copy_from_slice(&(ad.len() as u32).to_le_bytes());
    b[176..176 + ad.len()].copy_from_slice(ad);
    b
}

fn fid(location: u32, name: &str, chars: u8, icb_block: u32, icb_part: u16) -> Vec<u8> {
    let raw: Vec<u8> = if name.is_empty() {
        Vec::new()
    } else {
        let mut v = vec![8u8];
        v.extend_from_slice(name.as_bytes());
        v
    };
    let total = (38 + raw.len() + 3) & !3;
    let mut f = vec![0u8; total];
    f[..16].copy_from_slice(&tag(257, location));
    f[16..18].copy_from_slice(&1u16.to_le_bytes());
    f[18] = chars;
    f[19] = raw.len() as u8;
    f[20..36].copy_from_slice(&long_ad(SECTOR as u32, icb_block, icb_part));
    f[38..38 + raw.len()].copy_from_slice(&raw);
    f
}

const PART_START: u32 = 100;

/// A UDF image with the Blu-ray layout. With `metadata` the file system
/// structures live in a UDF 2.50 metadata partition (partition reference 1)
/// whose metadata file maps onto physical blocks 30..45; file data uses long
/// allocation descriptors into the physical partition, as on real discs.
fn build_udf(metadata: bool, with_artwork: bool, big_size: Option<u64>) -> Vec<u8> {
    let mut img = Image::new(300);
    let phys = |block: u32| (PART_START + block) as usize;

    // Anchor at sector 256 → main volume descriptor sequence at sector 32.
    let mut avdp = vec![0u8; 32];
    avdp[..16].copy_from_slice(&tag(2, 256));
    avdp[16..20].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[20..24].copy_from_slice(&32u32.to_le_bytes());
    avdp[24..28].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[28..32].copy_from_slice(&32u32.to_le_bytes());
    img.put(256, 0, &avdp);

    // Partition descriptor: partition 0 starts at PART_START.
    let mut pd = vec![0u8; 200];
    pd[..16].copy_from_slice(&tag(5, 32));
    pd[188..192].copy_from_slice(&PART_START.to_le_bytes());
    pd[192..196].copy_from_slice(&150u32.to_le_bytes());
    img.put(32, 0, &pd);

    // Logical volume descriptor.
    let mut lvd = vec![0u8; SECTOR];
    lvd[..16].copy_from_slice(&tag(6, 33));
    lvd[212..216].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    lvd[217..236].copy_from_slice(b"*OSTA UDF Compliant");
    let revision: u16 = if metadata { 0x0250 } else { 0x0102 };
    lvd[240..242].copy_from_slice(&revision.to_le_bytes());
    let fs_part: u16 = if metadata { 1 } else { 0 };
    lvd[248..264].copy_from_slice(&long_ad(SECTOR as u32, 0, fs_part));
    // Type 1 map: type, length, volume sequence number (1), partition number (0).
    let mut maps = vec![1u8, 6, 1, 0, 0, 0];
    if metadata {
        let mut m = vec![0u8; 64];
        m[0] = 2;
        m[1] = 64;
        m[5..28].copy_from_slice(b"*UDF Metadata Partition");
        m[36..38].copy_from_slice(&1u16.to_le_bytes()); // volume sequence number
        m[38..40].copy_from_slice(&0u16.to_le_bytes()); // physical partition 0
        m[40..44].copy_from_slice(&20u32.to_le_bytes()); // metadata file at block 20
        m[44..48].copy_from_slice(&21u32.to_le_bytes()); // mirror at block 21
        maps.extend(m);
    }
    lvd[264..268].copy_from_slice(&(maps.len() as u32).to_le_bytes());
    lvd[268..272].copy_from_slice(&(if metadata { 2u32 } else { 1u32 }).to_le_bytes());
    lvd[440..440 + maps.len()].copy_from_slice(&maps);
    img.put(33, 0, &lvd);
    img.put(34, 0, &tag(8, 34));

    // Where the file system structures live and which partition reference they use.
    let base: u32 = if metadata { 30 } else { 0 };
    if metadata {
        let extent = short_ad(15 * SECTOR as u32, base);
        img.put(phys(20), 0, &fe(20, 250, 15 * SECTOR as u64, 0, &extent));
        img.put(phys(21), 0, &fe(21, 251, 15 * SECTOR as u64, 0, &extent));
    }
    let at = |block: u32| phys(base + block);

    // File set descriptor → root directory ICB at block 1.
    let mut fsd = vec![0u8; SECTOR];
    fsd[..16].copy_from_slice(&tag(256, 0));
    fsd[400..416].copy_from_slice(&long_ad(SECTOR as u32, 1, fs_part));
    img.put(at(0), 0, &fsd);

    // Directories: FE at odd blocks, FIDs at the following block.
    let dir = |img: &mut Image, fe_block: u32, parent: u32, entries: &[(&str, u8, u32)]| {
        let mut fids = fid(fe_block + 1, "", 0x0A, parent, fs_part);
        for (name, chars, block) in entries {
            fids.extend(fid(fe_block + 1, name, *chars, *block, fs_part));
        }
        let ad = short_ad(fids.len() as u32, fe_block + 1);
        img.put(at(fe_block), 0, &fe(fe_block, 4, fids.len() as u64, 0, &ad));
        img.put(at(fe_block + 1), 0, &fids);
    };
    dir(&mut img, 1, 1, &[("BDMV", 0x02, 3)]);
    if with_artwork {
        dir(&mut img, 3, 1, &[("META", 0x02, 5), ("STREAM", 0x02, 12)]);
        dir(&mut img, 5, 3, &[("DL", 0x02, 7)]);
        dir(
            &mut img,
            7,
            5,
            &[
                ("bdmt_eng.xml", 0, 9),
                ("COVER_416x240.jpg", 0, 10),
                ("COVER_640x360.jpg", 0, 11),
            ],
        );
        dir(&mut img, 12, 3, &[("00000.m2ts", 0, 14)]);
    } else {
        dir(&mut img, 3, 1, &[("STREAM", 0x02, 12)]);
        dir(&mut img, 12, 3, &[("00000.m2ts", 0, 14)]);
    }

    // Regular files: data in physical blocks 50.. (partition 0).
    let file = |img: &mut Image, fe_block: u32, data_block: u32, data: &[u8], size: u64| {
        let (ad_type, ad): (u16, Vec<u8>) = if metadata {
            (1, long_ad(data.len() as u32, data_block, 0).to_vec())
        } else {
            (0, short_ad(data.len() as u32, data_block).to_vec())
        };
        img.put(at(fe_block), 0, &fe(fe_block, 5, size, ad_type, &ad));
        img.put(phys(data_block), 0, data);
    };
    file(&mut img, 9, 50, XML, XML.len() as u64);
    file(&mut img, 10, 51, JPEG_SMALL, JPEG_SMALL.len() as u64);
    file(
        &mut img,
        11,
        52,
        JPEG_BIG,
        big_size.unwrap_or(JPEG_BIG.len() as u64),
    );
    file(&mut img, 14, 53, b"stream", 6);
    img.data
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

fn assert_big_cover(found: &Extracted, filesystem: &str) {
    assert_eq!(found.filesystem, filesystem);
    assert!(
        found
            .thumbnail
            .path
            .eq_ignore_ascii_case("BDMV/META/DL/COVER_640x360.jpg"),
        "picked {}",
        found.thumbnail.path
    );
    assert_eq!(found.thumbnail.data, JPEG_BIG);
}

#[test]
fn iso9660_plain_picks_largest_cover() {
    let found = extract(&build_iso9660(false)).unwrap();
    assert_big_cover(&found, "ISO 9660");
}

#[test]
fn iso9660_joliet_picks_largest_cover() {
    let found = extract(&build_iso9660(true)).unwrap();
    assert_big_cover(&found, "ISO 9660 (Joliet)");
}

#[test]
fn udf_type1_partition_picks_largest_cover() {
    let found = extract(&build_udf(false, true, None)).unwrap();
    assert_big_cover(&found, "UDF 1.02");
    assert!(found.reads <= 12, "too many reads: {}", found.reads);
}

#[test]
fn udf_metadata_partition_picks_largest_cover() {
    let found = extract(&build_udf(true, true, None)).unwrap();
    assert_big_cover(&found, "UDF 2.50 (metadata partition)");
    assert!(found.reads <= 14, "too many reads: {}", found.reads);
}

#[test]
fn udf_without_artwork_reports_not_found() {
    assert_eq!(
        extract(&build_udf(true, false, None)).unwrap_err(),
        Error::NotFound
    );
}

#[test]
fn oversized_artwork_is_skipped_for_the_next_candidate() {
    let found = extract(&build_udf(true, true, Some(17 << 20))).unwrap();
    assert!(found.thumbnail.path.ends_with("COVER_416x240.jpg"));
    assert_eq!(found.thumbnail.data, JPEG_SMALL);
}

#[test]
fn empty_and_garbage_images_are_rejected() {
    assert_eq!(extract(&[]).unwrap_err(), Error::NoVolume);
    assert_eq!(extract(&[0u8; 4096]).unwrap_err(), Error::NoVolume);
    let noise: Vec<u8> = (0..600 * SECTOR).map(|i| (i * 31 % 251) as u8).collect();
    assert!(extract(&noise).is_err());
}

#[test]
fn truncated_images_fail_without_panicking() {
    for image in [
        build_iso9660(false),
        build_iso9660(true),
        build_udf(false, true, None),
        build_udf(true, true, None),
    ] {
        for cut in [
            1usize,
            100,
            16 * SECTOR + 7,
            19 * SECTOR + 40,
            33 * SECTOR,
            101 * SECTOR,
            257 * SECTOR,
        ] {
            let cut = cut.min(image.len() - 1);
            let result = extract(&image[..cut]);
            // Below 20 sectors no image has its directories yet; later cuts
            // only need to come back without panicking.
            if cut < 20 * SECTOR {
                assert!(result.is_err(), "cut at {cut} succeeded");
            }
        }
    }
}

#[test]
fn indirect_entry_is_followed() {
    // Type 1 partition image; move the big cover's FE (block 11) to block 40
    // and put a strategy-4096 Indirect Entry at block 11 pointing there.
    let mut data = build_udf(false, true, None);
    let at = |b: u32| (PART_START + b) as usize * SECTOR;
    let mut moved = data[at(11)..at(11) + SECTOR].to_vec();
    moved[..16].copy_from_slice(&tag(261, 40));
    data[at(40)..at(40) + SECTOR].copy_from_slice(&moved);
    let mut ie = vec![0u8; SECTOR];
    ie[..16].copy_from_slice(&tag(259, 11));
    ie[20..22].copy_from_slice(&4096u16.to_le_bytes()); // strategy type 4096
    ie[24..26].copy_from_slice(&2u16.to_le_bytes()); // maximum number of entries
    ie[27] = 3; // file type: indirect entry
    ie[36..52].copy_from_slice(&long_ad(SECTOR as u32, 40, 0)); // Indirect ICB
    data[at(11)..at(11) + SECTOR].copy_from_slice(&ie);
    let found = extract(&data).unwrap();
    assert_big_cover(&found, "UDF 1.02");
}

#[test]
fn broken_root_cover_does_not_hide_the_next_one() {
    // Data disc without BDMV. FOLDER.JPG is too large, COVER.JPG points past
    // the end of the image; POSTER.JPG, next in priority, must still be used.
    let mut img = Image::new(20);
    let mut pvd = vec![0u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[156..190].copy_from_slice(&iso_record(&[0], 18, SECTOR as u32, true));
    img.put(16, 0, &pvd);
    let mut term = vec![0u8; 7];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    img.put(17, 0, &term);
    img.put(
        18,
        0,
        &iso_dir(
            18,
            18,
            &[
                (b"FOLDER.JPG;1".to_vec(), 19, 17 << 20, false),
                (b"COVER.JPG;1".to_vec(), 400, 64, false),
                (b"POSTER.JPG;1".to_vec(), 19, JPEG_BIG.len() as u32, false),
            ],
        ),
    );
    img.put(19, 0, JPEG_BIG);
    let found = extract(&img.data).unwrap();
    assert_eq!(found.thumbnail.path, "POSTER.JPG");
    assert_eq!(found.thumbnail.data, JPEG_BIG);
}

#[test]
fn read_budget_stops_the_walk() {
    let image = build_udf(true, true, None);
    let mut rd = CachedReader::with_budget(SeekSource(Cursor::new(image)), 4096).unwrap();
    assert_eq!(Udf::open(&mut rd).err(), Some(Error::TooLarge));
}

/// Flips a few random bytes at a time; the parser must return an error or a
/// result, never panic or overflow.
#[test]
fn corrupted_images_never_panic() {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for image in [
        build_iso9660(false),
        build_iso9660(true),
        build_udf(false, true, None),
        build_udf(true, true, None),
    ] {
        // Only the sectors that carry structures are interesting to mutate.
        let sectors = image.len() / SECTOR;
        let hot: Vec<usize> = (16..35)
            .chain(100..160)
            .chain(std::iter::once(256))
            .filter(|&s| s < sectors)
            .collect();
        for _ in 0..600 {
            let mut copy = image.clone();
            let flips = 1 + (next() % 8) as usize;
            for _ in 0..flips {
                let sector = hot[(next() as usize) % hot.len()];
                let off = sector * SECTOR + (next() as usize) % SECTOR;
                copy[off] = next() as u8;
            }
            let _ = extract(&copy);
        }
    }
}

// ----------------------------------------------------------------------------
// Ranged reads (FileSystem::read_range)
// ----------------------------------------------------------------------------

/// Byte `i` of the multi-extent test file.
fn pattern(i: u64) -> u8 {
    (i * 7 + i / 251 + 1) as u8
}

/// Extent kinds of an allocation descriptor (the top two bits of its length).
const RECORDED: u32 = 0;
const NOT_RECORDED: u32 = 1;
const NOT_ALLOCATED: u32 = 2;

/// Replaces the file entry of `BDMV/STREAM/00000.m2ts` (block 14) in a
/// `build_udf` image with one that has `extents` = (kind, length, block) and
/// Information Length `size`, writes the pattern into the recorded extents
/// and returns the image and the file's expected content.
fn udf_with_extents(metadata: bool, extents: &[(u32, u32, u32)], size: u64) -> (Vec<u8>, Vec<u8>) {
    let mut img = Image {
        data: build_udf(metadata, true, None),
    };
    let mut ads = Vec::new();
    let mut expected = vec![0u8; size as usize];
    let mut pos = 0u64;
    for &(kind, len, block) in extents {
        let raw = len | kind << 30;
        if metadata {
            ads.extend_from_slice(&long_ad(raw, block, 0));
        } else {
            ads.extend_from_slice(&short_ad(raw, block));
        }
        let sector = (PART_START + block) as usize;
        if kind == RECORDED {
            let data: Vec<u8> = (pos..pos + len as u64).map(pattern).collect();
            img.put(sector, 0, &data);
            let end = (pos + len as u64).min(size);
            if pos < end {
                expected[pos as usize..end as usize].copy_from_slice(&data[..(end - pos) as usize]);
            }
        } else {
            // Whatever sits in the blocks of an unrecorded extent must not show.
            img.put(sector, 0, &vec![0xEE; len as usize]);
        }
        pos += len as u64;
    }
    let (ad_type, base) = if metadata { (1, 30) } else { (0, 0) };
    let entry = fe(14, 5, size, ad_type, &ads);
    img.put((PART_START + base + 14) as usize, 0, &entry);
    (img.data, expected)
}

fn udf_m2ts<S: ByteSource>(fs: &mut Udf<'_, S>) -> Icb {
    let root = fs.root().unwrap();
    let bdmv = fs.lookup(&root, "BDMV", true).unwrap().unwrap();
    let stream = fs.lookup(&bdmv, "STREAM", true).unwrap().unwrap();
    fs.lookup(&stream, "00000.m2ts", false).unwrap().unwrap()
}

/// Reads `len` bytes at `offset` and checks them against `expected` (the
/// whole file), including that nothing past the returned count is written.
fn check_range<F: FileSystem>(
    fs: &mut F,
    node: &F::Node,
    expected: &[u8],
    offset: u64,
    len: usize,
) {
    let mut buf = vec![0xA5u8; len];
    let n = fs.read_range(node, offset, &mut buf).unwrap();
    let start = offset.min(expected.len() as u64) as usize;
    let rest = &expected[start..];
    let want = &rest[..rest.len().min(len)];
    assert_eq!(n, want.len(), "count at {offset}+{len}");
    assert!(buf[..n] == *want, "bytes at {offset}+{len}");
    assert!(buf[n..].iter().all(|&b| b == 0xA5), "wrote past the count");
}

#[test]
fn udf_read_range_follows_the_extents() {
    let s = SECTOR as u32;
    // Recorded, allocated but not recorded, recorded, not allocated, a short
    // last extent; the Information Length reaches 500 bytes past the extents.
    let extents = [
        (RECORDED, 2 * s, 60),
        (NOT_RECORDED, s, 65),
        (RECORDED, 2 * s, 70),
        (NOT_ALLOCATED, s, 66),
        (RECORDED, 1000, 75),
    ];
    let size = 6 * SECTOR + 1000 + 500;
    for metadata in [false, true] {
        let (image, expected) = udf_with_extents(metadata, &extents, size as u64);
        assert!(expected[4096..6144].iter().all(|&b| b == 0));
        assert!(expected[size - 500..].iter().all(|&b| b == 0));
        let mut rd = CachedReader::new(SeekSource(Cursor::new(image))).unwrap();
        let mut fs = Udf::open(&mut rd).unwrap();
        let file = udf_m2ts(&mut fs);
        assert_eq!(fs.file_size(&file).unwrap(), size as u64);
        for (offset, len) in [
            (0, 100),
            (1, 4095),       // inside the first extent, up to its end
            (4000, 200),     // recorded → not recorded
            (6000, 5000),    // across four extents
            (11_000, 300),   // inside the not-allocated extent
            (12_288, 1000),  // exactly the short last extent
            (13_000, 500),   // last extent → zeros past the extents
            (0, size),       // everything
            (0, size + 100), // more than the file
            (size - 10, 100),
            (size, 10),
            (size + 5, 10),
            (100, 0),
        ] {
            check_range(&mut fs, &file, &expected, offset as u64, len);
        }
        check_range(&mut fs, &file, &expected, u64::MAX, 16);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..300 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let offset = seed % (size as u64 + 64);
            let len = ((seed >> 32) % 5000) as usize;
            check_range(&mut fs, &file, &expected, offset, len);
        }
        // Directories cannot be read as files.
        let root = fs.root().unwrap();
        assert!(fs.read_range(&root, 0, &mut [0u8; 16]).is_err());
    }
}

#[test]
fn udf_read_range_reads_embedded_data() {
    for metadata in [false, true] {
        let mut img = Image {
            data: build_udf(metadata, true, None),
        };
        let inline = b"inline file data, 34 bytes long..";
        // Information Length 40: the bytes beyond the embedded data read as zeros.
        let base = if metadata { 30 } else { 0 };
        img.put(
            (PART_START + base + 14) as usize,
            0,
            &fe(14, 5, 40, 3, inline),
        );
        let mut expected = inline.to_vec();
        expected.resize(40, 0);
        let mut rd = CachedReader::new(SeekSource(Cursor::new(img.data))).unwrap();
        let mut fs = Udf::open(&mut rd).unwrap();
        let file = udf_m2ts(&mut fs);
        for (offset, len) in [
            (0, 40),
            (0, 100),
            (5, 10),
            (30, 10),
            (33, 7),
            (39, 5),
            (40, 1),
            (41, 1),
        ] {
            check_range(&mut fs, &file, &expected, offset, len);
        }
    }
}

#[test]
fn udf_read_range_respects_the_read_budget() {
    let s = SECTOR as u32;
    let (image, expected) = udf_with_extents(false, &[(RECORDED, 40 * s, 60)], 40 * SECTOR as u64);
    // First measure what opening the volume and finding the file costs.
    let used = {
        let mut rd = CachedReader::new(SeekSource(Cursor::new(image.clone()))).unwrap();
        let mut fs = Udf::open(&mut rd).unwrap();
        udf_m2ts(&mut fs);
        drop(fs);
        rd.bytes
    };
    let budget = used + (40 << 10);
    let mut rd = CachedReader::with_budget(SeekSource(Cursor::new(image)), budget).unwrap();
    let mut fs = Udf::open(&mut rd).unwrap();
    let file = udf_m2ts(&mut fs);
    check_range(&mut fs, &file, &expected, 0, 100);
    let mut whole = vec![0u8; expected.len()];
    assert_eq!(fs.read_range(&file, 0, &mut whole), Err(Error::TooLarge));
}

#[test]
fn iso9660_read_range_reads_the_extent() {
    for joliet in [false, true] {
        let image = build_iso9660(joliet);
        let mut rd = CachedReader::new(SeekSource(Cursor::new(image))).unwrap();
        let mut fs = Iso9660::open(&mut rd).unwrap();
        let mut dir = fs.root().unwrap();
        for name in ["BDMV", "META", "DL"] {
            dir = fs.lookup(&dir, name, true).unwrap().unwrap();
        }
        let file = fs
            .lookup(&dir, "COVER_640X360.JPG", false)
            .unwrap()
            .unwrap();
        let n = JPEG_BIG.len();
        for (offset, len) in [
            (0, n),
            (0, 4),
            (3, 10),
            (n - 1, 5),
            (n, 1),
            (n + 100, 1),
            (0, 4096),
        ] {
            check_range(&mut fs, &file, JPEG_BIG, offset as u64, len);
        }
        check_range(&mut fs, &file, JPEG_BIG, u64::MAX, 3);
        assert!(fs.read_range(&dir, 0, &mut [0u8; 8]).is_err());
    }
}

/// Corrupting the file entry (sizes, allocation descriptors) must give an
/// error or a short read, never a panic or a count beyond the buffer.
#[test]
fn udf_read_range_survives_corrupt_file_entries() {
    let s = SECTOR as u32;
    let extents = [
        (RECORDED, 2 * s, 60),
        (NOT_RECORDED, s, 65),
        (RECORDED, 1000, 70),
    ];
    let mut seed: u64 = 0x1F2E_3D4C_5B6A_7988;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for metadata in [false, true] {
        let (image, _) = udf_with_extents(metadata, &extents, 4 * SECTOR as u64);
        let fe_at = (PART_START + if metadata { 44 } else { 14 }) as usize * SECTOR;
        for _ in 0..400 {
            let mut copy = image.clone();
            for _ in 0..1 + next() % 4 {
                // Information Length (56..64) and the descriptors (176..)
                // matter most; the tag checksum is recomputed below.
                let off = match next() % 3 {
                    0 => 56 + (next() % 8) as usize,
                    1 => 168 + (next() % 8) as usize,
                    _ => 176 + (next() % 48) as usize,
                };
                copy[fe_at + off] = next() as u8;
            }
            let mut sum = 0u8;
            for i in (0..16).filter(|&i| i != 4) {
                sum = sum.wrapping_add(copy[fe_at + i]);
            }
            copy[fe_at + 4] = sum;
            let mut rd = CachedReader::new(SeekSource(Cursor::new(copy))).unwrap();
            let mut fs = Udf::open(&mut rd).unwrap();
            let file = udf_m2ts(&mut fs);
            let mut buf = vec![0u8; 3000];
            for offset in [0u64, 4000, 1 << 40, u64::MAX - 10] {
                if let Ok(n) = fs.read_range(&file, offset, &mut buf) {
                    assert!(n <= buf.len());
                }
            }
        }
    }
}
