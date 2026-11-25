use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Drive letter to mount as; the next free letter is used if it is taken.
    pub drive_letter: String,
    /// Linux: folder the drive appears in (empty = `~/Internxt`).
    pub mount_dir: String,
    pub volume_label: String,
    /// Upper bound for the local copy of cloud files, in GiB.
    pub cache_size_gb: u64,
    /// Folder for the local copy (empty = inside the app data folder).
    pub cache_dir: String,
    pub upload_concurrency: usize,
    /// How often to look for changes made elsewhere, in seconds.
    pub poll_interval_secs: u64,
    pub start_with_windows: bool,
    /// Explorer status badges (synced / syncing).
    pub status_icons: bool,
    /// Flag not-yet-downloaded files as offline so Explorer and antivirus
    /// skip opening them for thumbnails and scans.
    pub mark_cloud_files_offline: bool,
    pub paused: bool,
    pub filesystem_name: String,
    pub log_level: String,
    pub gateway_url: String,
    pub web_url: String,
    pub client_name: String,
}

impl Default for Config {
    fn default() -> Self {
        let api = inxt_api::ClientConfig::default();
        Self {
            drive_letter: "I".into(),
            mount_dir: String::new(),
            volume_label: "Internxt".into(),
            cache_size_gb: 10,
            cache_dir: String::new(),
            upload_concurrency: 4,
            poll_interval_secs: 30,
            start_with_windows: true,
            status_icons: true,
            mark_cloud_files_offline: false,
            paused: false,
            filesystem_name: "exFAT".into(),
            log_level: "info".into(),
            gateway_url: api.gateway_url,
            web_url: api.web_url,
            client_name: api.client_name,
        }
    }
}

const HEADER: &str = "# InxtDrive settings. Easier to change from the app: tray icon > Settings.\n\n";

impl Config {
    pub fn load() -> Self {
        let path = paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "invalid config.toml, using defaults");
                Self::default()
            }),
            Err(_) => {
                let c = Self::default();
                let _ = c.save();
                c
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = paths::config_file();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let body = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}"))?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Clamps values a user could have typed into config.toml.
    pub fn normalized(mut self) -> Self {
        let c = self.drive_letter.trim().chars().next().unwrap_or('I').to_ascii_uppercase();
        self.drive_letter = if ('D'..='Z').contains(&c) { c.to_string() } else { "I".into() };
        self.mount_dir = self.mount_dir.trim().trim_end_matches('/').to_string();
        self.volume_label = self.volume_label.trim().chars().take(32).collect();
        if self.volume_label.is_empty() {
            self.volume_label = "Internxt".into();
        }
        self.cache_size_gb = self.cache_size_gb.clamp(1, 10_000);
        self.upload_concurrency = self.upload_concurrency.clamp(1, 8);
        self.poll_interval_secs = self.poll_interval_secs.clamp(5, 3600);
        if !matches!(self.filesystem_name.as_str(), "exFAT" | "NTFS") {
            self.filesystem_name = "exFAT".into();
        }
        if !matches!(self.log_level.as_str(), "error" | "warn" | "info" | "debug" | "trace") {
            self.log_level = "info".into();
        }
        self
    }

    pub fn api_config(&self) -> inxt_api::ClientConfig {
        inxt_api::ClientConfig {
            gateway_url: self.gateway_url.clone(),
            web_url: self.web_url.clone(),
            client_name: self.client_name.clone(),
            ..Default::default()
        }
    }

    pub fn engine_config(&self, user_uuid: &str) -> inxt_core::EngineConfig {
        let mut e = inxt_core::EngineConfig::new(paths::account_dir(user_uuid));
        e.cache_max_bytes = self.cache_size_gb.max(1) << 30;
        e.upload_concurrency = self.upload_concurrency.clamp(1, 8);
        e.poll_interval = Duration::from_secs(self.poll_interval_secs.clamp(5, 3600));
        e.cache_dir = Some(paths::cache_dir(&self.cache_dir, user_uuid));
        e
    }

    pub fn mount_path(&self) -> std::path::PathBuf {
        if self.mount_dir.is_empty() {
            paths::default_mount_dir()
        } else {
            std::path::PathBuf::from(&self.mount_dir)
        }
    }

    pub fn letter(&self) -> String {
        let c = self.drive_letter.trim().chars().next().unwrap_or('I').to_ascii_uppercase();
        format!("{c}:")
    }
}
