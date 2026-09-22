//! COM objects: the class factory and the thumbnail provider itself.

// The `_Impl` trait methods mirror the COM vtable signatures (raw out-pointers
// supplied by the caller); they cannot be marked `unsafe` and null-check first.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use core::ffi::c_void;
use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_FAIL, E_POINTER, E_UNEXPECTED};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::{
    IClassFactory, IClassFactory_Impl, IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_END,
    STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTSAT_ARGB, WTSAT_RGB, WTS_ALPHATYPE,
};

use crate::error::Error as ParseError;
use crate::image::decode_to_dib;
use crate::reader::ByteSource;

/// CLSID of the handler: {C767266A-4032-4099-9A92-F91D1FE98122}.
pub const CLSID_ISO_THUMBNAIL: GUID = GUID::from_u128(0xC767266A_4032_4099_9A92_F91D1FE98122);

/// Live COM objects plus `IClassFactory::LockServer` locks; drives `DllCanUnloadNow`.
static SERVER_LOCKS: AtomicIsize = AtomicIsize::new(0);

pub fn server_locked() -> bool {
    SERVER_LOCKS.load(Ordering::SeqCst) > 0
}

fn lock_server() {
    SERVER_LOCKS.fetch_add(1, Ordering::SeqCst);
}

fn unlock_server() {
    SERVER_LOCKS.fetch_sub(1, Ordering::SeqCst);
}

/// `ByteSource` over the `IStream` the shell hands us.
pub struct StreamSource(pub IStream);

impl ByteSource for StreamSource {
    fn size(&mut self) -> crate::error::Result<u64> {
        unsafe {
            let mut stat = STATSTG::default();
            if self.0.Stat(&mut stat, STATFLAG_NONAME).is_ok() && stat.cbSize > 0 {
                return Ok(stat.cbSize);
            }
            let mut end = 0u64;
            self.0
                .Seek(0, STREAM_SEEK_END, Some(&mut end as *mut u64))
                .map_err(|_| ParseError::Io)?;
            Ok(end)
        }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> crate::error::Result<()> {
        unsafe {
            self.0
                .Seek(offset as i64, STREAM_SEEK_SET, None)
                .map_err(|_| ParseError::Io)?;
            let mut done = 0usize;
            while done < buf.len() {
                let remaining = &mut buf[done..];
                let mut got = 0u32;
                let hr = self.0.Read(
                    remaining.as_mut_ptr() as *mut c_void,
                    remaining.len() as u32,
                    Some(&mut got as *mut u32),
                );
                if hr.is_err() || got == 0 {
                    return Err(ParseError::Io);
                }
                done += got as usize;
            }
            Ok(())
        }
    }
}

#[implement(IThumbnailProvider, IInitializeWithStream)]
pub struct ThumbnailProvider {
    stream: RefCell<Option<IStream>>,
}

impl ThumbnailProvider {
    pub fn new() -> Self {
        lock_server();
        Self {
            stream: RefCell::new(None),
        }
    }
}

impl Default for ThumbnailProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ThumbnailProvider {
    fn drop(&mut self) {
        unlock_server();
    }
}

impl IInitializeWithStream_Impl for ThumbnailProvider_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> Result<()> {
        let stream = pstream.ok()?.clone();
        *self.stream.borrow_mut() = Some(stream);
        Ok(())
    }
}

fn render(stream: IStream, cx: u32) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
    // Any parse failure (no artwork, not a disc image, ...) makes the shell fall
    // back to the ordinary .iso icon.
    let found = crate::extract_thumbnail(StreamSource(stream))
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
    let max_side = if cx == 0 { 256 } else { cx };
    let decoded = decode_to_dib(&found.thumbnail.data, max_side)?;
    let alpha = if decoded.has_alpha {
        WTSAT_ARGB
    } else {
        WTSAT_RGB
    };
    Ok((decoded.bitmap, alpha))
}

impl IThumbnailProvider_Impl for ThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let stream = self
            .stream
            .borrow()
            .clone()
            .ok_or_else(|| windows::core::Error::from(E_UNEXPECTED))?;
        // A panic must never cross the COM boundary.
        match catch_unwind(AssertUnwindSafe(|| render(stream, cx))) {
            Ok(Ok((bitmap, alpha))) => unsafe {
                *phbmp = bitmap;
                *pdwalpha = alpha;
                Ok(())
            },
            Ok(Err(e)) => Err(e),
            Err(_) => Err(E_FAIL.into()),
        }
    }
}

#[implement(IClassFactory)]
pub struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if ppvobject.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *ppvobject = std::ptr::null_mut();
        }
        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unknown: IUnknown = ThumbnailProvider::new().into();
        unsafe { unknown.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            lock_server();
        } else {
            unlock_server();
        }
        Ok(())
    }
}
