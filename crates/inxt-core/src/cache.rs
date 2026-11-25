//! Sparse on-disk cache of decrypted file contents, tracked in fixed blocks.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::platform;
use crate::tree::NodeId;

pub const BLOCK: u64 = 1 << 20;

/// Content identity for a cache entry. `Local` means the cache is the
/// authoritative copy (new or edited file not uploaded yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentKey {
    Remote(String),
    Local,
}

impl ContentKey {
    pub fn remote(file_id: &str, size: u64) -> Self {
        ContentKey::Remote(format!("{file_id}:{size}"))
    }

    fn encode(&self) -> String {
        match self {
            ContentKey::Remote(k) => format!("r:{k}"),
            ContentKey::Local => "l".into(),
        }
    }

    fn decode(s: &str) -> Self {
        match s.strip_prefix("r:") {
            Some(k) => ContentKey::Remote(k.to_string()),
            None => ContentKey::Local,
        }
    }
}

pub struct EntryState {
    pub key: ContentKey,
    pub size: u64,
    bits: Vec<u64>,
    present: u64,
    inflight: HashSet<u64>,
    pub failure: Option<String>,
    pub last_access: i64,
    pub meta_dirty: bool,
}

impl EntryState {
    fn blocks(size: u64) -> u64 {
        size.div_ceil(BLOCK)
    }

    fn resize_bits(&mut self) {
        let words = Self::blocks(self.size).div_ceil(64) as usize;
        self.bits.resize(words, 0);
        let nblocks = Self::blocks(self.size);
        for b in nblocks..(self.bits.len() as u64 * 64) {
            self.clear(b);
        }
        self.present = (0..nblocks).filter(|b| self.has(*b)).count() as u64;
    }

    pub fn has(&self, b: u64) -> bool {
        self.bits.get((b / 64) as usize).is_some_and(|w| w & (1 << (b % 64)) != 0)
    }

    fn set(&mut self, b: u64) {
        let i = (b / 64) as usize;
        if i < self.bits.len() && self.bits[i] & (1 << (b % 64)) == 0 {
            self.bits[i] |= 1 << (b % 64);
            self.present += 1;
            self.meta_dirty = true;
        }
    }

    fn clear(&mut self, b: u64) {
        let i = (b / 64) as usize;
        if i < self.bits.len() && self.bits[i] & (1 << (b % 64)) != 0 {
            self.bits[i] &= !(1 << (b % 64));
            self.present = self.present.saturating_sub(1);
            self.meta_dirty = true;
        }
    }

    fn fill_all(&mut self) {
        let n = Self::blocks(self.size);
        for b in 0..n {
            self.set(b);
        }
    }

    pub fn is_complete(&self) -> bool {
        self.present >= Self::blocks(self.size)
    }

    pub fn cached_bytes(&self) -> u64 {
        (self.present * BLOCK).min(self.size)
    }

    fn block_range(&self, start: u64, len: u64) -> std::ops::Range<u64> {
        if len == 0 || start >= self.size {
            return 0..0;
        }
        let end = (start + len).min(self.size);
        (start / BLOCK)..end.div_ceil(BLOCK)
    }
}

pub struct Entry {
    pub node: NodeId,
    file: File,
    pub state: Mutex<EntryState>,
    cv: Condvar,
}

/// A run of blocks one caller has claimed for download.
#[derive(Debug, Clone, Copy)]
pub struct Claim {
    pub first: u64,
    pub count: u64,
}

impl Claim {
    pub fn byte_range(&self, size: u64) -> (u64, u64) {
        let start = self.first * BLOCK;
        let end = ((self.first + self.count) * BLOCK).min(size);
        (start, end.saturating_sub(start))
    }
}

pub enum Need {
    Ready,
    /// Blocks this caller must fetch; the remaining ones are already in flight.
    Fetch(Vec<Claim>),
    Wait,
}

impl Entry {
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.state.lock().size;
        if offset >= size {
            return Ok(0);
        }
        let want = buf.len().min((size - offset) as usize);
        let mut done = 0;
        while done < want {
            let n = platform::read_at(&self.file, &mut buf[done..want], offset + done as u64)?;
            if n == 0 {
                buf[done..want].fill(0);
                break;
            }
            done += n;
        }
        self.state.lock().last_access = crate::now_ms();
        Ok(want)
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        platform::write_all_at(&self.file, data, offset)?;
        let mut st = self.state.lock();
        let end = offset + data.len() as u64;
        if end > st.size {
            st.size = end;
            st.resize_bits();
        }
        let r = st.block_range(offset, data.len() as u64);
        for b in r {
            st.set(b);
        }
        st.last_access = crate::now_ms();
        Ok(())
    }

    /// Writes downloaded plaintext. Marks blocks present once fully written.
    pub fn fill(&self, offset: u64, data: &[u8], run_end: u64) -> std::io::Result<()> {
        platform::write_all_at(&self.file, data, offset)?;
        let mut st = self.state.lock();
        let end = offset + data.len() as u64;
        let first = offset / BLOCK;
        let mut last_full = end / BLOCK;
        if end >= st.size.min(run_end) {
            last_full = end.div_ceil(BLOCK);
        }
        let mut changed = false;
        for b in first..last_full {
            if !st.has(b) && (b * BLOCK) < st.size {
                st.set(b);
                st.inflight.remove(&b);
                changed = true;
            }
        }
        drop(st);
        if changed {
            self.cv.notify_all();
        }
        Ok(())
    }

    pub fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)?;
        let mut st = self.state.lock();
        let old = st.size;
        st.size = len;
        st.resize_bits();
        if len > old {
            for b in (old.div_ceil(BLOCK))..EntryState::blocks(len) {
                st.set(b);
            }
        }
        st.meta_dirty = true;
        Ok(())
    }

    /// Decides what the caller must do to have `[start, start+len)` available.
    pub fn need(&self, start: u64, len: u64, readahead_blocks: u64) -> Need {
        let mut st = self.state.lock();
        let range = st.block_range(start, len);
        if range.clone().all(|b| st.has(b)) {
            return Need::Ready;
        }
        let total = EntryState::blocks(st.size);
        let mut claims: Vec<Claim> = Vec::new();
        let mut waiting = false;
        let mut b = range.start;
        let limit = (range.end + readahead_blocks).min(total);
        while b < limit {
            if st.has(b) {
                if b >= range.end {
                    break;
                }
                b += 1;
                continue;
            }
            if st.inflight.contains(&b) {
                if b < range.end {
                    waiting = true;
                }
                if b >= range.end {
                    break;
                }
                b += 1;
                continue;
            }
            st.inflight.insert(b);
            match claims.last_mut() {
                Some(c) if c.first + c.count == b => c.count += 1,
                _ => claims.push(Claim { first: b, count: 1 }),
            }
            b += 1;
        }
        st.failure = None;
        if !claims.is_empty() {
            Need::Fetch(claims)
        } else if waiting {
            Need::Wait
        } else {
            Need::Ready
        }
    }

    /// Releases claimed blocks after a failed download.
    pub fn abandon(&self, claims: &[Claim], error: String) {
        let mut st = self.state.lock();
        for c in claims {
            for b in c.first..c.first + c.count {
                st.inflight.remove(&b);
            }
        }
        st.failure = Some(error);
        drop(st);
        self.cv.notify_all();
    }

    /// Blocks until `[start, start+len)` is present, a download fails, or timeout.
    pub fn wait(&self, start: u64, len: u64, timeout: Duration) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.state.lock();
        loop {
            let range = st.block_range(start, len);
            if range.clone().all(|b| st.has(b)) {
                return Ok(());
            }
            if range.clone().any(|b| !st.has(b) && !st.inflight.contains(&b)) {
                return Err(st.failure.clone().unwrap_or_else(|| "download was interrupted".into()));
            }
            if self.cv.wait_until(&mut st, deadline).timed_out() {
                return Err("timed out waiting for data".into());
            }
        }
    }

    pub fn is_complete(&self) -> bool {
        self.state.lock().is_complete()
    }

    pub fn mark_local(&self) {
        let mut st = self.state.lock();
        st.key = ContentKey::Local;
        st.meta_dirty = true;
    }

    pub fn path_hint(&self) -> NodeId {
        self.node
    }
}

pub struct Cache {
    dir: PathBuf,
    entries: Mutex<HashMap<NodeId, Arc<Entry>>>,
}

pub struct CacheRow {
    pub node: NodeId,
    pub key: String,
    pub size: u64,
    pub bitmap: Vec<u8>,
    pub last_access: i64,
}

impl Cache {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        crate::platform::private_dir(dir)?;
        Ok(Self { dir: dir.to_path_buf(), entries: Mutex::new(HashMap::new()) })
    }

    pub fn file_path(&self, node: NodeId) -> PathBuf {
        self.dir.join(format!("{node}.blk"))
    }

    fn open_file(&self, node: NodeId, truncate: bool) -> std::io::Result<File> {
        let path = self.file_path(node);
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(truncate).open(&path)?;
        platform::make_sparse(&f);
        Ok(f)
    }

    /// Restores entries recorded in the database whose files still exist.
    pub fn restore(&self, rows: Vec<CacheRow>) -> Vec<NodeId> {
        let mut stale = Vec::new();
        let mut map = self.entries.lock();
        for r in rows {
            let path = self.file_path(r.node);
            let ok = std::fs::metadata(&path).map(|m| m.len() == r.size).unwrap_or(false);
            if !ok {
                stale.push(r.node);
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Ok(file) = self.open_file(r.node, false) else {
                stale.push(r.node);
                continue;
            };
            let mut bits = vec![0u64; r.bitmap.len().div_ceil(8)];
            for (i, chunk) in r.bitmap.chunks(8).enumerate() {
                let mut w = [0u8; 8];
                w[..chunk.len()].copy_from_slice(chunk);
                bits[i] = u64::from_le_bytes(w);
            }
            let mut st = EntryState {
                key: ContentKey::decode(&r.key),
                size: r.size,
                bits,
                present: 0,
                inflight: HashSet::new(),
                failure: None,
                last_access: r.last_access,
                meta_dirty: false,
            };
            st.resize_bits();
            map.insert(r.node, Arc::new(Entry { node: r.node, file, state: Mutex::new(st), cv: Condvar::new() }));
        }
        // Remove our own block files nobody references; never touch anything else.
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".blk").and_then(|s| s.parse::<u64>().ok()) {
                    if !map.contains_key(&id) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        stale
    }

    pub fn get(&self, node: NodeId) -> Option<Arc<Entry>> {
        self.entries.lock().get(&node).cloned()
    }

    /// Entry for remote content `key`; resets it if it held other content.
    pub fn for_remote(&self, node: NodeId, key: ContentKey, size: u64) -> std::io::Result<Arc<Entry>> {
        let mut map = self.entries.lock();
        if let Some(e) = map.get(&node) {
            let st = e.state.lock();
            if st.key == key || st.key == ContentKey::Local {
                drop(st);
                return Ok(e.clone());
            }
        }
        let file = self.open_file(node, true)?;
        file.set_len(size)?;
        let mut st = EntryState {
            key,
            size,
            bits: Vec::new(),
            present: 0,
            inflight: HashSet::new(),
            failure: None,
            last_access: crate::now_ms(),
            meta_dirty: true,
        };
        st.resize_bits();
        let e = Arc::new(Entry { node, file, state: Mutex::new(st), cv: Condvar::new() });
        map.insert(node, e.clone());
        Ok(e)
    }

    /// Fresh, authoritative local entry (new file or full overwrite).
    pub fn create_local(&self, node: NodeId, size: u64) -> std::io::Result<Arc<Entry>> {
        let file = self.open_file(node, true)?;
        file.set_len(size)?;
        let mut st = EntryState {
            key: ContentKey::Local,
            size,
            bits: Vec::new(),
            present: 0,
            inflight: HashSet::new(),
            failure: None,
            last_access: crate::now_ms(),
            meta_dirty: true,
        };
        st.resize_bits();
        st.fill_all();
        let e = Arc::new(Entry { node, file, state: Mutex::new(st), cv: Condvar::new() });
        self.entries.lock().insert(node, e.clone());
        Ok(e)
    }

    pub fn remove(&self, node: NodeId) {
        if self.entries.lock().remove(&node).is_some() {
            let _ = std::fs::remove_file(self.file_path(node));
        }
    }

    /// Moves cached content to another node (used for conflict copies).
    pub fn rekey(&self, from: NodeId, to: NodeId) -> std::io::Result<()> {
        let mut map = self.entries.lock();
        let Some(e) = map.remove(&from) else { return Ok(()) };
        let st = std::mem::replace(
            &mut *e.state.lock(),
            EntryState {
                key: ContentKey::Local,
                size: 0,
                bits: Vec::new(),
                present: 0,
                inflight: HashSet::new(),
                failure: None,
                last_access: 0,
                meta_dirty: false,
            },
        );
        drop(e);
        std::fs::rename(self.file_path(from), self.file_path(to))?;
        let file = self.open_file(to, false)?;
        let mut st = st;
        st.meta_dirty = true;
        map.insert(to, Arc::new(Entry { node: to, file, state: Mutex::new(st), cv: Condvar::new() }));
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.lock().values().map(|e| e.state.lock().cached_bytes()).sum()
    }

    /// Rows whose block maps changed since the last call.
    pub fn take_dirty_rows(&self) -> Vec<CacheRow> {
        let map = self.entries.lock();
        let mut out = Vec::new();
        for e in map.values() {
            let mut st = e.state.lock();
            if !st.meta_dirty {
                continue;
            }
            st.meta_dirty = false;
            let mut bitmap = Vec::with_capacity(st.bits.len() * 8);
            for w in &st.bits {
                bitmap.extend_from_slice(&w.to_le_bytes());
            }
            out.push(CacheRow { node: e.node, key: st.key.encode(), size: st.size, bitmap, last_access: st.last_access });
        }
        out
    }

    /// Least-recently-used eviction candidates, oldest first.
    pub fn lru(&self) -> Vec<(NodeId, u64, i64)> {
        let map = self.entries.lock();
        let mut v: Vec<_> = map
            .values()
            .map(|e| {
                let st = e.state.lock();
                (e.node, st.cached_bytes(), st.last_access)
            })
            .collect();
        v.sort_by_key(|x| x.2);
        v
    }

    pub fn ids(&self) -> Vec<NodeId> {
        self.entries.lock().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_and_fill() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path()).unwrap();
        let size = 5 * BLOCK + 10;
        let e = c.for_remote(7, ContentKey::remote("f", size), size).unwrap();
        let Need::Fetch(claims) = e.need(BLOCK + 5, 10, 2) else { panic!("expected fetch") };
        assert_eq!(claims.len(), 1);
        assert_eq!((claims[0].first, claims[0].count), (1, 3));
        assert!(matches!(e.need(BLOCK, 1, 0), Need::Wait));
        let (start, len) = claims[0].byte_range(size);
        e.fill(start, &vec![9u8; len as usize], start + len).unwrap();
        assert!(matches!(e.need(BLOCK, 3 * BLOCK, 0), Need::Ready));
        let mut buf = [0u8; 4];
        e.read_at(2 * BLOCK, &mut buf).unwrap();
        assert_eq!(buf, [9, 9, 9, 9]);

        let Need::Fetch(tail) = e.need(5 * BLOCK, 10, 0) else { panic!() };
        let (s, l) = tail[0].byte_range(size);
        assert_eq!((s, l), (5 * BLOCK, 10));
        e.fill(s, &[1u8; 10], s + l).unwrap();
        assert!(e.wait(5 * BLOCK, 10, Duration::from_millis(10)).is_ok());

        let rows = c.take_dirty_rows();
        assert_eq!(rows.len(), 1);
        drop(e);
        let c2 = Cache::new(dir.path()).unwrap();
        assert!(c2.restore(rows).is_empty());
        let e2 = c2.get(7).unwrap();
        assert!(e2.state.lock().has(5));
        assert!(!e2.state.lock().has(0));
    }

    #[test]
    fn local_entry_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path()).unwrap();
        let e = c.create_local(1, 0).unwrap();
        e.write_at(0, b"hello").unwrap();
        assert!(e.is_complete());
        e.set_len(3 * BLOCK).unwrap();
        assert!(e.is_complete());
        let mut b = [1u8; 3];
        e.read_at(2 * BLOCK, &mut b).unwrap();
        assert_eq!(b, [0, 0, 0]);
    }
}
