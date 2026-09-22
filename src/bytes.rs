//! Bounds-checked little-endian field access for on-disk structures.

use crate::error::{Error, Result};

const TRUNCATED: Error = Error::Corrupt("truncated descriptor");

#[inline]
pub fn slice(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    off.checked_add(len)
        .and_then(|end| b.get(off..end))
        .ok_or(TRUNCATED)
}

#[inline]
pub fn u8_at(b: &[u8], off: usize) -> Result<u8> {
    b.get(off).copied().ok_or(TRUNCATED)
}

#[inline]
pub fn u16le(b: &[u8], off: usize) -> Result<u16> {
    let s = slice(b, off, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
pub fn u32le(b: &[u8], off: usize) -> Result<u32> {
    let s = slice(b, off, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
pub fn u64le(b: &[u8], off: usize) -> Result<u64> {
    let s = slice(b, off, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}
