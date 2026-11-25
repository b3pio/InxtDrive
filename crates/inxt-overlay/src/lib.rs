//! Explorer status badges for InxtDrive drives.
//!
//! Explorer loads this DLL into its own process and asks each handler, for
//! every item it draws, whether its badge applies. Answers must be fast and
//! must never fail hard, so the decision uses only data Explorer already has:
//! the drive letter (InxtDrive lists its mounts in the registry) and the
//! item's file attributes, which the InxtDrive file system sets to describe
//! sync state. Nothing in here can panic.
#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{Interface, Ref, BOOL, GUID, HRESULT, PCWSTR, PWSTR};
use windows_core::implement;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_POINTER, HMODULE, MAX_PATH, S_FALSE, S_OK,
};
use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::{
    IShellIconOverlayIdentifier, IShellIconOverlayIdentifier_Impl, ISIOI_ICONFILE, ISIOI_ICONINDEX,
};

pub const CLSID_SYNCED: GUID = GUID::from_u128(0xE5A7F2B4_1C3D_4F6A_8B9E_0D1C2B3A4F51);
pub const CLSID_SYNCING: GUID = GUID::from_u128(0xE5A7F2B4_1C3D_4F6A_8B9E_0D1C2B3A4F52);

const ATTR_ARCHIVE: u32 = 0x20;
const ATTR_PINNED: u32 = 0x0008_0000;
const ATTR_UNPINNED: u32 = 0x0010_0000;
const REFRESH_MS: u64 = 2000;

static OBJECTS: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicIsize = AtomicIsize::new(0);
/// Bit n set = drive letter 'A'+n is an InxtDrive mount.
static LETTERS: AtomicU32 = AtomicU32::new(0);
static ENABLED: AtomicU32 = AtomicU32::new(1);
static CHECKED_AT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Synced,
    Syncing,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Re-reads `HKCU\Software\InxtDrive\Mounts` (values like `"I:"`) and the
/// `StatusIcons` switch at most every couple of seconds.
fn refresh_mounts() {
    let now = now_ms();
    let last = CHECKED_AT.load(Ordering::Relaxed);
    if now.saturating_sub(last) < REFRESH_MS
        || CHECKED_AT.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }
    let mut mask = 0u32;
    let mut enabled = 1u32;
    // SAFETY: plain registry reads into stack buffers with correct lengths.
    unsafe {
        let mut key = HKEY::default();
        let sub = wide("Software\\InxtDrive");
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), Some(0), KEY_READ, &mut key).is_ok() {
            let mut val = 0u32;
            let mut len = 4u32;
            let mut ty = REG_VALUE_TYPE::default();
            let name = wide("StatusIcons");
            if RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, Some(&mut ty), Some(&mut val as *mut u32 as *mut u8), Some(&mut len))
                .is_ok()
            {
                enabled = val;
            }
            let _ = RegCloseKey(key);
        }
        let sub = wide("Software\\InxtDrive\\Mounts");
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), Some(0), KEY_READ, &mut key).is_ok() {
            let mut i = 0u32;
            loop {
                let mut name = [0u16; 128];
                let mut name_len = name.len() as u32;
                let mut data = [0u16; 16];
                let mut data_len = (data.len() * 2) as u32;
                let r = RegEnumValueW(
                    key,
                    i,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    Some(data.as_mut_ptr() as *mut u8),
                    Some(&mut data_len),
                );
                if r.is_err() {
                    break;
                }
                let c = (data[0] as u8).to_ascii_uppercase();
                if c.is_ascii_uppercase() && data[1] == ':' as u16 {
                    mask |= 1 << (c - b'A');
                }
                i += 1;
                if i > 64 {
                    break;
                }
            }
            let _ = RegCloseKey(key);
        }
    }
    LETTERS.store(mask, Ordering::Relaxed);
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// The badge decision. `path` is a full Win32 path such as `I:\a\b.txt`.
pub fn applies(kind: Kind, path: &[u16], attrib: u32) -> bool {
    if path.len() < 4 || path[1] != ':' as u16 || path[2] != '\\' as u16 {
        return false;
    }
    let c = (path[0] as u8).to_ascii_uppercase();
    if !c.is_ascii_uppercase() || path[0] > 0x7f {
        return false;
    }
    refresh_mounts();
    if ENABLED.load(Ordering::Relaxed) == 0 || LETTERS.load(Ordering::Relaxed) & (1 << (c - b'A')) == 0 {
        return false;
    }
    let mut a = attrib;
    if a == 0 || a == INVALID_FILE_ATTRIBUTES {
        let mut z: Vec<u16> = path.to_vec();
        z.push(0);
        // SAFETY: NUL-terminated copy of the path.
        a = unsafe { GetFileAttributesW(PCWSTR(z.as_ptr())) };
        if a == INVALID_FILE_ATTRIBUTES {
            return false;
        }
    }
    match kind {
        Kind::Syncing => a & ATTR_ARCHIVE != 0,
        Kind::Synced => a & ATTR_ARCHIVE == 0 && a & (ATTR_PINNED | ATTR_UNPINNED) != 0,
    }
}

fn module_dir() -> &'static [u16] {
    static DIR: OnceLock<Vec<u16>> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut module = HMODULE::default();
        let mut buf = [0u16; MAX_PATH as usize * 2];
        // SAFETY: looks up the module containing this function; buffer sized above.
        let n = unsafe {
            if GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(module_dir as *const () as *const u16),
                &mut module,
            )
            .is_err()
            {
                0
            } else {
                GetModuleFileNameW(Some(module), &mut buf) as usize
            }
        };
        let path = &buf[..n.min(buf.len())];
        let end = path.iter().rposition(|c| *c == '\\' as u16).map(|i| i + 1).unwrap_or(0);
        path[..end].to_vec()
    })
}

#[implement(IShellIconOverlayIdentifier)]
struct Overlay {
    kind: Kind,
}

impl Overlay {
    fn new(kind: Kind) -> Self {
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        Self { kind }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IShellIconOverlayIdentifier_Impl for Overlay_Impl {
    fn IsMemberOf(&self, pwszpath: &PCWSTR, dwattrib: u32) -> windows::core::Result<()> {
        if pwszpath.is_null() {
            return Err(S_FALSE.into());
        }
        // SAFETY: Explorer passes a NUL-terminated path.
        let path = unsafe { pwszpath.as_wide() };
        if applies(self.kind, path, dwattrib) {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn GetOverlayInfo(&self, pwsziconfile: PWSTR, cchmax: i32, pindex: *mut i32, pdwflags: *mut u32) -> windows::core::Result<()> {
        if pwsziconfile.is_null() || pindex.is_null() || pdwflags.is_null() || cchmax <= 0 {
            return Err(E_POINTER.into());
        }
        let file = match self.kind {
            Kind::Synced => "overlay-synced.ico",
            Kind::Syncing => "overlay-syncing.ico",
        };
        let mut full: Vec<u16> = module_dir().to_vec();
        full.extend(file.encode_utf16());
        if full.len() + 1 > cchmax as usize {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: bounds checked against cchmax above.
        unsafe {
            std::ptr::copy_nonoverlapping(full.as_ptr(), pwsziconfile.0, full.len());
            *pwsziconfile.0.add(full.len()) = 0;
            *pindex = 0;
            *pdwflags = ISIOI_ICONFILE | ISIOI_ICONINDEX;
        }
        Ok(())
    }

    fn GetPriority(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
}

#[implement(IClassFactory)]
struct Factory {
    kind: Kind,
}

impl Factory {
    fn new(kind: Kind) -> Self {
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        Self { kind }
    }
}

impl Drop for Factory {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, punkouter: Ref<'_, windows::core::IUnknown>, riid: *const GUID, ppvobject: *mut *mut c_void) -> windows::core::Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null above.
        unsafe { *ppvobject = std::ptr::null_mut() };
        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let obj: IShellIconOverlayIdentifier = Overlay::new(self.kind).into();
        // SAFETY: riid and ppvobject validated above.
        unsafe { obj.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> windows::core::Result<()> {
        if flock.as_bool() {
            LOCKS.fetch_add(1, Ordering::SeqCst);
        } else {
            LOCKS.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// # Safety
/// Called by COM with valid pointers or null.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() || rclsid.is_null() || riid.is_null() {
        return E_POINTER;
    }
    // SAFETY: pointers checked above.
    unsafe {
        *ppv = std::ptr::null_mut();
        let kind = match *rclsid {
            c if c == CLSID_SYNCED => Kind::Synced,
            c if c == CLSID_SYNCING => Kind::Syncing,
            _ => return CLASS_E_CLASSNOTAVAILABLE,
        };
        let factory: IClassFactory = Factory::new(kind).into();
        factory.query(riid, ppv)
    }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::SeqCst) == 0 && LOCKS.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn rejects_paths_off_the_drive_cheaply() {
        LETTERS.store(1 << (b'I' - b'A'), Ordering::Relaxed);
        CHECKED_AT.store(u64::MAX / 2, Ordering::Relaxed);
        assert!(!applies(Kind::Synced, &w("C:\\Windows\\notepad.exe"), ATTR_PINNED));
        assert!(!applies(Kind::Synced, &w("I:\\"), ATTR_PINNED));
        assert!(!applies(Kind::Synced, &w("\\\\server\\share\\x"), ATTR_PINNED));
        assert!(applies(Kind::Synced, &w("I:\\photo.jpg"), ATTR_PINNED));
        assert!(applies(Kind::Synced, &w("i:\\cloud-only.mkv"), ATTR_UNPINNED));
        assert!(!applies(Kind::Synced, &w("I:\\new.txt"), ATTR_ARCHIVE));
        assert!(applies(Kind::Syncing, &w("I:\\new.txt"), ATTR_ARCHIVE));
        assert!(!applies(Kind::Syncing, &w("I:\\desktop.ini"), 0x80 | 0x02));
        assert!(!applies(Kind::Synced, &w("I:\\desktop.ini"), 0x80 | 0x02));
    }
}
