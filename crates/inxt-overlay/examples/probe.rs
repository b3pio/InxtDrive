//! Loads the badge DLL like Explorer does and asks both handlers about paths.
//! Usage: probe <path\to\inxt_overlay.dll> <file-or-folder>...
//! Windows only; elsewhere it builds to a stub so `cargo test --workspace` works.

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    win::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("probe only runs on Windows");
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use inxt_overlay::{CLSID_SYNCED, CLSID_SYNCING};
    use windows::core::{s, Interface, GUID, HRESULT, HSTRING, PCWSTR};
    use windows::Win32::Storage::FileSystem::GetFileAttributesW;
    use windows::Win32::System::Com::IClassFactory;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Shell::IShellIconOverlayIdentifier;

    type GetClassObject = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

    pub fn main() -> windows::core::Result<()> {
        let mut args = std::env::args().skip(1);
        let dll = args.next().expect("dll path");
        let paths: Vec<String> = args.collect();
        // SAFETY: test harness calling the DLL's documented COM entry points.
        unsafe {
            let module = LoadLibraryW(&HSTRING::from(dll))?;
            let get: GetClassObject = std::mem::transmute(GetProcAddress(module, s!("DllGetClassObject")).expect("export"));
            for (name, clsid) in [("synced ", CLSID_SYNCED), ("syncing", CLSID_SYNCING)] {
                let mut raw = std::ptr::null_mut();
                get(&clsid, &IClassFactory::IID, &mut raw).ok()?;
                let factory = IClassFactory::from_raw(raw);
                let overlay: IShellIconOverlayIdentifier = factory.CreateInstance(None)?;
                let mut icon = [0u16; 520];
                let (mut index, mut flags) = (0i32, 0u32);
                overlay.GetOverlayInfo(&mut icon, &mut index, &mut flags)?;
                let len = icon.iter().position(|c| *c == 0).unwrap_or(icon.len());
                println!("[{name}] icon {} priority {}", String::from_utf16_lossy(&icon[..len]), overlay.GetPriority()?);
                for p in &paths {
                    let h = HSTRING::from(p.as_str());
                    let attr = GetFileAttributesW(PCWSTR(h.as_ptr()));
                    // Explorer shows the badge only for S_OK; S_FALSE means "not mine".
                    let hr = (Interface::vtable(&overlay).IsMemberOf)(Interface::as_raw(&overlay), PCWSTR(h.as_ptr()), attr);
                    let member = hr == windows::Win32::Foundation::S_OK;
                    println!("  {:<9} attr {:#010x}  {p}", if member { "BADGE" } else { "-" }, attr);
                }
            }
        }
        Ok(())
    }
}
