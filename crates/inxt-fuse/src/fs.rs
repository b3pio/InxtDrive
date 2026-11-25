use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use inxt_core::{Attr, Engine, FsError, NodeId, ROOT};
use parking_lot::Mutex;

/// Short enough that changes pulled from the cloud show up quickly.
const TTL: Duration = Duration::from_secs(1);
const BLOCK: u32 = 4096;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;
pub const FS_TYPE: &str = "inxtdrive";

fn errno(e: FsError) -> i32 {
    match e {
        FsError::NotFound => libc::ENOENT,
        FsError::AlreadyExists => libc::EEXIST,
        FsError::NotEmpty => libc::ENOTEMPTY,
        FsError::NotADirectory => libc::ENOTDIR,
        FsError::IsADirectory => libc::EISDIR,
        FsError::InvalidName => libc::EINVAL,
        FsError::AccessDenied => libc::EACCES,
        FsError::Io => libc::EIO,
        FsError::Network => libc::ENETUNREACH,
    }
}

fn time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

fn ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn ms_of(t: TimeOrNow) -> i64 {
    match t {
        TimeOrNow::SpecificTime(s) => ms(s),
        TimeOrNow::Now => inxt_core::now_ms(),
    }
}

fn kind(a: &Attr) -> FileType {
    if a.is_dir {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

#[derive(Clone, Copy)]
struct Owner {
    uid: u32,
    gid: u32,
}

fn file_attr(a: &Attr, o: Owner) -> FileAttr {
    let mtime = time(a.mtime);
    let perm = if a.is_dir {
        0o755
    } else if a.attrs & FILE_ATTRIBUTE_READONLY != 0 {
        0o444
    } else {
        0o644
    };
    FileAttr {
        ino: a.id,
        size: a.size,
        // Cloud-only files report their full size: tools that see few blocks
        // treat a file as sparse and may skip reading it.
        blocks: a.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: time(a.ctime),
        kind: kind(a),
        perm,
        nlink: if a.is_dir { 2 } else { 1 },
        uid: o.uid,
        gid: o.gid,
        rdev: 0,
        blksize: 128 * 1024,
        flags: 0,
    }
}

#[derive(Debug, Clone)]
pub struct MountOptions {
    pub mount_dir: PathBuf,
    /// Shown as the source in `df` and `mount`.
    pub fs_name: String,
}

struct Handle {
    id: NodeId,
    writer: bool,
    modified: AtomicBool,
}

/// A listing snapshot taken at `opendir`: inode, type, name.
type DirEntry = (u64, FileType, String);

struct InxtFs {
    engine: Arc<Engine>,
    /// Reads and writes can wait minutes for the network, so they run here
    /// instead of on the single FUSE request thread.
    rt: tokio::runtime::Handle,
    owner: Owner,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    dirs: Mutex<HashMap<u64, Vec<DirEntry>>>,
    next_fh: AtomicU64,
}

impl InxtFs {
    fn child_path(&self, parent: u64, name: &OsStr) -> Result<String, i32> {
        let name = name.to_str().ok_or(libc::EINVAL)?;
        let base = self.engine.path_of(parent).ok_or(libc::ENOENT)?;
        Ok(if base.trim_end_matches('\\').is_empty() { format!("\\{name}") } else { format!("{base}\\{name}") })
    }

    fn parent_of(&self, id: NodeId) -> NodeId {
        if id == ROOT {
            return ROOT;
        }
        let Some(path) = self.engine.path_of(id) else { return ROOT };
        let parent = path.trim_end_matches('\\').rsplit_once('\\').map(|(p, _)| p).unwrap_or("");
        self.engine.lookup(parent).map(|a| a.id).unwrap_or(ROOT)
    }

    fn add_handle(&self, id: NodeId, writer: bool, modified: bool) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(fh, Arc::new(Handle { id, writer, modified: AtomicBool::new(modified) }));
        fh
    }

    fn reply_attr(&self, id: NodeId, reply: ReplyAttr) {
        match self.engine.attr(id) {
            Some(a) => reply.attr(&TTL, &file_attr(&a, self.owner)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn create_node(&self, parent: u64, name: &OsStr, is_dir: bool) -> Result<Attr, i32> {
        let path = self.child_path(parent, name)?;
        self.engine.create(&path, is_dir).map_err(errno)
    }

    fn remove(&self, parent: u64, name: &OsStr, want_dir: bool) -> Result<(), i32> {
        let path = self.child_path(parent, name)?;
        let a = self.engine.lookup(&path).ok_or(libc::ENOENT)?;
        match (want_dir, a.is_dir) {
            (true, false) => Err(libc::ENOTDIR),
            (false, true) => Err(libc::EISDIR),
            _ => self.engine.delete(a.id).map_err(errno),
        }
    }

    fn do_rename(&self, parent: u64, name: &OsStr, new_parent: u64, new_name: &OsStr, flags: u32) -> Result<(), i32> {
        if flags & RENAME_EXCHANGE != 0 {
            return Err(libc::EINVAL);
        }
        let from = self.child_path(parent, name)?;
        let to = self.child_path(new_parent, new_name)?;
        let a = self.engine.lookup(&from).ok_or(libc::ENOENT)?;
        let replace = flags & RENAME_NOREPLACE == 0;
        if let Some(target) = self.engine.lookup(&to).filter(|t| t.id != a.id) {
            if !replace {
                return Err(libc::EEXIST);
            }
            match (a.is_dir, target.is_dir) {
                (false, true) => return Err(libc::EISDIR),
                (true, false) => return Err(libc::ENOTDIR),
                // POSIX lets a folder replace an empty folder; the engine only replaces files.
                (true, true) => self.engine.delete(target.id).map_err(errno)?,
                (false, false) => {}
            }
        }
        self.engine.rename(a.id, &to, replace).map_err(errno)
    }
}

impl Filesystem for InxtFs {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        match self.engine.lookup(&path) {
            Some(a) => reply.entry(&TTL, &file_attr(&a, self.owner), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        self.reply_attr(ino, reply);
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(current) = self.engine.attr(ino) else { return reply.error(libc::ENOENT) };
        if let Some(m) = mode {
            let ro = if m & 0o222 == 0 { current.attrs | FILE_ATTRIBUTE_READONLY } else { current.attrs & !FILE_ATTRIBUTE_READONLY };
            if ro != current.attrs {
                self.engine.set_attrs(ino, ro);
            }
        }
        if mtime.is_some() || crtime.is_some() {
            self.engine.set_times(ino, crtime.map(ms), mtime.map(ms_of));
        }
        let Some(new_size) = size.filter(|_| !current.is_dir) else { return self.reply_attr(ino, reply) };
        let engine = self.engine.clone();
        let owner = self.owner;
        let handle = fh.and_then(|f| self.handles.lock().get(&f).cloned());
        self.rt.spawn_blocking(move || {
            // A truncate by path has no open file to report the change on close.
            if handle.is_none() {
                engine.open_handle(ino, true);
            }
            let res = engine.set_size(ino, new_size);
            match &handle {
                Some(h) => h.modified.store(true, Ordering::SeqCst),
                None => engine.release(ino, true, res.is_ok()),
            }
            match (res, engine.attr(ino)) {
                (Err(e), _) => reply.error(errno(e)),
                (Ok(()), Some(a)) => reply.attr(&TTL, &file_attr(&a, owner)),
                (Ok(()), None) => reply.error(libc::ENOENT),
            }
        });
    }

    fn mknod(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(libc::EPERM);
        }
        match self.create_node(parent, name, false) {
            Ok(a) => {
                self.engine.open_handle(a.id, true);
                self.engine.release(a.id, true, true);
                reply.entry(&TTL, &file_attr(&a, self.owner), 0)
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        match self.create_node(parent, name, true) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a, self.owner), 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        match self.do_rename(parent, name, newparent, newname, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(a) = self.engine.attr(ino) else { return reply.error(libc::ENOENT) };
        if a.is_dir {
            return reply.error(libc::EISDIR);
        }
        let writer = flags & libc::O_ACCMODE != libc::O_RDONLY;
        self.engine.open_handle(ino, writer);
        reply.opened(self.add_handle(ino, writer, false), 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let engine = self.engine.clone();
        self.rt.spawn_blocking(move || {
            let mut buf = vec![0u8; size as usize];
            match engine.read(ino, offset.max(0) as u64, &mut buf) {
                Ok(n) => reply.data(&buf[..n]),
                Err(e) => reply.error(errno(e)),
            }
        });
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let engine = self.engine.clone();
        let handle = self.handles.lock().get(&fh).cloned();
        let data = data.to_vec();
        self.rt.spawn_blocking(move || match engine.write(ino, offset.max(0) as u64, &data, false) {
            Ok((n, _)) => {
                if let Some(h) = handle {
                    h.modified.store(true, Ordering::SeqCst);
                }
                reply.written(n as u32)
            }
            Err(e) => reply.error(errno(e)),
        });
    }

    fn flush(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        self.engine.flush(ino);
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        self.engine.flush(ino);
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(h) = self.handles.lock().remove(&fh) {
            self.engine.release(h.id, h.writer, h.modified.load(Ordering::SeqCst));
        }
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let children = match self.engine.list(ino) {
            Ok(c) => c,
            Err(e) => return reply.error(errno(e)),
        };
        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push((ino, FileType::Directory, ".".to_string()));
        entries.push((self.parent_of(ino), FileType::Directory, "..".to_string()));
        entries.extend(children.into_iter().map(|(name, a)| (a.id, kind(&a), name)));
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dirs.lock().insert(fh, entries);
        reply.opened(fh, 0);
    }

    fn readdir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let dirs = self.dirs.lock();
        let Some(entries) = dirs.get(&fh) else { return reply.error(libc::EBADF) };
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(*ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.dirs.lock().remove(&fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let (total, free) = self.engine.volume_info();
        let b = BLOCK as u64;
        reply.statfs(total / b, free / b, free / b, 1 << 32, 1 << 31, BLOCK, 255, BLOCK);
    }

    fn access(&mut self, _req: &Request<'_>, ino: u64, _mask: i32, reply: ReplyEmpty) {
        if self.engine.attr(ino).is_some() {
            reply.ok()
        } else {
            reply.error(libc::ENOENT)
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.create_node(parent, name, false) {
            Ok(a) => {
                self.engine.open_handle(a.id, true);
                let fh = self.add_handle(a.id, true, true);
                reply.created(&TTL, &file_attr(&a, self.owner), 0, fh, 0)
            }
            Err(e) => reply.error(e),
        }
    }
}

/// The mounted folder. Call [`Drive::unmount`] to detach it.
pub struct Drive {
    session: Option<fuser::BackgroundSession>,
    mount_point: PathBuf,
}

impl Drive {
    pub fn mount(engine: Arc<Engine>, rt: tokio::runtime::Handle, opts: &MountOptions) -> anyhow::Result<Self> {
        if !Path::new("/dev/fuse").exists() {
            anyhow::bail!("FUSE is not available on this system (/dev/fuse is missing). Install the fuse3 package.");
        }
        let dir = opts.mount_dir.clone();
        prepare_dir(&dir)?;
        // SAFETY: getuid/getgid cannot fail and touch no memory.
        let owner = unsafe { Owner { uid: libc::getuid(), gid: libc::getgid() } };
        let fs = InxtFs {
            engine,
            rt,
            owner,
            handles: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        };
        let options = [
            MountOption::FSName(opts.fs_name.clone()),
            MountOption::Subtype(FS_TYPE.into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoAtime,
            MountOption::RW,
        ];
        let session = fuser::spawn_mount2(fs, &dir, &options)
            .map_err(|e| anyhow::anyhow!("could not mount {}: {e} (is the fuse3 package installed?)", dir.display()))?;
        tracing::info!(mount_point = %dir.display(), "drive mounted");
        Ok(Self { session: Some(session), mount_point: dir })
    }

    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Detaches lazily so a shell or file manager still inside the folder
    /// cannot keep the app from quitting.
    pub fn unmount(mut self) {
        lazy_unmount(&self.mount_point);
        if let Some(s) = self.session.take() {
            // `join` unwraps the session result; a failed session loop must not
            // take a panic message with it on the way out.
            std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.join()));
            });
        }
    }
}

fn lazy_unmount(dir: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        let ran = std::process::Command::new(tool)
            .args(["-u", "-z"])
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ran.is_ok() {
            return;
        }
    }
}

/// `/proc/self/mountinfo` escapes spaces and a few other bytes as `\ooo`.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether an InxtDrive mount (live or left over from a crash) sits on `dir`.
fn our_mount_on(dir: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else { return false };
    let want = dir.to_string_lossy();
    info.lines().any(|l| {
        let mut halves = l.splitn(2, " - ");
        let (Some(left), Some(right)) = (halves.next(), halves.next()) else { return false };
        let point = left.split(' ').nth(4).map(unescape_mount);
        let fstype = right.split(' ').next().unwrap_or("");
        point.as_deref() == Some(&*want) && fstype == format!("fuse.{FS_TYPE}")
    })
}

fn prepare_dir(dir: &Path) -> anyhow::Result<()> {
    let stale = matches!(std::fs::metadata(dir), Err(e) if e.raw_os_error() == Some(libc::ENOTCONN));
    if stale || our_mount_on(dir) {
        tracing::warn!(dir = %dir.display(), "removing a leftover InxtDrive mount");
        lazy_unmount(dir);
    }
    std::fs::create_dir_all(dir)?;
    if std::fs::read_dir(dir)?.next().is_some() {
        anyhow::bail!(
            "The folder {} is not empty. InxtDrive needs an empty folder for your drive; choose another one in Settings > Drive & storage.",
            dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mountinfo_escapes() {
        assert_eq!(super::unescape_mount(r"/home/a\040b/Internxt"), "/home/a b/Internxt");
        assert_eq!(super::unescape_mount("/plain"), "/plain");
    }
}
