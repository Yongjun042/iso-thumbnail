//! DVD-Video thumbnails on synthetic disc images.

extern crate IsoPreview as iso_preview;

#[path = "support/disc_builder.rs"]
mod disc_builder;

use std::io::Cursor;

use iso_preview::fs::FileSystem;
use iso_preview::iso9660::Iso9660;
use iso_preview::reader::{CachedReader, SeekSource};
use iso_preview::udf::Udf;

use disc_builder::{iso9660, udf102, UdfOptions};

fn reader(image: Vec<u8>) -> CachedReader<SeekSource<Cursor<Vec<u8>>>> {
    CachedReader::new(SeekSource(Cursor::new(image))).unwrap()
}

/// Opens every file of `files` by path and checks its size and contents.
fn open_tree<F: FileSystem>(fs: &mut F, files: &[(&str, &[u8])]) -> Vec<F::Node> {
    let mut nodes = Vec::new();
    for (path, data) in files {
        let mut node = fs.root().unwrap();
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            node = fs
                .lookup(&node, part, !last)
                .unwrap()
                .unwrap_or_else(|| panic!("{path}: {part} not found"));
        }
        assert_eq!(fs.file_size(&node).unwrap(), data.len() as u64, "{path}");
        assert_eq!(&fs.read(&node, 64 << 20).unwrap(), data, "{path}");
        nodes.push(node);
    }
    nodes
}

/// Reads every file back through `read_range` in odd-sized pieces.
fn check_ranges<F: FileSystem>(fs: &mut F, files: &[(&str, &[u8])], nodes: &[F::Node]) {
    for ((path, data), node) in files.iter().zip(nodes) {
        let mut got = Vec::new();
        let mut buf = vec![0u8; 3001];
        loop {
            let n = fs.read_range(node, got.len() as u64, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, data, "{path} via read_range");
    }
}

fn sample_files() -> Vec<(&'static str, Vec<u8>)> {
    let pattern = |len: usize, seed: u8| -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(seed as u32) as u8)
            .collect()
    };
    let mut files = vec![
        ("VIDEO_TS/VIDEO_TS.IFO", pattern(6000, 1)),
        ("VIDEO_TS/VIDEO_TS.VOB", pattern(10 * 2048, 2)),
        ("VIDEO_TS/VTS_01_0.IFO", pattern(4096, 3)),
        ("VIDEO_TS/VTS_01_1.VOB", pattern(123_457, 4)),
        ("VIDEO_TS/VTS_01_2.VOB", pattern(2048, 5)),
        ("AUDIO_TS/EMPTY.TXT", Vec::new()),
        ("JACKET_P/J00___5L.MP2", pattern(777, 6)),
        ("README.TXT", b"hello".to_vec()),
    ];
    // Enough entries for a directory spanning several sectors.
    for i in 0..90 {
        let name: &'static str =
            Box::leak(format!("VIDEO_TS/VTS_{:02}_0.BUP", i + 2).into_boxed_str());
        files.push((name, pattern(100 + i, i as u8)));
    }
    files
}

#[test]
fn builders_produce_readable_images() {
    let owned = sample_files();
    let files: Vec<(&str, &[u8])> = owned.iter().map(|(p, d)| (*p, d.as_slice())).collect();
    let images = [
        ("ISO 9660", iso9660(&files)),
        ("UDF", udf102(&files, UdfOptions::default())),
        (
            "UDF, 3-block extents",
            udf102(
                &files,
                UdfOptions {
                    extent_blocks: Some(3),
                },
            ),
        ),
    ];
    // Directory structure and whole-file reads first, then ranged reads.
    for (what, image) in &images {
        eprintln!("checking {what}");
        let mut rd = reader(image.clone());
        if what.starts_with("UDF") {
            let mut fs = Udf::open(&mut rd).unwrap();
            assert_eq!(fs.description(), "UDF 1.02");
            open_tree(&mut fs, &files);
        } else {
            let mut fs = Iso9660::open(&mut rd).unwrap();
            open_tree(&mut fs, &files);
        }
    }
    for (what, image) in &images {
        eprintln!("ranges of {what}");
        let mut rd = reader(image.clone());
        if what.starts_with("UDF") {
            let mut fs = Udf::open(&mut rd).unwrap();
            let nodes = open_tree(&mut fs, &files);
            check_ranges(&mut fs, &files, &nodes);
        } else {
            let mut fs = Iso9660::open(&mut rd).unwrap();
            let nodes = open_tree(&mut fs, &files);
            check_ranges(&mut fs, &files, &nodes);
        }
    }
}
