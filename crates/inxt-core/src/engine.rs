use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use inxt_api::types::BucketFileInfo;
use inxt_api::{ApiError, Client};
use parking_lot::{Mutex, RwLock};
use rusqlite::params;
use tokio::sync::Notify;

use crate::cache::{Cache, CacheRow, Claim, ContentKey, Entry, Need, BLOCK};
use crate::names::{self, LocalOnlyRules};
use crate::push::PushState;
use crate::remote::Orphans;
use crate::tree::{Node, NodeId, Tree, ROOT};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Per-account directory holding the database and the cache.
    pub data_dir: PathBuf,
    pub cache_dir: Option<PathBuf>,
    pub cache_max_bytes: u64,
    pub upload_concurrency: usize,
    pub poll_interval: Duration,
    /// Quiet period after a local change before it is pushed.
    pub push_debounce: Duration,
    pub local_only: LocalOnlyRules,
}

impl EngineConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            cache_dir: None,
            cache_max_bytes: 10 << 30,
            upload_concurrency: 4,
            poll_interval: Duration::from_secs(30),
            push_debounce: Duration::from_millis(1500),
            local_only: LocalOnlyRules::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotEmpty,
    NotADirectory,
    IsADirectory,
    InvalidName,
    AccessDenied,
    Io,
    Network,
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub id: NodeId,
    pub is_dir: bool,
    pub size: u64,
    pub ctime: i64,
    pub mtime: i64,
    pub attrs: u32,
    /// Content is fully available offline.
    pub cached: bool,
    pub pinned: bool,
    /// Not in the cloud yet: new, edited, moved or renamed and not pushed
    /// (for folders: something inside is).
    pub pending: bool,
    /// Matches the local-only rules and is never uploaded.
    pub excluded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transfer {
    pub path: String,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    pub path: String,
    pub size: u64,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    pub path: String,
    pub message: String,
    pub at: i64,
}

/// What the sync engine is doing, for the activity view.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Activity {
    pub uploading: Vec<Transfer>,
    pub queued: Vec<String>,
    pub queued_total: usize,
    pub recent: Vec<Finished>,
    pub problems: Vec<Problem>,
}

#[derive(Default)]
pub(crate) struct ActivityState {
    pub uploading: HashMap<NodeId, Transfer>,
    pub queued: Vec<String>,
    pub queued_total: usize,
    pub recent: VecDeque<Finished>,
    pub problems: VecDeque<Problem>,
}

const ACTIVITY_KEEP: usize = 30;

impl ActivityState {
    pub fn finished(&mut self, id: NodeId, path: String, size: u64) {
        self.uploading.remove(&id);
        self.problems.retain(|p| p.path != path);
        self.recent.push_front(Finished { path, size, at: crate::now_ms() });
        self.recent.truncate(ACTIVITY_KEEP);
    }

    pub fn failed(&mut self, id: NodeId, path: String, message: String) {
        self.uploading.remove(&id);
        self.problems.retain(|p| p.path != path);
        self.problems.push_front(Problem { path, message, at: crate::now_ms() });
        self.problems.truncate(ACTIVITY_KEEP);
    }
}

#[derive(Debug, Clone)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Renamed { old_path: String },
}

/// A change that arrived from the cloud, to be announced to the OS.
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub is_dir: bool,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub online: bool,
    pub auth_failed: bool,
    pub initial_sync_done: bool,
    pub items: usize,
    pub pending_uploads: usize,
    pub pending_ops: usize,
    pub uploading: usize,
    pub upload_bytes_total: u64,
    pub upload_bytes_done: u64,
    pub downloads_active: usize,
    pub cache_bytes: u64,
    pub last_error: Option<String>,
    pub last_remote_sync: Option<i64>,
}

pub struct Engine {
    pub(crate) cfg: EngineConfig,
    pub(crate) client: Arc<Client>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) tree: RwLock<Tree>,
    pub(crate) cache: Cache,
    info_cache: Mutex<HashMap<String, (Instant, Arc<BucketFileInfo>)>>,
    pub(crate) push_signal: Notify,
    pub(crate) poll_signal: Notify,
    changes: Mutex<Vec<Change>>,
    pub(crate) status: Mutex<Status>,
    volume: Mutex<(u64, u64)>,
    pub(crate) last_poll: Mutex<Option<Instant>>,
    reads: Mutex<HashMap<NodeId, (u64, u64)>>,
    pub(crate) sync_lock: tokio::sync::Mutex<()>,
    fetch_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) stopping: AtomicBool,
    pub(crate) paused: AtomicBool,
    pub(crate) push_state: Mutex<PushState>,
    pub(crate) orphans: Mutex<Orphans>,
    /// Folders with something inside that is not in the cloud yet.
    pub(crate) pending_dirs: RwLock<HashSet<NodeId>>,
    pub(crate) activity: Mutex<ActivityState>,
    cache_max: AtomicU64,
    pub(crate) upload_slots: AtomicUsize,
    pub(crate) poll_ms: AtomicU64,
    weak: std::sync::Weak<Engine>,
}

const INFO_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_FETCH_RUN: u64 = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(180);

fn io_err(e: std::io::Error) -> FsError {
    tracing::warn!(error = %e, "cache io error");
    FsError::Io
}

pub(crate) fn split_path(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches(['\\', '/']);
    match p.rfind(['\\', '/']) {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    }
}

impl Engine {
    pub fn open(cfg: EngineConfig, client: Arc<Client>, rt: tokio::runtime::Handle) -> anyhow::Result<Arc<Self>> {
        crate::platform::private_dir(&cfg.data_dir)?;
        let creds = client.credentials()?;
        let tree = Tree::open(&cfg.data_dir.join("meta.db"), &creds.root_folder_uuid)?;
        let cache_dir = cfg.cache_dir.clone().unwrap_or_else(|| cfg.data_dir.join("cache"));
        let cache = Cache::new(&cache_dir)?;

        let rows = tree.with_db(|db| -> rusqlite::Result<Vec<CacheRow>> {
            let mut st = db.prepare("SELECT node, content_key, size, bitmap, last_access FROM cache")?;
            let it = st.query_map([], |r| {
                Ok(CacheRow {
                    node: r.get::<_, i64>(0)? as u64,
                    key: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    bitmap: r.get(3)?,
                    last_access: r.get(4)?,
                })
            })?;
            it.collect()
        })?;
        let rows: Vec<CacheRow> = rows.into_iter().filter(|r| tree.get(r.node).is_some()).collect();
        let stale = cache.restore(rows);
        tree.with_db(|db| {
            for s in &stale {
                let _ = db.execute("DELETE FROM cache WHERE node = ?1", params![*s as i64]);
            }
            let _ = db.execute("DELETE FROM cache WHERE node NOT IN (SELECT id FROM nodes)", []);
        });

        let engine = Arc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            pending_dirs: RwLock::new(HashSet::new()),
            activity: Mutex::new(ActivityState::default()),
            cache_max: AtomicU64::new(cfg.cache_max_bytes),
            upload_slots: AtomicUsize::new(cfg.upload_concurrency.max(1)),
            poll_ms: AtomicU64::new(cfg.poll_interval.as_millis() as u64),
            cfg,
            client,
            rt,
            tree: RwLock::new(tree),
            cache,
            info_cache: Mutex::new(HashMap::new()),
            push_signal: Notify::new(),
            poll_signal: Notify::new(),
            changes: Mutex::new(Vec::new()),
            status: Mutex::new(Status { online: true, ..Default::default() }),
            volume: Mutex::new((0, 0)),
            last_poll: Mutex::new(None),
            reads: Mutex::new(HashMap::new()),
            sync_lock: tokio::sync::Mutex::new(()),
            fetch_slots: Arc::new(tokio::sync::Semaphore::new(6)),
            stopping: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            push_state: Mutex::new(PushState::default()),
            orphans: Mutex::new(Orphans::default()),
        });
        engine.reconcile_cache();
        Ok(engine)
    }

    /// Cache entries that hold local-only content must belong to dirty nodes.
    /// Repairs the state after a crash between a write and its bookkeeping.
    fn reconcile_cache(&self) {
        let mut t = self.tree.write();
        for id in self.cache.ids() {
            let Some(entry) = self.cache.get(id) else { continue };
            let (key, size) = {
                let st = entry.state.lock();
                (st.key.clone(), st.size)
            };
            let Some(n) = t.get(id).cloned() else {
                self.cache.remove(id);
                continue;
            };
            if n.is_dir {
                self.cache.remove(id);
                continue;
            }
            if key == ContentKey::Local && !n.is_dirty() {
                tracing::info!(node = id, "recovering unsynced local content");
                t.modify(id, |n| {
                    n.content_gen += 1;
                    n.size = size;
                    n.changed_at = crate::now_ms();
                });
            }
        }
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        self.rt.spawn(async move { me.poll_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.push_loop().await });
        let me = self.clone();
        self.rt.spawn(async move { me.maintenance_loop().await });
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.push_signal.notify_waiters();
        self.poll_signal.notify_waiters();
        self.persist_cache_rows();
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Pauses uploads and remote polling; reads of uncached files still work.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        if !paused {
            self.push_signal.notify_one();
            self.poll_signal.notify_one();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn set_cache_limit(&self, bytes: u64) {
        self.cache_max.store(bytes.max(1 << 30), Ordering::SeqCst);
    }

    pub fn set_upload_concurrency(&self, n: usize) {
        self.upload_slots.store(n.clamp(1, 8), Ordering::SeqCst);
        self.push_signal.notify_one();
    }

    pub fn set_poll_interval(&self, every: Duration) {
        self.poll_ms.store(every.as_millis().clamp(5_000, 3_600_000) as u64, Ordering::SeqCst);
    }

    pub fn activity(&self) -> Activity {
        let a = self.activity.lock();
        let mut uploading: Vec<Transfer> = a.uploading.values().cloned().collect();
        uploading.sort_by(|x, y| x.path.cmp(&y.path));
        Activity {
            uploading,
            queued: a.queued.clone(),
            queued_total: a.queued_total,
            recent: a.recent.iter().cloned().collect(),
            problems: a.problems.iter().cloned().collect(),
        }
    }

    /// Cloud identity of an item: `(uuid, is_dir)`, once it has been uploaded.
    pub fn remote_id(&self, path: &str) -> Result<Option<(String, bool)>, FsError> {
        let t = self.tree.read();
        let id = t.resolve(path).ok_or(FsError::NotFound)?;
        let n = t.get(id).ok_or(FsError::NotFound)?;
        if t.is_local_only(id, &self.cfg.local_only) {
            return Err(FsError::AccessDenied);
        }
        let synced = n.uuid.is_some() && (n.is_dir || !n.is_dirty());
        Ok(synced.then(|| (n.uuid.clone().unwrap_or_default(), n.is_dir)))
    }

    pub fn path_of_uuid(&self, uuid: &str) -> Option<String> {
        let t = self.tree.read();
        t.by_uuid(uuid).filter(|id| t.get(*id).is_some_and(|n| !n.deleted)).and_then(|id| t.path_of(id))
    }

    pub(crate) fn notify_modified(&self, ids: &[NodeId]) {
        let t = self.tree.read();
        let changes: Vec<Change> = ids
            .iter()
            .filter_map(|id| {
                let n = t.get(*id)?;
                if n.deleted {
                    return None;
                }
                Some(Change { path: t.path_of(*id)?, is_dir: n.is_dir, kind: ChangeKind::Modified })
            })
            .collect();
        drop(t);
        self.push_changes(changes);
    }

    /// Call after the user signed in again following an expired session.
    pub fn reset_auth(&self) {
        let mut s = self.status.lock();
        s.auth_failed = false;
        s.last_error = None;
        drop(s);
        self.push_signal.notify_one();
        self.poll_signal.notify_one();
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    pub fn status(&self) -> Status {
        let mut s = self.status.lock().clone();
        s.items = self.tree.read().len();
        s.cache_bytes = self.cache.total_bytes();
        s
    }

    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut *self.changes.lock())
    }

    pub(crate) fn push_changes(&self, mut c: Vec<Change>) {
        if !c.is_empty() {
            let mut q = self.changes.lock();
            q.append(&mut c);
            if q.len() > 5000 {
                let drop_n = q.len() - 5000;
                q.drain(..drop_n);
            }
        }
    }

    pub fn volume_info(&self) -> (u64, u64) {
        let (total, used) = *self.volume.lock();
        if total == 0 {
            let fallback = 1u64 << 40;
            return (fallback, fallback.saturating_sub(used));
        }
        (total, total.saturating_sub(used))
    }

    pub(crate) async fn refresh_volume(&self) {
        let (limit, usage) = tokio::join!(self.client.limit_bytes(), self.client.usage_bytes());
        if let (Ok(l), Ok(u)) = (limit, usage) {
            *self.volume.lock() = (l, u);
        }
    }

    // ---- attributes and lookups -------------------------------------------------

    fn attr_of(&self, t: &Tree, n: &Node) -> Attr {
        let cached = if n.is_dir || n.size == 0 {
            true
        } else {
            self.cache.get(n.id).is_some_and(|e| e.is_complete())
        };
        let excluded = n.id != ROOT && t.is_local_only(n.id, &self.cfg.local_only);
        let pending = !excluded
            && n.id != ROOT
            && (n.uuid.is_none()
                || n.r_trashed
                || (!n.is_dir && n.is_dirty())
                || Self::placement_pending(t, n)
                || (n.is_dir && self.pending_dirs.read().contains(&n.id)));
        Attr {
            id: n.id,
            is_dir: n.is_dir,
            size: n.size,
            ctime: n.ctime,
            mtime: n.mtime,
            attrs: n.attrs,
            cached,
            pinned: n.pinned,
            pending,
            excluded,
        }
    }

    /// Renamed or moved locally and not yet pushed.
    pub(crate) fn placement_pending(t: &Tree, n: &Node) -> bool {
        let Some(parent_uuid) = t.get(n.parent).and_then(|p| p.uuid.as_deref()) else { return false };
        n.uuid.is_some()
            && (n.r_parent.as_deref() != Some(parent_uuid)
                || n.r_name.as_deref() != Some(names::to_remote(&n.name).as_str()))
    }

    pub fn lookup(&self, path: &str) -> Option<Attr> {
        let t = self.tree.read();
        let id = t.resolve(path)?;
        t.get(id).map(|n| self.attr_of(&t, n))
    }

    pub fn attr(&self, id: NodeId) -> Option<Attr> {
        let t = self.tree.read();
        t.get(id).filter(|n| !n.deleted).map(|n| self.attr_of(&t, n))
    }

    pub fn path_of(&self, id: NodeId) -> Option<String> {
        self.tree.read().path_of(id)
    }

    pub fn list(&self, dir: NodeId) -> Result<Vec<(String, Attr)>, FsError> {
        let out = {
            let t = self.tree.read();
            let n = t.get(dir).ok_or(FsError::NotFound)?;
            if !n.is_dir {
                return Err(FsError::NotADirectory);
            }
            t.children(dir)
                .into_iter()
                .filter_map(|c| t.get(c))
                .map(|c| (c.name.clone(), self.attr_of(&t, c)))
                .collect::<Vec<_>>()
        };
        let stale = self.last_poll.lock().is_none_or(|t| t.elapsed() > Duration::from_secs(15));
        if stale {
            self.poll_signal.notify_one();
        }
        Ok(out)
    }

    // ---- namespace mutations ------------------------------------------------------

    pub fn create(&self, path: &str, is_dir: bool) -> Result<Attr, FsError> {
        let (parent_path, name) = split_path(path);
        if !names::is_valid_local(name) {
            return Err(FsError::InvalidName);
        }
        let attr = {
            let mut t = self.tree.write();
            let parent = t.resolve(parent_path).ok_or(FsError::NotFound)?;
            if !t.get(parent).is_some_and(|p| p.is_dir) {
                return Err(FsError::NotADirectory);
            }
            if t.child(parent, name).is_some() {
                return Err(FsError::AlreadyExists);
            }
            let now = crate::now_ms();
            let id = t.insert(Node {
                parent,
                name: name.to_string(),
                is_dir,
                ctime: now,
                mtime: now,
                content_gen: if is_dir { 0 } else { 1 },
                changed_at: now,
                ..Default::default()
            });
            if !is_dir {
                self.cache.create_local(id, 0).map_err(io_err)?;
            }
            self.attr_of(&t, t.get(id).expect("just inserted"))
        };
        self.push_signal.notify_one();
        Ok(attr)
    }

    pub fn open_handle(&self, id: NodeId, write: bool) {
        self.tree.write().modify_runtime(id, |n| {
            n.open_handles += 1;
            if write {
                n.open_writers += 1;
            }
        });
    }

    /// The last user handle to the file was closed.
    pub fn release(&self, id: NodeId, was_writer: bool, modified: bool) {
        let mut t = self.tree.write();
        t.modify_runtime(id, |n| {
            n.open_handles = n.open_handles.saturating_sub(1);
            if was_writer {
                n.open_writers = n.open_writers.saturating_sub(1);
            }
        });
        if modified && t.get(id).is_some() {
            t.modify(id, |n| n.changed_at = crate::now_ms());
            drop(t);
            self.push_signal.notify_one();
        }
    }

    pub fn rename(&self, id: NodeId, new_path: &str, replace: bool) -> Result<(), FsError> {
        let (parent_path, name) = split_path(new_path);
        if !names::is_valid_local(name) {
            return Err(FsError::InvalidName);
        }
        let mut purge_cache = Vec::new();
        {
            let mut t = self.tree.write();
            if id == ROOT {
                return Err(FsError::AccessDenied);
            }
            let new_parent = t.resolve(parent_path).ok_or(FsError::NotFound)?;
            if !t.get(new_parent).is_some_and(|p| p.is_dir) {
                return Err(FsError::NotADirectory);
            }
            if t.is_ancestor(id, new_parent) {
                return Err(FsError::AccessDenied);
            }
            if let Some(existing) = t.child(new_parent, name) {
                if existing != id {
                    if !replace {
                        return Err(FsError::AlreadyExists);
                    }
                    let ex = t.get(existing).cloned().ok_or(FsError::NotFound)?;
                    if ex.is_dir {
                        return Err(FsError::AccessDenied);
                    }
                    if ex.open_handles > 0 {
                        return Err(FsError::AccessDenied);
                    }
                    self.delete_locked(&mut t, existing, &mut purge_cache);
                }
            }
            let now = crate::now_ms();
            t.modify(id, |n| {
                n.parent = new_parent;
                n.name = name.to_string();
                n.changed_at = now;
            });
        }
        for c in purge_cache {
            self.cache.remove(c);
        }
        self.push_signal.notify_one();
        Ok(())
    }

    pub fn can_delete(&self, id: NodeId) -> Result<(), FsError> {
        if id == ROOT {
            return Err(FsError::AccessDenied);
        }
        let t = self.tree.read();
        let n = t.get(id).ok_or(FsError::NotFound)?;
        if n.is_dir && t.has_children(id) {
            return Err(FsError::NotEmpty);
        }
        Ok(())
    }

    pub fn delete(&self, id: NodeId) -> Result<(), FsError> {
        self.can_delete(id)?;
        let mut purge_cache = Vec::new();
        {
            let mut t = self.tree.write();
            self.delete_locked(&mut t, id, &mut purge_cache);
        }
        for c in purge_cache {
            self.cache.remove(c);
        }
        self.push_signal.notify_one();
        Ok(())
    }

    fn delete_locked(&self, t: &mut Tree, id: NodeId, purge_cache: &mut Vec<NodeId>) {
        let Some(n) = t.get(id).cloned() else { return };
        if n.uuid.is_none() {
            purge_cache.extend(t.purge(id));
        } else {
            let now = crate::now_ms();
            t.modify(id, |n| {
                n.deleted = true;
                n.changed_at = now;
            });
            purge_cache.push(id);
        }
    }

    pub fn set_times(&self, id: NodeId, ctime: Option<i64>, mtime: Option<i64>) {
        if ctime.is_none() && mtime.is_none() {
            return;
        }
        self.tree.write().modify(id, |n| {
            if let Some(c) = ctime {
                n.ctime = c;
            }
            if let Some(m) = mtime {
                n.mtime = m;
            }
        });
    }

    pub fn set_attrs(&self, id: NodeId, attrs: u32) {
        self.tree.write().modify(id, |n| n.attrs = attrs);
    }

    pub fn flush(&self, id: NodeId) {
        let mut t = self.tree.write();
        if t.get(id).is_some() {
            t.modify(id, |_| {});
        }
    }

    // ---- content ----------------------------------------------------------------

    pub(crate) async fn file_info(&self, file_id: &str, force: bool) -> Result<Arc<BucketFileInfo>, ApiError> {
        if !force {
            if let Some((at, info)) = self.info_cache.lock().get(file_id) {
                if at.elapsed() < INFO_TTL {
                    return Ok(info.clone());
                }
            }
        }
        let info = Arc::new(self.client.bucket_file_info(file_id).await?);
        self.info_cache.lock().insert(file_id.to_string(), (Instant::now(), info.clone()));
        Ok(info)
    }

    fn entry_for(&self, id: NodeId) -> Result<(Arc<Entry>, Option<String>, u64), FsError> {
        let (file_id, size, is_dir) = {
            let t = self.tree.read();
            let n = t.get(id).ok_or(FsError::NotFound)?;
            (n.file_id.clone(), n.size, n.is_dir)
        };
        if is_dir {
            return Err(FsError::IsADirectory);
        }
        if let Some(e) = self.cache.get(id) {
            if e.state.lock().key == ContentKey::Local {
                return Ok((e, None, size));
            }
        }
        match &file_id {
            Some(fid) if size > 0 => {
                let e = self.cache.for_remote(id, ContentKey::remote(fid, size), size).map_err(io_err)?;
                if e.state.lock().key == ContentKey::Local {
                    return Ok((e, None, size));
                }
                Ok((e, file_id, size))
            }
            _ => {
                let e = self.cache.create_local(id, 0).map_err(io_err)?;
                Ok((e, None, 0))
            }
        }
    }

    fn readahead_blocks(&self, id: NodeId, start: u64, len: u64) -> u64 {
        let mut reads = self.reads.lock();
        let entry = reads.entry(id).or_insert((u64::MAX, 2));
        let window = if entry.0 == start { (entry.1 * 2).min(32) } else { 2 };
        *entry = (start + len, window);
        if reads.len() > 1024 {
            reads.clear();
        }
        window
    }

    /// Makes `[start, start+len)` of a remote file available in the cache.
    fn ensure_range(&self, entry: &Arc<Entry>, file_id: &str, start: u64, len: u64, readahead: u64) -> Result<(), FsError> {
        let mut last_err = String::new();
        for _attempt in 0..3 {
            match entry.need(start, len, readahead) {
                Need::Ready => return Ok(()),
                Need::Wait => {}
                Need::Fetch(claims) => {
                    for c in claims {
                        let mut first = c.first;
                        let end = c.first + c.count;
                        while first < end {
                            let count = (end - first).min(MAX_FETCH_RUN);
                            self.spawn_fetch(entry.clone(), file_id.to_string(), Claim { first, count });
                            first += count;
                        }
                    }
                }
            }
            match entry.wait(start, len, READ_TIMEOUT) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = e,
            }
        }
        tracing::warn!(node = entry.node, error = %last_err, "read failed");
        self.status.lock().last_error = Some(format!("download failed: {last_err}"));
        Err(FsError::Network)
    }

    fn spawn_fetch(&self, entry: Arc<Entry>, file_id: String, claim: Claim) {
        let client = self.client.clone();
        let slots = self.fetch_slots.clone();
        let me = self.self_ref();
        self.rt.spawn(async move {
            let _permit = slots.acquire_owned().await;
            let Some(me) = me.upgrade() else { return };
            me.status.lock().downloads_active += 1;
            let size = entry.state.lock().size;
            let (start, len) = claim.byte_range(size);
            let mut result = Err(ApiError::Other("not started".into()));
            for force in [false, true] {
                let info = match me.file_info(&file_id, force).await {
                    Ok(i) => i,
                    Err(e) => {
                        result = Err(e);
                        continue;
                    }
                };
                let e2 = entry.clone();
                let run_end = start + len;
                let mut sink = move |off: u64, data: &[u8]| e2.fill(off, data, run_end);
                result = client.download_range(&info, start, len, &mut sink).await;
                match &result {
                    Ok(()) => break,
                    Err(e) if matches!(e.status(), Some(403) | Some(404)) => continue,
                    Err(_) => break,
                }
            }
            {
                let mut s = me.status.lock();
                s.downloads_active = s.downloads_active.saturating_sub(1);
                match &result {
                    Ok(()) => s.online = true,
                    Err(e) if e.is_transient() => s.online = false,
                    _ => {}
                }
            }
            if let Err(e) = result {
                entry.abandon(&[claim], e.to_string());
            } else {
                let st = entry.state.lock();
                let missing = (claim.first..claim.first + claim.count).any(|b| !st.has(b) && b * BLOCK < st.size);
                let complete = st.is_complete();
                drop(st);
                if missing {
                    entry.abandon(&[claim], "incomplete download".into());
                } else if complete {
                    me.notify_modified(&[entry.node]);
                }
            }
        });
    }

    pub fn read(&self, id: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let (entry, file_id, size) = self.entry_for(id)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(size - offset);
        if let Some(fid) = &file_id {
            let ra = self.readahead_blocks(id, offset, len);
            self.ensure_range(&entry, fid, offset, len, ra)?;
        }
        entry.read_at(offset, &mut buf[..len as usize]).map_err(io_err)
    }

    /// Ensures the whole file is local and authoritative, ready for edits.
    fn make_local(&self, id: NodeId) -> Result<Arc<Entry>, FsError> {
        let (entry, file_id, size) = self.entry_for(id)?;
        if let Some(fid) = file_id {
            self.ensure_range(&entry, &fid, 0, size, 0)?;
            entry.mark_local();
            self.tree.write().modify(id, |n| {
                if n.base_file_id.is_none() {
                    n.base_file_id = n.file_id.clone();
                }
            });
        }
        Ok(entry)
    }

    fn bump_content(&self, id: NodeId, new_size: u64) {
        let now = crate::now_ms();
        let mut t = self.tree.write();
        let was_clean = t.get(id).is_some_and(|n| !n.is_dirty());
        let apply = |n: &mut Node| {
            n.size = new_size;
            n.mtime = now;
            n.content_gen += 1;
            n.changed_at = now;
        };
        if was_clean {
            t.modify(id, apply);
        } else {
            t.modify_runtime(id, apply);
        }
    }

    pub fn write(&self, id: NodeId, offset: u64, data: &[u8], to_eof: bool) -> Result<(usize, u64), FsError> {
        let entry = self.make_local(id)?;
        let offset = if to_eof { entry.state.lock().size } else { offset };
        entry.write_at(offset, data).map_err(io_err)?;
        let size = entry.state.lock().size;
        self.bump_content(id, size);
        Ok((data.len(), size))
    }

    pub fn set_size(&self, id: NodeId, new_size: u64) -> Result<(), FsError> {
        let current = self.tree.read().get(id).map(|n| n.size).ok_or(FsError::NotFound)?;
        if new_size == current && self.cache.get(id).is_some_and(|e| e.state.lock().key == ContentKey::Local) {
            return Ok(());
        }
        if new_size == 0 {
            let open = self.cache.get(id).is_some();
            if open {
                self.cache.remove(id);
            }
            self.cache.create_local(id, 0).map_err(io_err)?;
            self.tree.write().modify(id, |n| {
                if n.base_file_id.is_none() {
                    n.base_file_id = n.file_id.clone();
                }
            });
        } else {
            let entry = self.make_local(id)?;
            entry.set_len(new_size).map_err(io_err)?;
        }
        self.bump_content(id, new_size);
        Ok(())
    }

    // ---- offline pinning -----------------------------------------------------------

    pub fn set_pinned(self: &Arc<Self>, path: &str, pinned: bool) -> Result<usize, FsError> {
        let ids = {
            let mut t = self.tree.write();
            let id = t.resolve(path).ok_or(FsError::NotFound)?;
            let mut ids = t.descendants(id);
            ids.push(id);
            for i in &ids {
                t.modify(*i, |n| n.pinned = pinned);
            }
            ids
        };
        if pinned {
            let me = self.clone();
            let files: Vec<NodeId> = ids.clone();
            self.rt.spawn_blocking(move || {
                for id in files {
                    if me.is_stopping() {
                        break;
                    }
                    let Ok((entry, Some(fid), size)) = me.entry_for(id) else { continue };
                    let _ = me.ensure_range(&entry, &fid, 0, size, 0);
                }
            });
        }
        Ok(ids.len())
    }

    /// Drops cached content under `path` that is safely stored in the cloud.
    pub fn free_up(&self, path: &str) -> Result<u64, FsError> {
        let ids = {
            let t = self.tree.read();
            let id = t.resolve(path).ok_or(FsError::NotFound)?;
            let mut ids = t.descendants(id);
            ids.push(id);
            ids
        };
        let mut freed = 0;
        let mut removed = Vec::new();
        for id in ids {
            if self.evictable(id) {
                if let Some(e) = self.cache.get(id) {
                    freed += e.state.lock().cached_bytes();
                    removed.push(id);
                }
                self.cache.remove(id);
            }
        }
        self.persist_cache_rows();
        self.notify_modified(&removed);
        Ok(freed)
    }

    pub(crate) fn evictable(&self, id: NodeId) -> bool {
        let t = self.tree.read();
        let Some(n) = t.get(id) else { return true };
        if n.is_dirty() || n.open_handles > 0 || n.pinned || n.uuid.is_none() {
            return false;
        }
        if t.is_local_only(id, &self.cfg.local_only) {
            return false;
        }
        self.cache.get(id).is_none_or(|e| e.state.lock().key != ContentKey::Local)
    }

    pub(crate) fn persist_cache_rows(&self) {
        let rows = self.cache.take_dirty_rows();
        let live: std::collections::HashSet<NodeId> = self.cache.ids().into_iter().collect();
        let t = self.tree.read();
        t.with_db(|db| {
            let _ = db.execute_batch("BEGIN");
            for r in rows {
                let _ = db.execute(
                    "INSERT OR REPLACE INTO cache (node, content_key, size, block_size, bitmap, last_access) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![r.node as i64, r.key, r.size as i64, BLOCK as i64, r.bitmap, r.last_access],
                );
            }
            if let Ok(mut st) = db.prepare("SELECT node FROM cache") {
                let gone: Vec<i64> = st
                    .query_map([], |r| r.get::<_, i64>(0))
                    .map(|it| it.flatten().filter(|n| !live.contains(&(*n as u64))).collect())
                    .unwrap_or_default();
                for g in gone {
                    let _ = db.execute("DELETE FROM cache WHERE node = ?1", params![g]);
                }
            }
            let _ = db.execute_batch("COMMIT");
        });
    }

    fn evict_if_needed(&self) {
        let max = self.cache_max.load(Ordering::SeqCst);
        let mut total = self.cache.total_bytes();
        if total <= max {
            return;
        }
        let target = max - max / 10;
        let mut removed = Vec::new();
        for (id, bytes, _) in self.cache.lru() {
            if total <= target {
                break;
            }
            if self.evictable(id) {
                self.cache.remove(id);
                removed.push(id);
                total = total.saturating_sub(bytes);
            }
        }
        self.persist_cache_rows();
        self.notify_modified(&removed);
    }

    async fn maintenance_loop(self: Arc<Self>) {
        self.refresh_volume().await;
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if self.is_stopping() {
                break;
            }
            tick += 1;
            let me = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                me.persist_cache_rows();
                me.evict_if_needed();
            })
            .await;
            if tick.is_multiple_of(30) {
                self.refresh_volume().await;
            }
            if tick.is_multiple_of(360) {
                if let Err(e) = self.client.ensure_fresh(chrono::Duration::days(3)).await {
                    if e.is_unauthorized() {
                        self.status.lock().auth_failed = true;
                    }
                    tracing::warn!(error = %e, "token refresh failed");
                }
            }
        }
    }

    fn self_ref(&self) -> std::sync::Weak<Engine> {
        self.weak.clone()
    }
}
