//! Resource budgets on crafted images: memory (counted by a global
//! allocator), reads and time.
//!
//! The tests of this file run one at a time (`SERIAL`), because the allocator
//! counts every thread of the process.

extern crate IsoPreview as iso_preview;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

use iso_preview::reader::{CachedReader, SeekSource};
use iso_preview::udf::Udf;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let c = CURRENT.fetch_add(l.size(), SeqCst) + l.size();
            PEAK.fetch_max(c, SeqCst);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            let c = CURRENT.fetch_add(l.size(), SeqCst) + l.size();
            PEAK.fetch_max(c, SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CURRENT.fetch_sub(l.size(), SeqCst);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            CURRENT.fetch_sub(l.size(), SeqCst);
            let c = CURRENT.fetch_add(new, SeqCst) + new;
            PEAK.fetch_max(c, SeqCst);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` and returns its result and the most memory it held at once
/// above what was allocated before.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(SeqCst);
    PEAK.store(base, SeqCst);
    let out = f();
    (out, PEAK.load(SeqCst) - base)
}

const MIB: usize = 1 << 20;

// ----------------------------------------------------------------------------
// A UDF image written by hand
// ----------------------------------------------------------------------------

const SECTOR: usize = 2048;
const PART_START: u32 = 100;

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
        if self.data.len() < start + bytes.len() {
            self.data.resize(start + bytes.len(), 0);
        }
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

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

/// A file entry of `file_type` and `size` with short allocation descriptors.
fn fe(location: u32, file_type: u8, size: u64, ad: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; SECTOR];
    b[..16].copy_from_slice(&tag(261, location));
    b[20..22].copy_from_slice(&4u16.to_le_bytes());
    b[24..26].copy_from_slice(&1u16.to_le_bytes());
    b[27] = file_type;
    b[56..64].copy_from_slice(&size.to_le_bytes());
    b[172..176].copy_from_slice(&(ad.len() as u32).to_le_bytes());
    b[176..176 + ad.len()].copy_from_slice(ad);
    b
}

/// The parent entry of a directory.
fn parent_fid(icb_block: u32) -> Vec<u8> {
    let mut f = vec![0u8; 40];
    f[..16].copy_from_slice(&tag(257, 0));
    f[16..18].copy_from_slice(&1u16.to_le_bytes());
    f[18] = 0x0A;
    f[20..36].copy_from_slice(&long_ad(SECTOR as u32, icb_block, 0));
    f
}

/// Anchor, volume descriptors (a partition at `PART_START`, a logical
/// volume with `maps`), file set descriptor and an empty root directory.
fn skeleton(img: &mut Image, maps: &[u8], n_maps: u32) {
    let mut avdp = vec![0u8; 32];
    avdp[..16].copy_from_slice(&tag(2, 256));
    avdp[16..20].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[20..24].copy_from_slice(&32u32.to_le_bytes());
    avdp[24..28].copy_from_slice(&(4 * SECTOR as u32).to_le_bytes());
    avdp[28..32].copy_from_slice(&32u32.to_le_bytes());
    img.put(256, 0, &avdp);
    let mut pd = vec![0u8; 200];
    pd[..16].copy_from_slice(&tag(5, 32));
    pd[188..192].copy_from_slice(&PART_START.to_le_bytes());
    pd[192..196].copy_from_slice(&1000u32.to_le_bytes());
    img.put(32, 0, &pd);
    let mut lvd = vec![0u8; SECTOR];
    lvd[..16].copy_from_slice(&tag(6, 33));
    lvd[212..216].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    lvd[240..242].copy_from_slice(&0x0150u16.to_le_bytes());
    lvd[248..264].copy_from_slice(&long_ad(SECTOR as u32, 0, 0));
    lvd[264..268].copy_from_slice(&(maps.len() as u32).to_le_bytes());
    lvd[268..272].copy_from_slice(&n_maps.to_le_bytes());
    lvd[440..440 + maps.len()].copy_from_slice(maps);
    img.put(33, 0, &lvd);
    img.put(34, 0, &tag(8, 34));
    let mut fsd = vec![0u8; SECTOR];
    fsd[..16].copy_from_slice(&tag(256, 0));
    fsd[400..416].copy_from_slice(&long_ad(SECTOR as u32, 1, 0));
    img.put(PART_START as usize, 0, &fsd);
    let fids = parent_fid(1);
    let ad = short_ad(fids.len() as u32, 2);
    img.put(
        (PART_START + 1) as usize,
        0,
        &fe(1, 4, fids.len() as u64, &ad),
    );
    img.put((PART_START + 2) as usize, 0, &fids);
}

/// A physical partition map and `virtual_maps` virtual ones, all over
/// partition 0, and an 8 MiB VAT that is one unrecorded extent: reading it
/// costs nothing, holding it 8 MiB.
fn vat_image(virtual_maps: u32) -> Vec<u8> {
    let sectors = 400;
    let mut img = Image::new(sectors);
    let mut maps = vec![1u8, 6, 1, 0, 0, 0];
    for _ in 0..virtual_maps {
        let mut m = vec![0u8; 64];
        m[0] = 2;
        m[1] = 64;
        m[5..27].copy_from_slice(b"*UDF Virtual Partition");
        m[36..38].copy_from_slice(&1u16.to_le_bytes());
        maps.extend(m);
    }
    skeleton(&mut img, &maps, 1 + virtual_maps);
    let size: u32 = 8 << 20;
    let vat = fe(0, 248, u64::from(size), &short_ad(size | 1 << 30, 0));
    img.put(sectors - 1, 0, &vat);
    img.data
}

#[test]
fn udf_virtual_partitions_load_one_vat() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // A volume of fifteen virtual partitions (real ones have one) holds one
    // VAT, not fifteen.
    for maps in [1, 15] {
        let image = vat_image(maps);
        let (opened, peak) = peak_of(|| {
            let mut rd = CachedReader::new(SeekSource(Cursor::new(&image[..]))).unwrap();
            Udf::open(&mut rd).is_ok()
        });
        assert!(opened, "{maps} maps");
        assert!(peak < 24 * MIB, "{maps} maps: {} MiB", peak / MIB);
        let (_, peak) =
            peak_of(|| iso_preview::extract_thumbnail(SeekSource(Cursor::new(&image[..]))));
        assert!(
            peak < 24 * MIB,
            "{maps} maps, extraction: {} MiB",
            peak / MIB
        );
    }
}
