//! The in-memory namespace of the drive, persisted write-through to SQLite.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::names;

pub type NodeId = u64;
pub const ROOT: NodeId = 1;

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub id: NodeId,
    pub parent: NodeId,
    /// Local (filesystem-safe) display name.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix milliseconds.
    pub ctime: i64,
    pub mtime: i64,
    /// Local-only attribute bits (hidden, read-only, ...).
    pub attrs: u32,
    pub uuid: Option<String>,
    /// Network id of the content this node currently represents.
    pub file_id: Option<String>,
    /// Last known remote placement: parent uuid and remote name.
    pub r_parent: Option<String>,
    pub r_name: Option<String>,
    /// The remote item was trashed because it moved into a local-only area.
    pub r_trashed: bool,
    /// Bumped on each local content change; equal to `synced_gen` when clean.
    pub content_gen: u64,
    pub synced_gen: u64,
    /// Remote content id local edits are based on, for conflict detection.
    pub base_file_id: Option<String>,
    /// Deleted locally, waiting to be trashed remotely.
    pub deleted: bool,
    pub pinned: bool,
    /// Unix milliseconds of the last local change, for debouncing pushes.
    pub changed_at: i64,
    pub remote_updated: Option<i64>,
    pub open_handles: u32,
    pub open_writers: u32,
}

impl Node {
    pub fn is_dirty(&self) -> bool {
        !self.is_dir && self.content_gen != self.synced_gen
    }
}

pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    children: HashMap<NodeId, BTreeMap<String, Vec<NodeId>>>,
    by_uuid: HashMap<String, NodeId>,
    deleted: HashSet<NodeId>,
    next_id: NodeId,
    db: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY,
    parent INTEGER NOT NULL,
    name TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    size INTEGER NOT NULL,
    ctime INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    attrs INTEGER NOT NULL DEFAULT 0,
    uuid TEXT,
    file_id TEXT,
    r_parent TEXT,
    r_name TEXT,
    r_trashed INTEGER NOT NULL DEFAULT 0,
    content_gen INTEGER NOT NULL DEFAULT 0,
    synced_gen INTEGER NOT NULL DEFAULT 0,
    base_file_id TEXT,
    deleted INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    changed_at INTEGER NOT NULL DEFAULT 0,
    remote_updated INTEGER
);
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cache (
    node INTEGER PRIMARY KEY,
    content_key TEXT NOT NULL,
    size INTEGER NOT NULL,
    block_size INTEGER NOT NULL,
    bitmap BLOB NOT NULL,
    last_access INTEGER NOT NULL
);
"#;

impl Tree {
    pub fn open(db_path: &Path, root_uuid: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(SCHEMA)?;
        let mut tree = Tree {
            nodes: HashMap::new(),
            children: HashMap::new(),
            by_uuid: HashMap::new(),
            deleted: HashSet::new(),
            next_id: ROOT + 1,
            db: Mutex::new(conn),
        };
        tree.load()?;
        if let std::collections::hash_map::Entry::Vacant(slot) = tree.nodes.entry(ROOT) {
            let now = crate::now_ms();
            slot.insert(Node {
                id: ROOT,
                parent: 0,
                name: String::new(),
                is_dir: true,
                ctime: now,
                mtime: now,
                uuid: Some(root_uuid.to_string()),
                ..Default::default()
            });
            tree.by_uuid.insert(root_uuid.to_string(), ROOT);
            tree.save(ROOT);
        } else if tree.nodes[&ROOT].uuid.as_deref() != Some(root_uuid) {
            return Err(rusqlite::Error::InvalidParameterName("database belongs to another account".into()));
        }
        Ok(tree)
    }

    fn load(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<Node> = {
            let db = self.db.lock();
            let mut st = db.prepare(
                "SELECT id,parent,name,is_dir,size,ctime,mtime,attrs,uuid,file_id,r_parent,r_name,r_trashed,
                        content_gen,synced_gen,base_file_id,deleted,pinned,changed_at,remote_updated FROM nodes",
            )?;
            let it = st.query_map([], |r| {
                Ok(Node {
                    id: r.get::<_, i64>(0)? as u64,
                    parent: r.get::<_, i64>(1)? as u64,
                    name: r.get(2)?,
                    is_dir: r.get::<_, i64>(3)? != 0,
                    size: r.get::<_, i64>(4)? as u64,
                    ctime: r.get(5)?,
                    mtime: r.get(6)?,
                    attrs: r.get::<_, i64>(7)? as u32,
                    uuid: r.get(8)?,
                    file_id: r.get(9)?,
                    r_parent: r.get(10)?,
                    r_name: r.get(11)?,
                    r_trashed: r.get::<_, i64>(12)? != 0,
                    content_gen: r.get::<_, i64>(13)? as u64,
                    synced_gen: r.get::<_, i64>(14)? as u64,
                    base_file_id: r.get(15)?,
                    deleted: r.get::<_, i64>(16)? != 0,
                    pinned: r.get::<_, i64>(17)? != 0,
                    changed_at: r.get(18)?,
                    remote_updated: r.get(19)?,
                    open_handles: 0,
                    open_writers: 0,
                })
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };
        for n in rows {
            self.next_id = self.next_id.max(n.id + 1);
            self.index(&n);
            self.nodes.insert(n.id, n);
        }
        Ok(())
    }

    fn index(&mut self, n: &Node) {
        if let Some(u) = &n.uuid {
            self.by_uuid.insert(u.clone(), n.id);
        }
        if n.id == ROOT {
            return;
        }
        if n.deleted {
            self.deleted.insert(n.id);
        } else {
            self.children.entry(n.parent).or_default().entry(names::key(&n.name)).or_default().push(n.id);
        }
    }

    fn unindex(&mut self, n: &Node) {
        if let Some(u) = &n.uuid {
            if self.by_uuid.get(u) == Some(&n.id) {
                self.by_uuid.remove(u);
            }
        }
        self.deleted.remove(&n.id);
        if let Some(kids) = self.children.get_mut(&n.parent) {
            let k = names::key(&n.name);
            if let Some(v) = kids.get_mut(&k) {
                v.retain(|x| *x != n.id);
                if v.is_empty() {
                    kids.remove(&k);
                }
            }
        }
    }

    fn save(&self, id: NodeId) {
        let Some(n) = self.nodes.get(&id) else { return };
        let db = self.db.lock();
        let res = db
            .prepare_cached(
                "INSERT OR REPLACE INTO nodes (id,parent,name,is_dir,size,ctime,mtime,attrs,uuid,file_id,r_parent,r_name,
                    r_trashed,content_gen,synced_gen,base_file_id,deleted,pinned,changed_at,remote_updated)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    n.id as i64,
                    n.parent as i64,
                    n.name,
                    n.is_dir as i64,
                    n.size as i64,
                    n.ctime,
                    n.mtime,
                    n.attrs as i64,
                    n.uuid,
                    n.file_id,
                    n.r_parent,
                    n.r_name,
                    n.r_trashed as i64,
                    n.content_gen as i64,
                    n.synced_gen as i64,
                    n.base_file_id,
                    n.deleted as i64,
                    n.pinned as i64,
                    n.changed_at,
                    n.remote_updated,
                ])
            });
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to persist node");
        }
    }

    /// Runs `f` inside one SQLite transaction (much faster for bulk changes).
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Tree) -> R) -> R {
        let _ = self.db.lock().execute_batch("BEGIN");
        let r = f(self);
        if let Err(e) = self.db.lock().execute_batch("COMMIT") {
            tracing::error!(error = %e, "commit failed");
        }
        r
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn by_uuid(&self, uuid: &str) -> Option<NodeId> {
        self.by_uuid.get(uuid).copied()
    }

    pub fn child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        self.children.get(&parent)?.get(&names::key(name))?.first().copied()
    }

    pub fn children(&self, parent: NodeId) -> Vec<NodeId> {
        self.children
            .get(&parent)
            .map(|m| m.values().flatten().copied().collect())
            .unwrap_or_default()
    }

    pub fn has_children(&self, parent: NodeId) -> bool {
        self.children.get(&parent).is_some_and(|m| !m.is_empty())
    }

    pub fn deleted_nodes(&self) -> Vec<NodeId> {
        self.deleted.iter().copied().collect()
    }

    pub fn all_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    /// Resolves a `\`-separated path relative to the root.
    pub fn resolve(&self, path: &str) -> Option<NodeId> {
        let mut cur = ROOT;
        for part in path.split(['\\', '/']).filter(|p| !p.is_empty()) {
            let n = self.nodes.get(&cur)?;
            if !n.is_dir {
                return None;
            }
            cur = self.child(cur, part)?;
        }
        Some(cur)
    }

    pub fn path_of(&self, id: NodeId) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = id;
        let mut guard = 0;
        while cur != ROOT {
            let n = self.nodes.get(&cur)?;
            parts.push(n.name.as_str());
            cur = n.parent;
            guard += 1;
            if guard > 4096 {
                return None;
            }
        }
        let mut s = String::new();
        for p in parts.iter().rev() {
            s.push('\\');
            s.push_str(p);
        }
        if s.is_empty() {
            s.push('\\');
        }
        Some(s)
    }

    /// True when `id` or one of its ancestors matches the local-only rules.
    pub fn is_local_only(&self, id: NodeId, rules: &names::LocalOnlyRules) -> bool {
        let mut cur = id;
        let mut guard = 0;
        while cur != ROOT && cur != 0 {
            let Some(n) = self.nodes.get(&cur) else { return false };
            if rules.matches(&n.name) {
                return true;
            }
            cur = n.parent;
            guard += 1;
            if guard > 4096 {
                break;
            }
        }
        false
    }

    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        let mut guard = 0;
        while id != 0 {
            if id == ancestor {
                return true;
            }
            id = match self.nodes.get(&id) {
                Some(n) => n.parent,
                None => return false,
            };
            guard += 1;
            if guard > 4096 {
                return false;
            }
        }
        false
    }

    /// Live descendants (depth first, parents before children).
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            for c in self.children(cur) {
                out.push(c);
                stack.push(c);
            }
        }
        out
    }

    pub fn insert(&mut self, mut node: Node) -> NodeId {
        node.id = self.next_id;
        self.next_id += 1;
        let id = node.id;
        self.index(&node);
        self.nodes.insert(id, node);
        self.save(id);
        id
    }

    /// Applies `f` to a node, keeping every index consistent and persisting it.
    pub fn modify<R>(&mut self, id: NodeId, f: impl FnOnce(&mut Node) -> R) -> Option<R> {
        let mut n = self.nodes.remove(&id)?;
        self.unindex(&n);
        let r = f(&mut n);
        n.id = id;
        self.index(&n);
        self.nodes.insert(id, n);
        self.save(id);
        Some(r)
    }

    /// Updates runtime-only fields without touching the database.
    pub fn modify_runtime(&mut self, id: NodeId, f: impl FnOnce(&mut Node)) {
        if let Some(n) = self.nodes.get_mut(&id) {
            f(n);
        }
    }

    /// Removes a node and its whole subtree. Returns the removed ids.
    pub fn purge(&mut self, id: NodeId) -> Vec<NodeId> {
        if id == ROOT {
            return Vec::new();
        }
        let mut ids = self.descendants(id);
        ids.push(id);
        let extra: Vec<NodeId> = self
            .deleted
            .iter()
            .copied()
            .filter(|d| ids.iter().any(|x| self.nodes.get(d).is_some_and(|n| n.parent == *x)))
            .collect();
        ids.extend(extra);
        {
            let db = self.db.lock();
            for x in &ids {
                let _ = db.execute("DELETE FROM nodes WHERE id = ?1", params![*x as i64]);
            }
        }
        for x in &ids {
            if let Some(n) = self.nodes.remove(x) {
                self.unindex(&n);
            }
            self.children.remove(x);
        }
        ids
    }

    pub fn kv_get(&self, k: &str) -> Option<String> {
        self.db
            .lock()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn kv_set(&self, k: &str, v: &str) {
        let _ = self.db.lock().execute("INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)", params![k, v]);
    }

    pub(crate) fn with_db<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.db.lock())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(parent: NodeId, name: &str) -> Node {
        Node { parent, name: name.into(), ..Default::default() }
    }

    #[test]
    fn index_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let (a, b);
        {
            let mut t = Tree::open(&db, "root-uuid").unwrap();
            a = t.insert(Node { is_dir: true, ..file(ROOT, "Docs") });
            b = t.insert(file(a, "Report.PDF"));
            assert_eq!(t.resolve("\\docs\\report.pdf"), Some(b));
            assert_eq!(t.path_of(b).unwrap(), "\\Docs\\Report.PDF");
            t.modify(b, |n| n.name = "final.pdf".into());
            assert_eq!(t.resolve("\\Docs\\report.pdf"), None);
            assert_eq!(t.resolve("\\Docs\\FINAL.pdf"), Some(b));
            t.modify(b, |n| n.deleted = true);
            assert_eq!(t.child(a, "final.pdf"), None);
            assert_eq!(t.deleted_nodes(), vec![b]);
        }
        let t = Tree::open(&db, "root-uuid").unwrap();
        assert_eq!(t.resolve("\\Docs"), Some(a));
        assert!(t.get(b).unwrap().deleted);
        assert!(Tree::open(&db, "other").is_err());
    }

    #[test]
    fn purge_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tree::open(&dir.path().join("t.db"), "r").unwrap();
        let a = t.insert(Node { is_dir: true, ..file(ROOT, "a") });
        let b = t.insert(Node { is_dir: true, ..file(a, "b") });
        let c = t.insert(file(b, "c"));
        let removed = t.purge(a);
        assert_eq!(removed.len(), 3);
        assert!(t.get(c).is_none());
        assert!(!t.has_children(ROOT));
    }
}
