//! Bit reader and start code scanner for MPEG video elementary streams.

/// MSB-first bit reader over one syntax unit (the bytes between two start
/// codes).
///
/// Bits are served from a 64-bit cache refilled eight bytes at a time. Reading
/// past the end of the data yields zero bits instead of failing: every VLC
/// table of the syntax treats a long run of zeros as an invalid code, so a
/// decoder that ran off the end stops on its own, and `overrun` tells the
/// caller afterwards. This keeps bounds checks out of the per-symbol path.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte of `data` to load into the cache (may pass `data.len()` by the
    /// zero bytes served after the end).
    pos: usize,
    /// Unread bits, left-aligned; the bits below the top `count` are zero.
    cache: u64,
    /// Valid bits in `cache` (0..=63).
    count: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut r = BitReader {
            data,
            pos: 0,
            cache: 0,
            count: 0,
        };
        r.refill();
        r
    }

    /// Tops the cache up to at least 56 valid bits.
    #[inline(always)]
    fn refill(&mut self) {
        if let Some(chunk) = self.data.get(self.pos..self.pos.wrapping_add(8)) {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            let word = u64::from_be_bytes(word);
            // Whole bytes that fit below the valid bits.
            let take = (63 - self.count) >> 3;
            let new_count = self.count + take * 8;
            // Keep only the bits of whole bytes so the next refill can OR
            // into zeros.
            self.cache = (self.cache | (word >> self.count)) & !(u64::MAX >> new_count);
            self.count = new_count;
            self.pos += take as usize;
        } else {
            self.refill_tail();
        }
    }

    /// Byte-by-byte refill near the end of the data, padding with zeros.
    #[cold]
    fn refill_tail(&mut self) {
        while self.count < 56 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.cache |= u64::from(byte) << (56 - self.count);
            self.count += 8;
            // Saturating: past the end `pos` only counts served zero bytes.
            self.pos = self.pos.saturating_add(1);
        }
    }

    /// The next 32 bits, MSB first, without consuming them.
    #[inline(always)]
    pub fn peek32(&mut self) -> u32 {
        if self.count < 32 {
            self.refill();
        }
        (self.cache >> 32) as u32
    }

    /// The next `n` bits (1..=32) without consuming them.
    #[inline(always)]
    pub fn peek(&mut self, n: u32) -> u32 {
        debug_assert!((1..=32).contains(&n));
        self.peek32() >> (32 - n)
    }

    /// Consumes `n` bits (0..=32). Must follow a `peek`/`peek32` that made
    /// those bits available.
    #[inline(always)]
    pub fn skip(&mut self, n: u32) {
        debug_assert!(n <= 32 && n <= self.count);
        self.cache <<= n;
        self.count -= n;
    }

    /// Reads `n` bits (0..=32) as an unsigned number.
    #[inline(always)]
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    /// Reads one bit as a flag.
    #[inline(always)]
    pub fn flag(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Bits consumed so far.
    #[inline]
    pub fn position(&self) -> u64 {
        (self.pos as u64)
            .saturating_mul(8)
            .saturating_sub(u64::from(self.count))
    }

    /// Whether more bits were consumed than the data holds.
    #[inline]
    pub fn overrun(&self) -> bool {
        self.position() > (self.data.len() as u64).saturating_mul(8)
    }
}

/// Offset of the first start code prefix (`00 00 01`) at or after `from`.
///
/// Skips three bytes at a time while the probed byte cannot be part of a
/// prefix, which makes the scan cheap on coded data.
pub fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from.checked_add(2)?;
    while i < data.len() {
        let b = data[i];
        if b > 1 {
            i += 3;
        } else if b == 1 && data[i - 1] == 0 && data[i - 2] == 0 {
            return Some(i - 2);
        } else {
            i += 1;
        }
    }
    None
}

/// One syntax unit: a start code and the bytes up to the next start code.
#[derive(Debug, Clone, Copy)]
pub struct Unit {
    /// The start code value (the byte after `00 00 01`).
    pub code: u8,
    /// Payload: the bytes after the start code value up to the next prefix
    /// (or the end of the data).
    pub payload: (usize, usize),
}

/// Iterator over the start code units of an elementary stream.
pub struct Units<'a> {
    data: &'a [u8],
    next: Option<usize>,
}

impl<'a> Units<'a> {
    pub fn new(data: &'a [u8], from: usize) -> Self {
        Units {
            data,
            next: find_start_code(data, from),
        }
    }
}

impl Iterator for Units<'_> {
    type Item = Unit;

    fn next(&mut self) -> Option<Unit> {
        let start = self.next?;
        let Some(&code) = self.data.get(start + 3) else {
            self.next = None;
            return None;
        };
        let body = start + 4;
        self.next = find_start_code(self.data, body);
        let end = self.next.unwrap_or(self.data.len());
        Some(Unit {
            code,
            payload: (body, end),
        })
    }
}
