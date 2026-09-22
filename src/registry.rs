//! Registration of the thumbnail handler in the Windows registry.
//!
//! Keys written (under `HKLM` or `HKCU` `Software\Classes`):
//! - `CLSID\{clsid}` + `InprocServer32` (path of this DLL, apartment threaded)
//! - `.iso\ShellEx\{E357FCCD-A995-4576-B01F-234630154E96}` = `{clsid}`
//!
//! For a machine-wide install the CLSID is also listed under
//! `Shell Extensions\Approved`, which only matters when the
//! "EnforceShellExtensionSecurity" policy is enabled.
//!
//! A per-user install additionally sets `DisableProcessIsolation=1` on the
//! CLSID: the shell's out-of-process thumbnail host cannot activate classes
//! that are registered only under `HKEY_CURRENT_USER`.

use windows::core::{Error, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

pub const CLSID_TEXT: &str = "{C767266A-4032-4099-9A92-F91D1FE98122}";
pub const HANDLER_NAME: &str = "ISO Blu-ray Thumbnail Provider";
const THUMBNAIL_HANDLER_IID: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
const APPROVED_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Shell Extensions\\Approved";
/// File extensions the handler is attached to.
pub const EXTENSIONS: [&str; 1] = [".iso"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// `HKEY_LOCAL_MACHINE`, needs administrator rights.
    Machine,
    /// `HKEY_CURRENT_USER`, no elevation needed.
    User,
}

impl Scope {
    fn root(self) -> HKEY {
        match self {
            Scope::Machine => HKEY_LOCAL_MACHINE,
            Scope::User => HKEY_CURRENT_USER,
        }
    }
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn create_key(root: HKEY, path: &str) -> Result<Key> {
    let mut handle = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            root,
            &HSTRING::from(path),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut handle,
            None,
        )
        .ok()?;
    }
    Ok(Key(handle))
}

fn open_key_write(root: HKEY, path: &str) -> Result<Key> {
    let mut handle = HKEY::default();
    unsafe {
        RegOpenKeyExW(root, &HSTRING::from(path), None, KEY_WRITE, &mut handle).ok()?;
    }
    Ok(Key(handle))
}

fn set_string(key: &Key, name: &str, value: &str) -> Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe { RegSetValueExW(key.0, &HSTRING::from(name), None, REG_SZ, Some(bytes)).ok() }
}

fn set_dword(key: &Key, name: &str, value: u32) -> Result<()> {
    unsafe {
        RegSetValueExW(
            key.0,
            &HSTRING::from(name),
            None,
            REG_DWORD,
            Some(&value.to_le_bytes()),
        )
        .ok()
    }
}

/// What the `.ext\ShellEx\{thumbnail handler}` key currently points at.
enum ShellExOwner {
    /// The key does not exist.
    Missing,
    /// The key names this handler, or has no usable default value.
    Us,
    /// Another handler took over the extension; leave it alone.
    Other,
}

fn shellex_owner(root: HKEY, path: &str) -> ShellExOwner {
    let mut handle = HKEY::default();
    let opened = unsafe { RegOpenKeyExW(root, &HSTRING::from(path), None, KEY_READ, &mut handle) };
    if opened == ERROR_FILE_NOT_FOUND {
        return ShellExOwner::Missing;
    }
    if opened != ERROR_SUCCESS {
        return ShellExOwner::Other;
    }
    let key = Key(handle);
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let mut ty = REG_VALUE_TYPE::default();
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            &HSTRING::from(""),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return ShellExOwner::Us;
    }
    if status != ERROR_SUCCESS || ty != REG_SZ {
        return ShellExOwner::Other;
    }
    let n = (len as usize / 2).min(buf.len());
    let value = String::from_utf16_lossy(&buf[..n]);
    if value.trim_end_matches('\0').eq_ignore_ascii_case(CLSID_TEXT) {
        ShellExOwner::Us
    } else {
        ShellExOwner::Other
    }
}

/// Deletes a key and everything below it; a missing key is not an error.
/// Returns whether the key existed.
fn delete_tree(root: HKEY, path: &str) -> Result<bool> {
    let err = unsafe { RegDeleteTreeW(root, &HSTRING::from(path)) };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    err.ok().map(|_| true)
}

pub fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) }
}

pub fn is_access_denied(e: &Error) -> bool {
    e.code() == ERROR_ACCESS_DENIED.to_hresult()
}

pub fn register(scope: Scope, dll_path: &str) -> Result<()> {
    let root = scope.root();
    let clsid_path = format!("Software\\Classes\\CLSID\\{CLSID_TEXT}");
    let key = create_key(root, &clsid_path)?;
    set_string(&key, "", HANDLER_NAME)?;
    if scope == Scope::User {
        // The shell's isolated thumbnail host (dllhost.exe) does not see per-user
        // COM registrations and fails with REGDB_E_CLASSNOTREG, so a per-user
        // install has to run the handler inside the requesting process instead.
        set_dword(&key, "DisableProcessIsolation", 1)?;
    }
    let key = create_key(root, &format!("{clsid_path}\\InprocServer32"))?;
    set_string(&key, "", dll_path)?;
    set_string(&key, "ThreadingModel", "Apartment")?;
    for ext in EXTENSIONS {
        let key = create_key(
            root,
            &format!("Software\\Classes\\{ext}\\ShellEx\\{THUMBNAIL_HANDLER_IID}"),
        )?;
        set_string(&key, "", CLSID_TEXT)?;
    }
    if scope == Scope::Machine {
        if let Ok(key) = create_key(root, APPROVED_PATH) {
            let _ = set_string(&key, CLSID_TEXT, HANDLER_NAME);
        }
    }
    notify_shell();
    Ok(())
}

/// Removes the registration. Returns whether anything was found to remove.
pub fn unregister(scope: Scope) -> Result<bool> {
    let root = scope.root();
    let mut removed = false;
    for ext in EXTENSIONS {
        let path = format!("Software\\Classes\\{ext}\\ShellEx\\{THUMBNAIL_HANDLER_IID}");
        match shellex_owner(root, &path) {
            ShellExOwner::Us => removed |= delete_tree(root, &path)?,
            ShellExOwner::Missing | ShellExOwner::Other => {}
        }
    }
    removed |= delete_tree(root, &format!("Software\\Classes\\CLSID\\{CLSID_TEXT}"))?;
    if scope == Scope::Machine {
        if let Ok(key) = open_key_write(root, APPROVED_PATH) {
            unsafe {
                let _ = RegDeleteValueW(key.0, &HSTRING::from(CLSID_TEXT));
            }
        }
    }
    if removed {
        notify_shell();
    }
    Ok(removed)
}
