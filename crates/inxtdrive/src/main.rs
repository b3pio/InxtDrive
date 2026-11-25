#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use inxtdrive::ui::message_box;
    use inxtdrive::{config::Config, core::AppCore, ipc, logging, paths, shell};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some(cmd @ ("pin" | "unpin" | "free" | "share")) => {
            let path = args.get(1).cloned().unwrap_or_default();
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            if let Err(e) = rt.block_on(ipc::send(&format!("{cmd} {path}"))) {
                message_box(&format!("InxtDrive is not running.\n\n{e}"), false);
            }
            return;
        }
        Some("--cleanup") => {
            shell::unregister();
            shell::set_mount(None);
            let _ = shell::set_autostart(false);
            return;
        }
        Some("--register-overlays") => {
            let dll = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("inxt_overlay.dll")))
                .unwrap_or_default();
            if shell::register_overlays(&dll).is_err() {
                std::process::exit(1);
            }
            return;
        }
        Some("--unregister-overlays") => {
            shell::unregister_overlays();
            return;
        }
        _ => {}
    }
    let background = args.iter().any(|a| a == "--background");

    let cfg = Config::load();
    let _log = logging::init_file(&cfg.log_level);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "InxtDrive starting");

    let mutex_name = HSTRING::from(format!("Local\\InxtDrive.{}", paths::instance_id()));
    // SAFETY: named mutex used only as a single-instance marker; handle lives until exit.
    let _instance = unsafe { CreateMutexW(None, true, &mutex_name) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let _ = rt.block_on(ipc::send("open"));
        return;
    }

    if !paths::is_dev_profile() && cfg.start_with_windows != shell::autostart_enabled() {
        let _ = shell::set_autostart(cfg.start_with_windows);
    }
    if is_elevated() {
        tracing::warn!("running elevated; the drive may be invisible to File Explorer");
        message_box(
            "InxtDrive is running as administrator, so its drive may not show up in File Explorer.\n\nQuit and start InxtDrive normally (not with \"Run as administrator\").",
            false,
        );
    }

    let core = match AppCore::new(cfg, true) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "startup failed");
            message_box(&format!("InxtDrive could not start:\n\n{e}"), false);
            return;
        }
    };
    core.start();
    std::thread::sleep(std::time::Duration::from_millis(100));
    if !background && matches!(core.snapshot().phase, inxtdrive::core::Phase::SignedOut) {
        core.sign_in();
    }
    inxtdrive::ui::run(core.clone(), !background);
    core.shutdown();
    tracing::info!("InxtDrive stopped");
}

/// Drive letters mounted by an elevated process live in a different logon
/// session than the user's normal Explorer.
#[cfg(windows)]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: queries the current process token into a correctly sized struct.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(target_os = "linux")]
fn main() {
    use inxtdrive::ui::message_box;
    use inxtdrive::{config::Config, core::AppCore, ipc, logging, paths, shell};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt_once = || tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    match args.first().map(String::as_str) {
        Some(cmd @ ("pin" | "unpin" | "free" | "share")) => {
            let path = args.get(1).cloned().unwrap_or_default();
            // The AppImage starts in its own folder; INXT_OWD is where the caller was.
            let base = std::env::var_os("INXT_OWD").map(std::path::PathBuf::from);
            let joined = match base {
                Some(b) if std::path::Path::new(&path).is_relative() => b.join(&path),
                _ => std::path::PathBuf::from(&path),
            };
            let full = std::path::absolute(&joined).map(|p| p.display().to_string()).unwrap_or(path);
            match rt_once().block_on(ipc::send(&format!("{cmd} {full}"))) {
                Ok(r) if r.starts_with("err ") => {
                    message_box(&format!("InxtDrive could not do that:\n\n{}", &r[4..]), false);
                }
                Ok(_) => {}
                Err(e) => {
                    message_box(&format!("InxtDrive is not running.\n\n{e}"), false);
                }
            }
            return;
        }
        Some("--cleanup") => {
            shell::unregister();
            let _ = shell::set_autostart(false);
            return;
        }
        Some("--help" | "-h") => {
            println!("inxtdrive [--background]   start the tray app (see `inxt` for the command line)");
            return;
        }
        _ => {}
    }
    let background = args.iter().any(|a| a == "--background");

    // WebKitGTK's GPU path shows a blank window with NVIDIA's driver.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() && std::path::Path::new("/proc/driver/nvidia").exists() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let cfg = Config::load();
    let _log = logging::init_file(&cfg.log_level);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "InxtDrive starting");

    let _instance = match ipc::InstanceLock::acquire() {
        Ok(Some(l)) => l,
        Ok(None) => {
            let _ = rt_once().block_on(ipc::send("open"));
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "could not take the single-instance lock");
            message_box(&format!("InxtDrive could not start:\n\n{e}"), false);
            return;
        }
    };

    if !paths::is_dev_profile() && cfg.start_with_windows != shell::autostart_enabled() {
        let _ = shell::set_autostart(cfg.start_with_windows);
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        tracing::warn!("running as root");
        message_box(
            "InxtDrive is running as root, so your drive would belong to root and your normal apps could not use it.\n\nQuit and start InxtDrive as your own user.",
            false,
        );
    }

    let core = match AppCore::new(cfg, true) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "startup failed");
            message_box(&format!("InxtDrive could not start:\n\n{e}"), false);
            return;
        }
    };
    core.start();
    std::thread::sleep(std::time::Duration::from_millis(100));
    if !background && matches!(core.snapshot().phase, inxtdrive::core::Phase::SignedOut) {
        core.sign_in();
    }
    {
        // SIGTERM (logout, systemctl, kill) should unmount cleanly like Quit does.
        let c = core.clone();
        core.runtime().spawn(async move {
            if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                term.recv().await;
                c.request_quit();
            }
        });
    }
    inxtdrive::ui::run(core.clone(), !background);
    core.shutdown();
    tracing::info!("InxtDrive stopped");
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("The InxtDrive tray app runs on Windows and Linux; use `inxt` instead.");
}
