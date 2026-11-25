use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use inxt_core::{Attr, Change, ChangeKind, Engine, FsError, NodeId, ROOT};
use widestring::U16CStr;
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, ModificationDescriptor, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};
use winfsp::notify::{Notifier, NotifyInfo, NotifyingFileSystemContext};
use winfsp::FspError;

use crate::security;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9u32 as i32;
const STATUS_UNEXPECTED_NETWORK_ERROR: i32 = 0xC000_00C4u32 as i32;
const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;

const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;

const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x04;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
const KEPT_ATTRIBUTES: u32 = FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;

const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x01;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x02;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x08;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;
const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// Offset between 1601-01-01 (FILETIME) and 1970-01-01, in milliseconds.
const EPOCH_DIFF_MS: i64 = 11_644_473_600_000;

fn filetime(ms: i64) -> u64 {
    ((ms + EPOCH_DIFF_MS).max(0) as u64) * 10_000
}

fn from_filetime(ft: u64) -> Option<i64> {
    if ft == 0 || ft == u64::MAX {
        return None;
    }
    Some((ft / 10_000) as i64 - EPOCH_DIFF_MS)
}

fn nt(code: i32) -> FspError {
    FspError::NTSTATUS(code)
}

fn map_err(e: FsError) -> FspError {
    nt(match e {
        FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        FsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        FsError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        FsError::InvalidName => STATUS_OBJECT_NAME_INVALID,
        FsError::AccessDenied => STATUS_ACCESS_DENIED,
        FsError::Io => STATUS_UNEXPECTED_IO_ERROR,
        FsError::Network => STATUS_UNEXPECTED_NETWORK_ERROR,
    })
}

#[derive(Debug, Clone)]
pub struct MountOptions {
    /// `"I:"`, or `None` for the next free letter.
    pub drive_letter: Option<String>,
    pub volume_label: String,
    pub filesystem_name: String,
    pub serial: u32,
    /// Show not-yet-downloaded files as offline, so Explorer and antivirus
    /// avoid opening them just to draw thumbnails.
    pub mark_cloud_files_offline: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            drive_letter: Some("I:".into()),
            volume_label: "Internxt".into(),
            filesystem_name: "exFAT".into(),
            serial: 0x1A7E_D21E,
            mark_cloud_files_offline: false,
        }
    }
}

pub struct FileCtx {
    id: NodeId,
    is_dir: bool,
    writer: bool,
    modified: AtomicBool,
    dir: DirBuffer,
}

pub struct InxtFs {
    engine: Arc<Engine>,
    sd: Vec<u8>,
    label: String,
    offline_attr: bool,
}

impl InxtFs {
    fn path(name: &U16CStr) -> String {
        name.to_string_lossy()
    }

    /// Sync state travels in attribute bits the Explorer status-badge
    /// extension reads, so it needs no connection to this process:
    /// ARCHIVE = not in the cloud yet (the classic "needs backup" bit),
    /// PINNED = in the cloud and on this PC, UNPINNED = in the cloud only.
    /// Local-only items carry none of them and get no badge.
    fn attributes(&self, a: &Attr) -> u32 {
        let mut x = a.attrs & KEPT_ATTRIBUTES;
        if a.is_dir {
            x |= FILE_ATTRIBUTE_DIRECTORY;
        }
        if !a.excluded {
            if a.pending {
                x |= FILE_ATTRIBUTE_ARCHIVE;
            } else if !a.is_dir && a.cached {
                x |= FILE_ATTRIBUTE_PINNED;
            } else {
                x |= FILE_ATTRIBUTE_UNPINNED;
                if self.offline_attr && !a.is_dir {
                    x |= FILE_ATTRIBUTE_OFFLINE;
                }
            }
        }
        if x == 0 {
            x = FILE_ATTRIBUTE_NORMAL;
        }
        x
    }

    fn fill(&self, a: &Attr, fi: &mut FileInfo) {
        fi.file_attributes = self.attributes(a);
        fi.reparse_tag = 0;
        fi.file_size = a.size;
        fi.allocation_size = a.size.div_ceil(4096) * 4096;
        fi.creation_time = filetime(a.ctime);
        fi.last_access_time = filetime(a.mtime);
        fi.last_write_time = filetime(a.mtime);
        fi.change_time = filetime(a.mtime);
        fi.index_number = a.id;
        fi.hard_links = 0;
        fi.ea_size = 0;
    }

    fn attr(&self, id: NodeId) -> winfsp::Result<Attr> {
        self.engine.attr(id).ok_or_else(|| nt(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    fn not_found(&self, path: &str) -> FspError {
        let parent = match path.trim_end_matches('\\').rfind('\\') {
            Some(i) => &path[..i],
            None => "",
        };
        if parent.is_empty() || self.engine.lookup(parent).is_some_and(|a| a.is_dir) {
            nt(STATUS_OBJECT_NAME_NOT_FOUND)
        } else {
            nt(STATUS_OBJECT_PATH_NOT_FOUND)
        }
    }
}

impl FileSystemContext for InxtFs {
    type FileContext = FileCtx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = Self::path(file_name);
        let a = self.engine.lookup(&path).ok_or_else(|| self.not_found(&path))?;
        if !security::copy_to(&self.sd, security_descriptor) {
            return Err(nt(STATUS_BUFFER_OVERFLOW));
        }
        Ok(FileSecurity { reparse: false, sz_security_descriptor: self.sd.len() as u64, attributes: self.attributes(&a) })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<FileCtx> {
        let path = Self::path(file_name);
        let a = self.engine.lookup(&path).ok_or_else(|| self.not_found(&path))?;
        if a.is_dir && create_options & FILE_NON_DIRECTORY_FILE != 0 {
            return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
        }
        if !a.is_dir && create_options & FILE_DIRECTORY_FILE != 0 {
            return Err(nt(STATUS_NOT_A_DIRECTORY));
        }
        let writer = !a.is_dir && granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        self.engine.open_handle(a.id, writer);
        self.fill(&a, file_info.as_mut());
        Ok(FileCtx { id: a.id, is_dir: a.is_dir, writer, modified: AtomicBool::new(false), dir: DirBuffer::new() })
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<FileCtx> {
        let path = Self::path(file_name);
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let a = self.engine.create(&path, is_dir).map_err(|e| match e {
            FsError::NotFound => nt(STATUS_OBJECT_PATH_NOT_FOUND),
            other => map_err(other),
        })?;
        let kept = file_attributes & KEPT_ATTRIBUTES;
        if kept != 0 {
            self.engine.set_attrs(a.id, kept);
        }
        let a = self.attr(a.id)?;
        self.engine.open_handle(a.id, !is_dir);
        self.fill(&a, file_info.as_mut());
        Ok(FileCtx { id: a.id, is_dir, writer: !is_dir, modified: AtomicBool::new(!is_dir), dir: DirBuffer::new() })
    }

    fn close(&self, ctx: FileCtx) {
        self.engine.release(ctx.id, ctx.writer, ctx.modified.load(Ordering::SeqCst));
    }

    fn cleanup(&self, ctx: &FileCtx, _file_name: Option<&U16CStr>, flags: u32) {
        if FspCleanupFlags::FspCleanupDelete.is_flagged(flags) {
            if let Err(e) = self.engine.delete(ctx.id) {
                tracing::warn!(node = ctx.id, error = ?e, "delete on close failed");
            }
        }
    }

    fn flush(&self, ctx: Option<&FileCtx>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if let Some(c) = ctx {
            self.engine.flush(c.id);
            let a = self.attr(c.id)?;
            self.fill(&a, file_info);
        }
        Ok(())
    }

    fn get_file_info(&self, ctx: &FileCtx, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let a = self.attr(ctx.id)?;
        self.fill(&a, file_info);
        Ok(())
    }

    fn get_security(&self, _ctx: &FileCtx, security_descriptor: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        if !security::copy_to(&self.sd, security_descriptor) {
            return Err(nt(STATUS_BUFFER_OVERFLOW));
        }
        Ok(self.sd.len() as u64)
    }

    fn set_security(&self, _ctx: &FileCtx, _info: u32, _desc: ModificationDescriptor) -> winfsp::Result<()> {
        Ok(())
    }

    fn overwrite(
        &self,
        ctx: &FileCtx,
        file_attributes: u32,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.engine.set_size(ctx.id, 0).map_err(map_err)?;
        let current = self.attr(ctx.id)?.attrs;
        let kept = file_attributes & KEPT_ATTRIBUTES;
        let attrs = if replace_file_attributes { kept } else { current | kept };
        if attrs != current {
            self.engine.set_attrs(ctx.id, attrs);
        }
        ctx.modified.store(true, Ordering::SeqCst);
        let a = self.attr(ctx.id)?;
        self.fill(&a, file_info);
        Ok(())
    }

    fn read_directory(
        &self,
        ctx: &FileCtx,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if let Ok(lock) = ctx.dir.acquire(marker.is_none(), None) {
            let mut di: DirInfo = DirInfo::new();
            if ctx.id != ROOT {
                let me = self.attr(ctx.id)?;
                for dots in [".", ".."] {
                    di.reset();
                    self.fill(&me, di.file_info_mut());
                    di.set_name(dots)?;
                    lock.write(&mut di)?;
                }
            }
            for (name, a) in self.engine.list(ctx.id).map_err(map_err)? {
                di.reset();
                self.fill(&a, di.file_info_mut());
                if di.set_name(&name).is_err() {
                    continue;
                }
                lock.write(&mut di)?;
            }
        }
        Ok(ctx.dir.read(marker, buffer))
    }

    fn rename(&self, ctx: &FileCtx, _file_name: &U16CStr, new_file_name: &U16CStr, replace_if_exists: bool) -> winfsp::Result<()> {
        let to = Self::path(new_file_name);
        self.engine.rename(ctx.id, &to, replace_if_exists).map_err(map_err)
    }

    fn set_basic_info(
        &self,
        ctx: &FileCtx,
        file_attributes: u32,
        creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if file_attributes != u32::MAX {
            self.engine.set_attrs(ctx.id, file_attributes & KEPT_ATTRIBUTES);
        }
        self.engine.set_times(ctx.id, from_filetime(creation_time), from_filetime(last_write_time));
        let a = self.attr(ctx.id)?;
        self.fill(&a, file_info);
        Ok(())
    }

    fn set_delete(&self, ctx: &FileCtx, _file_name: &U16CStr, delete_file: bool) -> winfsp::Result<()> {
        if delete_file {
            self.engine.can_delete(ctx.id).map_err(map_err)?;
        }
        Ok(())
    }

    fn set_file_size(&self, ctx: &FileCtx, new_size: u64, set_allocation_size: bool, file_info: &mut FileInfo) -> winfsp::Result<()> {
        let current = self.attr(ctx.id)?.size;
        if set_allocation_size {
            if new_size < current {
                self.engine.set_size(ctx.id, new_size).map_err(map_err)?;
                ctx.modified.store(true, Ordering::SeqCst);
            }
        } else if new_size != current {
            self.engine.set_size(ctx.id, new_size).map_err(map_err)?;
            ctx.modified.store(true, Ordering::SeqCst);
        }
        let a = self.attr(ctx.id)?;
        self.fill(&a, file_info);
        Ok(())
    }

    fn read(&self, ctx: &FileCtx, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if ctx.is_dir {
            return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
        }
        let n = self.engine.read(ctx.id, offset, buffer).map_err(map_err)?;
        if n == 0 && !buffer.is_empty() {
            return Err(nt(STATUS_END_OF_FILE));
        }
        Ok(n as u32)
    }

    fn write(
        &self,
        ctx: &FileCtx,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let mut data = buffer;
        if constrained_io {
            let size = self.attr(ctx.id)?.size;
            if offset >= size {
                let a = self.attr(ctx.id)?;
                self.fill(&a, file_info);
                return Ok(0);
            }
            let room = (size - offset) as usize;
            data = &buffer[..buffer.len().min(room)];
        }
        let (n, _) = self.engine.write(ctx.id, offset, data, write_to_eof).map_err(map_err)?;
        ctx.modified.store(true, Ordering::SeqCst);
        let a = self.attr(ctx.id)?;
        self.fill(&a, file_info);
        Ok(n as u32)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, free) = self.engine.volume_info();
        out.total_size = total;
        out.free_size = free;
        out.set_volume_label(&self.label);
        Ok(())
    }

    fn set_volume_label(&self, _label: &U16CStr, out: &mut VolumeInfo) -> winfsp::Result<()> {
        self.get_volume_info(out)
    }
}

impl NotifyingFileSystemContext<Vec<Change>> for InxtFs {
    fn should_notify(&self) -> Option<Vec<Change>> {
        let c = self.engine.take_changes();
        (!c.is_empty()).then_some(c)
    }

    fn notify(&self, changes: Vec<Change>, notifier: &Notifier) {
        let mut info: NotifyInfo = NotifyInfo::new();
        let mut send = |path: &str, filter: u32, action: u32| {
            info.reset();
            info.filter = filter;
            info.action = action;
            if info.set_name(path).is_ok() {
                notifier.notify(&info);
            }
        };
        for c in changes {
            let name_filter = if c.is_dir { FILE_NOTIFY_CHANGE_DIR_NAME } else { FILE_NOTIFY_CHANGE_FILE_NAME };
            match c.kind {
                ChangeKind::Added => send(&c.path, name_filter, FILE_ACTION_ADDED),
                ChangeKind::Removed => send(&c.path, name_filter, FILE_ACTION_REMOVED),
                ChangeKind::Modified => {
                    send(&c.path, FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_ACTION_MODIFIED)
                }
                ChangeKind::Renamed { old_path } => {
                    send(&old_path, name_filter, FILE_ACTION_RENAMED_OLD_NAME);
                    send(&c.path, name_filter, FILE_ACTION_RENAMED_NEW_NAME);
                }
            }
        }
    }
}

/// A mounted drive. Unmounts when dropped.
pub struct Drive {
    host: FileSystemHost<InxtFs, FineGuard>,
    mount_point: String,
}

impl Drive {
    pub fn mount(engine: Arc<Engine>, opts: &MountOptions) -> anyhow::Result<Self> {
        winfsp::winfsp_init().map_err(|e| anyhow::anyhow!("WinFsp is not installed or could not be loaded ({e:?})"))?;
        let mut vp = VolumeParams::new();
        vp.sector_size(4096)
            .sectors_per_allocation_unit(1)
            .max_component_length(255)
            .volume_creation_time(filetime(inxt_core::now_ms()))
            .volume_serial_number(opts.serial)
            .file_info_timeout(1000)
            .irp_timeout(600_000)
            .case_sensitive_search(false)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .persistent_acls(true)
            .post_cleanup_when_modified_only(true)
            .pass_query_directory_pattern(false)
            .flush_and_purge_on_cleanup(false)
            .filesystem_name(&opts.filesystem_name);

        let fs = InxtFs {
            engine,
            sd: security::default_descriptor(),
            label: opts.volume_label.clone(),
            offline_attr: opts.mark_cloud_files_offline,
        };
        let mut host: FileSystemHost<InxtFs, FineGuard> =
            FileSystemHost::new_with_timer::<Vec<Change>, 1000>(FileSystemParams::default_params(vp), fs)
                .map_err(|e| anyhow::anyhow!("could not create the filesystem: {e}"))?;

        let mount_point = match opts.drive_letter.as_deref().map(normalize_letter) {
            Some(l) if !drive_in_use(&l) => l,
            _ => free_letter().ok_or_else(|| anyhow::anyhow!("no free drive letter"))?,
        };
        host.mount(mount_point.as_str()).map_err(|e| anyhow::anyhow!("could not mount {mount_point}: {e}"))?;
        let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4).clamp(8, 32);
        host.start_with_threads(threads).map_err(|e| anyhow::anyhow!("could not start the filesystem: {e}"))?;
        tracing::info!(%mount_point, "drive mounted");
        Ok(Self { host, mount_point })
    }

    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }

    pub fn unmount(mut self) {
        self.host.stop();
        self.host.unmount();
    }
}

fn normalize_letter(l: &str) -> String {
    let c = l.trim().chars().next().unwrap_or('I').to_ascii_uppercase();
    format!("{c}:")
}

fn used_letters() -> u32 {
    // SAFETY: no arguments; returns a bitmask of existing drive letters.
    unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() }
}

fn drive_in_use(letter: &str) -> bool {
    let c = letter.chars().next().unwrap_or('A').to_ascii_uppercase();
    if !c.is_ascii_uppercase() {
        return true;
    }
    used_letters() & (1 << (c as u8 - b'A')) != 0
}

fn free_letter() -> Option<String> {
    let used = used_letters();
    ('D'..='Z').rev().find(|c| used & (1 << (*c as u8 - b'A')) == 0).map(|c| format!("{c}:"))
}
