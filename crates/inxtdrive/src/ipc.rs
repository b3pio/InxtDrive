//! Local command channel between `inxt`/file-manager verbs and the running app.

use std::time::Duration;

#[cfg(windows)]
pub fn pipe_name() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!(r"\\.\pipe\InxtDrive-{user}-{}", crate::paths::instance_id())
}

#[cfg(windows)]
pub async fn send(command: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut pipe = loop {
        match ClientOptions::new().open(pipe_name()) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(231) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => anyhow::bail!("InxtDrive is not running ({e})"),
        }
    };
    pipe.write_all(format!("{command}\n").as_bytes()).await?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        match tokio::time::timeout_at(deadline, pipe.read(&mut buf)).await? {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // The server closing its end shows up as "broken pipe" or "not connected".
            Err(e) if matches!(e.raw_os_error(), Some(109) | Some(233)) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

#[cfg(windows)]
pub async fn serve<F>(handler: F) -> anyhow::Result<()>
where
    F: Fn(String) -> String + Send + Sync + 'static,
{
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;
    let handler = Arc::new(handler);
    let name = pipe_name();
    let mut server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
    loop {
        server.connect().await?;
        let mut conn = server;
        server = ServerOptions::new().create(&name)?;
        let h = handler.clone();
        tokio::spawn(async move {
            let mut line = String::new();
            let mut reader = BufReader::new(&mut conn);
            if reader.read_line(&mut line).await.is_ok() {
                let cmd = line.trim().to_string();
                let reply = tokio::task::spawn_blocking(move || h(cmd)).await.unwrap_or_else(|_| "err internal".into());
                let _ = conn.write_all(reply.as_bytes()).await;
                let _ = conn.flush().await;
            }
            // Dropping closes the handle; DisconnectNamedPipe would discard the unread reply.
            drop(conn);
        });
    }
}

#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    crate::paths::runtime_dir().join(format!("inxtdrive-{}.sock", crate::paths::instance_id()))
}

#[cfg(unix)]
pub async fn send(command: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::UnixStream::connect(socket_path())
        .await
        .map_err(|e| anyhow::anyhow!("InxtDrive is not running ({e})"))?;
    s.write_all(format!("{command}\n").as_bytes()).await?;
    s.shutdown().await?;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(300), s.read_to_end(&mut out)).await??;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Serves commands until the runtime stops. The caller must hold the
/// single-instance lock, which makes removing a leftover socket safe.
#[cfg(unix)]
pub async fn serve<F>(handler: F) -> anyhow::Result<()>
where
    F: Fn(String) -> String + Send + Sync + 'static,
{
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let handler = Arc::new(handler);
    let path = socket_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (mut conn, _) = listener.accept().await?;
        let h = handler.clone();
        tokio::spawn(async move {
            let mut line = String::new();
            let (read, mut write) = conn.split();
            if BufReader::new(read).read_line(&mut line).await.is_ok() {
                let cmd = line.trim().to_string();
                let reply = tokio::task::spawn_blocking(move || h(cmd)).await.unwrap_or_else(|_| "err internal".into());
                let _ = write.write_all(reply.as_bytes()).await;
                let _ = write.shutdown().await;
            }
        });
    }
}

/// Held for the life of the app so a second launch can tell one is running.
#[cfg(target_os = "linux")]
pub struct InstanceLock(#[allow(dead_code)] std::fs::File);

#[cfg(target_os = "linux")]
impl InstanceLock {
    /// `None` when another instance of this profile already runs.
    pub fn acquire() -> anyhow::Result<Option<Self>> {
        use std::os::fd::AsRawFd;
        let dir = crate::paths::data_dir();
        // Holds the session and decrypted cache; see `inxt_core::private_dir`.
        inxt_core::private_dir(&dir)?;
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("instance.lock"))?;
        // SAFETY: flock on a descriptor owned by `f`, which outlives the lock.
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if r == 0 {
            Ok(Some(Self(f)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(not(any(windows, unix)))]
pub async fn send(_command: &str) -> anyhow::Result<String> {
    anyhow::bail!("not supported on this platform")
}
