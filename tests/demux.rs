//! Program stream demuxer tests: synthetic DVD-like streams from the test
//! writer, hand-built edge cases, random corruption, and ffmpeg as an oracle
//! for real DVD (MPEG-2) and MPEG-1 program streams.

extern crate IsoPreview as iso_preview;

#[path = "support/ps_writer.rs"]
mod ps_writer;

use std::path::{Path, PathBuf};
use std::process::Command;

use iso_preview::mpegps::{is_program_stream, Demuxer, MAX_UNIT_LEN};
use ps_writer::{mux, PsOptions, PACK_SIZE};

/// Small xorshift generator: deterministic tests without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A fake MPEG-2 video elementary stream: `gops` groups of pictures, each with
/// a sequence header, a GOP header and an I-picture followed by P and B
/// pictures of random sizes. Slice data never contains a zero byte, so the
/// only start codes are the ones written here.
fn synthetic_es(seed: u64, gops: usize) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut es = Vec::new();
    for g in 0..gops {
        es.extend_from_slice(&[
            0, 0, 1, 0xB3, 0x2D, 0x01, 0xE0, 0x24, 0xFF, 0xFF, 0xE0, 0x18,
        ]);
        es.extend_from_slice(&[0, 0, 1, 0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00]);
        es.extend_from_slice(&[0, 0, 1, 0xB8, 0x00, 0x08, (g as u8) << 1, 0x40]);
        for (n, kind) in [1u8, 3, 3, 2, 3, 3, 2].into_iter().enumerate() {
            // temporal_reference, picture_coding_type, vbv_delay.
            es.extend_from_slice(&[
                0,
                0,
                1,
                0x00,
                (n >> 2) as u8,
                ((n as u8) << 6) | (kind << 3),
                0xFF,
                0xF8,
            ]);
            es.extend_from_slice(&[0, 0, 1, 0xB5, 0x8F, 0xFF, 0xF3, 0x41, 0x80]);
            let slices = 1 + rng.below(30);
            for s in 0..slices {
                es.extend_from_slice(&[0, 0, 1, 1 + (s % 0xAF) as u8]);
                let len = rng.below(if kind == 1 { 4000 } else { 900 });
                es.extend((0..len).map(|_| 1 + rng.below(255) as u8));
            }
        }
    }
    es.extend_from_slice(&[0, 0, 1, 0xB7]);
    es
}

/// Feeds `ps` to a fresh demuxer in chunks of `chunk` bytes, the way a caller
/// reading a file would, and returns the demuxer, the elementary stream and
/// how many bytes were left unconsumed at the end.
fn demux(ps: &[u8], chunk: usize) -> (Demuxer, Vec<u8>, usize) {
    let mut d = Demuxer::new();
    let mut es = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    for part in ps.chunks(chunk.max(1)) {
        buf.extend_from_slice(part);
        let used = d.push(&buf, &mut es);
        assert!(used <= buf.len());
        buf.drain(..used);
        assert!(
            buf.len() <= MAX_UNIT_LEN,
            "demuxer stalls with {} bytes",
            buf.len()
        );
    }
    (d, es, buf.len())
}

#[test]
fn writer_layout_is_dvd_like() {
    let es = synthetic_es(1, 3);
    let m = mux(&es, &PsOptions::default());
    assert!(is_program_stream(&m.data));
    assert_eq!(m.data.len(), m.packs() * PACK_SIZE + 4);
    assert_eq!(m.nav_packs, 3);
    assert!(m.audio_packs > 0);
    for pack in m.data.chunks_exact(PACK_SIZE) {
        assert!(is_program_stream(pack));
        assert_eq!(pack[13] & 0xF8, 0xF8);
    }
}

#[test]
fn chunked_input_gives_the_exact_elementary_stream() {
    let es = synthetic_es(7, 4);
    let variants = [
        PsOptions::default(),
        PsOptions {
            audio_every: 0,
            end_code: false,
            ..PsOptions::default()
        },
        PsOptions {
            mpeg1: true,
            ..PsOptions::default()
        },
        PsOptions {
            video_id: 0xE3,
            audio_every: 1,
            ..PsOptions::default()
        },
    ];
    for opts in variants {
        let m = mux(&es, &opts);
        for chunk in [1, 7, 2048, 10_000, m.data.len()] {
            let (d, out, left) = demux(&m.data, chunk);
            assert_eq!(left, 0, "{opts:?} chunk {chunk}: bytes left");
            assert!(out == es, "{opts:?} chunk {chunk}: ES differs");
            assert_eq!(d.stream_id, Some(opts.video_id));
            assert_eq!(d.video_packets, m.video_packets);
            assert_eq!(d.scrambled_packets, 0);
        }
    }
}

#[test]
fn scrambled_packets_are_counted_not_extracted() {
    let es = synthetic_es(3, 2);
    let m = mux(
        &es,
        &PsOptions {
            scrambled: true,
            ..PsOptions::default()
        },
    );
    for chunk in [7, 2048, m.data.len()] {
        let (d, out, left) = demux(&m.data, chunk);
        assert!(out.is_empty());
        assert_eq!(left, 0);
        assert_eq!(d.stream_id, Some(0xE0));
        assert_eq!(d.video_packets, 0);
        assert_eq!(d.scrambled_packets, m.video_packets);
    }
}

/// Garbage that may hold zeros, partial start codes, video-level start codes
/// and a pack start code with broken marker bits, but no valid unit.
fn garbage(rng: &mut Rng) -> Vec<u8> {
    let mut g = Vec::new();
    let len = 1 + rng.below(300);
    while g.len() < len {
        match rng.below(8) {
            0 => g.extend_from_slice(&[0, 0]),
            1 => g.extend_from_slice(&[0, 0, 1, 0xB3]),
            2 => g.extend_from_slice(&[0, 0, 1, 0xBA, 0x00, 0x11]),
            3 => g.push(0),
            _ => g.push(0x10 + rng.below(0xE0) as u8),
        }
    }
    g
}

#[test]
fn garbage_between_packs_is_skipped() {
    let es = synthetic_es(11, 3);
    for opts in [
        PsOptions::default(),
        PsOptions {
            mpeg1: true,
            ..PsOptions::default()
        },
    ] {
        let m = mux(&es, &opts);
        let mut rng = Rng(0xDEAD_BEEF);
        let mut dirty = garbage(&mut rng);
        for pack in m.data.chunks(PACK_SIZE) {
            dirty.extend_from_slice(pack);
            if rng.below(2) == 0 {
                dirty.extend(garbage(&mut rng));
            }
        }
        for chunk in [1, 7, 2048, 10_000, dirty.len()] {
            let (d, out, _) = demux(&dirty, chunk);
            assert!(out == es, "{opts:?} chunk {chunk}: ES differs");
            assert_eq!(d.video_packets, m.video_packets);
        }
    }
}

#[test]
fn edge_cases() {
    let mut d = Demuxer::new();
    let mut es = Vec::new();
    // Possible beginnings of a start code wait for more data ...
    assert_eq!(d.push(&[], &mut es), 0);
    assert_eq!(d.push(&[0], &mut es), 0);
    assert_eq!(d.push(&[0, 0, 1], &mut es), 0);
    assert_eq!(d.push(&[0, 0, 1, 0xBA, 0x44], &mut es), 0);
    assert_eq!(d.push(&[0, 0, 1, 0xE0, 0x07], &mut es), 0);
    // ... anything that cannot become valid is dropped at once, except
    // trailing bytes that may begin the next start code.
    assert_eq!(d.push(&[1, 2, 3], &mut es), 3);
    assert_eq!(d.push(&[0, 0, 1, 0xB3, 0x12, 0, 0], &mut es), 5);
    assert_eq!(d.push(&[0, 0, 1, 0xBA, 0x00, 0x11], &mut es), 6);
    // A video PES packet with length 0 is corrupt, as are bad PES headers
    // (PTS_DTS_flags '01'; a header longer than the packet).
    assert_eq!(d.push(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0x11, 0x22], &mut es), 9);
    assert_eq!(
        d.push(&[0, 0, 1, 0xE0, 0, 4, 0x80, 0x40, 0, 0x55], &mut es),
        10
    );
    assert_eq!(d.push(&[0, 0, 1, 0xE0, 0, 3, 0x80, 0x80, 9], &mut es), 9);
    assert_eq!(d.stream_id, None);
    assert!(es.is_empty());

    // The first video stream is followed, later ones are skipped; MPEG-1
    // headers in all their forms are understood.
    let mut ps = vec![0, 0, 1, 0xB9];
    let mut pes = |id: u8, header: &[u8], payload: &[u8]| {
        ps.extend_from_slice(&[0, 0, 1, id]);
        ps.extend_from_slice(&((header.len() + payload.len()) as u16).to_be_bytes());
        ps.extend_from_slice(header);
        ps.extend_from_slice(payload);
    };
    pes(0xE1, &[0x0F], b"one ");
    pes(0xE0, &[0x0F], b"other");
    pes(0xE1, &[0xFF, 0xFF, 0x41, 0x00, 0x21, 0, 1, 0, 1], b"two ");
    pes(0xE1, &[0x31, 0, 1, 0, 1, 0x11, 0, 1, 0, 1], b"three ");
    pes(0xE1, &[0x80, 0x00, 0x02, 0xFF, 0xFF], b"four");
    pes(0xBE, &[], &[0xFF; 10]);
    let mut d = Demuxer::new();
    let mut es = Vec::new();
    assert_eq!(d.push(&ps, &mut es), ps.len());
    assert_eq!(es, b"one two three four");
    assert_eq!(d.stream_id, Some(0xE1));
    assert_eq!(d.video_packets, 4);
}

/// Random corruption must never panic, stall or produce more output than input.
#[test]
fn random_mutations_never_panic() {
    let es = synthetic_es(5, 2);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for opts in [
        PsOptions::default(),
        PsOptions {
            mpeg1: true,
            ..PsOptions::default()
        },
    ] {
        let m = mux(&es, &opts);
        for _ in 0..300 {
            let mut data = m.data.clone();
            for _ in 0..1 + rng.below(40) {
                let at = rng.below(data.len());
                match rng.below(4) {
                    0 => data[at] = rng.next() as u8,
                    1 => data[at] ^= 1 << rng.below(8),
                    2 => {
                        data.remove(at);
                    }
                    _ => data.insert(at, [0, 1, 0xBA, 0xE0, 0xFF][rng.below(5)]),
                }
            }
            let chunk = 1 + rng.below(5000);
            let (_, out, _) = demux(&data, chunk);
            assert!(out.len() <= data.len());
        }
    }
    // Pure noise, and a stream of nothing but start code prefixes.
    let noise: Vec<u8> = (0..100_000).map(|_| rng.next() as u8).collect();
    let (_, out, _) = demux(&noise, 4096);
    assert!(out.len() <= noise.len());
    let prefixes: Vec<u8> = [0u8, 0, 1].iter().copied().cycle().take(30_000).collect();
    let (_, out, left) = demux(&prefixes, 1000);
    assert!(out.is_empty() && left <= 3);
}

// ----------------------------------------------------------------------------
// ffmpeg as an oracle
// ----------------------------------------------------------------------------

/// `ffmpeg` (or `ffprobe`) from PATH, if installed.
fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&exe))
        .find(|p| p.is_file())
}

/// A temporary directory removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("isopreview-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(tool: &Path, args: &[&str]) {
    let out = Command::new(tool)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("UTF-8 temp path")
}

/// Demuxes `ps` in 64 KiB chunks.
fn demux_file(ps: &[u8]) -> (Demuxer, Vec<u8>) {
    let (d, es, left) = demux(ps, 64 << 10);
    assert_eq!(left, 0);
    (d, es)
}

#[test]
fn ffmpeg_dvd_vob_matches_ffmpeg_demuxer() {
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        eprintln!("skipping: ffmpeg not found on PATH");
        return;
    };
    let tmp = TempDir::new("vob");
    let (vob, reference) = (tmp.path("x.vob"), tmp.path("ref.m2v"));
    run(
        &ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=720x480:r=30000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000",
            "-t",
            "3",
            "-target",
            "ntsc-dvd",
            path_str(&vob),
        ],
    );
    run(
        &ffmpeg,
        &[
            "-i",
            path_str(&vob),
            "-map",
            "0:v",
            "-c",
            "copy",
            "-f",
            "mpeg2video",
            path_str(&reference),
        ],
    );
    let ps = std::fs::read(&vob).unwrap();
    let want = std::fs::read(&reference).unwrap();
    assert!(
        want.len() > 100_000,
        "reference ES only {} bytes",
        want.len()
    );
    assert!(is_program_stream(&ps));
    // ffmpeg's DVD muxer writes 2048-byte packs with NAV packets.
    assert!(ps.len() % PACK_SIZE <= 4);
    let (d, es) = demux_file(&ps);
    assert_eq!(d.stream_id, Some(0xE0));
    assert_eq!(d.scrambled_packets, 0);
    assert!(d.video_packets > 0);
    assert_eq!(es.len(), want.len());
    assert!(es == want, "demuxed ES differs from ffmpeg's");

    // The test writer's output must demux identically, in ffmpeg too: that
    // checks the writer against an independent implementation.
    let ours = tmp.path("ours.vob");
    let back = tmp.path("back.m2v");
    let m = mux(&want, &PsOptions::default());
    std::fs::write(&ours, &m.data).unwrap();
    run(
        &ffmpeg,
        &[
            "-i",
            path_str(&ours),
            "-map",
            "0:v",
            "-c",
            "copy",
            "-f",
            "mpeg2video",
            path_str(&back),
        ],
    );
    assert!(
        std::fs::read(&back).unwrap() == want,
        "ffmpeg reads the writer's stream differently"
    );
    let (d, es) = demux_file(&m.data);
    assert!(es == want);
    assert_eq!(d.video_packets, m.video_packets);
}

#[test]
fn ffmpeg_mpeg1_program_stream_matches_ffmpeg_demuxer() {
    let Some(ffmpeg) = find_tool("ffmpeg") else {
        eprintln!("skipping: ffmpeg not found on PATH");
        return;
    };
    let tmp = TempDir::new("mpg");
    let (mpg, reference) = (tmp.path("x.mpg"), tmp.path("ref.m1v"));
    run(
        &ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=352x240:r=30000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000",
            "-t",
            "3",
            "-c:v",
            "mpeg1video",
            "-b:v",
            "1150k",
            "-c:a",
            "mp2",
            "-ar",
            "44100",
            "-f",
            "mpeg",
            path_str(&mpg),
        ],
    );
    run(
        &ffmpeg,
        &[
            "-i",
            path_str(&mpg),
            "-map",
            "0:v",
            "-c",
            "copy",
            "-f",
            "mpeg1video",
            path_str(&reference),
        ],
    );
    let ps = std::fs::read(&mpg).unwrap();
    let want = std::fs::read(&reference).unwrap();
    assert!(
        want.len() > 100_000,
        "reference ES only {} bytes",
        want.len()
    );
    assert!(is_program_stream(&ps));
    // MPEG-1 pack header: '0010' after the start code.
    assert_eq!(ps[4] & 0xF0, 0x20);
    let (d, es) = demux_file(&ps);
    assert_eq!(d.stream_id, Some(0xE0));
    assert!(d.video_packets > 0);
    assert_eq!(es.len(), want.len());
    assert!(es == want, "demuxed ES differs from ffmpeg's");

    let ours = tmp.path("ours.mpg");
    let back = tmp.path("back.m1v");
    let m = mux(
        &want,
        &PsOptions {
            mpeg1: true,
            ..PsOptions::default()
        },
    );
    std::fs::write(&ours, &m.data).unwrap();
    run(
        &ffmpeg,
        &[
            "-i",
            path_str(&ours),
            "-map",
            "0:v",
            "-c",
            "copy",
            "-f",
            "mpeg1video",
            path_str(&back),
        ],
    );
    assert!(
        std::fs::read(&back).unwrap() == want,
        "ffmpeg reads the writer's stream differently"
    );
}
