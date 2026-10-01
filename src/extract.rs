//! Top-level "give me the thumbnail of this image" entry point.

use crate::error::{Error, Result};
use crate::finder::{find_thumbnail, Thumbnail};
use crate::fs::FileSystem;
use crate::iso9660::Iso9660;
use crate::reader::{ByteSource, CachedReader};
use crate::udf::Udf;

#[derive(Debug)]
pub struct Extracted {
    pub thumbnail: Thumbnail,
    /// Which file system the artwork came from ("UDF 2.50 (metadata partition)", ...).
    pub filesystem: String,
    /// Number of reads issued against the image.
    pub reads: u32,
    /// Number of bytes fetched from the image.
    pub bytes_read: u64,
}

fn try_udf<S: ByteSource>(
    rd: &mut CachedReader<S>,
    dvd_searched: &mut bool,
) -> Result<(Thumbnail, String)> {
    let mut fs = Udf::open(rd)?;
    let thumb = find_thumbnail(&mut fs, dvd_searched)?;
    Ok((thumb, fs.description()))
}

fn try_iso9660<S: ByteSource>(
    rd: &mut CachedReader<S>,
    dvd_searched: &mut bool,
) -> Result<(Thumbnail, String)> {
    let mut fs = Iso9660::open(rd)?;
    let thumb = find_thumbnail(&mut fs, dvd_searched)?;
    Ok((thumb, fs.description()))
}

/// Finds the thumbnail of a disc image. UDF is tried first because Blu-ray
/// discs are UDF 2.50 and DVDs UDF 1.02; ISO 9660 (with Joliet) is the fallback.
pub fn extract_thumbnail<S: ByteSource>(src: S) -> Result<Extracted> {
    let mut rd = CachedReader::new(src)?;
    // Shared by both passes so a DVD's frame search runs once.
    let mut dvd_searched = false;
    let (thumbnail, filesystem) = match try_udf(&mut rd, &mut dvd_searched) {
        Ok(found) => found,
        Err(udf_err) => match try_iso9660(&mut rd, &mut dvd_searched) {
            Ok(found) => found,
            Err(Error::NoVolume) => return Err(udf_err),
            Err(iso_err) => return Err(iso_err),
        },
    };
    Ok(Extracted {
        thumbnail,
        filesystem,
        reads: rd.reads,
        bytes_read: rd.bytes,
    })
}
