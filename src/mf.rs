//! Decodes one Blu-ray key frame (H.264, HEVC or VC-1) with the video
//! decoders registered with Media Foundation (those that come with Windows,
//! and for HEVC the Store's HEVC Video Extensions), the way Explorer's own
//! video thumbnails are made.
//!
//! The decoders run synchronously on the calling thread, in software only (no
//! Direct3D device is ever handed to them), asked for one worker thread and
//! their thumbnail mode. They are hardened for untrusted media, but not memory
//! safe; so the access unit is checked first (`crate::nal`: a key picture, and
//! every sequence parameter set in the unit within the decoders' documented
//! 4096x2304, 4:2:0 and 8 or 10 bits), the input is bounded, every buffer
//! size the decoder reports is checked before memory is allocated or read,
//! and the processing loop has a fixed number of steps.
//!
//! One rule matters for crash safety: streaming is only started once an output
//! type was set. The HEVC decoder crashes the process when it is started
//! without one (it needs the frame size on its input type for that).
//!
//! COM must be initialised on the calling thread (the shell does that for a
//! thumbnail handler; the CLI and tests do it themselves).

use core::mem::ManuallyDrop;
use core::ptr::null_mut;

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVDecNumWorkerThreads,
    CODECAPI_AVDecVideoThumbnailGenerationMode, IMF2DBuffer2, IMFActivate, IMFMediaType, IMFSample,
    IMFTransform, MF2DBuffer_LockFlags_Read, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFTEnumEx, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoFormat_P010, MFVideoFormat_WVC1, MFVideoPrimaries_BT2020, MFVideoTransFunc_2084,
    MFVideoTransFunc_HLG, MFVideoTransferMatrix_BT2020_10, MFVideoTransferMatrix_BT2020_12,
    MFVideoTransferMatrix_BT601, MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_GEOMETRIC_APERTURE,
    MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_USER_DATA, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_VIDEO_PROFILE, MF_MT_YUV_MATRIX, MF_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};

use crate::mpeg2::Frame;
use crate::nal::{MAX_HEIGHT, MAX_WIDTH};
use crate::yuv::{self, Colour, Matrix, Primaries, SemiPlanar, Transfer};

/// The WMV/VC-1 decoder (wmvdecod.dll); not in the windows crate.
const CLSID_WMV_DECODER: GUID = GUID::from_u128(0x82d353df_90bd_4382_8bc2_3f6192b76e34);
/// `eAVEncH265VProfile_Main_420_10`, for Main 10 input.
const HEVC_PROFILE_MAIN10: u32 = 2;
/// `MFNominalRange_0_255`.
const NOMINAL_RANGE_FULL: u32 = 1;

/// Largest access unit handed to a decoder.
pub const MAX_INPUT_BYTES: usize = 8 << 20;
/// Largest output buffer accepted: a 4096x2304 P010 picture.
const MAX_OUTPUT_BYTES: u32 = MAX_WIDTH * MAX_HEIGHT * 3;
/// Most `ProcessOutput` calls for one frame (stream changes and the drain
/// included); the observed sequences need at most four.
const MAX_OUTPUT_CALLS: usize = 16;
/// Most output types looked at when choosing NV12 / P010.
const MAX_OUTPUT_TYPES: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vc1,
}

/// One frame to decode.
pub struct Request<'a> {
    pub codec: Codec,
    /// The key frame's access unit (of an interlaced H.264 frame, both field
    /// pictures; in thumbnail mode the decoder makes a frame of the first).
    pub unit: &'a [u8],
    /// Picture size from the stream's own headers (`crate::nal`).
    pub width: u32,
    pub height: u32,
    /// HEVC Main 10.
    pub main10: bool,
    /// VC-1 codec private data (a zero byte, then the sequence and entry point headers).
    pub private_data: Option<&'a [u8]>,
    /// Colour description to use when the decoder does not report one.
    pub fallback_colour: Colour,
}

/// Keeps Media Foundation started for its lifetime (`MFStartup` / `MFShutdown`
/// are counted per process, so this never shuts down anyone else's use).
pub struct Session(());

impl Session {
    pub fn start() -> Option<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).ok()? };
        Some(Session(()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Whether Windows has a software decoder for `codec` (HEVC comes from the
/// optional "HEVC Video Extensions" package).
pub fn available(_session: &Session, codec: Codec) -> bool {
    create_decoder(codec).is_some()
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Vc1 => MFVideoFormat_WVC1,
    }
}

/// The first synchronous software decoder registered for `input`.
fn enumerate_decoder(input: GUID) -> Option<IMFTransform> {
    unsafe {
        let info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: input,
        };
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        );
        let mut list: *mut Option<IMFActivate> = null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&info),
            None,
            &mut list,
            &mut count,
        )
        .ok()?;
        if list.is_null() {
            return None;
        }
        let mut found = None;
        for i in 0..count as usize {
            // Take ownership of every entry so each is released exactly once.
            if let Some(activate) = (*list.add(i)).take() {
                if found.is_none() {
                    found = activate.ActivateObject::<IMFTransform>().ok();
                }
            }
        }
        CoTaskMemFree(Some(list as *const _));
        found
    }
}

fn create_decoder(codec: Codec) -> Option<IMFTransform> {
    unsafe {
        match codec {
            Codec::H264 => CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                .ok()
                .or_else(|| enumerate_decoder(MFVideoFormat_H264)),
            Codec::Hevc => enumerate_decoder(MFVideoFormat_HEVC),
            Codec::Vc1 => CoCreateInstance(&CLSID_WMV_DECODER, None, CLSCTX_INPROC_SERVER)
                .ok()
                .or_else(|| enumerate_decoder(MFVideoFormat_WVC1)),
        }
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Sets the first NV12 or P010 output type the decoder offers (P010 only for
/// 10-bit input, so its precision reaches the tone mapping).
fn set_output_type(decoder: &IMFTransform, ten_bit: bool) -> Option<()> {
    unsafe {
        let mut nv12 = None;
        let mut p010 = None;
        for i in 0..MAX_OUTPUT_TYPES {
            let Ok(t) = decoder.GetOutputAvailableType(0, i) else {
                break;
            };
            let Ok(sub) = t.GetGUID(&MF_MT_SUBTYPE) else {
                continue;
            };
            if sub == MFVideoFormat_NV12 && nv12.is_none() {
                nv12 = Some(t);
            } else if sub == MFVideoFormat_P010 && p010.is_none() {
                p010 = Some(t);
            }
        }
        let chosen = if ten_bit {
            p010.or(nv12)
        } else {
            nv12.or(p010)
        }?;
        decoder.SetOutputType(0, &chosen, 0).ok()
    }
}

fn make_sample(data: &[u8]) -> Option<IMFSample> {
    unsafe {
        let len = u32::try_from(data.len()).ok()?;
        let buffer = MFCreateMemoryBuffer(len).ok()?;
        let mut p = null_mut();
        buffer.Lock(&mut p, None, None).ok()?;
        if p.is_null() {
            let _ = buffer.Unlock();
            return None;
        }
        core::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buffer.Unlock().ok()?;
        buffer.SetCurrentLength(len).ok()?;
        let sample = MFCreateSample().ok()?;
        sample.AddBuffer(&buffer).ok()?;
        sample.SetSampleTime(0).ok()?;
        sample.SetSampleDuration(400_000).ok()?;
        let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
        Some(sample)
    }
}

enum Pulled {
    Frame(IMFSample),
    NeedMoreInput,
    StreamChange,
    Failed,
}

fn pull(decoder: &IMFTransform) -> Pulled {
    unsafe {
        let Ok(info) = decoder.GetOutputStreamInfo(0) else {
            return Pulled::Failed;
        };
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let sample = if provides {
            None
        } else {
            if info.cbSize == 0 || info.cbSize > MAX_OUTPUT_BYTES {
                return Pulled::Failed;
            }
            let Ok(s) = MFCreateSample() else {
                return Pulled::Failed;
            };
            let Ok(b) = MFCreateMemoryBuffer(info.cbSize) else {
                return Pulled::Failed;
            };
            if s.AddBuffer(&b).is_err() {
                return Pulled::Failed;
            }
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = decoder.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        ManuallyDrop::drop(&mut buffers[0].pEvents);
        match result {
            Ok(()) => sample.map_or(Pulled::Failed, Pulled::Frame),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Pulled::NeedMoreInput,
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Pulled::StreamChange,
            Err(_) => Pulled::Failed,
        }
    }
}

/// Decodes the request into an 8-bit BT.709 frame (`crate::yuv`).
pub fn decode(_session: &Session, request: &Request) -> Option<Frame> {
    if request.unit.is_empty()
        || request.unit.len() > MAX_INPUT_BYTES
        || request.width == 0
        || request.height == 0
        || request.width > MAX_WIDTH
        || request.height > MAX_HEIGHT
    {
        return None;
    }
    let decoder = create_decoder(request.codec)?;
    unsafe {
        if let Ok(attributes) = decoder.GetAttributes() {
            let _ = attributes.SetUINT32(&CODECAPI_AVDecNumWorkerThreads, 1);
            let _ = attributes.SetUINT32(&CODECAPI_AVDecVideoThumbnailGenerationMode, 1);
        }
        let input = MFCreateMediaType().ok()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
        input
            .SetGUID(&MF_MT_SUBTYPE, &subtype(request.codec))
            .ok()?;
        input
            .SetUINT64(&MF_MT_FRAME_SIZE, pack(request.width, request.height))
            .ok()?;
        if request.codec == Codec::Hevc && request.main10 {
            let _ = input.SetUINT32(&MF_MT_VIDEO_PROFILE, HEVC_PROFILE_MAIN10);
        }
        if let Some(private) = request.private_data {
            input.SetBlob(&MF_MT_USER_DATA, private).ok()?;
        }
        decoder.SetInputType(0, &input, 0).ok()?;
        // Without an output type the decoder must not be started (see the
        // module documentation).
        set_output_type(&decoder, request.main10)?;
        decoder
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .ok()?;
        decoder
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .ok()?;
        let mut next_input = Some(make_sample(request.unit)?);
        let mut drained = false;
        for _ in 0..MAX_OUTPUT_CALLS {
            if let Some(sample) = &next_input {
                match decoder.ProcessInput(0, sample, 0) {
                    Ok(()) => next_input = None,
                    // Output is pulled first and the unit offered again on
                    // the next round.
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                    Err(_) => return None,
                }
            }
            match pull(&decoder) {
                Pulled::Frame(sample) => {
                    return read_frame(&decoder, &sample, request.fallback_colour);
                }
                Pulled::StreamChange => set_output_type(&decoder, request.main10)?,
                Pulled::NeedMoreInput if next_input.is_some() => {}
                Pulled::NeedMoreInput => {
                    if drained {
                        return None;
                    }
                    // A single key frame stays in the decoder until the end of
                    // the stream is signalled.
                    let _ = decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                    decoder.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).ok()?;
                    drained = true;
                }
                Pulled::Failed => return None,
            }
        }
        None
    }
}

/// Reads an `MFVideoArea` attribute as (x, y, width, height).
fn aperture(t: &IMFMediaType, key: &GUID) -> Option<(u32, u32, u32, u32)> {
    unsafe {
        let mut area = MFVideoArea::default();
        let bytes = core::slice::from_raw_parts_mut(
            (&mut area as *mut MFVideoArea).cast::<u8>(),
            core::mem::size_of::<MFVideoArea>(),
        );
        t.GetBlob(key, bytes, None).ok()?;
        let x = u32::try_from(area.OffsetX.value).ok()?;
        let y = u32::try_from(area.OffsetY.value).ok()?;
        let w = u32::try_from(area.Area.cx).ok()?;
        let h = u32::try_from(area.Area.cy).ok()?;
        Some((x, y, w, h))
    }
}

/// The colour description of the output type, falling back to `fallback`
/// for what the decoder leaves out.
fn colour_of(t: &IMFMediaType, fallback: Colour) -> Colour {
    unsafe {
        let transfer = match t.GetUINT32(&MF_MT_TRANSFER_FUNCTION) {
            Ok(v) if v == MFVideoTransFunc_2084.0 as u32 => Transfer::Pq,
            Ok(v) if v == MFVideoTransFunc_HLG.0 as u32 => Transfer::Hlg,
            Ok(_) | Err(_) => fallback.transfer,
        };
        let primaries = match t.GetUINT32(&MF_MT_VIDEO_PRIMARIES) {
            Ok(v) if v == MFVideoPrimaries_BT2020.0 as u32 => Primaries::Bt2020,
            Ok(_) | Err(_) => fallback.primaries,
        };
        let matrix = match t.GetUINT32(&MF_MT_YUV_MATRIX) {
            Ok(v)
                if v == MFVideoTransferMatrix_BT2020_10.0 as u32
                    || v == MFVideoTransferMatrix_BT2020_12.0 as u32 =>
            {
                Matrix::Bt2020
            }
            Ok(v) if v == MFVideoTransferMatrix_BT601.0 as u32 => Matrix::Bt601,
            // The HEVC decoder reports the primaries but not the matrix;
            // BT.2020 primaries come with the BT.2020 matrix.
            Ok(_) | Err(_) if primaries == Primaries::Bt2020 => Matrix::Bt2020,
            Ok(_) | Err(_) => fallback.matrix,
        };
        let full_range = match t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) {
            Ok(v) => v == NOMINAL_RANGE_FULL,
            Err(_) => fallback.full_range,
        };
        Colour {
            transfer,
            primaries,
            matrix,
            full_range,
        }
    }
}

fn read_frame(decoder: &IMFTransform, sample: &IMFSample, fallback: Colour) -> Option<Frame> {
    unsafe {
        let t = decoder.GetOutputCurrentType(0).ok()?;
        let sub = t.GetGUID(&MF_MT_SUBTYPE).ok()?;
        let ten_bit = if sub == MFVideoFormat_P010 {
            true
        } else if sub == MFVideoFormat_NV12 {
            false
        } else {
            return None;
        };
        let (coded_w, coded_h) = unpack(t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?);
        if coded_w == 0 || coded_h == 0 || coded_w > MAX_WIDTH || coded_h > MAX_HEIGHT + 16 {
            return None;
        }
        let (x, y, w, h) = aperture(&t, &MF_MT_MINIMUM_DISPLAY_APERTURE)
            .or_else(|| aperture(&t, &MF_MT_GEOMETRIC_APERTURE))
            .unwrap_or((0, 0, coded_w, coded_h));
        if x.checked_add(w)? > coded_w || y.checked_add(h)? > coded_h {
            return None;
        }
        let pixel_aspect = t
            .GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO)
            .map(unpack)
            .ok()
            .filter(|&(n, d)| n > 0 && d > 0)
            .unwrap_or((1, 1));
        let colour = colour_of(&t, fallback);
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let bytes_per_sample = if ten_bit { 2usize } else { 1 };
        let min_stride = coded_w as usize * bytes_per_sample;
        let area = (x, y, w, h);
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
            // Lock2DSize also reports where the buffer starts and how long it
            // is, so the planes can be checked against its real extent.
            let mut scan0 = null_mut();
            let mut pitch = 0i32;
            let mut start = null_mut();
            let mut length = 0u32;
            if two_d
                .Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scan0,
                    &mut pitch,
                    &mut start,
                    &mut length,
                )
                .is_ok()
            {
                let frame = (|| {
                    let stride = usize::try_from(pitch).ok().filter(|&s| s >= min_stride)?;
                    if start.is_null() || scan0.is_null() {
                        return None;
                    }
                    // Plain addresses: the decoder's pointers are not trusted
                    // to lie in one allocation until checked.
                    let skip = (scan0 as usize).checked_sub(start as usize)?;
                    let whole = core::slice::from_raw_parts(start, length as usize);
                    convert(
                        whole.get(skip..)?,
                        stride,
                        coded_h,
                        ten_bit,
                        area,
                        colour,
                        pixel_aspect,
                    )
                })();
                let _ = two_d.Unlock2D();
                return frame;
            }
        }
        // A contiguous buffer (what decoders write into caller-allocated
        // memory buffers), with the type's default stride.
        let mut p = null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut p, None, Some(&mut length)).ok()?;
        let stride = match t.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
            // Negative: a bottom-up picture, which decoders do not write.
            Ok(s) => usize::try_from(s as i32).ok(),
            Err(_) => Some(min_stride),
        };
        let frame = match stride {
            Some(stride) if !p.is_null() && stride >= min_stride => {
                let data = core::slice::from_raw_parts(p, length as usize);
                convert(data, stride, coded_h, ten_bit, area, colour, pixel_aspect)
            }
            _ => None,
        };
        let _ = buffer.Unlock();
        frame
    }
}

fn convert(
    data: &[u8],
    stride: usize,
    coded_h: u32,
    ten_bit: bool,
    (x, y, w, h): (u32, u32, u32, u32),
    colour: Colour,
    pixel_aspect: (u32, u32),
) -> Option<Frame> {
    let chroma_offset = stride.checked_mul(coded_h as usize)?;
    let picture = SemiPlanar {
        data,
        stride,
        chroma_offset,
        ten_bit,
        x,
        y,
        width: w,
        height: h,
    };
    yuv::to_frame(&picture, colour, pixel_aspect)
}
