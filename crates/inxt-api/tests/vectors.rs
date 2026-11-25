//! Cross-checks the Rust crypto against vectors produced independently by
//! Node's OpenSSL (`tools/gen-vectors.mjs`).

use aes::cipher::StreamCipher;
use inxt_api::crypto;
use sha2::{Digest, Sha256};

fn vectors() -> serde_json::Value {
    serde_json::from_str(include_str!("vectors.json")).unwrap()
}

fn s<'a>(v: &'a serde_json::Value, path: &[&str]) -> &'a str {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_str().unwrap()
}

#[test]
fn openssl_passphrase_decrypt_matches_cryptojs() {
    let v = vectors();
    let plain = crypto::decrypt_text_with_key(s(&v, &["openssl", "cipher_hex"]), s(&v, &["openssl", "secret"])).unwrap();
    assert_eq!(plain, s(&v, &["openssl", "plain"]));
}

#[test]
fn password_hash_matches() {
    let v = vectors();
    let h = crypto::pass_to_hash(s(&v, &["password", "password"]), s(&v, &["password", "salt_hex"])).unwrap();
    assert_eq!(h, s(&v, &["password", "hash_hex"]));
}

#[test]
fn login_proof_roundtrips_through_server_salt() {
    let v = vectors();
    let encrypted_salt = s(&v, &["openssl", "cipher_hex"]);
    let proof = crypto::encrypt_password_hash("correct horse", encrypted_salt).unwrap();
    let hash = crypto::decrypt_text_with_key(&proof, crypto::APP_CRYPTO_SECRET).unwrap();
    assert_eq!(hash, s(&v, &["password", "hash_hex"]));
}

#[test]
fn share_link_encryption_matches_web_app() {
    let v = vectors();
    let k = &v["keys"];
    let bucket = crypto::BucketKey::derive(s(k, &["mnemonic"]), s(k, &["bucket_id"])).unwrap();
    assert_eq!(bucket.share_key_hex(), s(&v, &["share", "bucket_key_hex"]));

    let salt: [u8; 64] = hex::decode(s(&v, &["share", "salt_hex"])).unwrap().try_into().unwrap();
    let iv: [u8; 16] = hex::decode(s(&v, &["share", "iv_hex"])).unwrap().try_into().unwrap();
    let ours = inxt_api::share::lib_encrypt_with(&bucket.share_key_hex(), s(&v, &["share", "code"]), &salt, &iv);
    assert_eq!(ours, s(&v, &["share", "encrypted_key"]));
    let back = inxt_api::share::lib_decrypt(s(&v, &["share", "encrypted_key"]), s(&v, &["share", "code"])).unwrap();
    assert_eq!(back, bucket.share_key_hex());
}

#[test]
fn seed_and_file_encryption_match() {
    let v = vectors();
    let k = &v["keys"];
    let seed = crypto::mnemonic_to_seed(s(k, &["mnemonic"])).unwrap();
    assert_eq!(hex::encode(seed), s(k, &["seed_hex"]));

    let bucket = crypto::BucketKey::derive(s(k, &["mnemonic"]), s(k, &["bucket_id"])).unwrap();
    let fk = bucket.file_key(s(k, &["index_hex"])).unwrap();

    let len = k["plain_len"].as_u64().unwrap() as usize;
    let mut data: Vec<u8> = (0..len).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
    fk.cipher_at(0).apply_keystream(&mut data);

    assert_eq!(hex::encode(Sha256::digest(&data)), s(k, &["cipher_sha256_hex"]));
    assert_eq!(hex::encode(&data[40000..40100]), s(k, &["cipher_window_hex"]));

    let mut h = Sha256::new();
    h.update(&data);
    assert_eq!(crypto::content_hash(h), s(k, &["content_hash_hex"]));

    let mut window: Vec<u8> = (40000..40100).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
    fk.cipher_at(40000).apply_keystream(&mut window);
    assert_eq!(hex::encode(&window), s(k, &["cipher_window_hex"]));
}
