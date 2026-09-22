#![allow(non_snake_case)]
//! IsoPreview – a Windows Explorer thumbnail handler for Blu-ray `.iso` images.
//!
//! The parsing side (UDF / ISO 9660 readers and the artwork finder) is plain
//! Rust; the Windows side (COM object, WIC decoding, registration) is behind
//! `cfg(windows)` and is exported from the DLL.

pub mod bytes;
pub mod error;
pub mod extract;
pub mod finder;
pub mod fs;
pub mod iso9660;
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
