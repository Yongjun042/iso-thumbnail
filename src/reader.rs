//! Random-access byte source with a small block cache and a read budget.
//!
//! Blu-ray images are tens of gigabytes, so everything is read on demand in
//! small aligned chunks. The cache keeps the handful of sectors that the
//! volume/directory walk touches repeatedly (volume descriptors, file entries).
//! The budget bounds the total amount of data one extraction may pull from an
//! image, so a crafted image cannot keep the handler reading for long.

use crate::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// Something we can read bytes from at arbitrary offsets.
pub trait ByteSource {
    /// Total size in bytes.
    fn size(&mut self) -> Result<u64>;
    /// Fills `buf` completely from `offset`, or fails.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// Size of one cached chunk. Larger than a sector so that neighbouring
/// descriptors come in with a single read.
const CHUNK: usize = 32 * 1024;
/// Number of chunks kept in memory (512 KiB worst case).
const SLOTS: usize = 16;
/// Default upper bound on the bytes fetched from one image
/// (artwork + directories + volume structures).
pub const READ_BUDGET: u64 = 32 << 20;

struct Slot {
    offset: u64,
    len: usize,
    data: Box<[u8]>,
    stamp: u64,
}

pub struct CachedReader<S: ByteSource> {
    src: S,
    size: u64,
    slots: Vec<Slot>,
    clock: u64,
    budget: u64,
    /// Number of reads issued to the underlying source (diagnostics).
    pub reads: u32,
    /// Number of bytes fetched from the underlying source (diagnostics).
    pub bytes: u64,
}

impl<S: ByteSource> CachedReader<S> {
    pub fn new(src: S) -> Result<Self> {
        Self::with_budget(src, READ_BUDGET)
    }

    /// Like `new`, with a custom limit on the total bytes read from `src`.
    pub fn with_budget(mut src: S, budget: u64) -> Result<Self> {
        let size = src.size()?;
        Ok(Self {
            src,
            size,
            slots: Vec::with_capacity(SLOTS),
            clock: 0,
            budget,
            reads: 0,
            bytes: 0,
        })
    }

    /// Size of the image in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(Error::Corrupt("offset overflow"))?;
        if end > self.size {
            return Err(Error::Corrupt("read past end of image"));
        }
        Ok(())
    }

    /// Accounts `len` bytes against the budget before they are read.
    fn charge(&mut self, len: usize) -> Result<()> {
        let total = self.bytes.saturating_add(len as u64);
        if total > self.budget {
            return Err(Error::TooLarge);
        }
        self.bytes = total;
        self.reads += 1;
        Ok(())
    }

    /// Reads `len` bytes at `offset` into a new vector. The range is validated
    /// before anything is allocated.
    pub fn read_vec(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.check_range(offset, len)?;
        let mut v = vec![0u8; len];
        self.read_exact(offset, &mut v)?;
        Ok(v)
    }

    /// Fills `buf` from `offset`. Small reads go through the chunk cache,
    /// large ones (file contents) bypass it.
    pub fn read_exact(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.check_range(offset, buf.len())?;
        if buf.len() >= CHUNK {
            self.charge(buf.len())?;
            return self.src.read_at(offset, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let chunk_off = pos - pos % CHUNK as u64;
            let idx = self.fetch_chunk(chunk_off)?;
            let slot = &self.slots[idx];
            let within = (pos - chunk_off) as usize;
            let avail = slot.len.saturating_sub(within);
            let n = avail.min(buf.len() - done);
            if n == 0 {
                return Err(Error::Corrupt("read past end of image"));
            }
            buf[done..done + n].copy_from_slice(&slot.data[within..within + n]);
            done += n;
        }
        Ok(())
    }

    fn fetch_chunk(&mut self, chunk_off: u64) -> Result<usize> {
        self.clock += 1;
        if let Some(i) = self.slots.iter().position(|s| s.offset == chunk_off) {
            self.slots[i].stamp = self.clock;
            return Ok(i);
        }
        let len = (self.size - chunk_off).min(CHUNK as u64) as usize;
        self.charge(len)?;
        // Reuse the least recently used buffer once the cache is full.
        let (idx, mut data) = if self.slots.len() < SLOTS {
            (self.slots.len(), Box::default())
        } else {
            let victim = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.stamp)
                .map_or(0, |(i, _)| i);
            let data = std::mem::take(&mut self.slots[victim].data);
            // Invalidate the slot; if the read below fails it stays unused.
            self.slots[victim].offset = u64::MAX;
            self.slots[victim].len = 0;
            self.slots[victim].stamp = 0;
            (victim, data)
        };
        if data.len() < CHUNK {
            data = vec![0u8; CHUNK].into_boxed_slice();
        }
        self.src.read_at(chunk_off, &mut data[..len])?;
        let slot = Slot {
            offset: chunk_off,
            len,
            data,
            stamp: self.clock,
        };
        if idx == self.slots.len() {
            self.slots.push(slot);
        } else {
            self.slots[idx] = slot;
        }
        Ok(idx)
    }
}

/// `ByteSource` over anything seekable (used by the CLI and tests).
pub struct SeekSource<R: Read + Seek>(pub R);

impl<R: Read + Seek> ByteSource for SeekSource<R> {
    fn size(&mut self) -> Result<u64> {
        self.0.seek(SeekFrom::End(0)).map_err(|_| Error::Io)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0
            .seek(SeekFrom::Start(offset))
            .map_err(|_| Error::Io)?;
        self.0.read_exact(buf).map_err(|_| Error::Io)
    }
}
