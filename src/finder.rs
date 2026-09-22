//! Locates the artwork inside a disc image.
//!
//! Search order:
//! 1. `BDMV/META/DL/*.jpg` – Blu-ray Disc Library thumbnails (the standard place).
//! 2. `BDMV/META/TN/*.jpg` – Blu-ray track-name thumbnails.
//! 3. `cover.jpg`, `folder.jpg`, … in the root directory of any data disc.

use crate::error::{Error, Result};
use crate::fs::FileSystem;

/// Largest artwork file we are willing to load. Blu-ray thumbnails are a few
/// hundred KiB; this only limits the root-level cover fallback.
pub const MAX_IMAGE_BYTES: usize = 16 << 20;

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

#[derive(Debug)]
pub struct Thumbnail {
    /// Path of the picked file inside the image, for diagnostics.
    pub path: String,
    /// Raw encoded image bytes (JPEG/PNG/...).
    pub data: Vec<u8>,
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
    ROOT_COVER_NAMES.iter().any(|n| stem.eq_ignore_ascii_case(n))
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

/// Picks the largest picture in `dir`: by the dimensions in its name, then by
/// file size. Empty and oversized files are skipped.
fn best_image_in<F: FileSystem>(
    fs: &mut F,
    dir: &F::Node,
    prefix: &str,
) -> Result<Option<Thumbnail>> {
    let images = fs.list_where(dir, &mut |e| !e.is_dir && has_image_ext(&e.name))?;
    // (area from the file name, file size, index into `images`)
    let mut best: Option<(u64, u64, usize)> = None;
    for (i, e) in images.iter().enumerate() {
        let size = fs.file_size(&e.node).unwrap_or(0);
        if size == 0 || size > MAX_IMAGE_BYTES as u64 {
            continue;
        }
        let area = dims_from_name(&e.name).map_or(0, |(w, h)| w * h);
        if best.is_none_or(|(a, s, _)| (area, size) > (a, s)) {
            best = Some((area, size, i));
        }
    }
    match best {
        Some((_, _, i)) => {
            let e = &images[i];
            let data = fs.read(&e.node, MAX_IMAGE_BYTES)?;
            Ok(Some(Thumbnail {
                path: format!("{prefix}/{}", e.name),
                data,
            }))
        }
        None => Ok(None),
    }
}

fn named_cover_in<F: FileSystem>(fs: &mut F, dir: &F::Node) -> Result<Option<Thumbnail>> {
    let covers = fs.list_where(dir, &mut |e| {
        !e.is_dir && has_image_ext(&e.name) && is_cover_name(&e.name)
    })?;
    for wanted in ROOT_COVER_NAMES {
        let hit = covers
            .iter()
            .find(|e| split_ext(&e.name).0.eq_ignore_ascii_case(wanted));
        if let Some(e) = hit {
            let data = fs.read(&e.node, MAX_IMAGE_BYTES)?;
            return Ok(Some(Thumbnail {
                path: e.name.clone(),
                data,
            }));
        }
    }
    Ok(None)
}

pub fn find_thumbnail<F: FileSystem>(fs: &mut F) -> Result<Thumbnail> {
    let root = fs.root()?;
    if let Some(bdmv) = fs.lookup(&root, "BDMV", true)? {
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
    named_cover_in(fs, &root)?.ok_or(Error::NotFound)
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
