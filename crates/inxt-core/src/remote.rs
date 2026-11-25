//! Pulls changes from the cloud (account-wide delta feeds) into the tree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use inxt_api::types::{FileDto, FolderDto, ItemStatus};
use inxt_api::ApiError;

use crate::engine::{Change, ChangeKind, Engine};
use crate::names;
use crate::tree::{Node, NodeId, Tree, ROOT};

const REWIND: chrono::Duration = chrono::Duration::minutes(2);
const MAX_ORPHANS: usize = 50_000;
const MAX_CRAWL: usize = 2_000;

#[derive(Default)]
pub(crate) struct Orphans {
    folders: HashMap<String, Vec<FolderDto>>,
    files: HashMap<String, Vec<FileDto>>,
    count: usize,
}

impl Orphans {
    fn add_folder(&mut self, parent: String, f: FolderDto) {
        if self.count < MAX_ORPHANS {
            self.count += 1;
            self.folders.entry(parent).or_default().push(f);
        }
    }

    fn add_file(&mut self, parent: String, f: FileDto) {
        if self.count < MAX_ORPHANS {
            self.count += 1;
            self.files.entry(parent).or_default().push(f);
        }
    }

    fn take(&mut self, parent: &str) -> (Vec<FolderDto>, Vec<FileDto>) {
        let a = self.folders.remove(parent).unwrap_or_default();
        let b = self.files.remove(parent).unwrap_or_default();
        self.count = self.count.saturating_sub(a.len() + b.len());
        (a, b)
    }
}

#[derive(Default)]
struct Ctx {
    changes: Vec<Change>,
    new_folders: Vec<String>,
    uncache: Vec<NodeId>,
    rekey: Vec<(NodeId, NodeId)>,
    bootstrapping: bool,
    max_folder: Option<DateTime<Utc>>,
    max_file: Option<DateTime<Utc>>,
}

fn ms(t: Option<DateTime<Utc>>) -> Option<i64> {
    t.map(|t| t.timestamp_millis())
}

fn conflict_name(t: &Tree, parent: NodeId, name: &str) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S");
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut candidate = format!("{stem} (conflict {stamp}){ext}");
    let mut n = 2;
    while t.child(parent, &candidate).is_some() {
        candidate = format!("{stem} (conflict {stamp} {n}){ext}");
        n += 1;
    }
    candidate
}

impl Engine {
    fn checkpoint(&self, key: &str) -> DateTime<Utc> {
        self.tree
            .read()
            .kv_get(key)
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc) - REWIND)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
    }

    pub fn initial_sync_done(&self) -> bool {
        self.tree.read().kv_get("bootstrapped").is_some()
    }

    /// Fetches and applies everything that changed since the last checkpoint.
    pub async fn poll_remote(self: &Arc<Self>) -> Result<usize, ApiError> {
        let _guard = self.sync_lock.lock().await;
        let bootstrapping = !self.initial_sync_done();
        let started = Instant::now();

        let since = self.checkpoint("cp_folders");
        let mut folders = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.client.folders_sync_page(since, cursor.as_deref()).await?;
            folders.extend(page.items);
            match page.next_cursor {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => break,
            }
            if bootstrapping {
                self.status.lock().items = folders.len();
            }
        }

        let mut ctx = Ctx { bootstrapping, ..Default::default() };
        let mut applied = folders.len();
        {
            let me = self.clone();
            let mut t = self.tree.write();
            t.batch(|t| me.apply_folders(t, folders, &mut ctx));
        }

        let since = self.checkpoint("cp_files");
        let mut cursor: Option<String> = None;
        loop {
            let page = self.client.files_sync_page(since, cursor.as_deref()).await?;
            applied += page.items.len();
            {
                let me = self.clone();
                let mut t = self.tree.write();
                t.batch(|t| {
                    for f in page.items {
                        me.apply_file(t, f, &mut ctx);
                    }
                });
            }
            if bootstrapping {
                self.status.lock().items = self.tree.read().len();
            }
            match page.next_cursor {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => break,
            }
        }

        {
            let t = self.tree.read();
            if let Some(m) = ctx.max_folder {
                t.kv_set("cp_folders", &m.to_rfc3339());
            }
            if let Some(m) = ctx.max_file {
                t.kv_set("cp_files", &m.to_rfc3339());
            }
            if bootstrapping {
                t.kv_set("bootstrapped", "1");
            }
        }
        let crawl = std::mem::take(&mut ctx.new_folders);
        self.finish_apply(&mut ctx);
        drop(_guard);

        if !crawl.is_empty() {
            self.crawl(crawl).await;
        }
        *self.last_poll.lock() = Some(Instant::now());
        {
            let mut s = self.status.lock();
            s.online = true;
            s.initial_sync_done = true;
            s.last_remote_sync = Some(crate::now_ms());
        }
        if applied > 0 {
            tracing::debug!(applied, elapsed_ms = started.elapsed().as_millis() as u64, "remote changes applied");
        }
        Ok(applied)
    }

    fn finish_apply(&self, ctx: &mut Ctx) {
        for (from, to) in ctx.rekey.drain(..) {
            if let Err(e) = self.cache.rekey(from, to) {
                tracing::warn!(error = %e, "could not move conflicting content");
            }
        }
        for id in ctx.uncache.drain(..) {
            self.cache.remove(id);
        }
        self.push_changes(std::mem::take(&mut ctx.changes));
    }

    /// Lists folders that appeared through the delta feed, so their existing
    /// contents (which carry older timestamps) are picked up too.
    async fn crawl(self: &Arc<Self>, mut queue: Vec<String>) {
        let mut seen = HashSet::new();
        while let Some(uuid) = queue.pop() {
            if seen.len() >= MAX_CRAWL || !seen.insert(uuid.clone()) {
                continue;
            }
            let Ok((folders, files)) = self.client.list_children(&uuid).await else { continue };
            let _guard = self.sync_lock.lock().await;
            let mut ctx = Ctx::default();
            {
                let me = self.clone();
                let mut t = self.tree.write();
                t.batch(|t| {
                    me.apply_folders(t, folders, &mut ctx);
                    for f in files {
                        me.apply_file(t, f, &mut ctx);
                    }
                });
            }
            queue.append(&mut ctx.new_folders);
            self.finish_apply(&mut ctx);
        }
    }

    fn apply_folders(&self, t: &mut Tree, folders: Vec<FolderDto>, ctx: &mut Ctx) {
        let mut pending = folders;
        loop {
            let before = pending.len();
            let mut next = Vec::new();
            for f in pending {
                if let Some(u) = f.updated_at {
                    ctx.max_folder = Some(ctx.max_folder.map_or(u, |m| m.max(u)));
                }
                if let Some(f) = self.apply_folder(t, f, ctx) {
                    next.push(f);
                }
            }
            pending = next;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        let mut orphans = self.orphans.lock();
        for f in pending {
            if let Some(p) = f.parent_uuid.clone() {
                orphans.add_folder(p, f);
            }
        }
        drop(orphans);
        self.adopt_orphans(t, ctx);
    }

    fn adopt_orphans(&self, t: &mut Tree, ctx: &mut Ctx) {
        loop {
            let ready: Vec<String> = {
                let o = self.orphans.lock();
                o.folders.keys().chain(o.files.keys()).filter(|p| t.by_uuid(p).is_some()).cloned().collect()
            };
            if ready.is_empty() {
                break;
            }
            for p in ready {
                let (folders, files) = self.orphans.lock().take(&p);
                for f in folders {
                    if let Some(f) = self.apply_folder(t, f, ctx) {
                        if let Some(pp) = f.parent_uuid.clone() {
                            self.orphans.lock().add_folder(pp, f);
                        }
                    }
                }
                for f in files {
                    self.apply_file(t, f, ctx);
                }
            }
        }
    }

    fn placement_clean(t: &Tree, n: &Node) -> bool {
        let local_parent_uuid = t.get(n.parent).and_then(|p| p.uuid.clone());
        !n.deleted
            && n.r_parent.is_some()
            && n.r_parent == local_parent_uuid
            && n.r_name.as_deref() == Some(names::to_remote(&n.name).as_str())
    }

    /// Returns the folder back if its parent is not known yet.
    fn apply_folder(&self, t: &mut Tree, f: FolderDto, ctx: &mut Ctx) -> Option<FolderDto> {
        let root_uuid = t.get(ROOT).and_then(|r| r.uuid.clone());
        if Some(&f.uuid) == root_uuid.as_ref() {
            return None;
        }
        if f.status() != ItemStatus::Exists {
            if let Some(id) = t.by_uuid(&f.uuid) {
                if id != ROOT {
                    let path = t.path_of(id).unwrap_or_default();
                    self.remove_remote_subtree(t, id, ctx);
                    ctx.changes.push(Change { path, is_dir: true, kind: ChangeKind::Removed });
                }
            }
            return None;
        }
        let parent_uuid = f.parent_uuid.clone()?;
        let Some(parent) = t.by_uuid(&parent_uuid) else { return Some(f) };
        if !t.get(parent).is_some_and(|p| p.is_dir) {
            return None;
        }
        let rname = f.display_name();
        let lname = names::to_local(&rname);
        let updated = ms(f.updated_at);

        if let Some(id) = t.by_uuid(&f.uuid) {
            let n = t.get(id)?.clone();
            if let (Some(u), Some(prev)) = (updated, n.remote_updated) {
                if u < prev {
                    return None;
                }
            }
            if id == parent || t.is_ancestor(id, parent) {
                return None;
            }
            let clean = Self::placement_clean(t, &n);
            let old_path = t.path_of(id).unwrap_or_default();
            let moves = clean && (n.parent != parent || n.name != lname);
            t.modify(id, |n| {
                n.r_parent = Some(parent_uuid.clone());
                n.r_name = Some(rname.clone());
                n.remote_updated = updated.or(n.remote_updated);
                if clean {
                    n.parent = parent;
                    n.name = lname.clone();
                }
            });
            if moves {
                let path = t.path_of(id).unwrap_or_default();
                ctx.changes.push(Change { path, is_dir: true, kind: ChangeKind::Renamed { old_path } });
            }
            return None;
        }

        if let Some(tw) = t.child(parent, &lname) {
            if t.get(tw).is_some_and(|n| n.is_dir && n.uuid.is_none()) {
                t.modify(tw, |n| {
                    n.uuid = Some(f.uuid.clone());
                    n.r_parent = Some(parent_uuid.clone());
                    n.r_name = Some(rname.clone());
                    n.remote_updated = updated;
                });
                return None;
            }
        }
        let now = crate::now_ms();
        let id = t.insert(Node {
            parent,
            name: lname,
            is_dir: true,
            ctime: ms(f.ctime()).unwrap_or(now),
            mtime: ms(f.mtime()).unwrap_or(now),
            uuid: Some(f.uuid.clone()),
            r_parent: Some(parent_uuid),
            r_name: Some(rname),
            remote_updated: updated,
            ..Default::default()
        });
        if !ctx.bootstrapping {
            ctx.changes.push(Change { path: t.path_of(id).unwrap_or_default(), is_dir: true, kind: ChangeKind::Added });
            ctx.new_folders.push(f.uuid);
        }
        None
    }

    fn apply_file(&self, t: &mut Tree, f: FileDto, ctx: &mut Ctx) {
        if let Some(u) = f.updated_at {
            ctx.max_file = Some(ctx.max_file.map_or(u, |m| m.max(u)));
        }
        if f.status() != ItemStatus::Exists {
            let Some(id) = t.by_uuid(&f.uuid) else { return };
            let Some(n) = t.get(id).cloned() else { return };
            if n.is_dirty() && !n.deleted {
                t.modify(id, |n| {
                    n.uuid = None;
                    n.r_parent = None;
                    n.r_name = None;
                });
            } else {
                let path = t.path_of(id).unwrap_or_default();
                ctx.uncache.extend(t.purge(id));
                if !n.deleted {
                    ctx.changes.push(Change { path, is_dir: false, kind: ChangeKind::Removed });
                }
            }
            return;
        }
        let Some(parent_uuid) = f.folder_uuid.clone() else { return };
        let Some(parent) = t.by_uuid(&parent_uuid) else {
            self.orphans.lock().add_file(parent_uuid, f);
            return;
        };
        if !t.get(parent).is_some_and(|p| p.is_dir) {
            return;
        }
        let rname = f.display_name();
        let lname = names::to_local(&rname);
        let updated = ms(f.updated_at);
        let now = crate::now_ms();

        if let Some(id) = t.by_uuid(&f.uuid) {
            let Some(n) = t.get(id).cloned() else { return };
            if let (Some(u), Some(prev)) = (updated, n.remote_updated) {
                if u < prev {
                    return;
                }
            }
            let clean = Self::placement_clean(t, &n);
            let old_path = t.path_of(id).unwrap_or_default();
            let content_changed = n.file_id != f.file_id || n.size != f.size;

            if n.is_dirty() && content_changed && f.file_id != n.base_file_id && n.open_handles == 0 {
                let copy_name = conflict_name(t, n.parent, &n.name);
                let copy = t.insert(Node {
                    parent: n.parent,
                    name: copy_name.clone(),
                    is_dir: false,
                    size: n.size,
                    ctime: n.ctime,
                    mtime: n.mtime,
                    content_gen: 1,
                    changed_at: now,
                    ..Default::default()
                });
                ctx.rekey.push((id, copy));
                t.modify(id, |n| {
                    n.file_id = f.file_id.clone();
                    n.size = f.size;
                    n.mtime = ms(f.mtime()).unwrap_or(n.mtime);
                    n.synced_gen = n.content_gen;
                    n.base_file_id = None;
                });
                tracing::info!(file = %copy_name, "edited on both sides; kept local version as a conflict copy");
                ctx.changes.push(Change { path: t.path_of(copy).unwrap_or_default(), is_dir: false, kind: ChangeKind::Added });
                ctx.changes.push(Change { path: old_path.clone(), is_dir: false, kind: ChangeKind::Modified });
            } else if !n.is_dirty() && content_changed && !n.deleted {
                t.modify(id, |n| {
                    n.file_id = f.file_id.clone();
                    n.size = f.size;
                    n.mtime = ms(f.mtime()).unwrap_or(n.mtime);
                    n.base_file_id = None;
                });
                ctx.uncache.push(id);
                ctx.changes.push(Change { path: old_path.clone(), is_dir: false, kind: ChangeKind::Modified });
            }

            let moves = clean && (n.parent != parent || n.name != lname);
            t.modify(id, |n| {
                n.r_parent = Some(parent_uuid.clone());
                n.r_name = Some(rname.clone());
                n.remote_updated = updated.or(n.remote_updated);
                if clean {
                    n.parent = parent;
                    n.name = lname.clone();
                }
            });
            if moves {
                let path = t.path_of(id).unwrap_or_default();
                ctx.changes.push(Change { path, is_dir: false, kind: ChangeKind::Renamed { old_path } });
            }
            return;
        }

        if let Some(tw) = t.child(parent, &lname) {
            if let Some(twin) = t.get(tw).cloned() {
                if !twin.is_dir && twin.uuid.is_none() && !twin.deleted {
                    let renamed = conflict_name(t, parent, &twin.name);
                    let old_path = t.path_of(tw).unwrap_or_default();
                    t.modify(tw, |n| n.name = renamed);
                    ctx.changes.push(Change {
                        path: t.path_of(tw).unwrap_or_default(),
                        is_dir: false,
                        kind: ChangeKind::Renamed { old_path },
                    });
                }
            }
        }
        let id = t.insert(Node {
            parent,
            name: lname,
            is_dir: false,
            size: f.size,
            ctime: ms(f.ctime()).unwrap_or(now),
            mtime: ms(f.mtime()).unwrap_or(now),
            uuid: Some(f.uuid.clone()),
            file_id: f.file_id.clone(),
            r_parent: Some(parent_uuid),
            r_name: Some(rname),
            remote_updated: updated,
            ..Default::default()
        });
        if !ctx.bootstrapping {
            ctx.changes.push(Change { path: t.path_of(id).unwrap_or_default(), is_dir: false, kind: ChangeKind::Added });
        }
    }

    /// Removes a subtree deleted in the cloud, keeping unsynced local edits
    /// (and the folders leading to them) so they get uploaded again.
    fn remove_remote_subtree(&self, t: &mut Tree, id: NodeId, ctx: &mut Ctx) {
        let mut all = t.descendants(id);
        all.insert(0, id);
        let dirty: Vec<NodeId> = all.iter().copied().filter(|x| t.get(*x).is_some_and(|n| n.is_dirty() && !n.deleted)).collect();
        if dirty.is_empty() {
            ctx.uncache.extend(t.purge(id));
            return;
        }
        let mut keep = HashSet::new();
        for d in dirty {
            let mut cur = d;
            loop {
                keep.insert(cur);
                if cur == id || cur == ROOT {
                    break;
                }
                match t.get(cur) {
                    Some(n) => cur = n.parent,
                    None => break,
                }
            }
        }
        for x in all {
            if keep.contains(&x) {
                t.modify(x, |n| {
                    n.uuid = None;
                    n.r_parent = None;
                    n.r_name = None;
                    n.r_trashed = false;
                });
            } else if t.get(x).is_some() {
                ctx.uncache.extend(t.purge(x));
            }
        }
    }

    pub(crate) async fn poll_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(5);
        loop {
            if self.is_stopping() {
                break;
            }
            if self.is_paused() || self.status.lock().auth_failed {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    _ = self.poll_signal.notified() => {}
                }
                continue;
            }
            match self.poll_remote().await {
                Ok(_) => backoff = Duration::from_secs(5),
                Err(e) => {
                    tracing::warn!(error = %e, "remote sync failed");
                    {
                        let mut s = self.status.lock();
                        if e.is_unauthorized() {
                            s.auth_failed = true;
                        }
                        if e.is_transient() {
                            s.online = false;
                        }
                        s.last_error = Some(e.to_string());
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                    continue;
                }
            }
            let every = Duration::from_millis(self.poll_ms.load(std::sync::atomic::Ordering::SeqCst));
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                _ = self.poll_signal.notified() => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }
}
