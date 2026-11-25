//! Linux desktop integration, all per-user: start at login, a sidebar
//! bookmark for GTK file managers, right-click actions for Nautilus, Dolphin
//! and Nemo, clipboard, folder picker, opening and revealing files.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// (command, menu text, Nautilus script name, action id)
const VERBS: [(&str, &str, &str, &str); 3] = [
    ("share", "Share link…", "Share link", "inxtShare"),
    ("pin", "Always keep on this device", "Always keep on this device", "inxtPin"),
    ("free", "Free up space", "Free up space", "inxtFree"),
];

/// Variables an AppImage sets for its bundled libraries. Programs it starts
/// (the file manager, the browser) must not inherit them.
const BUNDLE_VARS: [&str; 16] = [
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "GTK_PATH",
    "GTK_EXE_PREFIX",
    "GTK_DATA_PREFIX",
    "GTK_IM_MODULE_FILE",
    "GTK_THEME",
    "GDK_PIXBUF_MODULE_FILE",
    "GIO_MODULE_DIR",
    "GIO_EXTRA_MODULES",
    "GSETTINGS_SCHEMA_DIR",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
    "GST_PLUGIN_SCANNER",
    "PYTHONHOME",
    "QT_PLUGIN_PATH",
];

/// The program to start for autostart and menu actions: the AppImage file
/// itself when running from one (its mount path changes every launch).
pub fn launcher() -> String {
    std::env::var("APPIMAGE")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::current_exe().ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "inxtdrive".into())
}

fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() {
        for v in BUNDLE_VARS {
            c.env_remove(v);
        }
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c
}

fn spawn_detached(mut c: Command) {
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "could not start a helper program"),
    }
}

pub fn open(target: &str) {
    let mut c = command("xdg-open");
    c.arg(target);
    spawn_detached(c);
}

fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Opens the file manager with the item selected, or its folder if the file
/// manager does not support that.
pub fn reveal(path: &str) {
    let p = PathBuf::from(path);
    std::thread::spawn(move || {
        let shown = command("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.FileManager1",
                "--object-path",
                "/org/freedesktop/FileManager1",
                "--method",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("['{}']", file_uri(&p)))
            .arg("")
            .status()
            .is_ok_and(|s| s.success());
        if !shown {
            open(&p.parent().unwrap_or(&p).display().to_string());
        }
    });
}

/// Puts text on the clipboard. GTK owns the clipboard on its main thread,
/// so this hands the work over and reports success optimistically.
pub fn copy_text(text: &str) -> bool {
    let text = text.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
    });
    true
}

/// Native folder picker (the desktop portal's dialog where available).
/// Call on the UI thread.
pub fn pick_folder(parent: Option<&gtk::Window>, title: &str) -> Option<String> {
    use gtk::prelude::*;
    let dialog = gtk::FileChooserNative::new(Some(title), parent, gtk::FileChooserAction::SelectFolder, Some("Select"), Some("Cancel"));
    dialog.set_modal(true);
    let picked = (dialog.run() == gtk::ResponseType::Accept).then(|| dialog.filename()).flatten();
    dialog.destroy();
    picked.map(|p| p.display().to_string())
}

// ---- start at login -------------------------------------------------------------

fn autostart_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("autostart").join("inxtdrive.desktop")
}

/// Quotes a program path for a desktop entry `Exec=` line.
fn exec_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    let path = autostart_file();
    if !on {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=InxtDrive\nComment=Connect your Internxt drive\nExec={} --background\nIcon=inxtdrive\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exec_quote(&launcher())
    );
    std::fs::write(&path, body)?;
    Ok(())
}

pub fn autostart_enabled() -> bool {
    std::fs::read_to_string(autostart_file()).is_ok_and(|s| !s.lines().any(|l| l.trim() == "Hidden=true"))
}

// ---- file manager integration ---------------------------------------------------

fn data_home() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir)
}

fn bookmarks_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("gtk-3.0").join("bookmarks")
}

/// Remembers which bookmark line this profile added, so sign-out removes
/// exactly that one.
fn bookmark_marker() -> PathBuf {
    crate::paths::data_dir().join("bookmark.txt")
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

fn nautilus_dir() -> PathBuf {
    data_home().join("nautilus").join("scripts").join("InxtDrive")
}

fn dolphin_files() -> [PathBuf; 2] {
    [
        data_home().join("kio").join("servicemenus").join("inxtdrive.desktop"),
        data_home().join("kservices5").join("ServiceMenus").join("inxtdrive.desktop"),
    ]
}

fn nemo_file(cmd: &str) -> PathBuf {
    data_home().join("nemo").join("actions").join(format!("inxtdrive-{cmd}.nemo_action"))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn write_file(path: &Path, body: &str, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))
}

fn add_bookmark(mount: &str, label: &str) -> std::io::Result<()> {
    let uri = file_uri(Path::new(mount));
    let path = bookmarks_file();
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!("{uri} {label}");
    if !current.lines().any(|l| l.split(' ').next() == Some(uri.as_str())) {
        let mut next = current.clone();
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&line);
        next.push('\n');
        write_file(&path, &next, false)?;
    }
    std::fs::write(bookmark_marker(), &uri)
}

fn remove_bookmark() {
    let Ok(uri) = std::fs::read_to_string(bookmark_marker()) else { return };
    let path = bookmarks_file();
    if let Ok(current) = std::fs::read_to_string(&path) {
        let kept: Vec<&str> = current.lines().filter(|l| l.split(' ').next() != Some(uri.trim())).collect();
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        if body != current {
            let _ = std::fs::write(&path, body);
        }
    }
    let _ = std::fs::remove_file(bookmark_marker());
}

pub fn register(mount: &str, label: &str) -> anyhow::Result<()> {
    add_bookmark(mount, label)?;
    let exe = launcher();

    if installed("nautilus") {
        let dir = nautilus_dir();
        for (cmd, _, name, _) in VERBS {
            let body = format!(
                "#!/bin/sh\n# Added by InxtDrive; removed when the drive disconnects.\nprintf '%s\\n' \"$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\" | while IFS= read -r f; do\n  [ -n \"$f\" ] && {} {cmd} \"$f\"\ndone\n",
                sh_quote(&exe)
            );
            write_file(&dir.join(name), &body, true)?;
        }
    }

    if installed("dolphin") {
        let mut body = String::from(
            "[Desktop Entry]\nType=Service\nMimeType=all/all;\nX-KDE-ServiceTypes=KonqPopupMenu/Plugin\nX-KDE-Protocols=file\nX-KDE-Submenu=InxtDrive\nIcon=inxtdrive\nActions=",
        );
        body.push_str(&VERBS.iter().map(|v| v.3).collect::<Vec<_>>().join(";"));
        body.push_str(";\n");
        for (cmd, text, _, id) in VERBS {
            body.push_str(&format!("\n[Desktop Action {id}]\nName={text}\nIcon=inxtdrive\nExec={} {cmd} %f\n", exec_quote(&exe)));
        }
        // Plasma 6 only runs service menus from the home folder when they are executable.
        for f in dolphin_files() {
            write_file(&f, &body, true)?;
        }
    }

    if installed("nemo") {
        for (cmd, text, _, _) in VERBS {
            let body = format!(
                "[Nemo Action]\nName={text}\nComment=InxtDrive\nExec={} {cmd} %F\nIcon-Name=inxtdrive\nSelection=s\nExtensions=any;\n",
                exec_quote(&exe)
            );
            write_file(&nemo_file(cmd), &body, false)?;
        }
    }
    Ok(())
}

pub fn unregister() {
    remove_bookmark();
    let _ = std::fs::remove_dir_all(nautilus_dir());
    for f in dolphin_files() {
        let _ = std::fs::remove_file(f);
    }
    for (cmd, _, _, _) in VERBS {
        let _ = std::fs::remove_file(nemo_file(cmd));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encodes_spaces_and_unicode() {
        assert_eq!(file_uri(Path::new("/home/a b/Ïnxt")), "file:///home/a%20b/%C3%8Fnxt");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/opt/it's"), r"'/opt/it'\''s'");
        assert_eq!(exec_quote("/a b/$x"), r#""/a b/\$x""#);
    }
}
