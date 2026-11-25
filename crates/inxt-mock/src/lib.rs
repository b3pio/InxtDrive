//! In-memory stand-in for the Internxt Drive and Network APIs.
//!
//! It stores ciphertext exactly as uploaded, verifies content hashes the way
//! the real network does, serves ranged downloads, and keeps `updatedAt`
//! timestamps so the delta feeds behave like the real ones.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use parking_lot::Mutex;
use ripemd::Ripemd160;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Folder {
    pub uuid: String,
    pub parent: String,
    pub name: String,
    pub status: &'static str,
    pub created: i64,
    pub updated: i64,
}

#[derive(Clone, Debug)]
pub struct File {
    pub uuid: String,
    pub folder: String,
    pub plain_name: String,
    pub file_type: String,
    pub size: u64,
    pub file_id: Option<String>,
    pub status: &'static str,
    pub created: i64,
    pub updated: i64,
    pub mtime: i64,
}

impl File {
    pub fn display_name(&self) -> String {
        if self.file_type.is_empty() {
            self.plain_name.clone()
        } else {
            format!("{}.{}", self.plain_name, self.file_type)
        }
    }
}

struct NetFile {
    index: String,
    hash: String,
    data: Vec<u8>,
}

struct Slot {
    size: u64,
    parts: BTreeMap<u32, Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct Sharing {
    pub id: String,
    pub item_id: String,
    pub item_type: String,
    pub encryption_key: String,
    pub encrypted_code: String,
    pub algorithm: String,
    pub encrypted_password: Option<String>,
    pub created: i64,
}

#[derive(Default)]
struct St {
    base: String,
    root: String,
    folders: HashMap<String, Folder>,
    files: HashMap<String, File>,
    net: HashMap<String, NetFile>,
    slots: HashMap<String, Slot>,
    sharings: HashMap<String, Sharing>,
    clock: i64,
    pub calls: Vec<String>,
}

impl St {
    fn tick(&mut self) -> i64 {
        let now = chrono::Utc::now().timestamp_millis();
        self.clock = now.max(self.clock + 1);
        self.clock
    }

    fn folder_json(&self, f: &Folder) -> Value {
        json!({
            "type": "folder", "uuid": f.uuid, "parentUuid": if f.parent.is_empty() { Value::Null } else { json!(f.parent) },
            "plainName": f.name, "name": f.name, "status": f.status,
            "deleted": f.status != "EXISTS", "removed": f.status == "DELETED",
            "createdAt": iso(f.created), "updatedAt": iso(f.updated),
            "creationTime": iso(f.created), "modificationTime": iso(f.updated),
        })
    }

    fn file_json(&self, f: &File) -> Value {
        json!({
            "uuid": f.uuid, "fileId": f.file_id, "plainName": f.plain_name, "name": f.plain_name, "type": f.file_type,
            "size": f.size.to_string(), "bucket": BUCKET, "folderUuid": f.folder, "encryptVersion": "03-aes",
            "status": f.status, "createdAt": iso(f.created), "updatedAt": iso(f.updated),
            "creationTime": iso(f.created), "modificationTime": iso(f.mtime),
        })
    }

    fn live_folder(&self, uuid: &str) -> bool {
        self.folders.get(uuid).is_some_and(|f| f.status == "EXISTS")
    }

    fn name_taken(&self, parent: &str, name: &str, except: &str) -> bool {
        self.folders.values().any(|f| f.parent == parent && f.status == "EXISTS" && f.name == name && f.uuid != except)
            || self
                .files
                .values()
                .any(|f| f.folder == parent && f.status == "EXISTS" && f.display_name() == name && f.uuid != except)
    }

    fn is_descendant(&self, folder: &str, maybe_ancestor: &str) -> bool {
        let mut cur = folder.to_string();
        for _ in 0..4096 {
            if cur == maybe_ancestor {
                return true;
            }
            match self.folders.get(&cur) {
                Some(f) if !f.parent.is_empty() => cur = f.parent.clone(),
                _ => return false,
            }
        }
        false
    }

    fn trash_folder(&mut self, uuid: &str) {
        let now = self.tick();
        let kids: Vec<String> = self.folders.values().filter(|f| f.parent == uuid).map(|f| f.uuid.clone()).collect();
        for f in self.files.values_mut().filter(|f| f.folder == uuid && f.status == "EXISTS") {
            f.status = "TRASHED";
            f.updated = now;
        }
        if let Some(f) = self.folders.get_mut(uuid) {
            f.status = "TRASHED";
            f.updated = now;
        }
        for k in kids {
            self.trash_folder(&k);
        }
    }
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_iso(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|t| t.timestamp_millis()).unwrap_or(0)
}

fn new_uuid() -> String {
    let b: [u8; 16] = rand::random();
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "message": msg }))).into_response()
}

type S = State<Arc<Mutex<St>>>;

#[derive(Clone)]
pub struct Mock {
    st: Arc<Mutex<St>>,
    pub addr: SocketAddr,
}

pub const ROOT_UUID: &str = "00000000-0000-4000-8000-000000000001";
pub const BUCKET: &str = "1f7a5a8c3e0d9b6f4a2c1e0d";

impl Mock {
    pub async fn start() -> anyhow::Result<Mock> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let mut st = St { base: format!("http://{addr}"), root: ROOT_UUID.into(), ..Default::default() };
        let now = st.tick();
        st.folders.insert(
            ROOT_UUID.into(),
            Folder { uuid: ROOT_UUID.into(), parent: String::new(), name: "root".into(), status: "EXISTS", created: now, updated: now },
        );
        let st = Arc::new(Mutex::new(st));
        let app = router(st.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Mock { st, addr })
    }

    pub fn gateway_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn token(&self) -> String {
        token(30)
    }

    /// Live paths (`/a/b.txt`) with sizes; folders end with `/`.
    pub fn tree(&self) -> Vec<(String, u64)> {
        let st = self.st.lock();
        let path_of = |mut uuid: String| -> Option<String> {
            let mut parts = Vec::new();
            for _ in 0..4096 {
                if uuid == st.root {
                    parts.reverse();
                    return Some(format!("/{}", parts.join("/")));
                }
                let f = st.folders.get(&uuid)?;
                if f.status != "EXISTS" {
                    return None;
                }
                parts.push(f.name.clone());
                uuid = f.parent.clone();
            }
            None
        };
        let mut out = Vec::new();
        for f in st.folders.values().filter(|f| f.status == "EXISTS" && f.uuid != st.root) {
            if let Some(p) = path_of(f.uuid.clone()) {
                out.push((format!("{p}/"), 0));
            }
        }
        for f in st.files.values().filter(|f| f.status == "EXISTS") {
            if let Some(p) = path_of(f.folder.clone()) {
                let sep = if p == "/" { "" } else { "/" };
                out.push((format!("{p}{sep}{}", f.display_name()), f.size));
            }
        }
        out.sort();
        out
    }

    pub fn calls(&self) -> Vec<String> {
        self.st.lock().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.st.lock().calls.clear();
    }

    pub fn find_folder(&self, name: &str) -> Option<String> {
        self.st.lock().folders.values().find(|f| f.name == name && f.status == "EXISTS").map(|f| f.uuid.clone())
    }

    pub fn find_file(&self, display_name: &str) -> Option<File> {
        self.st.lock().files.values().find(|f| f.display_name() == display_name && f.status == "EXISTS").cloned()
    }

    pub fn sharing_for(&self, item_id: &str) -> Option<Sharing> {
        self.st.lock().sharings.values().find(|s| s.item_id == item_id).cloned()
    }
}

fn token(days: i64) -> String {
    let exp = chrono::Utc::now().timestamp() + days * 86_400;
    format!("eyJhbGciOiJub25lIn0.{}.sig", URL_SAFE_NO_PAD.encode(json!({ "exp": exp }).to_string()))
}

fn router(st: Arc<Mutex<St>>) -> Router {
    Router::new()
        .route("/drive/users/refresh", get(refresh))
        .route("/drive/users/usage", get(usage))
        .route("/drive/users/limit", get(limit))
        .route("/drive/auth/logout", get(|| async { StatusCode::OK }))
        .route("/drive/folders/sync", get(folders_sync))
        .route("/drive/files/sync", get(files_sync))
        .route("/drive/folders/v2/content/:uuid/folders", get(content_folders))
        .route("/drive/folders/v2/content/:uuid/files", get(content_files))
        .route("/drive/folders", post(create_folder))
        .route("/drive/folders/:uuid/meta", put(rename_folder))
        .route("/drive/folders/:uuid", patch(move_folder))
        .route("/drive/files", post(create_file))
        .route("/drive/files/:uuid", put(replace_file).patch(move_file))
        .route("/drive/files/:uuid/meta", put(rename_file))
        .route("/drive/storage/trash/add", post(trash))
        .route("/network/buckets/:bucket/files/:id/info", get(net_info))
        .route("/network/v2/buckets/:bucket/files/start", post(start_upload))
        .route("/network/v2/buckets/:bucket/files/finish", post(finish_upload))
        .route("/blob/:id", get(blob))
        .route("/upload/:slot", put(upload_single))
        .route("/upload/:slot/:part", put(upload_part))
        .route("/drive/sharings", post(create_sharing))
        .route("/drive/sharings/files", get(shared_files))
        .route("/drive/sharings/folders", get(shared_folders))
        .route("/drive/sharings/:kind/:item/info", get(sharing_info))
        .route("/drive/sharings/:a/:b", delete(remove_sharing).patch(set_password))
        .route("/drive/storage/share/domains", get(|| async { Json(json!({ "list": ["https://share.example.invalid"] })) }))
        .route("/_tree", get(debug_tree))
        .route("/_calls", get(debug_calls))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(st)
}

fn log(st: &mut St, s: String) {
    st.calls.push(s);
}

fn sharing_json(s: &Sharing) -> Value {
    json!({
        "id": s.id, "itemId": s.item_id, "itemType": s.item_type, "ownerId": "user-uuid-0001",
        "encryptionKey": s.encryption_key, "encryptedCode": s.encrypted_code, "encryptionAlgorithm": s.algorithm,
        "encryptedPassword": s.encrypted_password, "type": "public",
    })
}

async fn create_sharing(State(st): S, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let item_id = b["itemId"].as_str().unwrap_or_default().to_string();
    let item_type = b["itemType"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("share {item_type} {item_id}"));
    let exists = if item_type == "folder" { st.live_folder(&item_id) } else { st.files.get(&item_id).is_some_and(|f| f.status == "EXISTS") };
    if !exists {
        return err(StatusCode::NOT_FOUND, "item not found");
    }
    if let Some(s) = st.sharings.values().find(|s| s.item_id == item_id) {
        if b["persistPreviousSharing"].as_bool().unwrap_or(false) {
            return Json(sharing_json(s)).into_response();
        }
    }
    let now = st.tick();
    let s = Sharing {
        id: new_uuid_v4(),
        item_id,
        item_type,
        encryption_key: b["encryptionKey"].as_str().unwrap_or_default().to_string(),
        encrypted_code: b["encryptedCode"].as_str().unwrap_or_default().to_string(),
        algorithm: b["encryptionAlgorithm"].as_str().unwrap_or_default().to_string(),
        encrypted_password: None,
        created: now,
    };
    let j = sharing_json(&s);
    st.sharings.insert(s.id.clone(), s);
    (StatusCode::CREATED, Json(j)).into_response()
}

async fn sharing_info(State(st): S, Path((_kind, item)): Path<(String, String)>) -> Response {
    let st = st.lock();
    match st.sharings.values().find(|s| s.item_id == item) {
        Some(s) => Json(json!({
            "type": "public",
            "publicSharing": { "id": s.id, "isPasswordProtected": s.encrypted_password.is_some(), "encryptedCode": s.encrypted_code },
            "invitationsCount": 0,
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "not shared"),
    }
}

/// `DELETE /sharings/{kind}/{itemId}` stops sharing; `DELETE /sharings/{id}/password` removes the password.
async fn remove_sharing(State(st): S, Path((a, b)): Path<(String, String)>) -> Response {
    let mut st = st.lock();
    if b == "password" {
        return match st.sharings.get_mut(&a) {
            Some(s) => {
                s.encrypted_password = None;
                StatusCode::OK.into_response()
            }
            None => err(StatusCode::NOT_FOUND, "no sharing"),
        };
    }
    log(&mut st, format!("unshare {a} {b}"));
    st.sharings.retain(|_, s| s.item_id != b);
    StatusCode::OK.into_response()
}

async fn set_password(State(st): S, Path((id, what)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    let mut st = st.lock();
    if what != "password" {
        return err(StatusCode::NOT_FOUND, "unknown");
    }
    match st.sharings.get_mut(&id) {
        Some(s) => {
            s.encrypted_password = body["encryptedPassword"].as_str().map(str::to_string);
            Json(sharing_json(s)).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no sharing"),
    }
}

async fn shared_files(State(st): S) -> Json<Value> {
    let st = st.lock();
    let files: Vec<Value> = st
        .sharings
        .values()
        .filter(|s| s.item_type == "file")
        .filter_map(|s| st.files.get(&s.item_id).map(|f| (s, f)))
        .map(|(s, f)| {
            let mut j = st.file_json(f);
            j["dateShared"] = json!(iso(s.created));
            j["sharedWithMe"] = json!(false);
            j
        })
        .collect();
    Json(json!({ "files": files, "folders": [] }))
}

async fn shared_folders(State(st): S) -> Json<Value> {
    let st = st.lock();
    let folders: Vec<Value> = st
        .sharings
        .values()
        .filter(|s| s.item_type == "folder")
        .filter_map(|s| st.folders.get(&s.item_id).map(|f| (s, f)))
        .map(|(s, f)| {
            let mut j = st.folder_json(f);
            j["dateShared"] = json!(iso(s.created));
            j["sharedWithMe"] = json!(false);
            j
        })
        .collect();
    Json(json!({ "folders": folders, "files": [] }))
}

fn new_uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

async fn debug_tree(State(st): S) -> Json<Value> {
    let m = Mock { st: st.clone(), addr: SocketAddr::from(([127, 0, 0, 1], 0)) };
    let v: Vec<Value> = m.tree().into_iter().map(|(p, s)| json!({ "path": p, "size": s })).collect();
    Json(json!(v))
}

async fn debug_calls(State(st): S) -> Json<Value> {
    Json(json!(st.lock().calls))
}

async fn refresh(State(st): S) -> Json<Value> {
    let root = st.lock().root.clone();
    Json(json!({
        "newToken": token(30),
        "user": {
            "email": "tester@example.invalid", "userId": "user-id-0001", "uuid": "user-uuid-0001",
            "mnemonic": "encrypted", "rootFolderId": root, "bucket": BUCKET, "bridgeUser": "bridge@example.invalid",
        }
    }))
}

async fn usage(State(st): S) -> Json<Value> {
    let used: u64 = st.lock().files.values().filter(|f| f.status == "EXISTS").map(|f| f.size).sum();
    Json(json!({ "drive": used }))
}

async fn limit() -> Json<Value> {
    Json(json!({ "maxSpaceBytes": 2u64 << 40 }))
}

#[derive(Deserialize)]
struct SyncQ {
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

fn page<T: Clone>(items: Vec<(i64, String, T)>, q: &SyncQ) -> (Vec<T>, Option<String>) {
    let (since, offset) = match &q.cursor {
        Some(c) => {
            let mut it = c.split(':');
            (it.next().and_then(|s| s.parse().ok()).unwrap_or(0), it.next().and_then(|s| s.parse().ok()).unwrap_or(0))
        }
        None => (q.updated_at.as_deref().map(parse_iso).unwrap_or(0), 0usize),
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let mut v: Vec<_> = items.into_iter().filter(|(u, _, _)| *u > since).collect();
    v.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = v.len();
    let slice: Vec<T> = v.into_iter().skip(offset).take(limit).map(|x| x.2).collect();
    let next = (offset + slice.len() < total).then(|| format!("{since}:{}", offset + slice.len()));
    (slice, next)
}

async fn folders_sync(State(st): S, Query(q): Query<SyncQ>) -> Json<Value> {
    let st = st.lock();
    let items = st.folders.values().map(|f| (f.updated, f.uuid.clone(), st.folder_json(f))).collect();
    let (v, next) = page(items, &q);
    Json(json!({ "folders": v, "nextCursor": next }))
}

async fn files_sync(State(st): S, Query(q): Query<SyncQ>) -> Json<Value> {
    let st = st.lock();
    let items = st.files.values().map(|f| (f.updated, f.uuid.clone(), st.file_json(f))).collect();
    let (v, next) = page(items, &q);
    Json(json!({ "files": v, "nextCursor": next }))
}

#[derive(Deserialize)]
struct ContentQ {
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn content_folders(State(st): S, Path(uuid): Path<String>, Query(q): Query<ContentQ>) -> Response {
    let st = st.lock();
    if !st.live_folder(&uuid) {
        return err(StatusCode::NOT_FOUND, "folder not found");
    }
    let mut v: Vec<&Folder> = st.folders.values().filter(|f| f.parent == uuid && f.status == "EXISTS").collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    let off: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let lim = q.limit.unwrap_or(50);
    let items: Vec<Value> = v.iter().skip(off).take(lim).map(|f| st.folder_json(f)).collect();
    let next = (off + items.len() < v.len()).then(|| (off + items.len()).to_string());
    Json(json!({ "folders": items, "nextCursor": next })).into_response()
}

async fn content_files(State(st): S, Path(uuid): Path<String>, Query(q): Query<ContentQ>) -> Response {
    let st = st.lock();
    if !st.live_folder(&uuid) {
        return err(StatusCode::NOT_FOUND, "folder not found");
    }
    let mut v: Vec<&File> = st.files.values().filter(|f| f.folder == uuid && f.status == "EXISTS").collect();
    v.sort_by_key(|a| a.display_name());
    let off: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let lim = q.limit.unwrap_or(50);
    let items: Vec<Value> = v.iter().skip(off).take(lim).map(|f| st.file_json(f)).collect();
    let next = (off + items.len() < v.len()).then(|| (off + items.len()).to_string());
    Json(json!({ "files": items, "nextCursor": next })).into_response()
}

async fn create_folder(State(st): S, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let name = b["plainName"].as_str().unwrap_or_default().to_string();
    let parent = b["parentFolderUuid"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("create folder {name}"));
    if !st.live_folder(&parent) {
        return err(StatusCode::NOT_FOUND, "parent not found");
    }
    if name.is_empty() || st.name_taken(&parent, &name, "") {
        return err(StatusCode::CONFLICT, "Folder with the same name already exists");
    }
    let now = st.tick();
    let f = Folder { uuid: new_uuid(), parent, name, status: "EXISTS", created: now, updated: now };
    let j = st.folder_json(&f);
    st.folders.insert(f.uuid.clone(), f);
    (StatusCode::CREATED, Json(j)).into_response()
}

async fn rename_folder(State(st): S, Path(uuid): Path<String>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let name = b["plainName"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("rename folder {name}"));
    let Some(parent) = st.folders.get(&uuid).filter(|f| f.status == "EXISTS").map(|f| f.parent.clone()) else {
        return err(StatusCode::NOT_FOUND, "folder not found");
    };
    if st.name_taken(&parent, &name, &uuid) {
        return err(StatusCode::CONFLICT, "name taken");
    }
    let now = st.tick();
    let f = st.folders.get_mut(&uuid).unwrap();
    f.name = name;
    f.updated = now;
    let f = f.clone();
    Json(st.folder_json(&f)).into_response()
}

async fn move_folder(State(st): S, Path(uuid): Path<String>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let dest = b["destinationFolder"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("move folder {uuid}"));
    let Some(cur) = st.folders.get(&uuid).cloned() else { return err(StatusCode::NOT_FOUND, "folder not found") };
    if !st.live_folder(&dest) || st.is_descendant(&dest, &uuid) {
        return err(StatusCode::BAD_REQUEST, "bad destination");
    }
    let name = b["name"].as_str().map(str::to_string).unwrap_or(cur.name.clone());
    if st.name_taken(&dest, &name, &uuid) {
        return err(StatusCode::CONFLICT, "name taken");
    }
    let now = st.tick();
    let f = st.folders.get_mut(&uuid).unwrap();
    f.parent = dest;
    f.name = name;
    f.status = "EXISTS";
    f.updated = now;
    let f = f.clone();
    Json(st.folder_json(&f)).into_response()
}

async fn create_file(State(st): S, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let plain = b["plainName"].as_str().unwrap_or_default().to_string();
    let ty = b["type"].as_str().unwrap_or_default().to_string();
    let folder = b["folderUuid"].as_str().unwrap_or_default().to_string();
    let size = b["size"].as_u64().unwrap_or(0);
    let file_id = b["fileId"].as_str().map(str::to_string);
    let f = File {
        uuid: new_uuid(),
        folder: folder.clone(),
        plain_name: plain,
        file_type: ty,
        size,
        file_id: file_id.clone(),
        status: "EXISTS",
        created: 0,
        updated: 0,
        mtime: b["modificationTime"].as_str().map(parse_iso).unwrap_or(0),
    };
    log(&mut st, format!("create file {}", f.display_name()));
    if !st.live_folder(&folder) {
        return err(StatusCode::NOT_FOUND, "folder not found");
    }
    if st.name_taken(&folder, &f.display_name(), "") {
        return err(StatusCode::CONFLICT, "File already exists");
    }
    match (&file_id, size) {
        (None, 0) => {}
        (Some(id), s) if s > 0 && st.net.get(id).is_some_and(|n| n.data.len() as u64 == s) => {}
        _ => return err(StatusCode::BAD_REQUEST, "fileId/size mismatch"),
    }
    let now = st.tick();
    let mut f = f;
    f.created = now;
    f.updated = now;
    let j = st.file_json(&f);
    st.files.insert(f.uuid.clone(), f);
    (StatusCode::CREATED, Json(j)).into_response()
}

async fn replace_file(State(st): S, Path(uuid): Path<String>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let size = b["size"].as_u64().unwrap_or(0);
    let file_id = b["fileId"].as_str().map(str::to_string);
    log(&mut st, format!("replace file {uuid}"));
    if let Some(id) = &file_id {
        if !st.net.get(id).is_some_and(|n| n.data.len() as u64 == size) {
            return err(StatusCode::BAD_REQUEST, "fileId/size mismatch");
        }
    }
    let now = st.tick();
    let Some(f) = st.files.get_mut(&uuid).filter(|f| f.status == "EXISTS") else {
        return err(StatusCode::NOT_FOUND, "file not found");
    };
    f.file_id = file_id;
    f.size = size;
    f.updated = now;
    f.mtime = b["modificationTime"].as_str().map(parse_iso).unwrap_or(now);
    let f = f.clone();
    Json(st.file_json(&f)).into_response()
}

async fn rename_file(State(st): S, Path(uuid): Path<String>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let plain = b["plainName"].as_str().unwrap_or_default().to_string();
    let ty = b["type"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("rename file {plain}.{ty}"));
    let Some(cur) = st.files.get(&uuid).filter(|f| f.status == "EXISTS").cloned() else {
        return err(StatusCode::NOT_FOUND, "file not found");
    };
    let probe = File { plain_name: plain.clone(), file_type: ty.clone(), ..cur.clone() };
    if st.name_taken(&cur.folder, &probe.display_name(), &uuid) {
        return err(StatusCode::CONFLICT, "name taken");
    }
    let now = st.tick();
    let f = st.files.get_mut(&uuid).unwrap();
    f.plain_name = plain;
    f.file_type = ty;
    f.updated = now;
    let f = f.clone();
    Json(st.file_json(&f)).into_response()
}

async fn move_file(State(st): S, Path(uuid): Path<String>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let dest = b["destinationFolder"].as_str().unwrap_or_default().to_string();
    log(&mut st, format!("move file {uuid}"));
    let Some(cur) = st.files.get(&uuid).cloned() else { return err(StatusCode::NOT_FOUND, "file not found") };
    if !st.live_folder(&dest) {
        return err(StatusCode::NOT_FOUND, "destination not found");
    }
    let plain = b["name"].as_str().map(str::to_string).unwrap_or(cur.plain_name.clone());
    let ty = if b.get("name").is_some() { b["type"].as_str().unwrap_or_default().to_string() } else { cur.file_type.clone() };
    let probe = File { plain_name: plain.clone(), file_type: ty.clone(), ..cur.clone() };
    if st.name_taken(&dest, &probe.display_name(), &uuid) {
        return err(StatusCode::CONFLICT, "name taken");
    }
    let now = st.tick();
    let f = st.files.get_mut(&uuid).unwrap();
    f.folder = dest;
    f.plain_name = plain;
    f.file_type = ty;
    f.status = "EXISTS";
    f.updated = now;
    let f = f.clone();
    Json(st.file_json(&f)).into_response()
}

async fn trash(State(st): S, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let items = b["items"].as_array().cloned().unwrap_or_default();
    for it in items {
        let uuid = it["uuid"].as_str().unwrap_or_default().to_string();
        log(&mut st, format!("trash {} {uuid}", it["type"].as_str().unwrap_or("?")));
        if it["type"] == "folder" {
            st.trash_folder(&uuid);
        } else {
            let now = st.tick();
            if let Some(f) = st.files.get_mut(&uuid) {
                f.status = "TRASHED";
                f.updated = now;
            }
        }
    }
    StatusCode::OK.into_response()
}

async fn net_info(State(st): S, Path((_bucket, id)): Path<(String, String)>) -> Response {
    let st = st.lock();
    let Some(n) = st.net.get(&id) else { return err(StatusCode::NOT_FOUND, "no such file") };
    Json(json!({
        "bucket": BUCKET, "index": n.index, "size": n.data.len(), "version": 2, "id": id,
        "shards": [{ "index": 0, "hash": n.hash, "url": format!("{}/blob/{id}", st.base) }],
    }))
    .into_response()
}

async fn blob(State(st): S, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let st = st.lock();
    let Some(n) = st.net.get(&id) else { return err(StatusCode::NOT_FOUND, "no such blob") };
    let len = n.data.len();
    if let Some(r) = headers.get("range").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes=")) {
        let mut it = r.split('-');
        let a: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let b: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(len.saturating_sub(1)).min(len.saturating_sub(1));
        if a >= len || a > b {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        return (StatusCode::PARTIAL_CONTENT, n.data[a..=b].to_vec()).into_response();
    }
    (StatusCode::OK, n.data.clone()).into_response()
}

#[derive(Deserialize)]
struct StartQ {
    multiparts: Option<u32>,
}

async fn start_upload(State(st): S, Query(q): Query<StartQ>, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let size = b["uploads"][0]["size"].as_u64().unwrap_or(0);
    let parts = q.multiparts.unwrap_or(1).max(1);
    let slot = new_uuid();
    st.slots.insert(slot.clone(), Slot { size, parts: BTreeMap::new() });
    let base = st.base.clone();
    let upload = if parts > 1 {
        let urls: Vec<String> = (1..=parts).map(|p| format!("{base}/upload/{slot}/{p}")).collect();
        json!({ "index": 0, "uuid": slot, "urls": urls, "UploadId": format!("mpu-{slot}") })
    } else {
        json!({ "index": 0, "uuid": slot, "url": format!("{base}/upload/{slot}") })
    };
    Json(json!({ "uploads": [upload] })).into_response()
}

async fn upload_single(State(st): S, Path(slot): Path<String>, body: Bytes) -> Response {
    let mut st = st.lock();
    let Some(s) = st.slots.get_mut(&slot) else { return err(StatusCode::NOT_FOUND, "no slot") };
    s.parts.insert(1, body.to_vec());
    ([("ETag", "\"etag-1\"")], StatusCode::OK).into_response()
}

async fn upload_part(State(st): S, Path((slot, part)): Path<(String, u32)>, body: Bytes) -> Response {
    let mut st = st.lock();
    let Some(s) = st.slots.get_mut(&slot) else { return err(StatusCode::NOT_FOUND, "no slot") };
    s.parts.insert(part, body.to_vec());
    let etag = format!("\"etag-{part}\"");
    ([("ETag", etag)], StatusCode::OK).into_response()
}

async fn finish_upload(State(st): S, Json(b): Json<Value>) -> Response {
    let mut st = st.lock();
    let index = b["index"].as_str().unwrap_or_default().to_string();
    let shard = &b["shards"][0];
    let slot_id = shard["uuid"].as_str().unwrap_or_default().to_string();
    let Some(slot) = st.slots.remove(&slot_id) else { return err(StatusCode::NOT_FOUND, "no slot") };
    if let Some(parts) = shard["parts"].as_array() {
        for (i, p) in parts.iter().enumerate() {
            let n = p["PartNumber"].as_u64().unwrap_or(0) as u32;
            if n != i as u32 + 1 || p["ETag"].as_str() != Some(format!("etag-{n}").as_str()) {
                return err(StatusCode::BAD_REQUEST, "bad part list");
            }
        }
    }
    let data: Vec<u8> = slot.parts.into_values().flatten().collect();
    if data.len() as u64 != slot.size {
        return err(StatusCode::BAD_REQUEST, "size mismatch");
    }
    let hash = hex::encode(Ripemd160::digest(Sha256::digest(&data)));
    if shard["hash"].as_str() != Some(hash.as_str()) {
        return err(StatusCode::BAD_REQUEST, "hash mismatch");
    }
    let id = hex::encode(rand::random::<[u8; 12]>());
    st.net.insert(id.clone(), NetFile { index: index.clone(), hash, data });
    Json(json!({ "id": id, "index": index, "bucket": BUCKET })).into_response()
}
