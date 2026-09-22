//! DLL entry points used by COM (`DllGetClassObject`, `DllCanUnloadNow`) and
//! by regsvr32 (`DllRegisterServer`, `DllUnregisterServer`, `DllInstall`).

// These are C ABI entry points whose pointer arguments come from COM; the
// signatures are fixed by Windows and every pointer is null-checked before use.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{IUnknown, Interface, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use crate::com::{server_locked, ClassFactory, CLSID_ISO_THUMBNAIL};
use crate::registry::{self, Scope};

static MODULE: AtomicIsize = AtomicIsize::new(0);

/// Only records the module handle; nothing else may run under the loader lock.
#[no_mangle]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0 as isize, Ordering::SeqCst);
    }
    true.into()
}

fn module_path() -> Option<String> {
    let handle = MODULE.load(Ordering::SeqCst);
    if handle == 0 {
        return None;
    }
    let mut buf = vec![0u16; 32 * 1024];
    let n = unsafe { GetModuleFileNameW(Some(HMODULE(handle as *mut c_void)), &mut buf) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n]))
}

#[no_mangle]
pub extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = std::ptr::null_mut();
        if *rclsid != CLSID_ISO_THUMBNAIL {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IUnknown = ClassFactory.into();
        factory.query(riid, ppv)
    }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if server_locked() {
        S_FALSE
    } else {
        S_OK
    }
}

fn register_in(scope: Option<Scope>) -> HRESULT {
    let Some(path) = module_path() else {
        return E_FAIL;
    };
    let outcome = match scope {
        Some(scope) => registry::register(scope, &path),
        // Default: machine-wide when elevated, otherwise the current user.
        None => registry::register(Scope::Machine, &path).or_else(|e| {
            if registry::is_access_denied(&e) {
                registry::register(Scope::User, &path)
            } else {
                Err(e)
            }
        }),
    };
    match outcome {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

fn unregister_in(scope: Option<Scope>) -> HRESULT {
    match scope {
        Some(scope) => match registry::unregister(scope) {
            Ok(_) => S_OK,
            Err(e) => e.code(),
        },
        None => {
            let machine = registry::unregister(Scope::Machine);
            let user = registry::unregister(Scope::User);
            match (machine, user) {
                (Ok(_), Ok(_)) | (Err(_), Ok(true)) => S_OK,
                (Err(e), Ok(false)) | (Ok(_), Err(e)) | (Err(e), Err(_)) => e.code(),
            }
        }
    }
}

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    register_in(None)
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    unregister_in(None)
}

/// `regsvr32 /n /i:user IsoPreview.dll` registers for the current user only;
/// `/i:machine` forces a machine-wide registration.
#[no_mangle]
pub extern "system" fn DllInstall(binstall: BOOL, pszcmdline: PCWSTR) -> HRESULT {
    let cmdline = if pszcmdline.is_null() {
        String::new()
    } else {
        unsafe { pszcmdline.to_string().unwrap_or_default() }
    }
    .to_ascii_lowercase();
    let scope = if cmdline.contains("user") {
        Some(Scope::User)
    } else if cmdline.contains("machine") {
        Some(Scope::Machine)
    } else {
        None
    };
    if binstall.as_bool() {
        register_in(scope)
    } else {
        unregister_in(scope)
    }
}
