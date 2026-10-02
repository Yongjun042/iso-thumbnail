#![allow(non_snake_case)]
//! IsoPreview – a Windows Explorer thumbnail handler for Blu-ray and DVD `.iso` images.
//!
//! The parsing side (UDF / ISO 9660 readers, the artwork finder, the MPEG
//! program stream demuxer and the intra-only MPEG-1/2 decoder) is plain Rust; the Windows side (COM object, WIC decoding, registration) is behind
//! `cfg(windows)` and is exported from the DLL.

pub mod bdnav;
pub mod bluray;
pub mod bytes;
pub mod clpi;
pub mod dvd;
pub mod error;
pub mod extract;
pub mod finder;
pub mod fs;
pub mod ifo;
pub mod igs;
pub mod iso9660;
pub mod m2ts;
pub mod mpeg2;
pub mod mpegps;
pub mod mpls;
pub mod nal;
pub mod picture;
pub mod reader;
pub mod udf;
pub mod yuv;

#[cfg(windows)]
pub mod com;
#[cfg(windows)]
mod exports;
#[cfg(windows)]
pub mod image;
#[cfg(windows)]
pub mod mf;
#[cfg(windows)]
pub mod registry;

pub use error::{Error, Result};
pub use extract::{extract_thumbnail, extract_thumbnail_for, Extracted};
