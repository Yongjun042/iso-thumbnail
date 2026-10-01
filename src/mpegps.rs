//! MPEG program stream demultiplexer (ISO/IEC 13818-1 and 11172-1), video only.
//!
//! DVD `.VOB` files are MPEG-2 program streams made of 2048-byte packs; MPEG-1
//! program streams are accepted too. Only the payload of the first video
//! stream (`stream_id` 0xE0–0xEF) is extracted; audio, sub-pictures,
//! navigation packets and padding are skipped. Scrambled PES packets (DVD
//! CSS, `PES_scrambling_control` ≠ 0) are counted and never decoded: this
//! module does not attempt any decryption.
//!
//! CONTRACT (fixed; other modules are written against it): `Demuxer`, its
//! public fields, `Demuxer::new`, `Demuxer::push` and `is_program_stream` keep
//! these signatures and meanings.

/// Incremental video demultiplexer. Feed it consecutive chunks of a program
/// stream with `push`.
#[derive(Debug, Default, Clone)]
pub struct Demuxer {
    /// Video stream being followed: the first `stream_id` in 0xE0..=0xEF seen.
    pub stream_id: Option<u8>,
    /// Video PES packets of that stream whose payload was appended.
    pub video_packets: u32,
    /// Video PES packets of that stream skipped because they are scrambled.
    pub scrambled_packets: u32,
}

impl Demuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Demultiplexes as many complete packs and packets at the start of `ps`
    /// as possible and appends the payload of unscrambled video PES packets to
    /// `es`.
    ///
    /// Returns how many bytes of `ps` were consumed. A packet cut off at the
    /// end of `ps` is not consumed: the caller keeps `ps[consumed..]`, appends
    /// the next chunk of the stream and calls `push` again. Bytes that do not
    /// start a valid pack or packet are skipped until the next start code
    /// (resynchronisation), so garbage never stalls the demuxer.
    pub fn push(&mut self, ps: &[u8], es: &mut Vec<u8>) -> usize {
        let _ = es;
        ps.len()
    }
}

/// Whether `data` starts with an MPEG program stream pack header (`00 00 01 BA`).
pub fn is_program_stream(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1, 0xBA])
}
