//! Application state machine (signed out → signing in → indexing → mounted)
//! and the operations the UI, the command pipe and the CLI call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use inxt_api::drive::ItemKind;
use inxt_api::share::ShareLink;
use inxt_api::{Client, Credentials};
use inxt_core::{Engine, Status};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::config::Config;
use crate::{paths, secrets};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    SignedOut,
    SigningIn,
    Indexing,
    /// Reconnecting the drive or moving the local copy after a settings change.
    Busy(String),
    Ready,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    pub status: Option<Status>,
    pub mount_point: Option<String>,
    pub email: Option<String>,
    pub paused: bool,
}

/// Requests from background code (command pipe) to the UI thread.
#[derive(Debug, Clone)]
pub enum UiRequest {
    OpenSettings(Option<String>),
    /// Drive-relative path and full path of the item to share.
    OpenShare { path: String, full: String },
}

type UiHook = Box<dyn Fn(UiRequest) + Send + Sync>;

pub struct AppCore {
    pub cfg: Mutex<Config>,
    rt: tokio::runtime::Runtime,
    client: Arc<Client>,
    phase: Mutex<Phase>,
    engine: Mutex<Option<Arc<Engine>>>,
    #[cfg(windows)]
    drive: Mutex<Option<inxt_winfsp::Drive>>,
    #[cfg(target_os = "linux")]
    drive: Mutex<Option<inxt_fuse::Drive>>,
    mount_point: Mutex<Option<String>>,
    signin: Mutex<Option<tokio::task::AbortHandle>>,
    ipc_started: AtomicBool,
    quit: AtomicBool,
    integrate_shell: bool,
    ui_hook: Mutex<Option<UiHook>>,
    links: Mutex<HashMap<String, ShareLink>>,
    volume: Mutex<(u64, u64)>,
    volume_at: Mutex<Option<std::time::Instant>>,
}

pub fn open_target(target: &str) {
    #[cfg(any(windows, target_os = "linux"))]
    crate::shell::open(target);
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

/// What to hand the file manager to show the top of the drive.
pub fn drive_root(mount_point: &str) -> String {
    if cfg!(windows) {
        format!("{mount_point}\\")
    } else {
        mount_point.to_string()
    }
}

fn kind(is_dir: bool) -> ItemKind {
    if is_dir {
        ItemKind::Folder
    } else {
        ItemKind::File
    }
}

impl AppCore {
    pub fn new(cfg: Config, integrate_shell: bool) -> anyhow::Result<Arc<Self>> {
        let cfg = cfg.normalized();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("inxtdrive-rt")
            .build()?;
        let client = Arc::new(Client::new(cfg.api_config())?);
        client.on_credentials_changed(|c| {
            if let Err(e) = secrets::save_session(c) {
                tracing::error!(error = %e, "could not save the refreshed session");
            }
        });
        Ok(Arc::new(Self {
            cfg: Mutex::new(cfg),
            rt,
            client,
            phase: Mutex::new(Phase::Starting),
            engine: Mutex::new(None),
            #[cfg(any(windows, target_os = "linux"))]
            drive: Mutex::new(None),
            mount_point: Mutex::new(None),
            signin: Mutex::new(None),
            ipc_started: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            integrate_shell: integrate_shell && !paths::is_dev_profile(),
            ui_hook: Mutex::new(None),
            links: Mutex::new(HashMap::new()),
            volume: Mutex::new((0, 0)),
            volume_at: Mutex::new(None),
        }))
    }

    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.rt
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    pub fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.lock().clone()
    }

    pub fn set_ui_hook(&self, f: impl Fn(UiRequest) + Send + Sync + 'static) {
        *self.ui_hook.lock() = Some(Box::new(f));
    }

    fn request_ui(&self, r: UiRequest) -> bool {
        match self.ui_hook.lock().as_ref() {
            Some(h) => {
                h(r);
                true
            }
            None => false,
        }
    }

    fn set_phase(&self, p: Phase) {
        tracing::info!(phase = ?p, "state");
        *self.phase.lock() = p;
    }

    pub fn snapshot(&self) -> Snapshot {
        let engine = self.engine.lock().clone();
        Snapshot {
            phase: self.phase.lock().clone(),
            status: engine.as_ref().map(|e| e.status()),
            mount_point: self.mount_point.lock().clone(),
            email: self.client.credentials().ok().map(|c| c.email.clone()),
            paused: engine.as_ref().is_some_and(|e| e.is_paused()),
        }
    }

    /// Restores the saved session, if any, and brings the drive up.
    pub fn start(self: &Arc<Self>) {
        match secrets::load_session() {
            Some(creds) => {
                let me = self.clone();
                std::thread::spawn(move || me.bring_up(creds));
            }
            None => self.set_phase(Phase::SignedOut),
        }
    }

    /// Opens the browser sign-in. Safe to call repeatedly.
    pub fn sign_in(self: &Arc<Self>) {
        if let Some(h) = self.signin.lock().take() {
            h.abort();
        }
        let had_engine = self.engine.lock().is_some();
        if !had_engine {
            self.set_phase(Phase::SigningIn);
        }
        let me = self.clone();
        let client = self.client.clone();
        let task = self.rt.spawn(async move {
            match crate::login::browser_login(client, open_target).await {
                Ok(creds) => {
                    if let Err(e) = secrets::save_session(&creds) {
                        tracing::error!(error = %e, "could not save session");
                    }
                    let current = me.engine.lock().clone();
                    match current {
                        Some(engine) if same_account(&me, &creds) => {
                            let _ = me.client.set_credentials(Some(creds));
                            engine.reset_auth();
                        }
                        Some(_) => {
                            let me2 = me.clone();
                            std::thread::spawn(move || {
                                me2.tear_down();
                                me2.bring_up(creds);
                            });
                        }
                        None => {
                            let me2 = me.clone();
                            std::thread::spawn(move || me2.bring_up(creds));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sign-in did not complete");
                    if me.engine.lock().is_none() {
                        me.set_phase(Phase::SignedOut);
                    }
                }
            }
        });
        *self.signin.lock() = Some(task.abort_handle());
    }

    fn bring_up(self: &Arc<Self>, creds: Credentials) {
        if let Err(e) = self.try_bring_up(creds) {
            tracing::error!(error = %format!("{e:#}"), "could not start the drive");
            if e.downcast_ref::<inxt_api::ApiError>().is_some_and(|a| a.is_unauthorized()) {
                secrets::clear_session();
                self.set_phase(Phase::SignedOut);
            } else {
                self.set_phase(Phase::Failed(format!("{e:#}")));
            }
        }
    }

    fn try_bring_up(self: &Arc<Self>, creds: Credentials) -> anyhow::Result<()> {
        self.client.set_credentials(Some(creds.clone()))?;
        self.set_phase(Phase::Indexing);
        if let Err(e) = self.rt.block_on(self.client.ensure_fresh(chrono::Duration::days(3))) {
            if e.is_unauthorized() {
                return Err(e.into());
            }
            tracing::warn!(error = %e, "could not refresh the session now; continuing offline");
        }
        let cfg = self.cfg.lock().clone();
        let engine = Engine::open(cfg.engine_config(&creds.user_uuid), self.client.clone(), self.rt.handle().clone())?;
        engine.set_paused(cfg.paused);
        *self.engine.lock() = Some(engine.clone());

        if !engine.initial_sync_done() {
            self.rt.block_on(engine.poll_remote())?;
        }
        engine.start();
        self.mount(&engine, &cfg)?;
        self.start_ipc();
        self.refresh_volume();
        self.set_phase(Phase::Ready);
        Ok(())
    }

    fn refresh_volume(self: &Arc<Self>) {
        *self.volume_at.lock() = Some(std::time::Instant::now());
        let me = self.clone();
        self.rt.spawn(async move {
            let (l, u) = tokio::join!(me.client.limit_bytes(), me.client.usage_bytes());
            if let (Ok(l), Ok(u)) = (l, u) {
                *me.volume.lock() = (l, u);
            }
        });
    }

    #[cfg(windows)]
    fn mount(&self, engine: &Arc<Engine>, cfg: &Config) -> anyhow::Result<()> {
        let serial = self
            .client
            .credentials()
            .map(|c| c.user_uuid.bytes().fold(0x811C_9DC5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193)))
            .unwrap_or(0x1A7E_D21E);
        let opts = inxt_winfsp::MountOptions {
            drive_letter: Some(cfg.letter()),
            volume_label: cfg.volume_label.clone(),
            filesystem_name: cfg.filesystem_name.clone(),
            serial,
            mark_cloud_files_offline: cfg.mark_cloud_files_offline,
        };
        let drive = inxt_winfsp::Drive::mount(engine.clone(), &opts)?;
        let mp = drive.mount_point().to_string();
        crate::shell::set_mount(Some(&mp));
        crate::shell::set_status_icons(cfg.status_icons);
        if self.integrate_shell {
            if let Err(e) = crate::shell::register(&mp, &cfg.volume_label) {
                tracing::warn!(error = %e, "Explorer integration failed");
            }
        }
        *self.mount_point.lock() = Some(mp);
        *self.drive.lock() = Some(drive);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn mount(&self, engine: &Arc<Engine>, cfg: &Config) -> anyhow::Result<()> {
        let opts = inxt_fuse::MountOptions { mount_dir: cfg.mount_path(), fs_name: "inxtdrive".into() };
        let drive = inxt_fuse::Drive::mount(engine.clone(), self.rt.handle().clone(), &opts)?;
        let mp = drive.mount_point().display().to_string();
        if self.integrate_shell {
            if let Err(e) = crate::shell::register(&mp, &cfg.volume_label) {
                tracing::warn!(error = %e, "file manager integration failed");
            }
        }
        *self.mount_point.lock() = Some(mp);
        *self.drive.lock() = Some(drive);
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    fn mount(&self, _engine: &Arc<Engine>, _cfg: &Config) -> anyhow::Result<()> {
        anyhow::bail!("mounting is not implemented on this platform yet")
    }

    fn start_ipc(self: &Arc<Self>) {
        #[cfg(any(windows, unix))]
        if !self.ipc_started.swap(true, Ordering::SeqCst) {
            let me = Arc::downgrade(self);
            self.rt.spawn(async move {
                let res = crate::ipc::serve(move |cmd| match me.upgrade() {
                    Some(core) => core.handle_command(&cmd),
                    None => "err shutting down".into(),
                })
                .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "command pipe stopped");
                }
            });
        }
    }

    /// Converts `I:\folder\file` (or `~/Internxt/folder/file`) into a drive-relative path.
    pub fn drive_path(&self, full: &str) -> Option<String> {
        let mp = self.mount_point.lock().clone()?;
        let full = full.trim().trim_matches('"');
        let head = full.get(..mp.len())?;
        let same = if cfg!(windows) { head.eq_ignore_ascii_case(&mp) } else { head == mp };
        let rest = &full[mp.len()..];
        if !same || !(rest.is_empty() || rest.starts_with(['\\', '/']) || mp.ends_with(':')) {
            return None;
        }
        Some(if rest.is_empty() { "\\".into() } else { rest.replace('/', "\\") })
    }

    pub fn full_path(&self, drive_path: &str) -> Option<String> {
        let mp = self.mount_point.lock().clone()?;
        Some(if cfg!(windows) { format!("{mp}{drive_path}") } else { format!("{mp}{}", drive_path.replace('\\', "/")) })
    }

    pub fn handle_command(&self, line: &str) -> String {
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        let engine = self.engine.lock().clone();
        match (cmd, engine) {
            ("status", _) => {
                let s = self.snapshot();
                format!("ok {}", json!({
                    "phase": format!("{:?}", s.phase),
                    "mount": s.mount_point,
                    "pending_uploads": s.status.as_ref().map(|x| x.pending_uploads),
                    "uploading": s.status.as_ref().map(|x| x.uploading),
                    "items": s.status.as_ref().map(|x| x.items),
                    "cache_bytes": s.status.as_ref().map(|x| x.cache_bytes),
                    "online": s.status.as_ref().map(|x| x.online),
                    "last_error": s.status.as_ref().and_then(|x| x.last_error.clone()),
                }))
            }
            ("open", _) => {
                if !self.request_ui(UiRequest::OpenSettings(None)) {
                    if let Some(mp) = self.mount_point.lock().clone() {
                        open_target(&drive_root(&mp));
                    }
                }
                "ok".into()
            }
            ("settings", _) => {
                self.request_ui(UiRequest::OpenSettings((!arg.is_empty()).then(|| arg.to_string())));
                "ok".into()
            }
            ("quit", _) => {
                self.quit.store(true, Ordering::SeqCst);
                "ok".into()
            }
            ("share", Some(_)) => match self.drive_path(arg) {
                Some(p) => {
                    let full = arg.trim().trim_matches('"').to_string();
                    if self.request_ui(UiRequest::OpenShare { path: p, full }) {
                        "ok".into()
                    } else {
                        "err no window available".into()
                    }
                }
                None => "err path is not on the drive".into(),
            },
            ("pin" | "unpin", Some(e)) => match self.drive_path(arg) {
                Some(p) => match e.set_pinned(&p, cmd == "pin") {
                    Ok(n) => format!("ok {n} items"),
                    Err(err) => format!("err {err:?}"),
                },
                None => "err path is not on the drive".into(),
            },
            ("free", Some(e)) => match self.drive_path(arg) {
                Some(p) => {
                    let _ = e.set_pinned(&p, false);
                    match e.free_up(&p) {
                        Ok(b) => format!("ok freed {b} bytes"),
                        Err(err) => format!("err {err:?}"),
                    }
                }
                None => "err path is not on the drive".into(),
            },
            (_, None) => "err drive is not ready".into(),
            _ => "err unknown command".into(),
        }
    }

    /// Set by the `quit` command (used by the installer before upgrading).
    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(e) = self.engine.lock().clone() {
            e.set_paused(paused);
        }
        let mut cfg = self.cfg.lock();
        cfg.paused = paused;
        let _ = cfg.save();
    }

    pub fn free_up_all(&self) -> u64 {
        let engine = self.engine.lock().clone();
        engine.and_then(|e| e.free_up("\\").ok()).unwrap_or(0)
    }

    pub fn pending_changes(&self) -> usize {
        self.engine
            .lock()
            .as_ref()
            .map(|e| {
                let s = e.status();
                s.pending_uploads + s.pending_ops
            })
            .unwrap_or(0)
    }

    // ---- settings ------------------------------------------------------------------

    /// Everything the settings window shows that does not change every second.
    pub fn settings_json(&self) -> Value {
        let cfg = self.cfg.lock().clone();
        #[cfg(windows)]
        let (letters, icons_installed, autostart) = {
            let mount = self.mount_point.lock().clone();
            let current = cfg.letter().chars().next().unwrap_or('I');
            let used = used_letters();
            let mounted = mount.as_deref().and_then(|m| m.chars().next());
            let free: Vec<String> = ('D'..='Z')
                .filter(|c| used & (1 << (*c as u8 - b'A')) == 0 || Some(*c) == mounted || *c == current)
                .map(|c| c.to_string())
                .collect();
            (free, crate::shell::status_icons_installed(), crate::shell::autostart_enabled())
        };
        #[cfg(target_os = "linux")]
        let (letters, icons_installed, autostart): (Vec<String>, bool, bool) = (vec![], false, crate::shell::autostart_enabled());
        #[cfg(not(any(windows, target_os = "linux")))]
        let (letters, icons_installed, autostart): (Vec<String>, bool, bool) = (vec![], false, false);
        let creds = self.client.credentials().ok();
        json!({
            "settings": cfg,
            "letters": letters,
            "autostart": autostart,
            "status_icons_installed": icons_installed,
            "dev_profile": paths::is_dev_profile(),
            "platform": if cfg!(windows) { "windows" } else { "linux" },
            "default_mount_dir": paths::default_mount_dir().display().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
            "email": creds.as_ref().map(|c| c.email.clone()),
            "default_cache_dir": creds.as_ref().map(|c| paths::cache_dir("", &c.user_uuid).display().to_string()),
            "paths": {
                "settings": paths::config_file().display().to_string(),
                "logs": paths::logs_dir().display().to_string(),
            },
        })
    }

    /// Fast-changing state for the overview: status, activity, usage.
    pub fn status_json(self: &Arc<Self>) -> Value {
        let stale = self.volume_at.lock().is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(60));
        if stale && self.client.credentials().is_ok() && matches!(*self.phase.lock(), Phase::Ready) {
            self.refresh_volume();
        }
        let s = self.snapshot();
        let engine = self.engine.lock().clone();
        let activity = engine.as_ref().map(|e| e.activity());
        let (limit, used) = *self.volume.lock();
        let phase = match &s.phase {
            Phase::Starting => json!({ "kind": "starting" }),
            Phase::SignedOut => json!({ "kind": "signed_out" }),
            Phase::SigningIn => json!({ "kind": "signing_in" }),
            Phase::Indexing => json!({ "kind": "indexing" }),
            Phase::Busy(m) => json!({ "kind": "busy", "message": m }),
            Phase::Ready => json!({ "kind": "ready" }),
            Phase::Failed(m) => json!({ "kind": "failed", "message": m }),
        };
        json!({
            "phase": phase,
            "mount": s.mount_point,
            "email": s.email,
            "paused": s.paused,
            "status": s.status.as_ref().map(|st| json!({
                "online": st.online,
                "auth_failed": st.auth_failed,
                "items": st.items,
                "pending_uploads": st.pending_uploads,
                "pending_ops": st.pending_ops,
                "uploading": st.uploading,
                "upload_bytes_total": st.upload_bytes_total,
                "upload_bytes_done": st.upload_bytes_done,
                "downloads_active": st.downloads_active,
                "cache_bytes": st.cache_bytes,
                "last_error": st.last_error,
            })),
            "activity": activity,
            "usage": { "used": used, "limit": limit },
            "cache_limit": self.cfg.lock().cache_size_gb << 30,
        })
    }

    /// Saves settings and applies them, reconnecting the drive if needed.
    /// Returns a short message for the user.
    pub fn apply_settings(self: &Arc<Self>, new: Config) -> anyhow::Result<String> {
        let new = new.normalized();
        let old = self.cfg.lock().clone();
        if new == old {
            return Ok("No changes".into());
        }
        new.save()?;
        *self.cfg.lock() = new.clone();

        #[cfg(windows)]
        {
            if new.start_with_windows != crate::shell::autostart_enabled() {
                crate::shell::set_autostart(new.start_with_windows)?;
            }
            crate::shell::set_status_icons(new.status_icons);
        }
        #[cfg(target_os = "linux")]
        if !paths::is_dev_profile() && new.start_with_windows != crate::shell::autostart_enabled() {
            crate::shell::set_autostart(new.start_with_windows)?;
        }
        if let Some(e) = self.engine.lock().clone() {
            e.set_cache_limit(new.cache_size_gb << 30);
            e.set_upload_concurrency(new.upload_concurrency);
            e.set_poll_interval(std::time::Duration::from_secs(new.poll_interval_secs));
            e.set_paused(new.paused);
        }

        let needs_restart = new.gateway_url != old.gateway_url || new.web_url != old.web_url || new.client_name != old.client_name;
        let needs_remount = new.drive_letter != old.drive_letter
            || new.mount_dir != old.mount_dir
            || new.volume_label != old.volume_label
            || new.filesystem_name != old.filesystem_name
            || new.mark_cloud_files_offline != old.mark_cloud_files_offline;
        let moves_cache = new.cache_dir.trim() != old.cache_dir.trim();
        if new.log_level != old.log_level {
            tracing::info!(level = %new.log_level, "log level applies after restart");
        }

        if (needs_remount || moves_cache) && self.engine.lock().is_some() {
            let creds = self.client.credentials()?;
            let me = self.clone();
            let creds = (*creds).clone();
            std::thread::spawn(move || {
                me.set_phase(Phase::Busy(if moves_cache { "Moving the local copy…".into() } else { "Reconnecting the drive…".into() }));
                me.tear_down();
                if moves_cache {
                    let from = paths::cache_dir(&old.cache_dir, &creds.user_uuid);
                    let to = paths::cache_dir(&new.cache_dir, &creds.user_uuid);
                    if let Err(e) = move_dir(&from, &to) {
                        tracing::error!(error = %e, "moving the local copy failed; keeping the old location");
                        let mut cfg = me.cfg.lock();
                        cfg.cache_dir = old.cache_dir.clone();
                        let _ = cfg.save();
                    }
                }
                me.bring_up(creds);
            });
        }
        Ok(if needs_restart {
            "Saved. Restart InxtDrive to use the new connection settings.".into()
        } else if moves_cache {
            "Saved. Moving the local copy; the drive reconnects when done.".into()
        } else if needs_remount {
            "Saved. The drive is reconnecting.".into()
        } else {
            "Saved.".into()
        })
    }

    // ---- sharing -------------------------------------------------------------------

    fn remember(&self, uuid: &str, link: &ShareLink) {
        self.links.lock().insert(uuid.to_string(), link.clone());
    }

    /// Creates or reuses a public link for an item on the drive. `Ok(None)`
    /// means it has not finished uploading yet.
    pub async fn share(&self, drive_path: &str) -> anyhow::Result<Option<Value>> {
        let engine = self.engine().ok_or_else(|| anyhow::anyhow!("the drive is not ready"))?;
        let remote = engine.remote_id(drive_path).map_err(|e| match e {
            inxt_core::FsError::AccessDenied => anyhow::anyhow!("this item stays on this PC and can't be shared"),
            _ => anyhow::anyhow!("item not found"),
        })?;
        let Some((uuid, is_dir)) = remote else { return Ok(None) };
        let link = match self.client.existing_public_link(&uuid, kind(is_dir)).await? {
            Some(l) => l,
            None => self.client.create_public_link(&uuid, kind(is_dir)).await?,
        };
        self.remember(&uuid, &link);
        Ok(Some(json!({ "uuid": uuid, "is_dir": is_dir, "url": link.url, "has_password": link.has_password })))
    }

    pub async fn share_password(&self, uuid: &str, password: Option<&str>) -> anyhow::Result<()> {
        let link = self.links.lock().get(uuid).cloned().ok_or_else(|| anyhow::anyhow!("open the link first"))?;
        self.client.set_share_password(&link, password).await?;
        if let Some(l) = self.links.lock().get_mut(uuid) {
            l.has_password = password.is_some_and(|p| !p.is_empty());
        }
        Ok(())
    }

    pub async fn stop_sharing(&self, uuid: &str, is_dir: bool) -> anyhow::Result<()> {
        self.client.stop_sharing(uuid, kind(is_dir)).await?;
        self.links.lock().remove(uuid);
        Ok(())
    }

    pub async fn shared_items(&self) -> anyhow::Result<Value> {
        let items = self.client.list_shared().await?;
        let engine = self.engine();
        let rows: Vec<Value> = items
            .into_iter()
            .map(|i| {
                let local = engine.as_ref().and_then(|e| e.path_of_uuid(&i.uuid)).and_then(|p| self.full_path(&p));
                json!({
                    "uuid": i.uuid, "name": i.name, "is_dir": i.is_dir, "size": i.size,
                    "shared_at": i.shared_at, "shared_with_me": i.shared_with_me, "owner": i.owner, "local": local,
                })
            })
            .collect();
        Ok(json!(rows))
    }

    pub async fn link_for(&self, uuid: &str, is_dir: bool) -> anyhow::Result<Option<String>> {
        let link = self.client.existing_public_link(uuid, kind(is_dir)).await?;
        if let Some(l) = &link {
            self.remember(uuid, l);
        }
        Ok(link.map(|l| l.url))
    }

    // ---- lifecycle -----------------------------------------------------------------

    /// Unmounts and stops the engine, keeping local data for next start.
    fn tear_down(&self) {
        #[cfg(windows)]
        if let Some(d) = self.drive.lock().take() {
            d.unmount();
            crate::shell::set_mount(None);
            if self.integrate_shell {
                crate::shell::unregister();
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(d) = self.drive.lock().take() {
            d.unmount();
            if self.integrate_shell {
                crate::shell::unregister();
            }
        }
        *self.mount_point.lock() = None;
        if let Some(e) = self.engine.lock().take() {
            e.shutdown();
        }
    }

    /// Signs out and deletes this account's local data (cache included).
    pub fn sign_out(&self) {
        let creds = self.client.credentials().ok();
        let cache_base = self.cfg.lock().cache_dir.clone();
        self.tear_down();
        self.rt.block_on(self.client.logout());
        secrets::clear_session();
        if let Some(c) = creds {
            let _ = std::fs::remove_dir_all(paths::cache_dir(&cache_base, &c.user_uuid));
            if let Err(e) = std::fs::remove_dir_all(paths::account_dir(&c.user_uuid)) {
                tracing::warn!(error = %e, "could not remove local data");
            }
        }
        self.links.lock().clear();
        self.set_phase(Phase::SignedOut);
    }

    pub fn shutdown(&self) {
        if let Some(h) = self.signin.lock().take() {
            h.abort();
        }
        self.tear_down();
    }
}

fn same_account(core: &AppCore, creds: &Credentials) -> bool {
    core.client.credentials().is_ok_and(|c| c.user_uuid == creds.user_uuid)
}

#[cfg(windows)]
fn used_letters() -> u32 {
    // SAFETY: no arguments; returns a bitmask of existing drive letters.
    unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() }
}

/// Moves a directory tree, across volumes if needed.
fn move_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if !from.exists() || from == to {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}
