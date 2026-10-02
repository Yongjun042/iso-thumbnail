//! Decodes the artwork with WIC and returns a 32-bit top-down DIB section,
//! which is what `IThumbnailProvider::GetThumbnail` hands to the shell.
//!
//! Limits: the encoded file is capped by the finder, the source dimensions by
//! `MAX_PIXELS`, and the requested size by `MAX_SIDE`. JPEGs are decoded at a
//! reduced resolution through `IWICBitmapSourceTransform` (DCT-domain scaling)
//! so a large cover never costs a full-size decode. Only JPEG, PNG, GIF and BMP
//! are decoded, by Windows' built-in decoders picked from the file signature.
//!
//! Most of these decoders hand the scaler a few rows at a time, but some
//! encodings make them hold the whole frame (progressive JPEG, interlaced PNG
//! and GIF, run-length coded BMP): several bytes per pixel, from a file that
//! may be tiny. Those get the lower `MAX_BUFFERED_PIXELS`.

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
/// The same for encodings the decoder holds in full (`buffers_whole_frame`):
/// about 45 MiB for a progressive JPEG.
pub const MAX_BUFFERED_PIXELS: u64 = 8_000_000;
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

/// Whether the decoder holds the whole frame while the image is read: a
/// progressive JPEG (its coefficients), an interlaced PNG or GIF (the passes),
/// or a BMP that is run-length coded or embeds another format. Only the
/// headers are looked at, with every offset checked.
fn buffers_whole_frame(data: &[u8]) -> bool {
    let u16be = |o: usize| {
        data.get(o..o + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
    };
    if data.starts_with(&[0xFF, 0xD8]) {
        // Markers up to the first frame header (SOFn) or scan (SOS).
        let mut at = 2;
        while let (Some(&0xFF), Some(&marker)) = (data.get(at), data.get(at + 1)) {
            match marker {
                0xFF => at += 1, // fill byte
                0xC2 | 0xC6 | 0xCA | 0xCE => return true,
                0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => return false,
                0xDA => return false,
                0x01 | 0xD0..=0xD7 => at += 2,
                _ => match u16be(at + 2) {
                    Some(len) if len >= 2 => at += 2 + len,
                    _ => return false,
                },
            }
        }
        false
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        // IHDR first: length, type, width, height, depth, colour type,
        // compression, filter, interlace.
        data.get(12..16) == Some(b"IHDR") && data.get(28).is_some_and(|&i| i != 0)
    } else if data.starts_with(b"GIF8") {
        // Logical screen descriptor, global colour table, then extensions
        // until the first image descriptor, whose flags hold the interlace bit.
        let mut at = 13;
        if let Some(&flags) = data.get(10) {
            if flags & 0x80 != 0 {
                at += 3 << ((flags & 7) + 1);
            }
        }
        loop {
            match data.get(at) {
                Some(0x2C) => return data.get(at + 9).is_some_and(|&f| f & 0x40 != 0),
                Some(0x21) => {
                    // Label, then sub-blocks up to an empty one.
                    at += 2;
                    while let Some(&n) = data.get(at) {
                        at += 1 + usize::from(n);
                        if n == 0 {
                            break;
                        }
                    }
                }
                _ => return false,
            }
        }
    } else if data.starts_with(b"BM") {
        // BITMAPINFOHEADER (or a later version): biCompression BI_RLE8, BI_RLE4,
        // BI_JPEG or BI_PNG.
        let header = data
            .get(14..18)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let compression = data
            .get(30..34)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        header.is_some_and(|h| h >= 40) && matches!(compression, Some(1 | 2 | 4 | 5))
    } else {
        false
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
        let max_pixels = if buffers_whole_frame(data) {
            MAX_BUFFERED_PIXELS
        } else {
            MAX_PIXELS
        };
        if w == 0 || h == 0 || w as u64 * h as u64 > max_pixels {
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
    use super::{buffers_whole_frame, decode_to_dib, decoder_for, fit, picture_to_dib};
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
    fn encodings_decoded_in_full_are_recognised() {
        // JPEG: baseline, progressive, after other segments and fill bytes.
        let jpeg = |markers: &[u8]| [&[0xFF, 0xD8][..], markers].concat();
        assert!(!buffers_whole_frame(&jpeg(&[0xFF, 0xC0, 0, 11])));
        assert!(buffers_whole_frame(&jpeg(&[0xFF, 0xC2, 0, 11])));
        assert!(buffers_whole_frame(&jpeg(&[
            0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xFF, 0xC2, 0, 11
        ])));
        assert!(!buffers_whole_frame(&jpeg(&[
            0xFF, 0xC4, 0, 4, 0, 0, 0xFF, 0xDA, 0, 8
        ])));
        assert!(!buffers_whole_frame(&jpeg(&[0xFF, 0xE0, 0, 0xFF])));
        // PNG: the interlace byte of IHDR.
        let mut png =
            b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\x0f\xa0\0\0\x0f\xa0\x08\x06\0\0\0".to_vec();
        assert!(!buffers_whole_frame(&png));
        png[28] = 1;
        assert!(buffers_whole_frame(&png));
        // GIF: past a global colour table and an extension.
        let mut gif = b"GIF89a\x10\0\x10\0\x80\0\0".to_vec();
        gif.extend([0u8; 6]); // 2-entry colour table
        gif.extend([0x21, 0xF9, 4, 0, 0, 0, 0, 0]);
        gif.extend([0x2C, 0, 0, 0, 0, 0x10, 0, 0x10, 0, 0x00]);
        assert!(!buffers_whole_frame(&gif));
        *gif.last_mut().unwrap() = 0x40;
        assert!(buffers_whole_frame(&gif));
        // BMP: the compression of the info header.
        let mut bmp = b"BM".to_vec();
        bmp.extend([0u8; 12]);
        bmp.extend(40u32.to_le_bytes());
        bmp.extend([0u8; 12]);
        bmp.extend(0u32.to_le_bytes());
        assert!(!buffers_whole_frame(&bmp));
        bmp[30] = 1;
        assert!(buffers_whole_frame(&bmp));
        // Short or foreign data.
        for d in [&b""[..], b"\xFF\xD8", b"GIF89a", b"BM", b"\x89PNG"] {
            assert!(!buffers_whole_frame(d));
        }
    }

    /// A `w` x `h` run-length coded (RLE8) BMP of one grey line repeated:
    /// a few bytes per line, a full frame for the decoder.
    fn rle8_bmp(w: u32, h: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for _ in 0..h {
            let mut left = w;
            while left > 0 {
                let n = left.min(255);
                pixels.extend([n as u8, 1]);
                left -= n;
            }
            pixels.extend([0, 0]); // end of line
        }
        pixels.extend([0, 1]); // end of bitmap
        let offset = 14 + 40 + 8;
        let mut d = b"BM".to_vec();
        d.extend((offset + pixels.len() as u32).to_le_bytes());
        d.extend([0u8; 4]);
        d.extend(offset.to_le_bytes());
        d.extend(40u32.to_le_bytes());
        d.extend((w as i32).to_le_bytes());
        d.extend((h as i32).to_le_bytes());
        d.extend(1u16.to_le_bytes());
        d.extend(8u16.to_le_bytes());
        d.extend(1u32.to_le_bytes()); // BI_RLE8
        d.extend((pixels.len() as u32).to_le_bytes());
        d.extend([0u8; 8]);
        d.extend(2u32.to_le_bytes());
        d.extend(0u32.to_le_bytes());
        d.extend([0, 0, 0, 0, 128, 128, 128, 0]); // palette
        d.extend(pixels);
        d
    }

    #[test]
    fn whole_frame_encodings_get_the_lower_pixel_cap() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // 2000 x 2000 decodes; 4000 x 4000 (16M pixels, under MAX_PIXELS, a
        // 62 MiB frame for the decoder, from a 130 KB file) is refused.
        let small = decode_to_dib(&rle8_bmp(2000, 2000), 256).expect("2000 x 2000");
        assert_eq!((small.width, small.height), (256, 256));
        unsafe {
            let _ = DeleteObject(small.bitmap.into());
        }
        assert!(decode_to_dib(&rle8_bmp(4000, 4000), 256).is_err());
        unsafe { CoUninitialize() };
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
