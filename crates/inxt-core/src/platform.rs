use std::fs::File;
use std::io;

#[cfg(windows)]
pub fn read_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, offset)
}

#[cfg(unix)]
pub fn read_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, offset)
}

pub fn write_all_at(f: &File, mut data: &[u8], mut offset: u64) -> io::Result<()> {
    while !data.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_write(f, data, offset)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::write_at(f, data, offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
        }
        data = &data[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Marks a cache file sparse so unfetched regions take no disk space.
#[cfg(windows)]
pub fn make_sparse(f: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows::Win32::System::IO::DeviceIoControl;
    let mut returned = 0u32;
    // SAFETY: the handle is valid for the lifetime of `f`; no buffers are passed.
    let _ = unsafe {
        DeviceIoControl(HANDLE(f.as_raw_handle()), FSCTL_SET_SPARSE, None, 0, None, 0, Some(&mut returned), None)
    };
}

/// Unix files are sparse by default.
#[cfg(unix)]
pub fn make_sparse(_f: &File) {}

/// Creates a folder only this user can open. It holds decrypted file
/// contents and the session, and home folders are not private everywhere.
/// Existing folders are tightened too. On Windows the user profile already
/// keeps other users out.
pub fn private_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
