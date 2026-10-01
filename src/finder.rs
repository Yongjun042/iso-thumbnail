//! Locates the artwork inside a disc image.
//!
//! Search order:
//! 1. `BDMV/META/DL/*.jpg` – Blu-ray Disc Library thumbnails (the standard place).
//! 2. `BDMV/META/TN/*.jpg` – Blu-ray track-name thumbnails.
//! 3. `JACKET_P/J00___5L.MP2` – the DVD-Video jacket picture (see `crate::dvd`).
//! 4. `cover.jpg`, `folder.jpg`, … in the root directory of any data disc.
//! 5. A picture of the DVD-Video's video (`VIDEO_TS`): its root or title menu,
//!    else a frame of the main title (see `crate::dvd`).

use crate::dvd;
use crate::error::{Error, Result};
use crate::fs::{DirEntry, FileSystem};
use crate::picture::Picture;

/// Largest artwork file we are willing to load. Blu-ray thumbnails are a few
/// hundred KiB; this only limits the root-level cover fallback.
pub const MAX_IMAGE_BYTES: usize = 16 << 20;
/// Most picture files considered in one directory. Real `META/DL` and
/// `META/TN` directories hold a handful; the cap bounds per-candidate work.
const MAX_IMAGE_CANDIDATES: usize = 64;

const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "bmp", "gif"];
const ROOT_COVER_NAMES: [&str; 7] = [
    "folder",
    "cover",
    "poster",
    "thumbnail",
    "thumb",
    "front",
    "artwork",
];

/// What a thumbnail is made from.
#[derive(Debug)]
pub enum Content {
    /// An encoded picture file (JPEG, PNG, GIF or BMP) still to be decoded.
    Encoded(Vec<u8>),
    /// A picture already decoded from MPEG video (DVD jacket or title frame).
    Picture(Picture),
}

#[derive(Debug)]
pub struct Thumbnail {
    /// Where the picture came from inside the image, for diagnostics.
    pub path: String,
    pub content: Content,
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    }
}

fn has_image_ext(name: &str) -> bool {
    let (_, ext) = split_ext(name);
    IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

fn is_cover_name(name: &str) -> bool {
    let (stem, _) = split_ext(name);
    ROOT_COVER_NAMES
        .iter()
        .any(|n| stem.eq_ignore_ascii_case(n))
}

/// Extracts `WxH` from names such as `MOVIE_640x360.jpg`.
fn dims_from_name(name: &str) -> Option<(u64, u64)> {
    let b = name.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c != b'x' && c != b'X' {
            continue;
        }
        let w_start = b[..i]
            .iter()
            .rposition(|c| !c.is_ascii_digit())
            .map_or(0, |p| p + 1);
        let h_end = b[i + 1..]
            .iter()
            .position(|c| !c.is_ascii_digit())
            .map_or(b.len(), |p| i + 1 + p);
        if w_start < i && h_end > i + 1 {
            let w: u64 = name[w_start..i].parse().ok()?;
            let h: u64 = name[i + 1..h_end].parse().ok()?;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
    }
    None
}

/// Pixel area from the `WxH` in a file name; 0 when absent or implausible.
fn area_from_name(name: &str) -> u64 {
    dims_from_name(name)
        .and_then(|(w, h)| w.checked_mul(h))
        .unwrap_or(0)
}

/// Reads the first candidate that is non-empty, within `MAX_IMAGE_BYTES` and
/// readable, so one broken file does not hide the next.
fn first_readable<F: FileSystem>(
    fs: &mut F,
    candidates: impl IntoIterator<Item = (String, F::Node)>,
) -> Option<Thumbnail> {
    for (path, node) in candidates {
        let size = fs.file_size(&node).unwrap_or(0);
        if size == 0 || size > MAX_IMAGE_BYTES as u64 {
            continue;
        }
        if let Ok(data) = fs.read(&node, MAX_IMAGE_BYTES) {
            return Some(Thumbnail {
                path,
                content: Content::Encoded(data),
            });
        }
    }
    None
}

/// Picks the largest picture in `dir`: by the dimensions in its name, then by
/// file size. Empty, oversized and unreadable files are skipped.
fn best_image_in<F: FileSystem>(
    fs: &mut F,
    dir: &F::Node,
    prefix: &str,
) -> Result<Option<Thumbnail>> {
    let mut images = Vec::new();
    fs.walk(dir, &mut |e| {
        if !e.is_dir && has_image_ext(&e.name) {
            images.push(e);
        }
        images.len() < MAX_IMAGE_CANDIDATES
    })?;
    // (area from the file name, file size, entry), largest first. The sort is
    // stable, so ties keep directory order.
    let mut ranked: Vec<(u64, u64, DirEntry<F::Node>)> = images
        .into_iter()
        .map(|e| {
            let size = fs.file_size(&e.node).unwrap_or(0);
            (area_from_name(&e.name), size, e)
        })
        .collect();
    ranked.sort_by_key(|&(area, size, _)| std::cmp::Reverse((area, size)));
    let candidates = ranked
        .into_iter()
        .map(|(_, _, e)| (format!("{prefix}/{}", e.name), e.node));
    Ok(first_readable(fs, candidates))
}

/// Finds the thumbnail of the file system's disc, in the order described in
/// the module documentation.
///
/// `dvd_searched` says whether the DVD steps (jacket picture, menus and
/// frames) already ran on another view of the same disc: a DVD carries the
/// same files on its UDF and ISO 9660 sides, and the video search is the
/// costly part, so it runs at most once. It is set once this call actually
/// read video packets from the DVD's title VOBs; a view whose title VOBs
/// cannot be read leaves it unset, so the other view still gets its turn.
pub fn find_thumbnail<F: FileSystem>(fs: &mut F, dvd_searched: &mut bool) -> Result<Thumbnail> {
    let root = fs.root()?;
    // One pass over the root collects the disc folders and the cover
    // fallbacks: every walk reads the whole directory from the image again.
    let mut bdmv = None;
    let mut video_ts = None;
    let mut jacket_p = None;
    let mut covers = Vec::new();
    fs.walk(&root, &mut |e| {
        if e.is_dir {
            let slot = if e.name.eq_ignore_ascii_case("BDMV") {
                &mut bdmv
            } else if e.name.eq_ignore_ascii_case("VIDEO_TS") {
                &mut video_ts
            } else if e.name.eq_ignore_ascii_case("JACKET_P") {
                &mut jacket_p
            } else {
                return true;
            };
            if slot.is_none() {
                *slot = Some(e.node);
            }
        } else if has_image_ext(&e.name)
            && is_cover_name(&e.name)
            && covers.len() < MAX_IMAGE_CANDIDATES
        {
            covers.push(e);
        }
        true
    })?;
    if let Some(bdmv) = bdmv {
        if let Some(meta) = fs.lookup(&bdmv, "META", true)? {
            for sub in ["DL", "TN"] {
                if let Some(dir) = fs.lookup(&meta, sub, true)? {
                    if let Some(t) = best_image_in(fs, &dir, &format!("BDMV/META/{sub}"))? {
                        return Ok(t);
                    }
                }
            }
        }
    }
    let search_dvd = !*dvd_searched;
    if let Some(dir) = jacket_p.as_ref().filter(|_| search_dvd) {
        if let Some(t) = dvd::jacket_picture(fs, dir) {
            return Ok(t);
        }
    }
    // Root covers in the priority order of ROOT_COVER_NAMES.
    let candidates = ROOT_COVER_NAMES.iter().flat_map(|wanted| {
        covers
            .iter()
            .filter(|e| split_ext(&e.name).0.eq_ignore_ascii_case(wanted))
            .map(|e| (e.name.clone(), e.node.clone()))
    });
    if let Some(t) = first_readable(fs, candidates) {
        return Ok(t);
    }
    // Last: the DVD's menus or a frame of its video, which costs the most.
    if let Some(dir) = video_ts.as_ref().filter(|_| search_dvd) {
        if let Some(t) = dvd::video_picture(fs, dir, dvd_searched) {
            return Ok(t);
        }
    }
    Err(Error::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dimensions_from_names() {
        assert_eq!(dims_from_name("MOVIE_640x360.jpg"), Some((640, 360)));
        assert_eq!(dims_from_name("cover_416X240.JPG"), Some((416, 240)));
        assert_eq!(dims_from_name("cover.jpg"), None);
        assert_eq!(dims_from_name("x.jpg"), None);
        assert_eq!(dims_from_name("0x1F.jpg"), None);
        assert_eq!(dims_from_name("é640x360.jpg"), Some((640, 360)));
    }

    #[test]
    fn huge_dimensions_in_names_do_not_overflow() {
        assert_eq!(area_from_name("COVER_18446744073709551615x2.jpg"), 0);
        assert_eq!(area_from_name("MOVIE_640x360.jpg"), 230_400);
        assert_eq!(area_from_name("cover.jpg"), 0);
    }

    #[test]
    fn recognises_image_extensions() {
        assert!(has_image_ext("a.JPG"));
        assert!(has_image_ext("a.jpeg"));
        assert!(has_image_ext("a.png"));
        assert!(!has_image_ext("a.xml"));
        assert!(!has_image_ext("jpg"));
        assert!(is_cover_name("Folder.jpg"));
        assert!(!is_cover_name("folder2.jpg"));
    }
}
