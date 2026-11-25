use std::path::PathBuf;

/// `INXTDRIVE_HOME` puts config and data under one directory (tests, portable use).
fn home_override() -> Option<PathBuf> {
    std::env::var_os("INXTDRIVE_HOME").map(PathBuf::from)
}

/// A profile under `INXTDRIVE_HOME` leaves Explorer integration alone so it
/// can run next to a normal installation.
pub fn is_dev_profile() -> bool {
    home_override().is_some()
}

pub fn config_dir() -> PathBuf {
    home_override()
        .unwrap_or_else(|| dirs::config_dir().unwrap_or_else(std::env::temp_dir).join(crate::APP_NAME))
}

pub fn data_dir() -> PathBuf {
    home_override()
        .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join(crate::APP_NAME))
}

/// Short stable id of this profile, used to keep instances apart.
pub fn instance_id() -> String {
    let key = data_dir().to_string_lossy().to_lowercase();
    let h = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{:08x}", h as u32)
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn session_file() -> PathBuf {
    data_dir().join("session.dat")
}

pub fn account_dir(user_uuid: &str) -> PathBuf {
    data_dir().join("accounts").join(safe(user_uuid))
}

fn safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect()
}

/// Where cached file contents live. A user-chosen base folder gets its own
/// subfolder so InxtDrive never mixes with other files there.
pub fn cache_dir(base: &str, user_uuid: &str) -> PathBuf {
    if base.trim().is_empty() {
        account_dir(user_uuid).join("cache")
    } else {
        PathBuf::from(base.trim()).join("InxtDrive cache").join(safe(user_uuid))
    }
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn webview_dir() -> PathBuf {
    data_dir().join("webview")
}

/// Linux mount folder when none is configured. A dev profile mounts inside
/// its own directory so it never collides with a normal installation.
pub fn default_mount_dir() -> PathBuf {
    match home_override() {
        Some(h) => h.join("Internxt"),
        None => dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Internxt"),
    }
}

/// Per-login directory for the command socket (`$XDG_RUNTIME_DIR`).
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(data_dir)
}
