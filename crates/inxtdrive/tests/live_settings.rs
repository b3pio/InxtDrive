//! Mounts real drive letters against the mock API. Needs WinFsp; run with
//! `cargo test -p inxtdrive --test live_settings -- --ignored`.
#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use inxt_api::auth::SsoResult;
use inxt_api::Client;
use inxtdrive::config::Config;
use inxtdrive::core::{AppCore, Phase};
use inxtdrive::secrets;

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out: {what}");
}

#[test]
#[ignore = "mounts drive letters V: and W:"]
fn settings_change_reconnects_and_moves_cache() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("INXTDRIVE_HOME", home.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(inxt_mock::Mock::start()).unwrap();

    let mut cfg = Config { drive_letter: "V".into(), start_with_windows: false, poll_interval_secs: 5, ..Config::default() };
    cfg.gateway_url = mock.gateway_url();
    cfg.web_url = mock.gateway_url();
    cfg.save().unwrap();

    let client = Client::new(cfg.api_config()).unwrap();
    let creds = rt
        .block_on(client.complete_sso(SsoResult {
            token: mock.token(),
            mnemonic: "legal winner thank year wave sausage worth useful legal winner thank yellow".into(),
        }))
        .unwrap();
    secrets::save_session(&creds).unwrap();

    let core = AppCore::new(cfg.clone(), true).unwrap();
    core.start();
    wait("first mount", 30, || core.snapshot().phase == Phase::Ready);
    assert_eq!(core.snapshot().mount_point.as_deref(), Some("V:"));

    std::fs::write("V:\\a.txt", b"survives a reconnect").unwrap();
    wait("upload", 30, || mock.tree().iter().any(|(p, _)| p == "/a.txt"));

    let big = home.path().join("bigdisk");
    let new = Config { drive_letter: "W".into(), cache_dir: big.display().to_string(), ..core.cfg.lock().clone() };
    let msg = core.apply_settings(new).unwrap();
    assert!(msg.contains("Moving"), "{msg}");
    wait("remount on W:", 60, || {
        core.snapshot().phase == Phase::Ready && core.snapshot().mount_point.as_deref() == Some("W:")
    });
    assert!(!Path::new("V:\\").exists(), "old letter released");
    assert_eq!(std::fs::read("W:\\a.txt").unwrap(), b"survives a reconnect");
    let moved = big.join("InxtDrive cache").join(&creds.user_uuid);
    assert!(moved.is_dir(), "cache moved to {}", moved.display());
    assert!(std::fs::read_dir(&moved).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".blk")));

    let saved = Config::load();
    assert_eq!(saved.drive_letter, "W");

    core.shutdown();
    assert!(!Path::new("W:\\").exists());
}
