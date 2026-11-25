//! Windows desktop integration, all per-user (HKCU): navigation-pane entry,
//! drive icon, context-menu verbs, start-with-Windows, the mount list the
//! status-badge extension reads, clipboard, folder picker.

use windows::core::{HSTRING, PCWSTR};
use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
use winreg::RegKey;

/// Fixed identifier of the navigation-pane entry.
const CLSID: &str = "{6B1A4E8C-3F0D-4E7A-9C51-2D8B7F4A1E63}";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "InxtDrive";
const APP_KEY: &str = r"Software\InxtDrive";
const VERBS: [(&str, &str, &str); 3] = [
    ("InxtDrive.Share", "Share link…", "share"),
    ("InxtDrive.KeepOffline", "Always keep on this device", "pin"),
    ("InxtDrive.FreeUp", "Free up space", "free"),
];

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

fn hkcu() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

pub fn register(letter: &str, label: &str) -> anyhow::Result<()> {
    let exe = exe();
    let icon = format!("{exe},0");
    let l = letter.trim_end_matches([':', '\\']).to_ascii_uppercase();
    let root = format!("{l}:\\");

    let (drive, _) = hkcu().create_subkey(format!(r"Software\Classes\Applications\Explorer.exe\Drives\{l}\DefaultIcon"))?;
    drive.set_value("", &icon)?;

    let (c, _) = hkcu().create_subkey(format!(r"Software\Classes\CLSID\{CLSID}"))?;
    c.set_value("", &label.to_string())?;
    c.set_value("System.IsPinnedToNameSpaceTree", &1u32)?;
    c.set_value("SortOrderIndex", &0x42u32)?;
    c.create_subkey("DefaultIcon")?.0.set_value("", &icon)?;
    c.create_subkey("InProcServer32")?.0.set_value("", &r"C:\Windows\System32\shell32.dll".to_string())?;
    let (inst, _) = c.create_subkey("Instance")?;
    inst.set_value("CLSID", &"{0E5AAE11-A475-4c5b-AB00-C66DE400274E}".to_string())?;
    let (bag, _) = inst.create_subkey("InitPropertyBag")?;
    bag.set_value("Attributes", &0x11u32)?;
    bag.set_value("TargetFolderPath", &root)?;
    let (sf, _) = c.create_subkey("ShellFolder")?;
    sf.set_value("FolderValueFlags", &0x28u32)?;
    sf.set_value("Attributes", &0xF080_004Du32)?;
    hkcu()
        .create_subkey(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{CLSID}"))?
        .0
        .set_value("", &label.to_string())?;
    hkcu()
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel")?
        .0
        .set_value(CLSID, &1u32)?;

    let applies = format!("System.ItemPathDisplay:~<\"{root}\"");
    for class in ["*", "Directory"] {
        for (key, text, cmd) in VERBS {
            let (v, _) = hkcu().create_subkey(format!(r"Software\Classes\{class}\shell\{key}"))?;
            v.set_value("MUIVerb", &text.to_string())?;
            v.set_value("AppliesTo", &applies)?;
            v.set_value("Icon", &icon)?;
            v.create_subkey("command")?.0.set_value("", &format!("\"{exe}\" {cmd} \"%1\""))?;
        }
    }
    notify_shell();
    Ok(())
}

pub fn unregister() {
    let h = hkcu();
    let _ = h.delete_subkey_all(format!(r"Software\Classes\CLSID\{CLSID}"));
    let _ = h.delete_subkey_all(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{CLSID}"));
    if let Ok(k) = h.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel", KEY_ALL_ACCESS) {
        let _ = k.delete_value(CLSID);
    }
    if let Ok(drives) = h.open_subkey_with_flags(r"Software\Classes\Applications\Explorer.exe\Drives", KEY_ALL_ACCESS) {
        let mine = format!("{},0", exe());
        let letters: Vec<String> = drives.enum_keys().flatten().collect();
        for l in letters {
            let ours = drives
                .open_subkey(format!(r"{l}\DefaultIcon"))
                .and_then(|k| k.get_value::<String, _>(""))
                .is_ok_and(|v| v.eq_ignore_ascii_case(&mine) || v.to_ascii_lowercase().contains("inxtdrive"));
            if ours {
                let _ = drives.delete_subkey_all(&l);
            }
        }
    }
    for class in ["*", "Directory"] {
        for (key, _, _) in VERBS {
            let _ = h.delete_subkey_all(format!(r"Software\Classes\{class}\shell\{key}"));
        }
    }
    notify_shell();
}

/// Publishes (or withdraws) this profile's drive letter for the badge DLL.
pub fn set_mount(letter: Option<&str>) {
    let name = crate::paths::instance_id();
    match letter {
        Some(l) => {
            if let Ok((k, _)) = hkcu().create_subkey(format!(r"{APP_KEY}\Mounts")) {
                let _ = k.set_value(&name, &l.to_string());
            }
        }
        None => {
            if let Ok(k) = hkcu().open_subkey_with_flags(format!(r"{APP_KEY}\Mounts"), KEY_ALL_ACCESS) {
                let _ = k.delete_value(&name);
            }
        }
    }
}

pub fn set_status_icons(on: bool) {
    if let Ok((k, _)) = hkcu().create_subkey(APP_KEY) {
        let _ = k.set_value("StatusIcons", &(on as u32));
    }
}

/// Status-badge handlers: (CLSID, overlay key name, description). Three
/// leading spaces sort the keys after pCloud's and other common ones but
/// before most of OneDrive's, inside the ~11 slots Explorer loads.
const OVERLAYS: [(&str, &str, &str); 2] = [
    ("{E5A7F2B4-1C3D-4F6A-8B9E-0D1C2B3A4F51}", "   InxtDrive1Synced", "InxtDrive status: in the cloud"),
    ("{E5A7F2B4-1C3D-4F6A-8B9E-0D1C2B3A4F52}", "   InxtDrive2Syncing", "InxtDrive status: syncing"),
];
const OVERLAY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers";
const APPROVED_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";

/// Machine-wide registration of the badge DLL. Needs administrator rights
/// (run by the installer).
pub fn register_overlays(dll: &std::path::Path) -> anyhow::Result<()> {
    let hklm = RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    for (clsid, name, desc) in OVERLAYS {
        let (c, _) = hklm.create_subkey(format!(r"Software\Classes\CLSID\{clsid}"))?;
        c.set_value("", &desc.to_string())?;
        let (inproc, _) = c.create_subkey("InprocServer32")?;
        inproc.set_value("", &dll.display().to_string())?;
        inproc.set_value("ThreadingModel", &"Apartment".to_string())?;
        hklm.create_subkey(format!(r"{OVERLAY_KEY}\{name}"))?.0.set_value("", &clsid.to_string())?;
        hklm.create_subkey(APPROVED_KEY)?.0.set_value(clsid, &desc.to_string())?;
    }
    notify_shell();
    Ok(())
}

pub fn unregister_overlays() {
    let hklm = RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    for (clsid, name, _) in OVERLAYS {
        let _ = hklm.delete_subkey_all(format!(r"Software\Classes\CLSID\{clsid}"));
        let _ = hklm.delete_subkey_all(format!(r"{OVERLAY_KEY}\{name}"));
        if let Ok(k) = hklm.open_subkey_with_flags(APPROVED_KEY, KEY_ALL_ACCESS) {
            let _ = k.delete_value(clsid);
        }
    }
    notify_shell();
}

/// Whether the installer registered the status-badge extension.
pub fn status_icons_installed() -> bool {
    RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"Software\Classes\CLSID\{E5A7F2B4-1C3D-4F6A-8B9E-0D1C2B3A4F51}\InprocServer32")
        .is_ok()
}

pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    let (k, _) = hkcu().create_subkey(RUN_KEY)?;
    if on {
        k.set_value(RUN_VALUE, &format!("\"{}\" --background", exe()))?;
    } else {
        let _ = k.delete_value(RUN_VALUE);
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    hkcu().open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>(RUN_VALUE)).is_ok()
}

fn notify_shell() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    // SAFETY: documented broadcast with no item pointers.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

pub fn open(target: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // SAFETY: plain ShellExecute "open" of a path or URL.
    unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL);
    }
}

/// Opens Explorer with the item selected.
pub fn reveal(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{path}")).spawn();
}

/// Restarts Explorer so newly installed status-badge handlers load.
pub fn restart_explorer() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill.exe")
        .args(["/f", "/im", "explorer.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let _ = std::process::Command::new("explorer.exe").spawn();
}

pub fn copy_text(text: &str) -> bool {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: standard clipboard sequence; the allocation is handed to the clipboard on success.
    unsafe {
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                let ok = (|| -> Option<()> {
                    EmptyClipboard().ok()?;
                    let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
                    let p = GlobalLock(mem) as *mut u16;
                    if p.is_null() {
                        return None;
                    }
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    let _ = GlobalUnlock(mem);
                    SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).ok()?;
                    Some(())
                })()
                .is_some();
                let _ = CloseClipboard();
                return ok;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        false
    }
}

/// Native folder picker. `owner` is the parent window handle.
pub fn pick_folder(owner: isize, title: &str) -> Option<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    // SAFETY: COM dialog on the UI thread; returned string freed with CoTaskMemFree.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dlg.SetTitle(&HSTRING::from(title));
        dlg.Show(Some(HWND(owner as *mut core::ffi::c_void))).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        s
    }
}

#[allow(dead_code)]
fn pcwstr(s: &HSTRING) -> PCWSTR {
    PCWSTR(s.as_ptr())
}
