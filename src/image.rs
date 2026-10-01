//! Decodes the artwork with WIC and returns a 32-bit top-down DIB section,
//! which is what `IThumbnailProvider::GetThumbnail` hands to the shell.
//!
//! Limits: the encoded file is capped by the finder, the source dimensions by
//! `MAX_PIXELS`, and the requested size by `MAX_SIDE`. JPEGs are decoded at a
//! reduced resolution through `IWICBitmapSourceTransform` (DCT-domain scaling)
//! so a large cover never costs a full-size decode. Only JPEG, PNG, GIF and BMP
//! are decoded, by Windows' built-in decoders picked from the file signature.

use core::ffi::c_void;

use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICBmpDecoder, CLSID_WICGifDecoder, CLSID_WICImagingFactory, CLSID_WICJpegDecoder,
    CLSID_WICPngDecoder, GUID_WICPixelFormat32bppBGRA, IWICBitmapDecoder, IWICBitmapFrameDecode,
    IWICBitmapSource, IWICBitmapSourceTransform, IWICImagingFactory, IWICPixelFormatInfo,
    IWICPixelFormatInfo2, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICBitmapTransformRotate0, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// Largest source image we are willing to decode (width × height).
pub const MAX_PIXELS: u64 = 16_000_000;
/// Largest thumbnail edge we produce; the shell never asks for more.
pub const MAX_SIDE: u32 = 2560;
/// Largest intermediate buffer for a decoder-scaled frame.
const MAX_SCALED_BYTES: u64 = 64 << 20;

pub struct Decoded {
    /// Top-down 32bpp BGRA DIB section. The caller owns it.
    pub bitmap: HBITMAP,
    pub width: u32,
    pub height: u32,
    /// Whether the source format carries transparency (PNG/GIF); JPEG does not.
    pub has_alpha: bool,
}

/// Windows' built-in decoder for the file signature, or `None` for anything
/// that is not JPEG, PNG, GIF or BMP. Letting WIC pick a codec from the content
/// would hand attacker bytes to every installed codec (TIFF, JPEG XR, camera
/// RAW, third-party codecs), some of which ignore the pixel limits below.
fn decoder_for(data: &[u8]) -> Option<&'static GUID> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(&CLSID_WICJpegDecoder)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(&CLSID_WICPngDecoder)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some(&CLSID_WICGifDecoder)
    } else if data.starts_with(b"BM") {
        Some(&CLSID_WICBmpDecoder)
    } else {
        None
    }
}

/// Scales (w, h) down so the longer side is at most `max_side`; never scales up.
fn fit(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    if w.max(h) <= max_side {
        return (w, h);
    }
    if w >= h {
        let th = (h as u64 * max_side as u64) / w as u64;
        (max_side, th.max(1) as u32)
    } else {
        let tw = (w as u64 * max_side as u64) / h as u64;
        (tw.max(1) as u32, max_side)
    }
}

/// Asks the decoder for a natively downscaled frame (the JPEG codec decodes at
/// 1/2, 1/4 or 1/8 without touching the full image). Returns `None` when the
/// codec cannot help; the caller then scales the full frame.
///
/// # Safety
/// Plain COM calls; `frame` must be a live decoder frame.
unsafe fn decode_scaled(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    full: (u32, u32),
    target: (u32, u32),
) -> Option<IWICBitmapSource> {
    let transform: IWICBitmapSourceTransform = frame.cast().ok()?;
    let (mut cw, mut ch) = target;
    transform.GetClosestSize(&mut cw, &mut ch).ok()?;
    // Only worth it when the codec gives us something smaller than the full
    // frame but not smaller than what we need (we never upscale).
    if cw == 0 || ch == 0 || cw >= full.0 || ch >= full.1 || cw < target.0 || ch < target.1 {
        return None;
    }
    let mut fmt: GUID = GUID_WICPixelFormat32bppBGRA;
    transform.GetClosestPixelFormat(&mut fmt).ok()?;
    let bpp = factory
        .CreateComponentInfo(&fmt)
        .ok()?
        .cast::<IWICPixelFormatInfo>()
        .ok()?
        .GetBitsPerPixel()
        .ok()?;
    if bpp == 0 {
        return None;
    }
    let stride = (cw as u64 * bpp as u64).div_ceil(8);
    let size = stride * ch as u64;
    if size > MAX_SCALED_BYTES {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    transform
        .CopyPixels(
            std::ptr::null(),
            cw,
            ch,
            &fmt,
            WICBitmapTransformRotate0,
            stride as u32,
            &mut buf,
        )
        .ok()?;
    let bitmap = factory
        .CreateBitmapFromMemory(cw, ch, &fmt, stride as u32, &buf)
        .ok()?;
    bitmap.cast().ok()
}

/// Decodes `data` and scales it down (never up) so the longer side is at most `max_side`.
pub fn decode_to_dib(data: &[u8], max_side: u32) -> Result<Decoded> {
    let decoder_clsid = decoder_for(data).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        // A WIC stream over the caller's buffer: no second copy of the file.
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(data)?;
        let decoder: IWICBitmapDecoder =
            CoCreateInstance(decoder_clsid, None, CLSCTX_INPROC_SERVER)?;
        decoder.Initialize(&stream, WICDecodeMetadataCacheOnDemand)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
            return Err(E_FAIL.into());
        }
        let has_alpha = frame
            .GetPixelFormat()
            .and_then(|fmt| factory.CreateComponentInfo(&fmt))
            .and_then(|info| info.cast::<IWICPixelFormatInfo2>())
            .and_then(|pf| pf.SupportsTransparency())
            .map(|b| b.as_bool())
            .unwrap_or(false);

        let max_side = max_side.clamp(1, MAX_SIDE);
        let (tw, th) = fit(w, h, max_side);
        let mut source: IWICBitmapSource = frame.cast()?;
        if (tw, th) != (w, h) {
            if let Some(scaled) = decode_scaled(&factory, &frame, (w, h), (tw, th)) {
                source = scaled;
            }
            let (mut sw, mut sh) = (0u32, 0u32);
            source.GetSize(&mut sw, &mut sh)?;
            if (sw, sh) != (tw, th) {
                let scaler = factory.CreateBitmapScaler()?;
                scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
                source = scaler.cast()?;
            }
        }
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &source,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let bitmap = copy_to_dib(&converter, tw, th)?;
        Ok(Decoded {
            bitmap,
            width: tw,
            height: th,
            has_alpha,
        })
    }
}

/// Copies a 32bpp BGRA `source` of `width` × `height` pixels into a new
/// top-down 32-bit DIB section, the form the shell takes thumbnails in.
///
/// # Safety
/// Plain COM and GDI calls; `source` must be a live bitmap source of that size
/// and pixel format.
unsafe fn copy_to_dib(source: &IWICBitmapSource, width: u32, height: u32) -> Result<HBITMAP> {
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    let len = stride as usize * height as usize;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    if bits.is_null() {
        let _ = DeleteObject(bitmap.into());
        return Err(E_FAIL.into());
    }
    let buffer = std::slice::from_raw_parts_mut(bits as *mut u8, len);
    if let Err(e) = source.CopyPixels(std::ptr::null(), stride, buffer) {
        let _ = DeleteObject(bitmap.into());
        return Err(e);
    }
    Ok(bitmap)
}

/// Scales a decoded video picture (see `crate::picture`) to fit `max_side`
/// with its pixel aspect ratio applied, and returns it as a DIB like
/// `decode_to_dib` (never scaled up beyond its display size; `has_alpha` false).
///
/// The target is `fit(display_size, max_side)`, so an anamorphic picture is
/// stretched or squeezed horizontally by the same Fant scaler that resizes it.
/// Fails for an empty picture, one above `MAX_PIXELS`, or a pixel buffer
/// shorter than `width * height * 4`.
pub fn picture_to_dib(picture: &crate::picture::Picture, max_side: u32) -> Result<Decoded> {
    let fail = || windows::core::Error::from(E_FAIL);
    let (w, h) = (picture.width, picture.height);
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
        return Err(fail());
    }
    let stride = w.checked_mul(4).ok_or_else(fail)?;
    let pixels = picture
        .bgra
        .get(..stride as usize * h as usize)
        .ok_or_else(fail)?;
    let (dw, dh) = picture.display_size();
    let (tw, th) = fit(dw, dh, max_side.clamp(1, MAX_SIDE));
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        // WIC copies the pixels once; the scaler then reads from that copy.
        let bitmap =
            factory.CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppBGRA, stride, pixels)?;
        let mut source: IWICBitmapSource = bitmap.cast()?;
        if (tw, th) != (w, h) {
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
            source = scaler.cast()?;
        }
        let bitmap = copy_to_dib(&source, tw, th)?;
        Ok(Decoded {
            bitmap,
            width: tw,
            height: th,
            has_alpha: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{decoder_for, fit, picture_to_dib};
    use crate::picture::Picture;
    use core::ffi::c_void;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    #[test]
    fn only_builtin_formats_get_a_decoder() {
        assert!(decoder_for(b"\xFF\xD8\xFF\xE0jfif").is_some());
        assert!(decoder_for(b"\x89PNG\r\n\x1a\n....").is_some());
        assert!(decoder_for(b"GIF89a..").is_some());
        assert!(decoder_for(b"BM......").is_some());
        assert!(decoder_for(b"II*\0tiff").is_none());
        assert!(decoder_for(b"II\xBC\x01jxr").is_none());
        assert!(decoder_for(b"").is_none());
    }

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit(640, 360, 256), (256, 144));
        assert_eq!(fit(360, 640, 256), (144, 256));
        assert_eq!(fit(100, 50, 256), (100, 50));
        assert_eq!(fit(4000, 1, 256), (256, 1));
    }

    /// Converts `picture` and returns (width, height, top-down BGRA pixels) of the DIB.
    fn dib_pixels(picture: &Picture, max_side: u32) -> (u32, u32, Vec<u8>) {
        let decoded = picture_to_dib(picture, max_side).expect("picture_to_dib");
        assert!(!decoded.has_alpha);
        unsafe {
            let mut bm = BITMAP::default();
            let got = GetObjectW(
                decoded.bitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut BITMAP as *mut c_void),
            );
            assert_eq!(got as usize, std::mem::size_of::<BITMAP>());
            assert_eq!(bm.bmBitsPixel, 32);
            let (w, h) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
            assert_eq!((w, h), (decoded.width, decoded.height));
            assert!(!bm.bmBits.is_null());
            let len = bm.bmWidthBytes as usize * h as usize;
            let pixels = std::slice::from_raw_parts(bm.bmBits as *const u8, len).to_vec();
            let _ = DeleteObject(decoded.bitmap.into());
            (w, h, pixels)
        }
    }

    #[test]
    fn picture_to_dib_applies_the_pixel_aspect() {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        assert!(com.is_ok(), "{com:?}");
        // 54 x 8: left half red, right half blue, bottom row green. With
        // 32:27 pixels it is 64 wide on screen.
        let (w, h) = (54u32, 8u32);
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let px = match (x < w / 2, y == h - 1) {
                    (_, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 255, 255],
                    (false, false) => [255, 0, 0, 255],
                };
                bgra.extend_from_slice(&px);
            }
        }
        let picture = Picture {
            width: w,
            height: h,
            bgra,
            pixel_aspect: (32, 27),
        };
        assert_eq!(picture.display_size(), (64, 8));

        let (dw, dh, px) = dib_pixels(&picture, 256);
        assert_eq!((dw, dh), (64, 8));
        let at = |x: usize, y: usize| px[(y * 64 + x) * 4..][..4].to_vec();
        assert_eq!(at(0, 0), [0, 0, 255, 255], "red, top left");
        assert_eq!(at(63, 0), [255, 0, 0, 255], "blue, top right");
        assert_eq!(at(10, 3), [0, 0, 255, 255]);
        assert_eq!(at(50, 3), [255, 0, 0, 255]);
        assert_eq!(at(0, 7), [0, 255, 0, 255], "green bottom row");
        assert_eq!(at(63, 7), [0, 255, 0, 255]);

        // Fitting into 32 keeps the display aspect.
        let (dw, dh, px) = dib_pixels(&picture, 32);
        assert_eq!((dw, dh), (32, 4));
        assert_eq!(px[..4], [0, 0, 255, 255]);
        // A max_side of 0 is clamped to 1.
        assert_eq!(dib_pixels(&picture, 0).0, 1);

        // Broken pictures fail instead of reading out of bounds.
        let mut short = picture.clone();
        short.bgra.truncate(100);
        assert!(picture_to_dib(&short, 256).is_err());
        let empty = Picture {
            width: 0,
            height: 0,
            bgra: Vec::new(),
            pixel_aspect: (1, 1),
        };
        assert!(picture_to_dib(&empty, 256).is_err());
        unsafe { CoUninitialize() };
    }
}
