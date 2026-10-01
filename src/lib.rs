#![allow(non_snake_case)]
//! IsoPreview – a Windows Explorer thumbnail handler for Blu-ray and DVD `.iso` images.
//!
//! The parsing side (UDF / ISO 9660 readers, the artwork finder, the MPEG
//! program stream demuxer and the intra-only MPEG-1/2 decoder) is plain Rust; the Windows side (COM object, WIC decoding, registration) is behind
//! `cfg(windows)` and is exported from the DLL.

pub mod bytes;
pub mod dvd;
pub mod error;
pub mod extract;
pub mod finder;
pub mod fs;
pub mod iso9660;
pub mod mpeg2;
pub mod mpegps;
pub mod picture;
pub mod reader;
pub mod udf;

#[cfg(windows)]
pub mod com;
#[cfg(windows)]
mod exports;
#[cfg(windows)]
pub mod image;
#[cfg(windows)]
pub mod registry;

pub use error::{Error, Result};
pub use extract::{extract_thumbnail, Extracted};
