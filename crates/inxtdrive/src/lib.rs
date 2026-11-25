//! Application layer: configuration, sign-in, session storage, mount
//! lifecycle and desktop integration. Shared by the tray app (`inxtdrive`)
//! and the command-line tool (`inxt`).

pub mod config;
pub mod core;
pub mod ipc;
pub mod logging;
pub mod login;
pub mod paths;
pub mod secrets;
#[cfg(windows)]
pub mod shell;
#[cfg(target_os = "linux")]
#[path = "shell_linux.rs"]
pub mod shell;
#[cfg(any(windows, target_os = "linux"))]
pub mod ui;

pub const APP_NAME: &str = "InxtDrive";
