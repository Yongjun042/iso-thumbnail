//! Command-line companion: tests the extractor outside Explorer and installs
//! the handler for the current user without administrator rights.
//!
//! ```text
//! isopreview-cli <image.iso> [--out FILE] [--size N] [--mode extract|com|shell] [--dll PATH]
//! isopreview-cli --install [--dll PATH]
//! isopreview-cli --install-machine [--dll PATH]     (elevated prompt)
//! isopreview-cli --uninstall
//! ```
//!
//! Modes:
//! - `extract`: run the parser in-process and write the raw artwork file.
//! - `com`: load IsoPreview.dll, create the COM object through its class
//!   factory and call `IThumbnailProvider::GetThumbnail`; write a PNG.
//! - `shell`: ask the Windows shell (`IShellItemImageFactory`) for the
//!   thumbnail, which exercises the registered handler exactly like Explorer.

extern crate IsoPreview as iso_preview;

use core::ffi::c_void;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use windows::core::{s, Error, IUnknown, Interface, GUID, HRESULT, HSTRING};
use windows::Win32::Foundation::{E_FAIL, GENERIC_WRITE, SIZE};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HPALETTE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, IWICBitmapFrameEncode, IWICImagingFactory,
    WICBitmapEncoderNoCache, WICBitmapUseAlpha,
};

use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IBindCtx, IClassFactory, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_WRITE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, IThumbnailProvider, SHCreateItemFromParsingName,
    SHCreateStreamOnFileEx, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY, WTSAT_ARGB, WTSAT_RGB,
    WTS_ALPHATYPE,
};

use iso_preview::reader::SeekSource;
use iso_preview::registry::{self, Scope};

const CLSID_ISO_THUMBNAIL: GUID = GUID::from_u128(0xC767266A_4032_4099_9A92_F91D1FE98122);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Extract,
    Com,
    Shell,
}

enum Action {
    Thumbnail,
    Install(Scope),
    Uninstall,
}

struct Opts {
    action: Action,
    image: Option<PathBuf>,
    out: Option<PathBuf>,
    size: u32,
    mode: Mode,
    dll: Option<PathBuf>,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  isopreview-cli <image.iso> [--out FILE] [--size N] [--mode extract|com|shell] [--dll PATH]\n  isopreview-cli --install [--dll PATH]\n  isopreview-cli --install-machine [--dll PATH]\n  isopreview-cli --uninstall"
    );
    ExitCode::from(2)
}

fn parse_args() -> Option<Opts> {
    let mut opts = Opts {
        action: Action::Thumbnail,
        image: None,
        out: None,
        size: 256,
        mode: Mode::Extract,
        dll: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => opts.out = Some(PathBuf::from(args.next()?)),
            "--size" => opts.size = args.next()?.parse().ok()?,
            "--dll" => opts.dll = Some(PathBuf::from(args.next()?)),
            "--mode" => {
                opts.mode = match args.next()?.as_str() {
                    "extract" => Mode::Extract,
                    "com" => Mode::Com,
                    "shell" => Mode::Shell,
                    _ => return None,
                }
            }
            "--install" => opts.action = Action::Install(Scope::User),
            "--install-machine" => opts.action = Action::Install(Scope::Machine),
            "--uninstall" => opts.action = Action::Uninstall,
            "-h" | "--help" => return None,
            _ if opts.image.is_none() && !a.starts_with("--") => {
                opts.image = Some(PathBuf::from(a))
            }
            _ => return None,
        }
    }
    Some(opts)
}

fn default_dll() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("IsoPreview.dll")))
        .unwrap_or_else(|| PathBuf::from("IsoPreview.dll"))
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn guess_ext(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if data.starts_with(b"\x89PNG") {
        "png"
    } else if data.starts_with(b"GIF8") {
        "gif"
    } else if data.starts_with(b"BM") {
        "bmp"
    } else {
        "bin"
    }
}

fn save_png(bitmap: HBITMAP, path: &Path) -> windows::core::Result<(u32, u32)> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let source =
            factory.CreateBitmapFromHBITMAP(bitmap, HPALETTE::default(), WICBitmapUseAlpha)?;
        let (mut w, mut h) = (0u32, 0u32);
        source.GetSize(&mut w, &mut h)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromFilename(
            &HSTRING::from(path.to_string_lossy().as_ref()),
            GENERIC_WRITE.0,
        )?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut props: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut props)?;
        let frame = frame.ok_or_else(|| Error::from(E_FAIL))?;
        frame.Initialize(props.as_ref())?;
        frame.WriteSource(&source, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
        Ok((w, h))
    }
}

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn thumbnail_via_dll(
    dll: &Path,
    image: &Path,
    size: u32,
) -> windows::core::Result<(HBITMAP, WTS_ALPHATYPE)> {
    unsafe {
        let module = LoadLibraryW(&HSTRING::from(dll.to_string_lossy().as_ref()))?;
        let entry =
            GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
        let get_class_object: DllGetClassObjectFn = std::mem::transmute(entry);
        let mut raw: *mut c_void = std::ptr::null_mut();
        get_class_object(&CLSID_ISO_THUMBNAIL, &IClassFactory::IID, &mut raw).ok()?;
        let factory = IClassFactory::from_raw(raw);
        let provider: IThumbnailProvider = factory.CreateInstance(None::<&IUnknown>)?;
        let init: IInitializeWithStream = provider.cast()?;
        let stream = SHCreateStreamOnFileEx(
            &HSTRING::from(image.to_string_lossy().as_ref()),
            (STGM_READ | STGM_SHARE_DENY_WRITE).0,
            0,
            false,
            None,
        )?;
        init.Initialize(&stream, STGM_READ.0)?;
        let mut bitmap = HBITMAP::default();
        let mut alpha = WTS_ALPHATYPE::default();
        provider.GetThumbnail(size, &mut bitmap, &mut alpha)?;
        Ok((bitmap, alpha))
    }
}

fn thumbnail_via_shell(image: &Path, size: u32) -> windows::core::Result<HBITMAP> {
    unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(
            &HSTRING::from(image.to_string_lossy().as_ref()),
            None::<&IBindCtx>,
        )?;
        item.GetImage(
            SIZE {
                cx: size as i32,
                cy: size as i32,
            },
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
}

fn run_thumbnail(opts: &Opts) -> ExitCode {
    let Some(image) = opts.image.as_deref() else {
        return usage();
    };
    let image = absolute(image);
    let out = opts.out.clone();
    match opts.mode {
        Mode::Extract => {
            let file = match File::open(&image) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("cannot open {}: {e}", image.display());
                    return ExitCode::from(1);
                }
            };
            let started = Instant::now();
            let result = iso_preview::extract_thumbnail(SeekSource(file));
            let elapsed = started.elapsed();
            match result {
                Ok(found) => {
                    println!("filesystem : {}", found.filesystem);
                    println!("thumbnail  : {}", found.thumbnail.path);
                    println!("bytes      : {}", found.thumbnail.data.len());
                    println!("image reads: {} ({} bytes)", found.reads, found.bytes_read);
                    println!("elapsed    : {:.2} ms", elapsed.as_secs_f64() * 1000.0);
                    let out = out.unwrap_or_else(|| {
                        image.with_extension(format!("thumb.{}", guess_ext(&found.thumbnail.data)))
                    });
                    if let Err(e) = std::fs::write(&out, &found.thumbnail.data) {
                        eprintln!("cannot write {}: {e}", out.display());
                        return ExitCode::from(1);
                    }
                    println!("written    : {}", out.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!(
                        "{}: {e} ({:.2} ms)",
                        image.display(),
                        elapsed.as_secs_f64() * 1000.0
                    );
                    ExitCode::from(3)
                }
            }
        }
        Mode::Com | Mode::Shell => {
            let started = Instant::now();
            let result = if opts.mode == Mode::Com {
                let dll = absolute(&opts.dll.clone().unwrap_or_else(default_dll));
                println!("dll        : {}", dll.display());
                thumbnail_via_dll(&dll, &image, opts.size)
            } else {
                thumbnail_via_shell(&image, opts.size).map(|b| (b, WTS_ALPHATYPE(-1)))
            };
            let elapsed = started.elapsed();
            match result {
                Ok((bitmap, alpha)) => {
                    let out = out.unwrap_or_else(|| image.with_extension("thumb.png"));
                    let saved = save_png(bitmap, &out);
                    unsafe {
                        let _ = DeleteObject(bitmap.into());
                    }
                    match saved {
                        Ok((w, h)) => {
                            println!("bitmap     : {w}x{h}");
                            if alpha == WTSAT_ARGB {
                                println!("alpha      : WTSAT_ARGB");
                            } else if alpha == WTSAT_RGB {
                                println!("alpha      : WTSAT_RGB");
                            }
                            println!("elapsed    : {:.2} ms", elapsed.as_secs_f64() * 1000.0);
                            println!("written    : {}", out.display());
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("cannot save PNG: {e}");
                            ExitCode::from(1)
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "{}: no thumbnail ({e}) ({:.2} ms)",
                        image.display(),
                        elapsed.as_secs_f64() * 1000.0
                    );
                    ExitCode::from(3)
                }
            }
        }
    }
}

fn run_install(scope: Scope, dll: Option<&Path>) -> ExitCode {
    let dll = absolute(&dll.map(Path::to_path_buf).unwrap_or_else(default_dll));
    if !dll.is_file() {
        eprintln!("DLL not found: {}", dll.display());
        return ExitCode::from(1);
    }
    match registry::register(scope, &dll.to_string_lossy()) {
        Ok(()) => {
            let who = match scope {
                Scope::User => "the current user (HKCU)",
                Scope::Machine => "all users (HKLM)",
            };
            println!("registered {} for {who}", dll.display());
            ExitCode::SUCCESS
        }
        Err(e) if registry::is_access_denied(&e) => {
            eprintln!("access denied: a machine-wide registration needs an elevated prompt");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("registration failed: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_uninstall() -> ExitCode {
    let mut removed = false;
    for scope in [Scope::User, Scope::Machine] {
        match registry::unregister(scope) {
            Ok(true) => {
                println!("removed the {scope:?} registration");
                removed = true;
            }
            Ok(false) => {}
            Err(e) if scope == Scope::Machine && registry::is_access_denied(&e) => {
                eprintln!("a machine-wide registration exists; remove it from an elevated prompt");
            }
            Err(e) => eprintln!("{scope:?}: {e}"),
        }
    }
    if !removed {
        println!("nothing to remove");
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let Some(opts) = parse_args() else {
        return usage();
    };
    unsafe {
        // Ignore RPC_E_CHANGED_MODE etc.; a failed init surfaces in the COM calls.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    match opts.action {
        Action::Thumbnail => run_thumbnail(&opts),
        Action::Install(scope) => run_install(scope, opts.dll.as_deref()),
        Action::Uninstall => run_uninstall(),
    }
}
