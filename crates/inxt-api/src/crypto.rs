use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipherSeek};
use md5::{Digest, Md5};
use rand::RngCore;
use ripemd::Ripemd160;
use sha2::{Sha256, Sha512};

use crate::error::{ApiError, Result};

/// Public constant shared by all Internxt clients; it only obfuscates the
/// login salt exchange, it is not a secret.
pub const APP_CRYPTO_SECRET: &str = "6KYQBP847D4ATSFA";

const OPENSSL_MAGIC: &[u8; 8] = b"Salted__";
const PASSWORD_HASH_ITERATIONS: u32 = 10_000;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
pub type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// OpenSSL `EVP_BytesToKey` with MD5 and one iteration, as used by CryptoJS
/// when it is given a passphrase instead of a key.
fn evp_bytes_to_key(secret: &[u8], salt: &[u8]) -> ([u8; 32], [u8; 16]) {
    let mut material = Vec::with_capacity(48);
    let mut prev: Vec<u8> = Vec::new();
    while material.len() < 48 {
        let mut h = Md5::new();
        h.update(&prev);
        h.update(secret);
        h.update(salt);
        prev = h.finalize().to_vec();
        material.extend_from_slice(&prev);
    }
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&material[..32]);
    iv.copy_from_slice(&material[32..48]);
    (key, iv)
}

/// CryptoJS-compatible `AES.encrypt(text, passphrase)`, hex encoded.
pub fn encrypt_text_with_key(plain: &str, secret: &str) -> String {
    let mut salt = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut salt);
    encrypt_text_with_salt(plain, secret, &salt)
}

fn encrypt_text_with_salt(plain: &str, secret: &str, salt: &[u8; 8]) -> String {
    let (key, iv) = evp_bytes_to_key(secret.as_bytes(), salt);
    let ct = Aes256CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    let mut out = Vec::with_capacity(16 + ct.len());
    out.extend_from_slice(OPENSSL_MAGIC);
    out.extend_from_slice(salt);
    out.extend_from_slice(&ct);
    hex::encode(out)
}

/// Inverse of [`encrypt_text_with_key`].
pub fn decrypt_text_with_key(encrypted_hex: &str, secret: &str) -> Result<String> {
    let raw = hex::decode(encrypted_hex.trim()).map_err(|e| ApiError::Crypto(format!("hex: {e}")))?;
    if raw.len() < 32 || &raw[..8] != OPENSSL_MAGIC {
        return Err(ApiError::Crypto("not an OpenSSL salted ciphertext".into()));
    }
    let (key, iv) = evp_bytes_to_key(secret.as_bytes(), &raw[8..16]);
    let plain = Aes256CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&raw[16..])
        .map_err(|_| ApiError::Crypto("bad padding (wrong password?)".into()))?;
    String::from_utf8(plain).map_err(|_| ApiError::Crypto("decrypted text is not UTF-8".into()))
}

/// PBKDF2-HMAC-SHA1, 10 000 rounds, 256-bit output, hex in and out.
pub fn pass_to_hash(password: &str, salt_hex: &str) -> Result<String> {
    let salt = hex::decode(salt_hex.trim()).map_err(|e| ApiError::Crypto(format!("salt hex: {e}")))?;
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), &salt, PASSWORD_HASH_ITERATIONS, &mut out);
    Ok(hex::encode(out))
}

/// The password proof sent to `/auth/login/access`.
pub fn encrypt_password_hash(password: &str, encrypted_salt: &str) -> Result<String> {
    let salt = decrypt_text_with_key(encrypted_salt, APP_CRYPTO_SECRET)?;
    let hash = pass_to_hash(password, &salt)?;
    Ok(encrypt_text_with_key(&hash, APP_CRYPTO_SECRET))
}

pub fn validate_mnemonic(mnemonic: &str) -> bool {
    bip39::Mnemonic::parse_normalized(mnemonic.trim()).is_ok()
}

/// BIP-39 seed with an empty passphrase.
pub fn mnemonic_to_seed(mnemonic: &str) -> Result<[u8; 64]> {
    let m = bip39::Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| ApiError::Crypto(format!("invalid mnemonic: {e}")))?;
    Ok(m.to_seed(""))
}

fn sha512_concat(a: &[u8], b: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(a);
    h.update(b);
    h.finalize().into()
}

/// Account-wide key for one bucket: `SHA512(seed || bucket_id)`.
#[derive(Clone)]
pub struct BucketKey([u8; 64]);

impl BucketKey {
    pub fn derive(mnemonic: &str, bucket_id_hex: &str) -> Result<Self> {
        let seed = mnemonic_to_seed(mnemonic)?;
        let bucket = hex::decode(bucket_id_hex).map_err(|e| ApiError::Crypto(format!("bucket id: {e}")))?;
        Ok(Self(sha512_concat(&seed, &bucket)))
    }

    /// Key material handed to share links: hex of the first 32 bytes.
    pub fn share_key_hex(&self) -> String {
        hex::encode(&self.0[..32])
    }

    /// Per-file key and IV from the file's random index (64 hex chars).
    pub fn file_key(&self, index_hex: &str) -> Result<FileKey> {
        let index = hex::decode(index_hex).map_err(|e| ApiError::Crypto(format!("file index: {e}")))?;
        if index.len() < 16 {
            return Err(ApiError::Crypto("file index too short".into()));
        }
        let derived = sha512_concat(&self.0[..32], &index);
        let mut key = [0u8; 32];
        let mut iv = [0u8; 16];
        key.copy_from_slice(&derived[..32]);
        iv.copy_from_slice(&index[..16]);
        Ok(FileKey { key, iv })
    }
}

impl std::fmt::Debug for BucketKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BucketKey(..)")
    }
}

#[derive(Clone)]
pub struct FileKey {
    key: [u8; 32],
    iv: [u8; 16],
}

impl FileKey {
    /// AES-256-CTR keystream positioned at `offset` bytes into the file.
    /// CTR is symmetric, so the same stream encrypts and decrypts.
    pub fn cipher_at(&self, offset: u64) -> Aes256Ctr {
        let mut c = Aes256Ctr::new(&self.key.into(), &self.iv.into());
        c.seek(offset);
        c
    }
}

impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKey(..)")
    }
}

pub fn random_index_hex() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Network content hash: `RIPEMD160(SHA256(ciphertext))`, given the SHA-256 state.
pub fn content_hash(sha256: Sha256) -> String {
    let digest = sha256.finalize();
    hex::encode(Ripemd160::digest(digest))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::StreamCipher;

    #[test]
    fn openssl_roundtrip() {
        let ct = encrypt_text_with_key("hello world", "secret");
        assert!(ct.starts_with(&hex::encode(OPENSSL_MAGIC)));
        assert_eq!(decrypt_text_with_key(&ct, "secret").unwrap(), "hello world");
        assert!(decrypt_text_with_key(&ct, "wrong").is_err());
    }

    #[test]
    fn ctr_seek_matches_sequential() {
        let key = BucketKey([7u8; 64]).file_key(&"ab".repeat(32)).unwrap();
        let mut data = vec![0u8; 100_000];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut whole = data.clone();
        key.cipher_at(0).apply_keystream(&mut whole);
        for start in [0usize, 1, 15, 16, 17, 4095, 65_537] {
            let mut part = data[start..].to_vec();
            key.cipher_at(start as u64).apply_keystream(&mut part);
            assert_eq!(&part[..], &whole[start..], "offset {start}");
        }
    }

    #[test]
    fn bip39_reference_vector() {
        // Trezor reference vector, passphrase "TREZOR".
        let m = bip39::Mnemonic::parse_normalized(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .unwrap();
        assert_eq!(
            hex::encode(m.to_seed("TREZOR")),
            "c55257c360c07c72029aebc1b53c05ed0362ada38ead3e3e9efa3708e53495531f09a6987599d18264c1e1c92f2cf141630c7a3c4ab7c81b2f001698e7463b04"
        );
    }
}
