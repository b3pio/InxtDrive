//! Stores the session (token and mnemonic) encrypted for the current user.

use inxt_api::Credentials;

use crate::paths;

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    const ENTROPY: &[u8] = b"InxtDrive.session.v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// DPAPI: only this Windows user on this machine can decrypt the result.
    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input buffers outlive the call; output is freed with LocalFree.
        unsafe {
            CryptProtectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(v)
        }
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as above.
        unsafe {
            CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)?;
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(v)
        }
    }
}

/// AES-GCM with a random key kept in the desktop keyring (Secret Service:
/// GNOME Keyring, KWallet, KeePassXC). Without a keyring the key falls back
/// to a file only this user can read, next to the session.
#[cfg(target_os = "linux")]
mod imp {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use rand::RngCore;

    const MAGIC: &[u8; 4] = b"IXS1";
    const FROM_KEYRING: u8 = 1;
    const FROM_FILE: u8 = 2;

    fn keyring_entry() -> keyring::Result<keyring::Entry> {
        keyring::Entry::new("InxtDrive", &format!("session-key-{}", crate::paths::instance_id()))
    }

    fn key_file() -> std::path::PathBuf {
        crate::paths::data_dir().join("session.key")
    }

    fn as_key(v: &[u8]) -> anyhow::Result<[u8; 32]> {
        v.try_into().map_err(|_| anyhow::anyhow!("stored session key has the wrong length"))
    }

    fn new_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        k
    }

    fn keyring_key(create: bool) -> anyhow::Result<[u8; 32]> {
        let e = keyring_entry()?;
        match e.get_secret() {
            Ok(v) => as_key(&v),
            Err(keyring::Error::NoEntry) if create => {
                let k = new_key();
                e.set_secret(&k)?;
                Ok(k)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn file_key(create: bool) -> anyhow::Result<[u8; 32]> {
        let path = key_file();
        match std::fs::read(&path) {
            Ok(v) => as_key(&v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                if let Some(d) = path.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let k = new_key();
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
                f.write_all(&k)?;
                Ok(k)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (mode, key) = match keyring_key(true) {
            Ok(k) => (FROM_KEYRING, k),
            Err(e) => {
                tracing::warn!(error = %e, "no desktop keyring; keeping the session key in a private file");
                (FROM_FILE, file_key(true)?)
            }
        };
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let sealed = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
            .encrypt(Nonce::from_slice(&nonce), data)
            .map_err(|_| anyhow::anyhow!("could not encrypt the session"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + 1 + nonce.len() + sealed.len());
        out.extend_from_slice(MAGIC);
        out.push(mode);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    pub fn unprotect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if data.len() < 17 || &data[..4] != MAGIC {
            anyhow::bail!("unknown session file format");
        }
        let key = match data[4] {
            FROM_KEYRING => keyring_key(false)?,
            FROM_FILE => file_key(false)?,
            _ => anyhow::bail!("unknown session key source"),
        };
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
            .decrypt(Nonce::from_slice(&data[5..17]), &data[17..])
            .map_err(|_| anyhow::anyhow!("the session could not be decrypted"))
    }

    pub fn forget_key() {
        if let Ok(e) = keyring_entry() {
            let _ = e.delete_credential();
        }
        let _ = std::fs::remove_file(key_file());
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn protect(_data: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("secure session storage is not implemented on this platform yet")
    }

    pub fn unprotect(_data: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("secure session storage is not implemented on this platform yet")
    }

    pub fn forget_key() {}
}

pub fn save_session(creds: &Credentials) -> anyhow::Result<()> {
    let path = paths::session_file();
    if let Some(d) = path.parent() {
        inxt_core::private_dir(d)?;
    }
    let plain = serde_json::to_vec(creds)?;
    let sealed = imp::protect(&plain)?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&tmp)?, &sealed)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_session() -> Option<Credentials> {
    let sealed = std::fs::read(paths::session_file()).ok()?;
    let plain = imp::unprotect(&sealed).map_err(|e| tracing::warn!(error = %e, "could not unlock saved session")).ok()?;
    serde_json::from_slice(&plain).ok()
}

pub fn clear_session() {
    let _ = std::fs::remove_file(paths::session_file());
    #[cfg(not(windows))]
    imp::forget_key();
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn dpapi_roundtrip() {
        let sealed = super::imp::protect(b"secret words").unwrap();
        assert_ne!(sealed, b"secret words");
        assert_eq!(super::imp::unprotect(&sealed).unwrap(), b"secret words");
    }
}
