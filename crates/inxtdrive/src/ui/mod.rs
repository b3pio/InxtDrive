//! Tray icon plus the settings and share windows (WebView2 pages).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::dpi::LogicalSize;
#[cfg(windows)]
use tao::dpi::PhysicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::run_return::EventLoopExtRunReturn;
#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
#[cfg(windows)]
use tao::platform::windows::{IconExtWindows, WindowExtWindows};
use tao::window::{Icon as WindowIcon, Theme, Window, WindowBuilder, WindowId};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
#[cfg(windows)]
use windows::core::{w, HSTRING, PCWSTR};
#[cfg(windows)]
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetSystemMetrics, MessageBoxW, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, SM_CXSMICON,
};
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;
use wry::{WebContext, WebView, WebViewBuilder};

use crate::config::Config;
use crate::core::{drive_root, open_target, AppCore, Phase, Snapshot, UiRequest};
use crate::{paths, shell};

const APP_HTML: &str = include_str!("app.html");

#[cfg(windows)]
const THIS_DEVICE: &str = "this PC";
#[cfg(not(windows))]
const THIS_DEVICE: &str = "this computer";

#[cfg(windows)]
pub fn message_box(text: &str, ask: bool) -> bool {
    let style = if ask { MB_YESNO | MB_ICONWARNING } else { MB_OK | MB_ICONINFORMATION };
    // SAFETY: modal message box with owned strings.
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("InxtDrive"), style) == IDYES }
}

#[cfg(target_os = "linux")]
fn gtk_dialog(text: &str, ask: bool) -> bool {
    use gtk::prelude::*;
    let (kind, buttons) = if ask {
        (gtk::MessageType::Warning, gtk::ButtonsType::YesNo)
    } else {
        (gtk::MessageType::Info, gtk::ButtonsType::Ok)
    };
    let d = gtk::MessageDialog::new(None::<&gtk::Window>, gtk::DialogFlags::MODAL, kind, buttons, text);
    d.set_title("InxtDrive");
    let answer = d.run();
    // SAFETY: the dialog is ours and not used after this.
    unsafe { d.destroy() };
    answer == gtk::ResponseType::Yes
}

/// Asking needs the UI thread; from other threads the message is shown
/// without waiting and the answer is "no".
#[cfg(target_os = "linux")]
pub fn message_box(text: &str, ask: bool) -> bool {
    if gtk::is_initialized_main_thread() {
        return gtk_dialog(text, ask);
    }
    if !gtk::is_initialized() {
        if gtk::init().is_ok() {
            return gtk_dialog(text, ask);
        }
        eprintln!("{text}");
        return false;
    }
    let text = text.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        gtk_dialog(&text, false);
    });
    false
}

/// `~/Internxt` instead of the full home path, for menus and titles.
pub fn pretty_mount(mp: &str) -> String {
    if cfg!(windows) {
        return format!("Drive {mp}");
    }
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if mp.starts_with(&h) && mp[h.len()..].starts_with('/') => format!("~{}", &mp[h.len()..]),
        _ => mp.to_string(),
    }
}

fn base_name(full: &str) -> String {
    full.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(full).to_string()
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn headline(s: &Snapshot) -> String {
    match &s.phase {
        Phase::Starting => "Starting…".into(),
        Phase::SignedOut => "Not signed in".into(),
        Phase::SigningIn => "Waiting for sign-in in your browser…".into(),
        Phase::Indexing => match &s.status {
            Some(st) if st.items > 1 => format!("Preparing your drive… ({} items)", st.items),
            _ => "Preparing your drive…".into(),
        },
        Phase::Busy(m) => m.clone(),
        Phase::Failed(e) => {
            let short: String = e.chars().take(80).collect();
            format!("Problem: {short}")
        }
        Phase::Ready => {
            let Some(st) = &s.status else { return "Ready".into() };
            if st.auth_failed {
                "Session expired - sign in again".into()
            } else if s.paused {
                "Syncing paused".into()
            } else if !st.online {
                "Offline - changes will sync when back online".into()
            } else if st.uploading > 0 {
                let pct = (st.upload_bytes_done * 100).checked_div(st.upload_bytes_total).unwrap_or(0).min(100);
                format!("Uploading {} - {pct}%", plural(st.uploading, "file", "files"))
            } else if st.pending_uploads + st.pending_ops > 0 {
                format!("{} waiting to sync", plural(st.pending_uploads + st.pending_ops, "change", "changes"))
            } else if st.downloads_active > 0 {
                "Downloading…".into()
            } else {
                "Up to date".into()
            }
        }
    }
}

pub enum UiEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Ipc(WindowId, String),
    Eval(WindowId, String),
    Request(UiRequest),
}

enum ViewKind {
    Settings,
    Share { path: String, full: String },
}

struct View {
    window: Window,
    webview: WebView,
    kind: ViewKind,
}

struct TrayMenu {
    status: MenuItem,
    detail: MenuItem,
    open: MenuItem,
    pause: CheckMenuItem,
    free: MenuItem,
    settings: MenuItem,
    account: MenuItem,
    quit: MenuItem,
    last: (String, String, String, bool),
}

struct Ui {
    core: Arc<AppCore>,
    proxy: EventLoopProxy<UiEvent>,
    tray: Option<TrayIcon>,
    menu: Option<TrayMenu>,
    views: HashMap<WindowId, View>,
    settings_id: Option<WindowId>,
    context: WebContext,
    next_tick: Instant,
    exit: bool,
}

#[cfg(windows)]
fn app_icon(size: u32) -> Option<WindowIcon> {
    WindowIcon::from_resource(1, Some(PhysicalSize::new(size, size))).ok()
}

#[cfg(windows)]
fn tray_icon() -> Option<Icon> {
    let px = tray_icon_px();
    Icon::from_resource(1, Some((px, px))).ok()
}

#[cfg(target_os = "linux")]
const ICON_PNG: &[u8] = include_bytes!("../../../../assets/inxtdrive-256.png");

/// Decodes the bundled 256 px icon to RGBA.
#[cfg(target_os = "linux")]
fn icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((rgba, info.width, info.height))
}

#[cfg(target_os = "linux")]
fn app_icon(_size: u32) -> Option<WindowIcon> {
    let (rgba, w, h) = icon_rgba()?;
    WindowIcon::from_rgba(rgba, w, h).ok()
}

#[cfg(target_os = "linux")]
fn tray_icon() -> Option<Icon> {
    let (rgba, w, h) = icon_rgba()?;
    Icon::from_rgba(rgba, w, h).ok()
}

/// The notification area draws icons at the small-icon size of the taskbar's
/// monitor. Handing it exactly that size keeps Windows from rescaling the art.
#[cfg(windows)]
fn tray_icon_px() -> u32 {
    // SAFETY: handle and metric queries only; the class name is a static string.
    let px = unsafe {
        match FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).map(|taskbar| GetDpiForWindow(taskbar)) {
            Ok(dpi) if dpi > 0 => GetSystemMetricsForDpi(SM_CXSMICON, dpi),
            _ => GetSystemMetrics(SM_CXSMICON),
        }
    };
    px.max(16) as u32
}

fn js_call(f: &str, arg: &Value) -> String {
    format!("window.inxt && window.inxt.{f}({});", serde_json::to_string(arg).unwrap_or_else(|_| "null".into()))
}

pub fn run(core: Arc<AppCore>, open_window: bool) {
    let mut event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UiEvent::Menu(e));
        }));
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UiEvent::Tray(e));
        }));
        let p = proxy.clone();
        core.set_ui_hook(move |r| {
            let _ = p.send_event(UiEvent::Request(r));
        });
    }
    let _ = std::fs::create_dir_all(paths::webview_dir());
    let mut ui = Ui {
        core,
        proxy,
        tray: None,
        menu: None,
        views: HashMap::new(),
        settings_id: None,
        context: WebContext::new(Some(paths::webview_dir())),
        next_tick: Instant::now(),
        exit: false,
    };
    event_loop.run_return(move |event, target, control_flow| {
        match event {
            Event::NewEvents(StartCause::Init) => {
                ui.build_tray();
                if open_window {
                    ui.open_settings(target, None);
                }
            }
            Event::UserEvent(e) => ui.handle(e, target),
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => ui.close(window_id),
            _ => {}
        }
        let now = Instant::now();
        if now >= ui.next_tick {
            ui.tick();
            ui.next_tick = now + Duration::from_millis(1000);
            if ui.core.quit_requested() {
                ui.exit = true;
            }
        }
        *control_flow = if ui.exit { ControlFlow::Exit } else { ControlFlow::WaitUntil(ui.next_tick) };
    });
}

impl Ui {
    fn build_tray(&mut self) {
        let m = TrayMenu {
            status: MenuItem::new("Starting…", false, None),
            detail: MenuItem::new("", false, None),
            open: MenuItem::new("Open drive", true, None),
            pause: CheckMenuItem::new("Pause syncing", true, self.core.cfg.lock().paused, None),
            free: MenuItem::new(format!("Free up space on {THIS_DEVICE}"), true, None),
            settings: MenuItem::new("Settings…", true, None),
            account: MenuItem::new("Sign in…", true, None),
            quit: MenuItem::new("Quit InxtDrive", true, None),
            last: Default::default(),
        };
        let menu = Menu::new();
        let _ = menu.append_items(&[
            &m.status,
            &m.detail,
            &PredefinedMenuItem::separator(),
            &m.open,
            &PredefinedMenuItem::separator(),
            &m.pause,
            &m.free,
            &PredefinedMenuItem::separator(),
            &m.settings,
            &m.account,
            &PredefinedMenuItem::separator(),
            &m.quit,
        ]);
        let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("InxtDrive").with_menu_on_left_click(false);
        if let Some(i) = tray_icon() {
            builder = builder.with_icon(i);
        }
        #[cfg(target_os = "linux")]
        {
            // The indicator writes the icon to a file; keep it out of /tmp clutter.
            builder = builder.with_temp_dir_path(paths::data_dir().join("tray"));
        }
        match builder.build() {
            Ok(t) => self.tray = Some(t),
            Err(e) => tracing::error!(error = %e, "could not create the tray icon"),
        }
        self.menu = Some(m);
    }

    fn tick(&mut self) {
        let s = self.core.snapshot();
        if let (Some(m), Some(tray)) = (self.menu.as_mut(), self.tray.as_ref()) {
            let head = headline(&s);
            let det = match (&s.mount_point, &s.status) {
                (Some(mp), Some(st)) => format!("{} · {} on {THIS_DEVICE}", pretty_mount(mp), human_bytes(st.cache_bytes)),
                (Some(mp), None) => pretty_mount(mp),
                _ => s.email.clone().unwrap_or_else(|| "Internxt cloud drive".into()),
            };
            let auth_failed = s.status.as_ref().is_some_and(|x| x.auth_failed);
            let signed_in = matches!(s.phase, Phase::Ready | Phase::Indexing | Phase::Busy(_)) && !auth_failed;
            let account = if signed_in {
                format!("Sign out ({})", s.email.clone().unwrap_or_default())
            } else if auth_failed {
                "Sign in again…".into()
            } else {
                "Sign in…".into()
            };
            let next = (head.clone(), det.clone(), account.clone(), s.paused);
            if next != m.last {
                m.status.set_text(&head);
                m.detail.set_text(&det);
                m.account.set_text(&account);
                m.pause.set_checked(s.paused);
                let ready = s.mount_point.is_some();
                m.open.set_enabled(ready);
                m.free.set_enabled(ready);
                m.pause.set_enabled(ready);
                let mut tip = format!("InxtDrive - {head}");
                tip.truncate(120);
                let _ = tray.set_tooltip(Some(tip));
                m.last = next;
            }
        }
        let ids: Vec<WindowId> = self.views.iter().filter(|(_, v)| matches!(v.kind, ViewKind::Settings)).map(|(id, _)| *id).collect();
        if !ids.is_empty() {
            let status = self.core.status_json();
            for id in ids {
                self.eval(id, &js_call("status", &status));
            }
        }
    }

    fn eval(&self, id: WindowId, js: &str) {
        if let Some(v) = self.views.get(&id) {
            let _ = v.webview.evaluate_script(js);
        }
    }

    fn post_later(&self, id: WindowId, f: &'static str, arg: Value) {
        let _ = self.proxy.send_event(UiEvent::Eval(id, js_call(f, &arg)));
    }

    fn handle(&mut self, e: UiEvent, target: &EventLoopWindowTarget<UiEvent>) {
        match e {
            UiEvent::Menu(ev) => self.on_menu(ev.id(), target),
            UiEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) => {
                match self.core.snapshot().mount_point {
                    Some(mp) => open_target(&drive_root(&mp)),
                    None => self.open_settings(target, None),
                }
            }
            UiEvent::Tray(_) => {}
            UiEvent::Eval(id, js) => self.eval(id, &js),
            UiEvent::Ipc(id, body) => self.on_ipc(id, &body, target),
            UiEvent::Request(UiRequest::OpenSettings(tab)) => self.open_settings(target, tab),
            UiEvent::Request(UiRequest::OpenShare { path, full }) => self.open_share(target, path, full),
        }
    }

    fn on_menu(&mut self, id: &MenuId, target: &EventLoopWindowTarget<UiEvent>) {
        let Some(m) = self.menu.as_ref() else { return };
        if id == m.open.id() {
            if let Some(mp) = self.core.snapshot().mount_point {
                open_target(&drive_root(&mp));
            }
        } else if id == m.settings.id() {
            self.open_settings(target, Some("overview".into()));
        } else if id == m.pause.id() {
            self.core.set_paused(m.pause.is_checked());
        } else if id == m.free.id() {
            let c = self.core.clone();
            std::thread::spawn(move || {
                let freed = c.free_up_all();
                message_box(&format!("Freed {} on {THIS_DEVICE}. Files stay available in the cloud.", human_bytes(freed)), false);
            });
        } else if id == m.account.id() {
            let s = self.core.snapshot();
            let auth_failed = s.status.as_ref().is_some_and(|x| x.auth_failed);
            if matches!(s.phase, Phase::Ready | Phase::Indexing | Phase::Busy(_)) && !auth_failed {
                self.open_settings(target, Some("account".into()));
            } else {
                self.core.sign_in();
            }
        } else if id == m.quit.id() {
            let pending = self.core.pending_changes();
            if pending == 0
                || message_box(
                    &format!(
                        "{} still waiting to upload. They will continue next time InxtDrive starts.\n\nQuit now?",
                        plural(pending, "change is", "changes are")
                    ),
                    true,
                )
            {
                self.exit = true;
            }
        }
    }

    fn window_bg(&self, target: &EventLoopWindowTarget<UiEvent>) -> (u8, u8, u8, u8) {
        let _ = target;
        let dark = self.views.values().next().map(|v| v.window.theme() == Theme::Dark).unwrap_or_else(system_dark);
        if dark {
            (16, 18, 22, 255)
        } else {
            (245, 246, 248, 255)
        }
    }

    fn create_view(
        &mut self,
        target: &EventLoopWindowTarget<UiEvent>,
        title: &str,
        size: (f64, f64),
        min: (f64, f64),
        init: String,
        kind: ViewKind,
    ) -> Option<WindowId> {
        let window = WindowBuilder::new()
            .with_title(title)
            .with_inner_size(LogicalSize::new(size.0, size.1))
            .with_min_inner_size(LogicalSize::new(min.0, min.1))
            .with_window_icon(app_icon(32))
            .build(target)
            .map_err(|e| tracing::error!(error = %e, "could not open window"))
            .ok()?;
        let id = window.id();
        let proxy = self.proxy.clone();
        let bg = self.window_bg(target);
        let builder = WebViewBuilder::new_with_web_context(&mut self.context)
            .with_html(APP_HTML)
            .with_initialization_script(init)
            .with_background_color(bg)
            .with_devtools(cfg!(debug_assertions))
            .with_navigation_handler(|url| url.starts_with("data:") || url.starts_with("about:") || url.starts_with("http://localhost"))
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UiEvent::Ipc(id, req.into_body()));
            });
        #[cfg(windows)]
        let built = builder.build(&window);
        // tao windows on GTK hold their content in a box the web view must be packed into.
        #[cfg(target_os = "linux")]
        let built = match window.default_vbox() {
            Some(vbox) => builder.build_gtk(vbox),
            None => builder.build(&window),
        };
        let webview = built
            .map_err(|e| tracing::error!(error = %e, "could not create the web view (is WebView2 / WebKitGTK installed?)"))
            .ok()?;
        self.views.insert(id, View { window, webview, kind });
        Some(id)
    }

    fn open_settings(&mut self, target: &EventLoopWindowTarget<UiEvent>, tab: Option<String>) {
        if let Some(id) = self.settings_id {
            if let Some(v) = self.views.get(&id) {
                v.window.set_minimized(false);
                v.window.set_visible(true);
                v.window.set_focus();
                if let Some(t) = tab {
                    self.eval(id, &js_call("selectTab", &json!(t)));
                }
                return;
            }
        }
        let init = format!("window.INXT_VIEW = 'settings'; window.INXT_TAB = {};", json!(tab.unwrap_or_else(|| "overview".into())));
        self.settings_id = self.create_view(target, "InxtDrive", (940.0, 660.0), (760.0, 520.0), init, ViewKind::Settings);
    }

    fn open_share(&mut self, target: &EventLoopWindowTarget<UiEvent>, path: String, full: String) {
        let existing = self.views.iter().find(|(_, v)| matches!(&v.kind, ViewKind::Share { path: p, .. } if *p == path)).map(|(id, _)| *id);
        if let Some(id) = existing {
            if let Some(v) = self.views.get(&id) {
                v.window.set_focus();
            }
            return;
        }
        let name = base_name(&full);
        let init = "window.INXT_VIEW = 'share';".to_string();
        self.create_view(target, &format!("Share “{name}”"), (520.0, 400.0), (440.0, 360.0), init, ViewKind::Share { path, full });
    }

    fn close(&mut self, id: WindowId) {
        self.views.remove(&id);
        if self.settings_id == Some(id) {
            self.settings_id = None;
        }
    }

    fn on_ipc(&mut self, id: WindowId, body: &str, target: &EventLoopWindowTarget<UiEvent>) {
        let msg: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let cmd = msg["cmd"].as_str().unwrap_or_default().to_string();
        let core = self.core.clone();
        match cmd.as_str() {
            "init" => match self.views.get(&id).map(|v| &v.kind) {
                Some(ViewKind::Settings) => {
                    let js = js_call("init", &json!({ "settings": core.settings_json(), "status": core.status_json() }));
                    self.eval(id, &js);
                }
                Some(ViewKind::Share { path, full }) => {
                    let (path, full) = (path.clone(), full.clone());
                    let name = base_name(&full);
                    let is_dir = core.engine().and_then(|e| e.lookup(&path)).is_some_and(|a| a.is_dir);
                    self.eval(id, &js_call("initShare", &json!({ "name": name, "full": full, "is_dir": is_dir })));
                    self.start_share(id, path);
                }
                None => {}
            },
            "save" => {
                let mut new: Config = match serde_json::from_value(msg["settings"].clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        self.eval(id, &js_call("saved", &json!({ "ok": false, "message": format!("Invalid settings: {e}") })));
                        return;
                    }
                };
                new.paused = core.cfg.lock().paused;
                let res = match core.apply_settings(new) {
                    Ok(m) => json!({ "ok": true, "message": m, "settings": core.settings_json() }),
                    Err(e) => json!({ "ok": false, "message": format!("{e:#}") }),
                };
                self.eval(id, &js_call("saved", &res));
            }
            "action" => self.on_action(id, &msg, target),
            "shared" => {
                let proxy = self.proxy.clone();
                self.core.runtime().spawn(async move {
                    let res = match core.shared_items().await {
                        Ok(items) => json!({ "ok": true, "items": items }),
                        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }),
                    };
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("shared", &res)));
                });
            }
            "copy_link" => {
                let uuid = msg["uuid"].as_str().unwrap_or_default().to_string();
                let is_dir = msg["is_dir"].as_bool().unwrap_or(false);
                let proxy = self.proxy.clone();
                self.core.runtime().spawn(async move {
                    let res = match core.link_for(&uuid, is_dir).await {
                        Ok(Some(url)) => json!({ "ok": shell::copy_text(&url), "uuid": uuid, "url": url }),
                        Ok(None) => json!({ "ok": false, "uuid": uuid, "message": "This item has no public link (it is shared with people directly)." }),
                        Err(e) => json!({ "ok": false, "uuid": uuid, "message": format!("{e:#}") }),
                    };
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("linkCopied", &res)));
                });
            }
            "stop_share" => {
                let uuid = msg["uuid"].as_str().unwrap_or_default().to_string();
                let is_dir = msg["is_dir"].as_bool().unwrap_or(false);
                let proxy = self.proxy.clone();
                self.core.runtime().spawn(async move {
                    let res = match core.stop_sharing(&uuid, is_dir).await {
                        Ok(()) => json!({ "ok": true, "uuid": uuid }),
                        Err(e) => json!({ "ok": false, "uuid": uuid, "message": format!("{e:#}") }),
                    };
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("shareStopped", &res)));
                });
            }
            "share_password" => {
                let uuid = msg["uuid"].as_str().unwrap_or_default().to_string();
                let password = msg["password"].as_str().map(str::to_string);
                let proxy = self.proxy.clone();
                self.core.runtime().spawn(async move {
                    let res = match core.share_password(&uuid, password.as_deref()).await {
                        Ok(()) => json!({ "ok": true, "has_password": password.as_deref().is_some_and(|p| !p.is_empty()) }),
                        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }),
                    };
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("passwordSaved", &res)));
                });
            }
            "close" => self.close(id),
            _ => tracing::debug!(%cmd, "unknown ui message"),
        }
    }

    fn start_share(&self, id: WindowId, path: String) {
        let core = self.core.clone();
        let proxy = self.proxy.clone();
        core.clone().runtime().spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(30 * 60);
            loop {
                match core.share(&path).await {
                    Ok(Some(v)) => {
                        let copied = v["url"].as_str().is_some_and(shell::copy_text);
                        let mut v = v;
                        v["copied"] = json!(copied);
                        let _ = proxy.send_event(UiEvent::Eval(id, js_call("shareReady", &v)));
                        return;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        let _ = proxy.send_event(UiEvent::Eval(id, js_call("shareWaiting", &json!(null))));
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Ok(None) => {
                        let _ = proxy.send_event(UiEvent::Eval(id, js_call("shareError", &json!("The item is still uploading. Try again later."))));
                        return;
                    }
                    Err(e) => {
                        let _ = proxy.send_event(UiEvent::Eval(id, js_call("shareError", &json!(format!("{e:#}")))));
                        return;
                    }
                }
            }
        });
    }

    fn on_action(&mut self, id: WindowId, msg: &Value, target: &EventLoopWindowTarget<UiEvent>) {
        let _ = target;
        let core = self.core.clone();
        let arg = msg["arg"].clone();
        match msg["name"].as_str().unwrap_or_default() {
            "open_drive" => {
                if let Some(mp) = core.snapshot().mount_point {
                    open_target(&drive_root(&mp));
                }
            }
            "open_web" => open_target(&core.cfg.lock().web_url.clone()),
            "pause" => core.set_paused(arg.as_bool().unwrap_or(true)),
            "sign_in" => core.sign_in(),
            "sign_out" => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    core.sign_out();
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("init", &json!({ "settings": core.settings_json(), "status": core.status_json() }))));
                });
            }
            "free_up" => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let freed = core.free_up_all();
                    let msg = format!("Freed {} on {THIS_DEVICE}. Files stay in the cloud.", human_bytes(freed));
                    let _ = proxy.send_event(UiEvent::Eval(id, js_call("toast", &json!({ "text": msg }))));
                });
            }
            "open_logs" => open_target(&paths::logs_dir().display().to_string()),
            "open_settings_file" => {
                let _ = core.cfg.lock().save();
                #[cfg(windows)]
                let _ = std::process::Command::new("notepad.exe").arg(paths::config_file()).spawn();
                #[cfg(not(windows))]
                open_target(&paths::config_file().display().to_string());
            }
            "pick_cache_dir" => {
                let picked = self.pick_folder(id, "Choose where InxtDrive keeps the local copy of your files");
                self.eval(id, &js_call("picked", &json!(picked)));
            }
            "pick_mount_dir" => {
                let picked = self.pick_folder(id, "Choose an empty folder for your Internxt drive");
                self.eval(id, &js_call("pickedMount", &json!(picked)));
            }
            #[cfg(windows)]
            "restart_explorer" => {
                std::thread::spawn(shell::restart_explorer);
            }
            "reveal" => {
                if let Some(p) = arg.as_str() {
                    shell::reveal(p);
                }
            }
            "open_url" => {
                if let Some(u) = arg.as_str().filter(|u| u.starts_with("https://")) {
                    open_target(u);
                }
            }
            "copy" => {
                let ok = arg.as_str().is_some_and(shell::copy_text);
                self.post_later(id, "toast", json!({ "text": if ok { "Copied to clipboard" } else { "Could not copy" } }));
            }
            other => tracing::debug!(action = %other, "unknown ui action"),
        }
    }
}

impl Ui {
    #[cfg(windows)]
    fn pick_folder(&self, id: WindowId, title: &str) -> Option<String> {
        let hwnd = self.views.get(&id).map(|v| v.window.hwnd()).unwrap_or(0);
        shell::pick_folder(hwnd, title)
    }

    #[cfg(target_os = "linux")]
    fn pick_folder(&self, id: WindowId, title: &str) -> Option<String> {
        use gtk::prelude::*;
        let parent = self.views.get(&id).map(|v| v.window.gtk_window().clone());
        shell::pick_folder(parent.as_ref().map(|w| w.upcast_ref::<gtk::Window>()), title)
    }
}

#[cfg(target_os = "linux")]
fn system_dark() -> bool {
    use gtk::prelude::*;
    let prefer = gtk::Settings::default().is_some_and(|s| s.is_gtk_application_prefer_dark_theme());
    prefer || std::env::var("GTK_THEME").is_ok_and(|t| t.to_ascii_lowercase().contains("dark"))
}

#[cfg(windows)]
fn system_dark() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .is_ok_and(|v| v == 0)
}
