//! Header parsing for the Blu-ray video codecs decoded by Windows (H.264,
//! HEVC, VC-1): only what is needed to check an access unit before handing it
//! to a decoder (picture size, chroma format, bit depth, field coding) and to
//! find the parameter sets. Nothing here decodes pictures.
//!
//! Everything is bounds-checked: the bytes come from untrusted disc images.

/// Largest picture accepted, in luma samples: the documented maximum of the
/// Windows H.264 and HEVC decoders.
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2304;
/// Largest parameter set parsed (real ones are tens of bytes), and largest
/// VC-1 sequence + entry point header.
const MAX_PARAMETER_SET: usize = 4096;
/// NAL units looked at before the first slice of an access unit. Blu-ray ones
/// have a delimiter, parameter sets and a few SEI messages: about ten.
const MAX_HEADER_NALS: usize = 64;
/// NAL units in one access unit. Blu-ray pictures have at most a few dozen
/// slices.
const MAX_NALS: usize = 1024;
/// Largest VC-1 sequence and entry point headers together (real ones are
/// tens of bytes; leaky bucket parameters add at most a few hundred).
const MAX_VC1_HEADERS: usize = 512;

/// Iterates over the NAL units of an Annex-B byte stream: each item is the
/// unit's bytes without its start code (trailing zero bytes of a following
/// four-byte start code removed).
pub fn nal_units(es: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = find_start_code(es, 0);
    std::iter::from_fn(move || {
        let start = pos? + 3;
        let next = find_start_code(es, start);
        let mut end = next.unwrap_or(es.len());
        while end > start && es[end - 1] == 0 {
            end -= 1;
        }
        pos = next;
        Some(&es[start..end])
    })
}

/// Offset of the next `00 00 01` at or after `from`.
pub fn find_start_code(es: &[u8], from: usize) -> Option<usize> {
    es.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|p| from + p)
}

/// Removes emulation prevention bytes (`00 00 03` → `00 00`) from the first
/// `MAX_PARAMETER_SET` bytes of a NAL unit.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let nal = &nal[..nal.len().min(MAX_PARAMETER_SET)];
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb codes; every read is bounds-checked.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos = self.pos.checked_add(n)?;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    /// Unsigned Exp-Golomb (up to 2^32 - 2; more than 31 leading zeros fail).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        let magnitude = i32::try_from(k.div_ceil(2)).ok()?;
        Some(if k % 2 == 1 { magnitude } else { -magnitude })
    }
}

/// What a decoder needs to know before it is given an access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureFormat {
    /// Displayed size after cropping, in luma samples.
    pub width: u32,
    pub height: u32,
    /// Luma bit depth.
    pub bit_depth: u8,
    /// HEVC Main 10 (`general_profile_idc` 2) or any 10-bit stream.
    pub main10: bool,
}

impl PictureFormat {
    fn within_limits(self) -> Option<Self> {
        (self.width >= 16
            && self.height >= 16
            && self.width <= MAX_WIDTH
            && self.height <= MAX_HEIGHT)
            .then_some(self)
    }
}

// ----------------------------------------------------------------------------
// H.264
// ----------------------------------------------------------------------------

const H264_SLICE: u8 = 1;
const H264_IDR: u8 = 5;
const H264_SPS: u8 = 7;
const H264_PPS: u8 = 8;
const H264_AUD: u8 = 9;
const H264_SUBSET_SPS: u8 = 15;
const H264_SLICE_EXTENSION: u8 = 20;

#[derive(Debug, Clone, Copy)]
struct H264Sps {
    id: u32,
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma_format: u32,
}

fn skip_scaling_list(b: &mut Bits, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn parse_h264_sps(nal: &[u8]) -> Option<H264Sps> {
    let rbsp = unescape(nal.get(1..)?);
    let mut b = Bits::new(&rbsp);
    let profile = b.bits(8)?;
    b.skip(16)?; // constraint flags, level
    let id = b.ue()?;
    if id > 31 {
        return None;
    }
    let mut chroma_format = 1;
    let mut separate_colour_plane = false;
    let mut bit_depth = 8u32;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = b.ue()?;
        if chroma_format > 3 {
            return None;
        }
        if chroma_format == 3 {
            separate_colour_plane = b.flag()?;
        }
        bit_depth = b.ue()?.checked_add(8)?;
        if b.ue()?.checked_add(8)? != bit_depth {
            return None; // chroma and luma depths differ
        }
        b.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if b.flag()? {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = b.ue()?.checked_add(4)?;
    if log2_max_frame_num > 16 {
        return None;
    }
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            let cycle = b.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = b.ue()?.checked_add(1)?;
    let height_units = b.ue()?.checked_add(1)?;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        b.skip(1)?; // mb_adaptive_frame_field_flag
    }
    b.skip(1)?; // direct_8x8_inference_flag
    let frame_factor = if frame_mbs_only { 1 } else { 2 };
    let mut width = width_mbs.checked_mul(16)?;
    let mut height = height_units.checked_mul(16 * frame_factor)?;
    // The decoder allocates the coded size, not the cropped one.
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return None;
    }
    if b.flag()? {
        let (unit_x, unit_y) = match (chroma_format, separate_colour_plane) {
            (1, false) => (2, 2 * frame_factor),
            (2, false) => (2, frame_factor),
            _ => (1, frame_factor),
        };
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let crop_x = l.checked_add(r)?.checked_mul(unit_x)?;
        let crop_y = t.checked_add(bo)?.checked_mul(unit_y)?;
        width = width.checked_sub(crop_x)?;
        height = height.checked_sub(crop_y)?;
    }
    Some(H264Sps {
        id,
        width,
        height,
        bit_depth: u8::try_from(bit_depth).ok()?,
        chroma_format,
    })
}

/// (pps id, sps id) of a picture parameter set.
fn parse_h264_pps(nal: &[u8]) -> Option<(u32, u32)> {
    let rbsp = unescape(nal.get(1..)?);
    let mut b = Bits::new(&rbsp);
    let pps = b.ue()?;
    let sps = b.ue()?;
    (pps <= 255 && sps <= 31).then_some((pps, sps))
}

/// The SPS an I slice refers to (through its PPS); `None` for other slices.
fn parse_h264_slice(nal: &[u8], sps_for: impl Fn(u32) -> Option<H264Sps>) -> Option<H264Sps> {
    let rbsp = unescape(nal.get(1..)?);
    let mut b = Bits::new(&rbsp);
    b.ue()?; // first_mb_in_slice
    if b.ue()? % 5 != 2 {
        return None; // slice_type: not I
    }
    sps_for(b.ue()?)
}

/// Checks one H.264 access unit (Annex B, starting at its access unit
/// delimiter as on Blu-ray) and returns the picture format, when the unit
/// carries its SPS and PPS and starts an I-picture. Only 8-bit 4:2:0 streams
/// are accepted (what the Windows decoder handles).
pub fn h264_access_unit(au: &[u8]) -> Option<PictureFormat> {
    let mut units = nal_units(au);
    // primary_pic_type 0: only I slices.
    let aud = units.next()?;
    if *aud.first()? & 0x1F != H264_AUD || aud.get(1)? >> 5 != 0 {
        return None;
    }
    let mut sps_list: Vec<H264Sps> = Vec::new();
    let mut pps_list: Vec<(u32, u32)> = Vec::new();
    let mut format = None;
    // The whole unit goes to the decoder, so every parameter set in it is
    // checked, also those after the first slice.
    for (i, nal) in units.enumerate() {
        if i >= MAX_NALS || (format.is_none() && i >= MAX_HEADER_NALS) {
            return None;
        }
        let Some(&header) = nal.first() else {
            continue;
        };
        if header & 0x80 != 0 {
            return None; // forbidden_zero_bit
        }
        match header & 0x1F {
            // Subset SPS and slice extensions: the dependent view of a
            // stereoscopic stream, which has a stream of its own on Blu-ray.
            H264_SUBSET_SPS | H264_SLICE_EXTENSION => return None,
            H264_SPS => {
                let sps = parse_h264_sps(nal)?;
                if sps.chroma_format != 1 || sps.bit_depth != 8 {
                    return None;
                }
                sps_list.retain(|s| s.id != sps.id);
                sps_list.push(sps);
            }
            H264_PPS => {
                let pps = parse_h264_pps(nal)?;
                pps_list.retain(|p| p.0 != pps.0);
                pps_list.push(pps);
            }
            H264_SLICE | H264_IDR if format.is_none() => {
                let lookup = |pps_id: u32| {
                    let sps_id = pps_list.iter().find(|p| p.0 == pps_id)?.1;
                    sps_list.iter().find(|s| s.id == sps_id).copied()
                };
                let sps = parse_h264_slice(nal, lookup)?;
                format = Some(
                    PictureFormat {
                        width: sps.width,
                        height: sps.height,
                        bit_depth: 8,
                        main10: false,
                    }
                    .within_limits()?,
                );
            }
            // Later access units of the packet (an interlaced frame's
            // second field) need nothing more than their parameter sets
            // checked.
            _ => {}
        }
    }
    format
}

// ----------------------------------------------------------------------------
// HEVC
// ----------------------------------------------------------------------------

const HEVC_BLA_W_LP: u8 = 16;
const HEVC_CRA: u8 = 21;
const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;
const HEVC_AUD: u8 = 35;

fn hevc_type(nal: &[u8]) -> Option<u8> {
    Some((*nal.first()? >> 1) & 0x3F)
}

/// The id and picture format of an HEVC sequence parameter set.
fn parse_hevc_sps(nal: &[u8]) -> Option<(u32, PictureFormat)> {
    let rbsp = unescape(nal.get(2..)?);
    let mut b = Bits::new(&rbsp);
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)?;
    if max_sub_layers_minus1 > 6 {
        return None;
    }
    b.skip(1)?; // temporal_id_nesting
                // profile_tier_level: general profile space 2, tier 1, profile_idc 5.
    b.skip(3)?;
    let profile_idc = b.bits(5)?;
    b.skip(32 + 4 + 43 + 1 + 8)?; // compatibility flags .. general_level_idc
    let mut sub_profile = [false; 7];
    let mut sub_level = [false; 7];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = b.flag()?;
        sub_level[i] = b.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            b.skip(88)?;
        }
        if sub_level[i] {
            b.skip(8)?;
        }
    }
    let sps_id = b.ue()?;
    if sps_id > 15 {
        return None;
    }
    let chroma_format = b.ue()?;
    if chroma_format != 1 {
        return None;
    }
    let mut width = b.ue()?;
    let mut height = b.ue()?;
    // The decoder allocates the coded size, not the cropped one.
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return None;
    }
    if b.flag()? {
        // Conformance window, in units of 2 for 4:2:0.
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        width = width.checked_sub(l.checked_add(r)?.checked_mul(2)?)?;
        height = height.checked_sub(t.checked_add(bo)?.checked_mul(2)?)?;
    }
    let bit_depth = b.ue()?.checked_add(8)?;
    let chroma_depth = b.ue()?.checked_add(8)?;
    if !(8..=10).contains(&bit_depth) || !(8..=10).contains(&chroma_depth) {
        return None;
    }
    PictureFormat {
        width,
        height,
        bit_depth: bit_depth as u8,
        main10: profile_idc == 2 || bit_depth > 8 || chroma_depth > 8,
    }
    .within_limits()
    .map(|f| (sps_id, f))
}

/// (pps id, sps id) of an HEVC picture parameter set.
fn parse_hevc_pps(nal: &[u8]) -> Option<(u32, u32)> {
    let rbsp = unescape(nal.get(2..)?);
    let mut b = Bits::new(&rbsp);
    let pps = b.ue()?;
    let sps = b.ue()?;
    (pps <= 63 && sps <= 15).then_some((pps, sps))
}

/// The picture parameter set an HEVC slice of NAL type `kind` refers to.
fn hevc_slice_pps(nal: &[u8], kind: u8) -> Option<u32> {
    let rbsp = unescape(nal.get(2..)?);
    let mut b = Bits::new(&rbsp);
    b.skip(1)?; // first_slice_segment_in_pic_flag
    if (HEVC_BLA_W_LP..=23).contains(&kind) {
        b.skip(1)?; // no_output_of_prior_pics_flag
    }
    b.ue()
}

/// Checks one HEVC access unit (starting at its access unit delimiter, with
/// VPS, SPS and PPS inline as on Blu-ray) and returns the picture format when
/// it starts an intra picture (pic_type 0). 4:2:0 at 8 or 10 bits only.
pub fn hevc_access_unit(au: &[u8]) -> Option<PictureFormat> {
    let mut units = nal_units(au);
    let aud = units.next()?;
    if hevc_type(aud)? != HEVC_AUD || aud.get(2)? >> 5 != 0 {
        return None;
    }
    let mut vps = false;
    let mut sps_list: Vec<(u32, PictureFormat)> = Vec::new();
    let mut pps_list: Vec<(u32, u32)> = Vec::new();
    let mut format = None;
    // As for H.264, every parameter set in the unit is checked.
    for (i, nal) in units.enumerate() {
        if i >= MAX_NALS || (format.is_none() && i >= MAX_HEADER_NALS) {
            return None;
        }
        let Some(kind) = hevc_type(nal) else {
            continue;
        };
        // forbidden_zero_bit, and layers other than the base layer.
        if nal[0] & 0x80 != 0 || nal[0] & 1 != 0 || nal.get(1)? >> 3 != 0 {
            return None;
        }
        match kind {
            HEVC_VPS => vps = true,
            HEVC_SPS => {
                let sps = parse_hevc_sps(nal)?;
                sps_list.retain(|s| s.0 != sps.0);
                sps_list.push(sps);
            }
            HEVC_PPS => {
                let pps = parse_hevc_pps(nal)?;
                pps_list.retain(|p| p.0 != pps.0);
                pps_list.push(pps);
            }
            // The first slice, after the parameter sets: it must belong to a
            // random access point (BLA, IDR or CRA picture).
            0..=31 if format.is_none() => {
                if !(HEVC_BLA_W_LP..=HEVC_CRA).contains(&kind) {
                    return None;
                }
                // The format of the SPS this slice uses, through its PPS.
                let pps_id = hevc_slice_pps(nal, kind)?;
                let sps_id = pps_list.iter().find(|p| p.0 == pps_id)?.1;
                format = Some(sps_list.iter().find(|s| s.0 == sps_id)?.1);
            }
            _ => {}
        }
    }
    format.filter(|_| vps)
}

// ----------------------------------------------------------------------------
// VC-1 (SMPTE 421M advanced profile, as on Blu-ray)
// ----------------------------------------------------------------------------

const VC1_SEQUENCE_HEADER: u8 = 0x0F;
const VC1_ENTRY_POINT: u8 = 0x0E;
const VC1_FRAME: u8 = 0x0D;

/// A VC-1 access unit checked for decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vc1Unit {
    pub format: PictureFormat,
    /// The decoder's codec private data: one leading zero byte, then the
    /// sequence header and entry point header with their start codes.
    pub private_data: Vec<u8>,
}

/// Checks one VC-1 access unit that starts with a sequence header and an
/// entry point header followed by an I frame (an entry point on Blu-ray).
pub fn vc1_access_unit(au: &[u8]) -> Option<Vc1Unit> {
    let seq = find_start_code(au, 0)?;
    if *au.get(seq + 3)? != VC1_SEQUENCE_HEADER {
        return None;
    }
    let rbsp = unescape(au.get(seq + 4..)?);
    let mut b = Bits::new(&rbsp);
    if b.bits(2)? != 3 {
        return None; // advanced profile only
    }
    b.skip(3)?; // level
    if b.bits(2)? != 1 {
        return None; // colordiff_format: 4:2:0
    }
    b.skip(3 + 5 + 1)?; // frmrtq, bitrtq, postprocflag
    let width = b.bits(12)?.checked_add(1)?.checked_mul(2)?;
    let height = b.bits(12)?.checked_add(1)?.checked_mul(2)?;
    b.skip(1)?; // pulldown
    let interlace = b.flag()?;
    b.skip(4)?; // tfcntrflag, finterpflag, reserved, psf
    if b.flag()? {
        // Display extension: display size, aspect ratio, frame rate, colour.
        b.skip(28)?;
        if b.flag()? && b.bits(4)? == 15 {
            b.skip(16)?;
        }
        if b.flag()? {
            let exponent = b.flag()?;
            b.skip(if exponent { 16 } else { 12 })?;
        }
        if b.flag()? {
            b.skip(24)?;
        }
    }
    let leaky_buckets = if b.flag()? {
        let n = b.bits(5)? as usize;
        b.skip(8 + 32 * n)?; // rate and buffer exponents, per bucket rate and size
        n
    } else {
        0
    };
    let entry = find_start_code(au, seq + 4)?;
    if *au.get(entry + 3)? != VC1_ENTRY_POINT {
        return None;
    }
    let frame = find_start_code(au, entry + 4)?;
    if *au.get(frame + 3)? != VC1_FRAME || frame - seq > MAX_VC1_HEADERS {
        return None;
    }
    // Entry point header: flags and quantizer (13 bits), the buckets'
    // fullness, then an optional coded size that must fit the sequence's.
    let entry_rbsp = unescape(au.get(entry + 4..frame)?);
    let mut e = Bits::new(&entry_rbsp);
    e.skip(13 + 8 * leaky_buckets)?;
    if e.flag()? {
        let coded_width = e.bits(12)?.checked_add(1)?.checked_mul(2)?;
        let coded_height = e.bits(12)?.checked_add(1)?.checked_mul(2)?;
        if coded_width > width || coded_height > height {
            return None;
        }
    }
    // Picture header: FCM (interlaced streams only), then the picture type.
    let header = unescape(au.get(frame + 4..)?);
    let mut p = Bits::new(&header);
    let field_pair = interlace && p.flag()? && p.flag()?; // FCM '11'
    let intra = if field_pair {
        p.bits(3)? <= 1 // FPTYPE: I/I or I/P
    } else {
        p.flag()? && p.flag()? && !p.flag()? // PTYPE '110': I
    };
    if !intra {
        return None;
    }
    let mut private_data = Vec::with_capacity(frame - seq + 1);
    private_data.push(0);
    private_data.extend_from_slice(&au[seq..frame]);
    let format = PictureFormat {
        width,
        height,
        bit_depth: 8,
        main10: false,
    }
    .within_limits()?;
    Some(Vc1Unit {
        format,
        private_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MSB-first bit writer with Exp-Golomb codes (test helper).
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bit: u32,
    }

    impl Writer {
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                if self.bit % 8 == 0 {
                    self.bytes.push(0);
                }
                let last = self.bytes.last_mut().unwrap();
                *last |= (((v >> i) & 1) as u8) << (7 - self.bit % 8);
                self.bit += 1;
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let n = 32 - x.leading_zeros();
            self.put(0, n - 1);
            self.put(x, n);
        }
        fn finish(mut self) -> Vec<u8> {
            self.put(1, 1); // rbsp stop bit
            while self.bit % 8 != 0 {
                self.put(0, 1);
            }
            self.bytes
        }
    }

    fn nal(header: &[u8], body: Vec<u8>) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(header);
        // Emulation prevention, so the parser's unescape is exercised.
        let mut zeros = 0;
        for b in body {
            if zeros >= 2 && b <= 3 {
                v.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            v.push(b);
        }
        v
    }

    /// H.264 High profile 1920x1080 SPS (cropped from 1088), interlaced when
    /// `frame_mbs_only` is false.
    pub(crate) fn h264_sps(frame_mbs_only: bool) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(100, 8);
        w.put(0, 8);
        w.put(41, 8);
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(0); // bit depth luma - 8
        w.ue(0); // chroma
        w.put(0, 1);
        w.put(1, 1); // seq_scaling_matrix_present
        for i in 0..8 {
            w.put(u32::from(i == 0), 1);
            if i == 0 {
                for _ in 0..16 {
                    w.ue(0); // delta 0 (se 0)
                }
            }
        }
        w.ue(0); // log2_max_frame_num - 4
        w.ue(0); // poc type 0
        w.ue(2);
        w.ue(4); // max refs
        w.put(0, 1);
        w.ue(119); // 120 MBs wide
        if frame_mbs_only {
            w.ue(67); // 68 MB rows
            w.put(1, 1);
        } else {
            w.ue(33); // 34 map units (field pairs)
            w.put(0, 1);
            w.put(1, 1); // mbaff
        }
        w.put(1, 1); // direct_8x8
        w.put(1, 1); // cropping
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(if frame_mbs_only { 4 } else { 2 });
        w.put(0, 1); // vui
        nal(&[0x67], w.finish())
    }

    fn h264_pps() -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(0);
        w.ue(0);
        w.put(0, 6);
        nal(&[0x68], w.finish())
    }

    fn h264_slice(idr: bool, field: bool) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(0); // first mb
        w.ue(7); // I
        w.ue(0); // pps
        w.put(0, 4); // frame_num
        if field {
            w.put(1, 1);
        } else {
            w.put(0, 1);
        }
        w.put(0xAB, 8);
        nal(&[if idr { 0x65 } else { 0x41 }], w.finish())
    }

    fn h264_au(frame_mbs_only: bool, field: bool) -> Vec<u8> {
        let mut au = nal(&[0x09], vec![0x10]);
        au.extend(h264_sps(frame_mbs_only));
        au.extend(h264_pps());
        au.extend(nal(&[0x06], vec![5, 1, 0x80]));
        au.extend(h264_slice(false, field));
        au
    }

    #[test]
    fn h264_progressive_access_unit() {
        let f = h264_access_unit(&h264_au(true, false)).unwrap();
        assert_eq!((f.width, f.height, f.bit_depth), (1920, 1080, 8));
    }

    #[test]
    fn h264_interlaced_access_units_give_the_frame_size() {
        // Field pictures and MBAFF frames alike: the size of the whole frame.
        for field in [true, false] {
            let f = h264_access_unit(&h264_au(false, field)).unwrap();
            assert_eq!((f.width, f.height), (1920, 1080));
        }
    }

    #[test]
    fn h264_needs_an_intra_aud_and_parameter_sets() {
        let mut au = h264_au(true, false);
        // primary_pic_type 1 (I and P)
        au[5] = 0x30;
        assert!(h264_access_unit(&au).is_none());
        let mut no_sps = nal(&[0x09], vec![0x10]);
        no_sps.extend(h264_pps());
        no_sps.extend(h264_slice(true, false));
        assert!(h264_access_unit(&no_sps).is_none());
    }

    fn hevc_sps(width: u32, height: u32, bit_depth: u32) -> Vec<u8> {
        hevc_sps_cropped(width, height, bit_depth, None)
    }

    fn hevc_sps_cropped(
        width: u32,
        height: u32,
        bit_depth: u32,
        crop: Option<(u32, u32)>,
    ) -> Vec<u8> {
        hevc_sps_full(0, width, height, bit_depth, crop)
    }

    /// An HEVC SPS `id`, with a conformance window cutting (right, bottom)
    /// samples.
    fn hevc_sps_full(
        id: u32,
        width: u32,
        height: u32,
        bit_depth: u32,
        crop: Option<(u32, u32)>,
    ) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(0, 4);
        w.put(0, 3); // one sub-layer
        w.put(1, 1);
        w.put(0, 3);
        w.put(if bit_depth > 8 { 2 } else { 1 }, 5);
        w.put(0x6000_0000, 32);
        w.put(0b1001, 4);
        w.put(0, 32);
        w.put(0, 11);
        w.put(0, 1);
        w.put(153, 8);
        w.ue(id);
        w.ue(1); // 4:2:0
        w.ue(width);
        w.ue(height);
        w.put(u32::from(crop.is_some()), 1);
        if let Some((right, bottom)) = crop {
            w.ue(0);
            w.ue(right / 2);
            w.ue(0);
            w.ue(bottom / 2);
        }
        w.ue(bit_depth - 8);
        w.ue(bit_depth - 8);
        nal(&[0x42, 0x01], w.finish())
    }

    fn hevc_au(width: u32, height: u32, bit_depth: u32) -> Vec<u8> {
        let mut au = nal(&[0x46, 0x01], vec![0x10]);
        au.extend(nal(&[0x40, 0x01], vec![0x0C, 0x01, 0xFF]));
        au.extend(hevc_sps(width, height, bit_depth));
        au.extend(nal(&[0x44, 0x01], vec![0xC1, 0x72]));
        au.extend(nal(&[0x26, 0x01], vec![0xAF, 0x00, 0x11]));
        au
    }

    #[test]
    fn hevc_access_units() {
        let f = hevc_access_unit(&hevc_au(3840, 2160, 10)).unwrap();
        assert_eq!((f.width, f.height, f.bit_depth), (3840, 2160, 10));
        assert!(f.main10);
        let f = hevc_access_unit(&hevc_au(1920, 1080, 8)).unwrap();
        assert_eq!((f.width, f.height), (1920, 1080));
        assert!(!f.main10);
        assert!(hevc_access_unit(&hevc_au(8192, 4320, 10)).is_none());
    }

    /// A VC-1 advanced profile 1920x1080 access unit whose frame header
    /// starts with `picture` (FCM and picture type bits).
    fn vc1_au(interlace: bool, picture: u8) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(3, 2);
        w.put(3, 3);
        w.put(1, 2);
        w.put(0, 9);
        w.put(959, 12);
        w.put(539, 12);
        w.put(0, 1); // pulldown
        w.put(u32::from(interlace), 1);
        w.put(0, 14);
        let mut au = vec![0, 0, 1, 0x0F];
        au.extend(w.finish());
        au.extend([0, 0, 1, 0x0E, 0x4A, 0x10]);
        au.extend([0, 0, 1, 0x0D, picture, 0x22]);
        au
    }

    #[test]
    fn vc1_access_unit_and_private_data() {
        let unit = vc1_access_unit(&vc1_au(false, 0b1100_0000)).unwrap();
        assert_eq!((unit.format.width, unit.format.height), (1920, 1080));
        assert_eq!(unit.private_data[0], 0);
        assert_eq!(&unit.private_data[1..5], &[0, 0, 1, 0x0F]);
        assert!(unit.private_data.ends_with(&[0, 0, 1, 0x0E, 0x4A, 0x10]));
    }

    #[test]
    fn vc1_needs_an_intra_frame() {
        // Progressive: PTYPE '110' is I; P ('0'), B ('10'), BI ('1110') are not.
        for (picture, ok) in [
            (0b1100_0000, true),
            (0, false),
            (0b1000_0000, false),
            (0b1110_0000, false),
        ] {
            assert_eq!(
                vc1_access_unit(&vc1_au(false, picture)).is_some(),
                ok,
                "{picture:#b}"
            );
        }
        // Interlaced: FCM '0' progressive, '10' frame, '11' field pair with
        // FPTYPE I/I (000) or I/P (001); P/I (010) is not.
        for (picture, ok) in [
            (0b0110_0000, true),
            (0b1011_0000, true),
            (0b1010_0000, false),
            (0b1100_0000, true),
            (0b1100_1000, true),
            (0b1101_0000, false),
        ] {
            assert_eq!(
                vc1_access_unit(&vc1_au(true, picture)).is_some(),
                ok,
                "{picture:#b}"
            );
        }
    }

    /// An H.264 High profile SPS of `mbs` x `mbs` macroblocks cropped to
    /// 1920x1080, with the given chroma format and bit depths.
    fn h264_sps_coded(mbs: u32, chroma_format: u32, luma_depth: u32, chroma_depth: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(if chroma_format == 3 { 244 } else { 100 }, 8);
        w.put(0, 8);
        w.put(51, 8);
        w.ue(0);
        w.ue(chroma_format);
        if chroma_format == 3 {
            w.put(0, 1); // separate_colour_plane_flag
        }
        w.ue(luma_depth - 8);
        w.ue(chroma_depth - 8);
        w.put(0, 1);
        w.put(0, 1); // no scaling matrices
        w.ue(0);
        w.ue(2); // poc type 2
        w.ue(1);
        w.put(0, 1);
        w.ue(mbs - 1);
        w.ue(mbs - 1);
        w.put(1, 1); // frame_mbs_only
        w.put(1, 1);
        w.put(1, 1); // cropping, in units of 2 (4:2:0) or 1 (4:4:4)
        let unit = if chroma_format == 3 { 1 } else { 2 };
        w.ue(0);
        w.ue((mbs * 16 - 1920) / unit);
        w.ue(0);
        w.ue((mbs * 16 - 1080) / unit);
        w.put(0, 1);
        nal(&[0x67], w.finish())
    }

    fn h264_au_with(sps: Vec<u8>) -> Vec<u8> {
        let mut au = nal(&[0x09], vec![0x10]);
        au.extend(sps);
        au.extend(h264_pps());
        au.extend(h264_slice(true, false));
        au
    }

    #[test]
    fn the_coded_size_is_limited_not_just_the_cropped_one() {
        // 144x144 macroblocks (2304x2304, within the limits) cropped.
        let ok = h264_access_unit(&h264_au_with(h264_sps_coded(144, 1, 8, 8)));
        assert_eq!(ok.map(|f| (f.width, f.height)), Some((1920, 1080)));
        // 16384x16384 coded, cropped to 1920x1080.
        assert!(h264_access_unit(&h264_au_with(h264_sps_coded(1024, 1, 8, 8))).is_none());
        // Chroma deeper than luma.
        assert!(h264_access_unit(&h264_au_with(h264_sps_coded(144, 1, 8, 14))).is_none());
        // HEVC: 8192x8192 coded, cropped to 3840x2160.
        let au = hevc_au(3840, 2160, 10);
        let sps_at = au
            .windows(5)
            .position(|x| x == [0, 0, 1, 0x42, 0x01])
            .unwrap()
            - 1;
        let pps_at = au
            .windows(5)
            .position(|x| x == [0, 0, 1, 0x44, 0x01])
            .unwrap()
            - 1;
        let cropped = hevc_sps_cropped(3840 + 64, 2160 + 16, 10, Some((64, 16)));
        let huge = hevc_sps_cropped(8192, 8192, 10, Some((8192 - 3840, 8192 - 2160)));
        let with = |sps: &[u8]| [&au[..sps_at], sps, &au[pps_at..]].concat();
        let f = hevc_access_unit(&with(&cropped)).unwrap();
        assert_eq!((f.width, f.height), (3840, 2160));
        assert!(hevc_access_unit(&with(&huge)).is_none());
    }

    #[test]
    fn hevc_format_comes_from_the_sps_the_slice_uses() {
        // SPS 0 (2160p, 10-bit) is the one the PPS and slice use; an unused
        // SPS 1 (1080p, 8-bit) follows it.
        let base = hevc_au(3840, 2160, 10);
        let pps_at = base
            .windows(5)
            .position(|x| x == [0, 0, 1, 0x44, 0x01])
            .unwrap()
            - 1;
        let other = hevc_sps_full(1, 1920, 1080, 8, None);
        let au = [&base[..pps_at], &other[..], &base[pps_at..]].concat();
        let f = hevc_access_unit(&au).unwrap();
        assert_eq!((f.width, f.height, f.main10), (3840, 2160, true));
        // A slice whose PPS is not in the unit.
        let no_pps = [&base[..pps_at], &base[pps_at + 8..]].concat();
        assert!(hevc_access_unit(&no_pps).is_none());
    }

    #[test]
    fn h264_mvc_units_are_refused() {
        for kind in [0x6F, 0x74] {
            // Subset SPS (15), slice extension (20).
            let mut au = h264_au(true, false);
            au.extend(nal(&[kind], vec![0x42, 0x80]));
            assert!(h264_access_unit(&au).is_none(), "{kind:#x}");
        }
    }

    #[test]
    fn every_parameter_set_of_the_unit_is_checked() {
        // A valid picture, then a 4:4:4 14-bit SPS and a second picture
        // using it: the decoder would see the second SPS too.
        let mut au = h264_au(true, false);
        au.extend(h264_sps_coded(120, 3, 14, 14));
        au.extend(h264_slice(true, false));
        assert!(h264_access_unit(&au).is_none());
        // Both fields of an interlaced frame in one packet, as on real
        // discs: fine, but not with a bad SPS before the second one.
        let mut two = h264_au(false, true);
        two.extend(nal(&[0x09], vec![0x10]));
        two.extend(h264_slice(false, true));
        assert!(h264_access_unit(&two).is_some());
        let mut two_bad = h264_au(false, true);
        two_bad.extend(nal(&[0x09], vec![0x10]));
        two_bad.extend(h264_sps_coded(1024, 1, 8, 8));
        two_bad.extend(h264_slice(false, true));
        assert!(h264_access_unit(&two_bad).is_none());
        // A NAL unit with the forbidden bit set.
        let mut forbidden = h264_au(true, false);
        forbidden.extend(nal(&[0x85], vec![0x88, 0x80]));
        assert!(h264_access_unit(&forbidden).is_none());
        // HEVC: a 4:4:4 SPS after the first slice, and a second unit.
        let mut au = hevc_au(1920, 1080, 8);
        au.extend(hevc_sps(8192, 4320, 10));
        assert!(hevc_access_unit(&au).is_none());
        let mut two = hevc_au(1920, 1080, 8);
        two.extend(hevc_au(8192, 4320, 10));
        assert!(hevc_access_unit(&two).is_none());
        // An enhancement layer NAL unit (nuh_layer_id 1).
        let mut layered = hevc_au(1920, 1080, 8);
        layered.extend(nal(&[0x26, 0x09], vec![0xAF, 0x00, 0x11]));
        assert!(hevc_access_unit(&layered).is_none());
    }

    #[test]
    fn vc1_entry_point_coded_size_must_fit_the_sequence() {
        // The Windows encoder's headers: HRD parameters with leaky buckets.
        let fixture = include_bytes!("../tests/data/vc1_1080p.bin");
        let unit = vc1_access_unit(fixture).unwrap();
        assert_eq!((unit.format.width, unit.format.height), (1920, 1080));
        // Entry point with a coded size (no HRD in `vc1_au`): 13 flag bits,
        // CODED_SIZE_FLAG, then width and height.
        let with_coded = |w: u32, h: u32| {
            let mut au = vc1_au(false, 0b1100_0000);
            let at = au.windows(4).position(|x| x == [0, 0, 1, 0x0E]).unwrap() + 4;
            let end = au.windows(4).position(|x| x == [0, 0, 1, 0x0D]).unwrap();
            let mut e = Writer::default();
            e.put(0, 13);
            e.put(1, 1);
            e.put(w / 2 - 1, 12);
            e.put(h / 2 - 1, 12);
            e.put(0, 6);
            au.splice(at..end, e.finish());
            au
        };
        assert!(vc1_access_unit(&with_coded(1440, 1080)).is_some());
        assert!(vc1_access_unit(&with_coded(1920, 1080)).is_some());
        assert!(vc1_access_unit(&with_coded(8192, 8192)).is_none());
        // Headers padded far beyond any real ones.
        let mut padded = vc1_au(false, 0b1100_0000);
        let at = padded
            .windows(4)
            .position(|x| x == [0, 0, 1, 0x0D])
            .unwrap();
        padded.splice(at..at, std::iter::repeat_n(0x55, MAX_VC1_HEADERS));
        assert!(vc1_access_unit(&padded).is_none());
    }

    #[test]
    fn h264_and_hevc_need_a_key_picture() {
        // An H.264 P slice behind an AUD that claims an I-picture.
        let mut au = nal(&[0x09], vec![0x10]);
        au.extend(h264_sps(true));
        au.extend(h264_pps());
        let mut w = Writer::default();
        w.ue(0);
        w.ue(5); // P
        w.ue(0);
        w.put(0, 4);
        w.put(0xAB, 8);
        au.extend(nal(&[0x41], w.finish()));
        assert!(h264_access_unit(&au).is_none());
        // HEVC: a trailing (non-IRAP) picture, and parameter sets alone.
        let base = hevc_au(1920, 1080, 8);
        let mut trail = base.clone();
        let slice = trail
            .windows(4)
            .rposition(|x| x == [0, 0, 1, 0x26])
            .unwrap();
        trail[slice + 3] = 0x02; // TRAIL_R
        assert!(hevc_access_unit(&trail).is_none());
        assert!(hevc_access_unit(&base[..slice]).is_none());
        // Parameter sets behind more SEI messages than any real unit has.
        let mut padded = base[..slice].to_vec();
        for _ in 0..MAX_HEADER_NALS {
            padded.extend(nal(&[0x4E, 0x01], vec![5, 1, 0x80]));
        }
        padded.extend_from_slice(&base[slice..]);
        assert!(hevc_access_unit(&padded).is_none());
        assert!(hevc_access_unit(&base).is_some());
    }

    #[test]
    fn hostile_units_never_panic() {
        let bases = [
            h264_au(false, true),
            hevc_au(3840, 2160, 10),
            h264_au(true, false),
        ];
        let mut seed: u64 = 0x1234_5678_9ABC_DEF1;
        for base in &bases {
            for _ in 0..3000 {
                let mut au = base.clone();
                for _ in 0..1 + (seed % 5) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let i = (seed as usize) % au.len();
                    au[i] = (seed >> 40) as u8;
                }
                let _ = h264_access_unit(&au);
                let _ = hevc_access_unit(&au);
                let _ = vc1_access_unit(&au);
                let cut = (seed as usize >> 4) % au.len();
                let _ = h264_access_unit(&au[..cut]);
            }
        }
    }
}
