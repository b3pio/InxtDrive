//! Engine against the in-memory mock API, with a second client acting as
//! another device.

use std::sync::Arc;
use std::time::{Duration, Instant};

use inxt_api::drive::split_name;
use inxt_api::types::CreateFileRequest;
use inxt_api::{Client, ClientConfig, Credentials};
use inxt_core::{Engine, EngineConfig, ROOT};
use inxt_mock::Mock;

const MNEMONIC: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

struct Env {
    rt: tokio::runtime::Runtime,
    mock: Mock,
    engine: Arc<Engine>,
    other: Arc<Client>,
    _dir: tempfile::TempDir,
}

fn creds(mock: &Mock) -> Credentials {
    Credentials {
        token: mock.token(),
        mnemonic: MNEMONIC.into(),
        email: "tester@example.invalid".into(),
        user_uuid: "user-uuid-0001".into(),
        user_id: "user-id-0001".into(),
        bridge_user: "bridge@example.invalid".into(),
        bucket: inxt_mock::BUCKET.into(),
        root_folder_uuid: inxt_mock::ROOT_UUID.into(),
    }
}

fn client(mock: &Mock) -> Arc<Client> {
    let c = Client::new(ClientConfig { gateway_url: mock.gateway_url(), ..Default::default() }).unwrap();
    c.set_credentials(Some(creds(mock))).unwrap();
    Arc::new(c)
}

fn setup_with(seed: impl FnOnce(&tokio::runtime::Runtime, &Mock, &Arc<Client>)) -> Env {
    let _ = tracing_subscriber::fmt().with_env_filter("inxt_core=debug,inxt_api=info").with_test_writer().try_init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build().unwrap();
    let mock = rt.block_on(Mock::start()).unwrap();
    let other = client(&mock);
    seed(&rt, &mock, &other);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new(dir.path().join("acct"));
    cfg.push_debounce = Duration::from_millis(150);
    cfg.poll_interval = Duration::from_millis(500);
    let engine = Engine::open(cfg, client(&mock), rt.handle().clone()).unwrap();
    rt.block_on(engine.poll_remote()).unwrap();
    engine.start();
    Env { rt, mock, engine, other, _dir: dir }
}

fn setup() -> Env {
    setup_with(|_, _, _| {})
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i as u64 * 31 + seed as u64) % 251) as u8).collect()
}

/// Uploads `data` as `name` into `folder` from the other device.
async fn remote_put(other: &Client, folder: &str, name: &str, data: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("payload");
    std::fs::write(&p, data).unwrap();
    let file_id = if data.is_empty() {
        None
    } else {
        Some(other.upload_file(&p, data.len() as u64, Arc::new(|_| {})).await.unwrap())
    };
    let (plain, ty) = split_name(name);
    let now = chrono::Utc::now();
    let dto = other
        .create_file(&CreateFileRequest {
            bucket: inxt_mock::BUCKET.into(),
            file_id,
            encrypt_version: "03-aes",
            folder_uuid: folder.into(),
            size: data.len() as u64,
            plain_name: plain,
            file_type: ty,
            creation_time: now,
            date: now,
            modification_time: now,
        })
        .await
        .unwrap();
    dto.uuid
}

async fn remote_get(other: &Client, display_name: &str, mock: &Mock) -> Vec<u8> {
    let f = mock.find_file(display_name).unwrap_or_else(|| panic!("{display_name} not in cloud"));
    let Some(id) = f.file_id else { return Vec::new() };
    let info = other.bucket_file_info(&id).await.unwrap();
    let mut out = vec![0u8; info.size as usize];
    let mut sink = |off: u64, d: &[u8]| {
        out[off as usize..off as usize + d.len()].copy_from_slice(d);
        Ok(())
    };
    other.download_range(&info, 0, info.size, &mut sink).await.unwrap();
    out
}

fn read_all(e: &Engine, path: &str) -> Vec<u8> {
    let a = e.lookup(path).unwrap_or_else(|| panic!("{path} missing locally"));
    let mut out = vec![0u8; a.size as usize];
    let mut off = 0;
    while off < out.len() {
        let n = e.read(a.id, off as u64, &mut out[off..]).unwrap();
        assert!(n > 0);
        off += n;
    }
    out
}

fn write_file(e: &Engine, path: &str, data: &[u8]) {
    let a = e.lookup(path).unwrap_or_else(|| e.create(path, false).unwrap());
    e.open_handle(a.id, true);
    e.set_size(a.id, 0).unwrap();
    for (i, chunk) in data.chunks(64 * 1024).enumerate() {
        e.write(a.id, (i * 64 * 1024) as u64, chunk, false).unwrap();
    }
    e.release(a.id, true, true);
}

fn has(mock: &Mock, path: &str) -> bool {
    mock.tree().iter().any(|(p, _)| p == path)
}

#[test]
fn bootstrap_and_ranged_reads() {
    let big = pattern(3 * 1024 * 1024 + 123, 7);
    let big2 = big.clone();
    let env = setup_with(move |rt, mock, other| {
        rt.block_on(async {
            let docs = other.create_folder(inxt_mock::ROOT_UUID, "Docs", chrono::Utc::now(), chrono::Utc::now()).await.unwrap();
            remote_put(other, &docs.uuid, "big.bin", &big2).await;
            remote_put(other, inxt_mock::ROOT_UUID, "empty.txt", b"").await;
            remote_put(other, inxt_mock::ROOT_UUID, "what?.txt", b"odd name").await;
        });
        let _ = mock;
    });
    let e = &env.engine;
    assert!(e.lookup("\\Docs").unwrap().is_dir);
    assert_eq!(e.lookup("\\Docs\\big.bin").unwrap().size, big.len() as u64);
    assert_eq!(read_all(e, "\\empty.txt"), b"");
    // Windows shows `?` as its full-width look-alike; Linux allows it as is.
    let odd = if cfg!(windows) { "\\what？.txt" } else { "\\what?.txt" };
    assert_eq!(read_all(e, odd), b"odd name");

    let id = e.lookup("\\docs\\BIG.bin").unwrap().id;
    let mut buf = vec![0u8; 5000];
    let off = 2 * 1024 * 1024 - 100;
    assert_eq!(e.read(id, off as u64, &mut buf).unwrap(), 5000);
    assert_eq!(&buf[..], &big[off..off + 5000]);
    assert!(!e.attr(id).unwrap().cached);
    assert_eq!(read_all(e, "\\Docs\\big.bin"), big);
    assert!(e.attr(id).unwrap().cached);
}

#[test]
fn local_changes_reach_the_cloud() {
    let env = setup();
    let e = &env.engine;
    e.create("\\Projects", true).unwrap();
    e.create("\\Projects\\Sub", true).unwrap();
    let data = pattern(700_000, 3);
    write_file(e, "\\Projects\\Sub\\notes.txt", &data);
    wait_until("upload", || has(&env.mock, "/Projects/Sub/notes.txt"));
    assert_eq!(env.rt.block_on(remote_get(&env.other, "notes.txt", &env.mock)), data);

    let id = e.lookup("\\Projects\\Sub\\notes.txt").unwrap().id;
    e.rename(id, "\\Projects\\renamed.md", false).unwrap();
    wait_until("move+rename", || has(&env.mock, "/Projects/renamed.md") && !has(&env.mock, "/Projects/Sub/notes.txt"));

    let sub = e.lookup("\\Projects\\Sub").unwrap().id;
    e.rename(sub, "\\Archive", false).unwrap();
    wait_until("folder move", || has(&env.mock, "/Archive/") && !has(&env.mock, "/Projects/Sub/"));

    e.delete(id).unwrap();
    wait_until("trash", || !has(&env.mock, "/Projects/renamed.md"));
    assert!(e.lookup("\\Projects\\renamed.md").is_none());

    e.create("\\empty.dat", false).unwrap();
    let ea = e.lookup("\\empty.dat").unwrap();
    e.release(ea.id, true, true);
    wait_until("empty file", || has(&env.mock, "/empty.dat"));
}

#[test]
fn editor_style_save_keeps_the_cloud_file() {
    let env = setup();
    let e = &env.engine;
    write_file(e, "\\report.docx", b"version one");
    wait_until("first upload", || has(&env.mock, "/report.docx"));
    let original_uuid = env.mock.find_file("report.docx").unwrap().uuid;
    std::thread::sleep(Duration::from_millis(300));
    env.mock.clear_calls();

    write_file(e, "\\~WRL0001.tmp", b"version two, longer");
    let orig = e.lookup("\\report.docx").unwrap().id;
    e.rename(orig, "\\~WRL0002.tmp", false).unwrap();
    let tmp = e.lookup("\\~WRL0001.tmp").unwrap().id;
    e.rename(tmp, "\\report.docx", false).unwrap();
    let backup = e.lookup("\\~WRL0002.tmp").unwrap().id;
    e.delete(backup).unwrap();

    wait_until("replace", || {
        env.mock.find_file("report.docx").is_some_and(|f| f.size == b"version two, longer".len() as u64)
    });
    std::thread::sleep(Duration::from_millis(800));
    let f = env.mock.find_file("report.docx").unwrap();
    assert_eq!(f.uuid, original_uuid, "file identity (and history) should be kept");
    assert_eq!(env.mock.tree().len(), 1, "no temp files left in the cloud: {:?}", env.mock.tree());
    assert_eq!(env.rt.block_on(remote_get(&env.other, "report.docx", &env.mock)), b"version two, longer");
    assert!(!env.mock.calls().iter().any(|c| c.starts_with("create file")), "{:?}", env.mock.calls());
}

#[test]
fn replace_on_rename_over_existing() {
    let env = setup();
    let e = &env.engine;
    write_file(e, "\\data.json", b"{\"v\":1}");
    wait_until("upload", || has(&env.mock, "/data.json"));
    let uuid = env.mock.find_file("data.json").unwrap().uuid;
    write_file(e, "\\data.json.tmp", b"{\"v\":2}");
    let tmp = e.lookup("\\data.json.tmp").unwrap().id;
    e.rename(tmp, "\\data.json", true).unwrap();
    wait_until("replaced", || env.mock.find_file("data.json").is_some_and(|f| f.size == 7 && f.file_id.is_some()) && !has(&env.mock, "/data.json.tmp"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.mock.find_file("data.json").unwrap().uuid, uuid);
    assert_eq!(env.rt.block_on(remote_get(&env.other, "data.json", &env.mock)), b"{\"v\":2}");
}

#[test]
fn remote_changes_show_up() {
    let env = setup();
    let e = &env.engine;
    let (folder_uuid, file_uuid) = env.rt.block_on(async {
        let f = env.other.create_folder(inxt_mock::ROOT_UUID, "Shared", chrono::Utc::now(), chrono::Utc::now()).await.unwrap();
        let u = remote_put(&env.other, &f.uuid, "photo.jpg", &pattern(10_000, 1)).await;
        (f.uuid, u)
    });
    env.rt.block_on(env.engine.poll_remote()).unwrap();
    assert_eq!(read_all(e, "\\Shared\\photo.jpg"), pattern(10_000, 1));
    assert!(e.take_changes().iter().any(|c| c.path == "\\Shared\\photo.jpg"));

    env.rt.block_on(async {
        env.other.rename_file(&file_uuid, "holiday.jpg").await.unwrap();
        env.other.rename_folder(&folder_uuid, "Family").await.unwrap();
    });
    env.rt.block_on(env.engine.poll_remote()).unwrap();
    assert!(e.lookup("\\Family\\holiday.jpg").is_some());
    assert!(e.lookup("\\Shared").is_none());

    env.rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, pattern(20_000, 9)).unwrap();
        let id = env.other.upload_file(&p, 20_000, Arc::new(|_| {})).await.unwrap();
        env.other.replace_file(&file_uuid, Some(&id), 20_000, chrono::Utc::now()).await.unwrap();
    });
    env.rt.block_on(env.engine.poll_remote()).unwrap();
    assert_eq!(read_all(e, "\\Family\\holiday.jpg"), pattern(20_000, 9));

    env.rt.block_on(env.other.trash(&[(inxt_api::drive::ItemKind::Folder, folder_uuid.clone())])).unwrap();
    env.rt.block_on(env.engine.poll_remote()).unwrap();
    assert!(e.lookup("\\Family").is_none());
    assert!(e.list(ROOT).unwrap().is_empty());
}

#[test]
fn concurrent_edits_make_a_conflict_copy() {
    let env = setup();
    let e = &env.engine;
    write_file(e, "\\plan.txt", b"base");
    wait_until("upload", || has(&env.mock, "/plan.txt"));
    std::thread::sleep(Duration::from_millis(300));
    let uuid = env.mock.find_file("plan.txt").unwrap().uuid;

    let id = e.lookup("\\plan.txt").unwrap().id;
    e.open_handle(id, true);
    e.write(id, 0, b"local edit", false).unwrap();

    env.rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, b"remote edit").unwrap();
        let fid = env.other.upload_file(&p, 11, Arc::new(|_| {})).await.unwrap();
        env.other.replace_file(&uuid, Some(&fid), 11, chrono::Utc::now()).await.unwrap();
    });
    e.release(id, true, true);
    env.rt.block_on(env.engine.poll_remote()).unwrap();

    assert_eq!(read_all(e, "\\plan.txt"), b"remote edit");
    let names: Vec<String> = e.list(ROOT).unwrap().into_iter().map(|(n, _)| n).collect();
    let copy = names.iter().find(|n| n.starts_with("plan (conflict")).expect("conflict copy");
    assert_eq!(read_all(e, &format!("\\{copy}")), b"local edit");
    wait_until("conflict copy uploaded", || env.mock.tree().len() == 2);
}

#[test]
fn sync_state_drives_status_badges() {
    let env = setup();
    let e = &env.engine;
    e.create("\\Album", true).unwrap();
    write_file(e, "\\Album\\photo.jpg", &pattern(300_000, 2));
    let a = e.lookup("\\Album\\photo.jpg").unwrap();
    assert!(a.pending && a.cached && !a.excluded);
    write_file(e, "\\Album\\desktop.ini", b"[x]");
    assert!(e.lookup("\\Album\\desktop.ini").unwrap().excluded);
    assert!(!e.lookup("\\Album\\desktop.ini").unwrap().pending);

    wait_until("upload", || has(&env.mock, "/Album/photo.jpg"));
    wait_until("not pending", || !e.lookup("\\Album\\photo.jpg").unwrap().pending);
    wait_until("folder settles", || !e.lookup("\\Album").unwrap().pending);
    let changes = e.take_changes();
    assert!(changes.iter().any(|c| c.path == "\\Album\\photo.jpg"), "{changes:?}");
    let act = e.activity();
    assert!(act.recent.iter().any(|r| r.path == "\\Album\\photo.jpg"), "{act:?}");
    assert!(act.uploading.is_empty());

    e.free_up("\\Album").unwrap();
    assert!(!e.lookup("\\Album\\photo.jpg").unwrap().cached);
    assert!(e.take_changes().iter().any(|c| c.path == "\\Album\\photo.jpg"));
    assert!(!e.remote_id("\\Album\\photo.jpg").unwrap().unwrap().1);
    assert!(e.remote_id("\\Album\\desktop.ini").is_err());
}

#[test]
fn share_links_roundtrip() {
    use inxt_api::drive::ItemKind;
    use inxt_api::share::lib_decrypt;
    let env = setup();
    let e = &env.engine;
    e.create("\\Pics", true).unwrap();
    write_file(e, "\\Pics\\cat.jpg", &pattern(1000, 4));
    wait_until("upload", || e.remote_id("\\Pics\\cat.jpg").ok().flatten().is_some());
    let (uuid, is_dir) = e.remote_id("\\Pics\\cat.jpg").unwrap().unwrap();
    assert!(!is_dir);
    let c = &env.other;
    let link = env.rt.block_on(c.create_public_link(&uuid, ItemKind::File)).unwrap();
    assert!(link.url.starts_with("https://share.example.invalid/sh/file/"), "{}", link.url);
    assert!(link.url.ends_with(&format!("/{}", link.code)));
    assert_eq!(link.code.len(), 8);

    let stored = env.mock.sharing_for(&uuid).unwrap();
    let bucket = inxt_api::crypto::BucketKey::derive(MNEMONIC, inxt_mock::BUCKET).unwrap();
    assert_eq!(lib_decrypt(&stored.encryption_key, &link.code).unwrap(), bucket.share_key_hex());
    assert_eq!(stored.algorithm, "inxt-v3");

    let again = env.rt.block_on(c.create_public_link(&uuid, ItemKind::File)).unwrap();
    assert_eq!(again.url, link.url, "an existing link is reused, not replaced");
    let existing = env.rt.block_on(c.existing_public_link(&uuid, ItemKind::File)).unwrap().unwrap();
    assert_eq!(existing.url, link.url);

    let listed = env.rt.block_on(c.list_shared()).unwrap();
    assert!(listed.iter().any(|i| i.uuid == uuid && i.name == "cat.jpg" && !i.is_dir), "{listed:?}");
    assert_eq!(e.path_of_uuid(&uuid).as_deref(), Some("\\Pics\\cat.jpg"));

    env.rt.block_on(c.set_share_password(&link, Some("hunter2"))).unwrap();
    let pw = env.mock.sharing_for(&uuid).unwrap().encrypted_password.unwrap();
    assert_eq!(lib_decrypt(&pw, &link.code).unwrap(), "hunter2");
    env.rt.block_on(c.set_share_password(&link, None)).unwrap();
    assert!(env.mock.sharing_for(&uuid).unwrap().encrypted_password.is_none());

    env.rt.block_on(c.stop_sharing(&uuid, ItemKind::File)).unwrap();
    assert!(env.mock.sharing_for(&uuid).is_none());
    assert!(env.rt.block_on(c.existing_public_link(&uuid, ItemKind::File)).unwrap().is_none());
}

#[test]
fn local_only_files_stay_local() {
    let env = setup();
    let e = &env.engine;
    write_file(e, "\\desktop.ini", b"[.ShellClassInfo]");
    write_file(e, "\\~$report.docx", b"lock");
    write_file(e, "\\real.txt", b"real");
    wait_until("upload", || has(&env.mock, "/real.txt"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.mock.tree().len(), 1, "{:?}", env.mock.tree());
    assert_eq!(read_all(e, "\\desktop.ini"), b"[.ShellClassInfo]");
}

#[test]
fn restart_keeps_state_and_cache() {
    let env = setup();
    write_file(&env.engine, "\\keep.txt", b"persisted");
    wait_until("upload", || has(&env.mock, "/keep.txt"));
    std::thread::sleep(Duration::from_millis(300));
    env.engine.shutdown();
    let data_dir = env._dir.path().join("acct");
    let mut cfg = EngineConfig::new(data_dir);
    cfg.push_debounce = Duration::from_millis(150);
    let e2 = Engine::open(cfg, client(&env.mock), env.rt.handle().clone()).unwrap();
    assert!(e2.initial_sync_done());
    let a = e2.lookup("\\keep.txt").unwrap();
    assert!(a.cached);
    assert_eq!(read_all(&e2, "\\keep.txt"), b"persisted");
}

#[test]
#[ignore = "uploads 105 MiB; run with --ignored"]
fn multipart_upload_roundtrip() {
    let env = setup();
    let data = pattern(105 * 1024 * 1024, 5);
    write_file(&env.engine, "\\big.iso", &data);
    wait_until("multipart", || has(&env.mock, "/big.iso"));
    assert!(env.rt.block_on(remote_get(&env.other, "big.iso", &env.mock)) == data);
}
