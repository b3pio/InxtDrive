//! Pushes local changes (creates, edits, renames, moves, deletes) to the cloud.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use inxt_api::drive::{split_name, ItemKind};
use inxt_api::types::{CreateFileRequest, FileDto};
use inxt_api::ApiError;

use crate::cache::ContentKey;
use crate::engine::Engine;
use crate::names;
use crate::tree::{Node, NodeId, Tree, ROOT};

#[derive(Default)]
pub(crate) struct PushState {
    uploading: HashSet<NodeId>,
    backoff: HashMap<NodeId, (u32, Instant)>,
}

impl PushState {
    fn ready(&self, id: NodeId) -> bool {
        self.backoff.get(&id).is_none_or(|(_, next)| Instant::now() >= *next)
    }

    fn fail(&mut self, id: NodeId, permanent: bool) {
        let e = self.backoff.entry(id).or_insert((0, Instant::now()));
        e.0 += 1;
        let secs = if permanent { 3600 } else { (5u64 << e.0.min(7)).min(600) };
        e.1 = Instant::now() + Duration::from_secs(secs);
    }

    fn ok(&mut self, id: NodeId) {
        self.backoff.remove(&id);
    }
}

#[derive(Default)]
struct Plan {
    trash_files: Vec<(NodeId, String)>,
    trash_folders: Vec<(NodeId, String)>,
    meta: Vec<NodeId>,
    local_only_trash: Vec<NodeId>,
    restore: Vec<NodeId>,
    create_folders: Vec<(usize, NodeId)>,
    uploads: Vec<NodeId>,
    pending_uploads: usize,
    pending_ops: usize,
    /// Folders containing anything not yet in the cloud.
    pending_dirs: HashSet<NodeId>,
    queued: Vec<String>,
}

const QUEUE_PREVIEW: usize = 50;

fn ms_to_utc(ms: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

fn depth(t: &Tree, mut id: NodeId) -> usize {
    let mut d = 0;
    while id != ROOT && d < 4096 {
        id = match t.get(id) {
            Some(n) => n.parent,
            None => break,
        };
        d += 1;
    }
    d
}

impl Engine {
    fn parent_uuid(t: &Tree, n: &Node) -> Option<String> {
        t.get(n.parent).and_then(|p| if p.deleted { None } else { p.uuid.clone() })
    }

    /// A deleted remote file that a new local file (same folder, same name)
    /// is about to replace, as editors do when saving via a temp file.
    fn replaced_by(t: &Tree, d: &Node, rules: &names::LocalOnlyRules) -> Option<NodeId> {
        let (Some(rp), Some(rn)) = (d.r_parent.as_deref(), d.r_name.as_deref()) else { return None };
        let parent = t.by_uuid(rp)?;
        let key = names::key(rn);
        t.children(parent).into_iter().find(|c| {
            t.get(*c).is_some_and(|n| {
                !n.is_dir && n.uuid.is_none() && names::key(&names::to_remote(&n.name)) == key && !rules.matches(&n.name)
            })
        })
    }

    fn replace_target(t: &Tree, n: &Node) -> Option<NodeId> {
        let pu = Self::parent_uuid(t, n)?;
        let key = names::key(&names::to_remote(&n.name));
        t.deleted_nodes().into_iter().find(|d| {
            t.get(*d).is_some_and(|dn| {
                !dn.is_dir
                    && dn.uuid.is_some()
                    && dn.r_parent.as_deref() == Some(pu.as_str())
                    && dn.r_name.as_deref().map(names::key).as_deref() == Some(key.as_str())
            })
        })
    }

    fn plan(&self) -> Plan {
        let now = crate::now_ms();
        let debounce = self.cfg.push_debounce.as_millis() as i64;
        let rules = &self.cfg.local_only;
        let t = self.tree.read();
        let ps = self.push_state.lock();
        let mut p = Plan::default();
        let ready = |n: &Node| now - n.changed_at >= debounce && ps.ready(n.id);

        for d in t.deleted_nodes() {
            let Some(n) = t.get(d) else { continue };
            let Some(uuid) = n.uuid.clone() else { continue };
            p.pending_ops += 1;
            if !ready(n) {
                continue;
            }
            let mut cur = n.parent;
            let mut covered = false;
            while cur != ROOT && cur != 0 {
                match t.get(cur) {
                    Some(a) if a.deleted && a.uuid.is_some() => {
                        covered = true;
                        break;
                    }
                    Some(a) => cur = a.parent,
                    None => break,
                }
            }
            if covered {
                continue;
            }
            if n.is_dir {
                p.trash_folders.push((d, uuid));
            } else if Self::replaced_by(&t, n, rules).is_none() {
                p.trash_files.push((d, uuid));
            }
        }

        let mut pending_leaves = Vec::new();
        for id in t.all_ids() {
            if id == ROOT {
                continue;
            }
            let Some(n) = t.get(id) else { continue };
            if n.deleted {
                continue;
            }
            let local_only = t.is_local_only(id, rules);
            let parent_uuid = Self::parent_uuid(&t, n);
            if local_only {
                if n.uuid.is_some() && !n.r_trashed && ready(n) {
                    p.local_only_trash.push(id);
                }
                continue;
            }
            if n.uuid.is_none() || n.r_trashed || (!n.is_dir && n.is_dirty()) || Self::placement_pending(&t, n) {
                pending_leaves.push(id);
                if !n.is_dir && p.queued.len() < QUEUE_PREVIEW {
                    if let Some(path) = t.path_of(id) {
                        p.queued.push(path);
                    }
                }
            }
            if n.r_trashed && n.uuid.is_some() {
                if parent_uuid.is_some() && ready(n) {
                    p.restore.push(id);
                }
                continue;
            }
            match (&n.uuid, n.is_dir) {
                (None, true) => {
                    p.pending_ops += 1;
                    if parent_uuid.is_some() && ready(n) {
                        p.create_folders.push((depth(&t, id), id));
                    }
                }
                (Some(_), _) => {
                    let remote_name = names::to_remote(&n.name);
                    if parent_uuid.is_some()
                        && (n.r_parent != parent_uuid || n.r_name.as_deref() != Some(remote_name.as_str()))
                    {
                        p.pending_ops += 1;
                        if ready(n) {
                            p.meta.push(id);
                        }
                    }
                }
                _ => {}
            }
            if !n.is_dir && (n.is_dirty() || n.uuid.is_none()) {
                p.pending_uploads += 1;
                if n.open_writers == 0 && parent_uuid.is_some() && ready(n) && !ps.uploading.contains(&id) {
                    p.uploads.push(id);
                }
            }
        }
        p.create_folders.sort();
        for leaf in pending_leaves {
            let mut cur = t.get(leaf).map(|n| n.parent).unwrap_or(0);
            let mut guard = 0;
            while cur != 0 && guard < 4096 && p.pending_dirs.insert(cur) {
                cur = t.get(cur).map(|n| n.parent).unwrap_or(0);
                guard += 1;
            }
        }
        p
    }

    /// Publishes folder roll-up state and the upload queue; announces folders
    /// whose state flipped so Explorer redraws their status badge.
    fn publish_plan_state(&self, plan: &mut Plan) {
        let fresh = std::mem::take(&mut plan.pending_dirs);
        let flipped: Vec<NodeId> = {
            let mut cur = self.pending_dirs.write();
            let f = cur.symmetric_difference(&fresh).copied().filter(|id| *id != ROOT).collect();
            *cur = fresh;
            f
        };
        self.notify_modified(&flipped);
        let mut a = self.activity.lock();
        a.queued = std::mem::take(&mut plan.queued);
        a.queued_total = plan.pending_uploads;
    }

    pub(crate) async fn push_loop(self: Arc<Self>) {
        loop {
            if self.is_stopping() {
                break;
            }
            if self.status.lock().auth_failed || self.is_paused() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    _ = self.push_signal.notified() => {}
                }
                continue;
            }
            let more = self.push_pass().await;
            let wait = if more { Duration::from_millis(500) } else { Duration::from_secs(5) };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.push_signal.notified() => {
                    tokio::time::sleep(self.cfg.push_debounce + Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn record_error(&self, id: NodeId, what: &str, e: &ApiError) {
        let permanent = !e.is_transient() && !e.is_conflict() && !e.is_unauthorized();
        self.push_state.lock().fail(id, permanent);
        let name = self.tree.read().path_of(id).unwrap_or_default();
        tracing::warn!(path = %name, error = %e, "{what} failed");
        self.activity.lock().failed(id, name.clone(), format!("{what} failed: {e}"));
        let mut s = self.status.lock();
        s.last_error = Some(format!("{what} {name}: {e}"));
        if e.is_unauthorized() {
            s.auth_failed = true;
        }
        if e.is_transient() {
            s.online = false;
        }
    }

    /// One pass over pending work. Returns true when more work is waiting.
    async fn push_pass(self: &Arc<Self>) -> bool {
        let mut plan = self.plan();
        self.publish_plan_state(&mut plan);
        {
            let mut s = self.status.lock();
            s.pending_uploads = plan.pending_uploads;
            s.pending_ops = plan.pending_ops;
        }
        let mut progressed = false;

        if !plan.trash_files.is_empty() {
            progressed |= self.trash_batch(&plan.trash_files, ItemKind::File).await;
        }
        for id in plan.meta {
            progressed |= self.push_meta(id).await;
        }
        for id in plan.local_only_trash {
            progressed |= self.trash_local_only(id).await;
        }
        for id in plan.restore {
            progressed |= self.restore_from_trash(id).await;
        }
        if !plan.trash_folders.is_empty() {
            progressed |= self.trash_batch(&plan.trash_folders, ItemKind::Folder).await;
        }
        let mut adopted = Vec::new();
        for (_, id) in plan.create_folders {
            match self.create_folder(id).await {
                Ok(Some(uuid)) => adopted.push(uuid),
                Ok(None) => {}
                Err(e) => self.record_error(id, "create folder", &e),
            }
            progressed = true;
        }
        if !adopted.is_empty() {
            let me = self.clone();
            tokio::spawn(async move { me.crawl_adopted(adopted).await });
        }

        let slots = self.upload_slots.load(std::sync::atomic::Ordering::SeqCst).max(1);
        for id in plan.uploads {
            {
                let mut ps = self.push_state.lock();
                if ps.uploading.len() >= slots {
                    break;
                }
                ps.uploading.insert(id);
            }
            let me = self.clone();
            tokio::spawn(async move {
                me.status.lock().uploading += 1;
                let res = me.upload_one(id).await;
                {
                    let mut s = me.status.lock();
                    s.uploading = s.uploading.saturating_sub(1);
                    if s.uploading == 0 {
                        s.upload_bytes_total = 0;
                        s.upload_bytes_done = 0;
                    }
                }
                me.push_state.lock().uploading.remove(&id);
                match res {
                    Ok(()) => {
                        me.push_state.lock().ok(id);
                        me.status.lock().online = true;
                    }
                    Err(e) => me.record_error(id, "upload", &e),
                }
                me.push_signal.notify_one();
            });
            progressed = true;
        }
        progressed
    }

    async fn crawl_adopted(self: Arc<Self>, uuids: Vec<String>) {
        let _ = self.poll_remote().await;
        let _ = uuids;
    }

    async fn trash_batch(self: &Arc<Self>, items: &[(NodeId, String)], kind: ItemKind) -> bool {
        let list: Vec<(ItemKind, String)> = items.iter().map(|(_, u)| (kind, u.clone())).collect();
        let _g = self.sync_lock.lock().await;
        match self.client.trash(&list).await {
            Ok(()) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => {
                for (id, _) in items {
                    self.record_error(*id, "delete", &e);
                }
                return false;
            }
        }
        let mut removed = Vec::new();
        {
            let mut t = self.tree.write();
            t.batch(|t| {
                for (id, _) in items {
                    removed.extend(t.purge(*id));
                }
            });
        }
        for id in removed {
            self.cache.remove(id);
        }
        true
    }

    async fn push_meta(self: &Arc<Self>, id: NodeId) -> bool {
        let snap = {
            let t = self.tree.read();
            let Some(n) = t.get(id).cloned() else { return false };
            let Some(pu) = Self::parent_uuid(&t, &n) else { return false };
            (n, pu)
        };
        let (n, parent_uuid) = snap;
        let Some(uuid) = n.uuid.clone() else { return false };
        let remote_name = names::to_remote(&n.name);
        let _g = self.sync_lock.lock().await;
        let res = if n.r_parent.as_deref() != Some(parent_uuid.as_str()) {
            let rename = (n.r_name.as_deref() != Some(remote_name.as_str())).then_some(remote_name.as_str());
            if n.is_dir {
                self.client.move_folder(&uuid, &parent_uuid, rename).await
            } else {
                self.client.move_file(&uuid, &parent_uuid, rename).await
            }
        } else if n.is_dir {
            self.client.rename_folder(&uuid, &remote_name).await
        } else {
            self.client.rename_file(&uuid, &remote_name).await
        };
        match res {
            Ok(()) => {
                self.tree.write().modify(id, |n| {
                    n.r_parent = Some(parent_uuid.clone());
                    n.r_name = Some(remote_name.clone());
                });
                self.push_state.lock().ok(id);
                self.notify_modified(&[id]);
                true
            }
            Err(e) if e.is_not_found() => {
                tracing::info!(node = id, "item vanished remotely; uploading it again");
                self.forget_remote(id);
                true
            }
            Err(e) => {
                self.record_error(id, if n.is_dir { "move folder" } else { "move file" }, &e);
                false
            }
        }
    }

    /// Drops the remote identity of a node so it is created again; files
    /// without local content cannot be re-created and are removed.
    fn forget_remote(&self, id: NodeId) {
        let complete = self.cache.get(id).is_some_and(|e| e.is_complete());
        let mut t = self.tree.write();
        let Some(n) = t.get(id).cloned() else { return };
        if !n.is_dir && !complete && n.size > 0 {
            let removed = t.purge(id);
            drop(t);
            for r in removed {
                self.cache.remove(r);
            }
            return;
        }
        t.modify(id, |n| {
            n.uuid = None;
            n.r_parent = None;
            n.r_name = None;
            n.r_trashed = false;
            if !n.is_dir && !n.is_dirty() {
                n.content_gen += 1;
            }
        });
        if let Some(e) = self.cache.get(id) {
            e.mark_local();
        }
    }

    async fn trash_local_only(self: &Arc<Self>, id: NodeId) -> bool {
        let Some((uuid, is_dir)) = self.tree.read().get(id).and_then(|n| n.uuid.clone().map(|u| (u, n.is_dir))) else {
            return false;
        };
        let kind = if is_dir { ItemKind::Folder } else { ItemKind::File };
        let _g = self.sync_lock.lock().await;
        match self.client.trash(&[(kind, uuid)]).await {
            Ok(()) => {
                self.tree.write().modify(id, |n| n.r_trashed = true);
                true
            }
            Err(e) if e.is_not_found() => {
                self.tree.write().modify(id, |n| n.r_trashed = true);
                true
            }
            Err(e) => {
                self.record_error(id, "delete", &e);
                false
            }
        }
    }

    async fn restore_from_trash(self: &Arc<Self>, id: NodeId) -> bool {
        let snap = {
            let t = self.tree.read();
            t.get(id).cloned().and_then(|n| Self::parent_uuid(&t, &n).map(|p| (n, p)))
        };
        let Some((n, parent_uuid)) = snap else { return false };
        let Some(uuid) = n.uuid.clone() else { return false };
        let name = names::to_remote(&n.name);
        let _g = self.sync_lock.lock().await;
        let res = if n.is_dir {
            self.client.move_folder(&uuid, &parent_uuid, Some(&name)).await
        } else {
            self.client.move_file(&uuid, &parent_uuid, Some(&name)).await
        };
        drop(_g);
        match res {
            Ok(()) => {
                self.tree.write().modify(id, |n| {
                    n.r_trashed = false;
                    n.r_parent = Some(parent_uuid.clone());
                    n.r_name = Some(name.clone());
                });
            }
            Err(e) if e.is_transient() => {
                self.record_error(id, "restore", &e);
                return false;
            }
            Err(_) => self.forget_remote(id),
        }
        true
    }

    /// Returns the uuid of an existing remote folder that was adopted.
    async fn create_folder(self: &Arc<Self>, id: NodeId) -> Result<Option<String>, ApiError> {
        let snap = {
            let t = self.tree.read();
            t.get(id).cloned().and_then(|n| Self::parent_uuid(&t, &n).map(|p| (n, p)))
        };
        let Some((n, parent_uuid)) = snap else { return Ok(None) };
        if n.uuid.is_some() || n.deleted {
            return Ok(None);
        }
        let name = names::to_remote(&n.name);
        let _g = self.sync_lock.lock().await;
        match self.client.create_folder(&parent_uuid, &name, ms_to_utc(n.ctime), ms_to_utc(n.mtime)).await {
            Ok(dto) => {
                self.tree.write().modify(id, |x| {
                    x.uuid = Some(dto.uuid.clone());
                    x.r_parent = Some(parent_uuid.clone());
                    x.r_name = Some(name.clone());
                    x.remote_updated = dto.updated_at.map(|t| t.timestamp_millis());
                });
                self.notify_modified(&[id]);
                Ok(None)
            }
            Err(e) if e.is_conflict() => {
                let (folders, _) = self.client.list_children(&parent_uuid).await?;
                let key = names::key(&name);
                let Some(existing) = folders.into_iter().find(|f| names::key(&f.display_name()) == key) else {
                    return Err(e);
                };
                self.tree.write().modify(id, |x| {
                    x.uuid = Some(existing.uuid.clone());
                    x.r_parent = Some(parent_uuid.clone());
                    x.r_name = Some(existing.display_name());
                });
                Ok(Some(existing.uuid))
            }
            Err(e) => Err(e),
        }
    }

    async fn upload_one(self: &Arc<Self>, id: NodeId) -> Result<(), ApiError> {
        let snap = self.tree.read().get(id).cloned();
        let Some(n) = snap else { return Ok(()) };
        if n.is_dir || n.deleted || (!n.is_dirty() && n.uuid.is_some()) {
            return Ok(());
        }
        let complete = self.cache.get(id).is_some_and(|e| e.is_complete());
        if !complete && n.size > 0 {
            let me = self.clone();
            let ok = tokio::task::spawn_blocking(move || me.read_all_for_upload(id)).await.unwrap_or(false);
            if !ok {
                return Err(ApiError::Other("file content is not available locally".into()));
            }
        }
        let (gen, size, mtime, ctime) = {
            let t = self.tree.read();
            let Some(n) = t.get(id) else { return Ok(()) };
            let size = self.cache.get(id).map(|e| e.state.lock().size).unwrap_or(n.size);
            (n.content_gen, size, n.mtime, n.ctime)
        };

        let path = self.tree.read().path_of(id).unwrap_or_default();
        self.activity
            .lock()
            .uploading
            .insert(id, crate::engine::Transfer { path: path.clone(), done: 0, total: size });
        let file_id = if size > 0 {
            {
                let mut s = self.status.lock();
                s.upload_bytes_total += size;
            }
            let me = self.clone();
            let progress: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |n| {
                me.status.lock().upload_bytes_done += n;
                if let Some(t) = me.activity.lock().uploading.get_mut(&id) {
                    t.done = (t.done + n).min(t.total);
                }
            });
            match self.client.upload_file(&self.cache.file_path(id), size, progress).await {
                Ok(f) => Some(f),
                Err(e) => {
                    self.activity.lock().uploading.remove(&id);
                    return Err(e);
                }
            }
        } else {
            None
        };

        let _g = self.sync_lock.lock().await;
        let snap = {
            let t = self.tree.read();
            let Some(n) = t.get(id).cloned() else { return Ok(()) };
            let pu = Self::parent_uuid(&t, &n);
            let target = if n.uuid.is_none() { Self::replace_target(&t, &n) } else { None };
            let target = target.and_then(|d| t.get(d).cloned());
            (n, pu, target)
        };
        let (n, parent_uuid, target) = snap;
        let mtime_utc = ms_to_utc(mtime);

        let dto: FileDto;
        let mut adopt_placement: Option<(String, String)> = None;
        let mut drop_target: Option<NodeId> = None;
        if let Some(uuid) = &n.uuid {
            dto = self.client.replace_file(uuid, file_id.as_deref(), size, mtime_utc).await?;
        } else if let Some(t) = target {
            let tu = t.uuid.clone().expect("target has uuid");
            dto = self.client.replace_file(&tu, file_id.as_deref(), size, mtime_utc).await?;
            adopt_placement = Some((t.r_parent.clone().unwrap_or_default(), t.r_name.clone().unwrap_or_default()));
            drop_target = Some(t.id);
            tracing::info!(file = %n.name, "saved over an existing cloud file (kept its history)");
        } else {
            let Some(parent_uuid) = parent_uuid.clone() else {
                return Err(ApiError::Other("parent folder is not in the cloud yet".into()));
            };
            let remote_name = names::to_remote(&n.name);
            let (plain, ty) = split_name(&remote_name);
            let req = CreateFileRequest {
                bucket: self.client.credentials()?.bucket.clone(),
                file_id: file_id.clone(),
                encrypt_version: "03-aes",
                folder_uuid: parent_uuid.clone(),
                size,
                plain_name: plain,
                file_type: ty,
                creation_time: ms_to_utc(ctime),
                date: mtime_utc,
                modification_time: mtime_utc,
            };
            dto = match self.client.create_file(&req).await {
                Ok(d) => d,
                Err(e) if e.is_conflict() => {
                    let (_, files) = self.client.list_children(&parent_uuid).await?;
                    let key = names::key(&remote_name);
                    let Some(existing) = files.into_iter().find(|f| names::key(&f.display_name()) == key) else {
                        return Err(e);
                    };
                    tracing::info!(file = %n.name, "a cloud file with this name existed; replacing its content");
                    self.client.replace_file(&existing.uuid, file_id.as_deref(), size, mtime_utc).await?
                }
                Err(e) => return Err(e),
            };
            adopt_placement = Some((parent_uuid, dto.display_name()));
        }

        let mut purge = Vec::new();
        {
            let mut t = self.tree.write();
            if let Some(d) = drop_target {
                purge.extend(t.purge(d));
            }
            t.modify(id, |x| {
                x.uuid = Some(dto.uuid.clone());
                x.file_id = file_id.clone();
                x.base_file_id = file_id.clone();
                x.synced_gen = gen;
                x.remote_updated = dto.updated_at.map(|t| t.timestamp_millis()).or(x.remote_updated);
                if let Some((rp, rn)) = &adopt_placement {
                    x.r_parent = Some(rp.clone());
                    x.r_name = Some(rn.clone());
                }
            });
            let clean = t.get(id).is_some_and(|x| !x.is_dirty());
            if clean {
                if let (Some(e), Some(fid)) = (self.cache.get(id), &file_id) {
                    let mut st = e.state.lock();
                    if st.size == size {
                        st.key = ContentKey::remote(fid, size);
                        st.meta_dirty = true;
                    }
                }
            }
        }
        for p in purge {
            if p != id {
                self.cache.remove(p);
            }
        }
        let final_path = self.tree.read().path_of(id).unwrap_or(path);
        self.activity.lock().finished(id, final_path, size);
        self.notify_modified(&[id]);
        Ok(())
    }

    /// Blocking: downloads whatever is missing so a file can be re-uploaded.
    fn read_all_for_upload(&self, id: NodeId) -> bool {
        let size = self.tree.read().get(id).map(|n| n.size).unwrap_or(0);
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0;
        while off < size {
            match self.read(id, off, &mut buf) {
                Ok(0) => break,
                Ok(n) => off += n as u64,
                Err(_) => return false,
            }
        }
        self.cache.get(id).is_some_and(|e| e.is_complete())
    }
}
