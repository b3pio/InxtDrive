// Generates crypto test vectors with Node's OpenSSL, mirroring the official
// JavaScript clients (CryptoJS passphrase AES, PBKDF2-SHA1 password hashing,
// BIP-39 seed, bucket/file key derivation, AES-256-CTR, RIPEMD160(SHA256)).
// Usage: node tools/gen-vectors.mjs > crates/inxt-api/tests/vectors.json
import crypto from 'node:crypto';

const APP_CRYPTO_SECRET = '6KYQBP847D4ATSFA';

function evpBytesToKey(pass, salt) {
  let prev = Buffer.alloc(0);
  const out = [];
  let len = 0;
  while (len < 48) {
    prev = crypto.createHash('md5').update(Buffer.concat([prev, Buffer.from(pass), salt])).digest();
    out.push(prev);
    len += prev.length;
  }
  const all = Buffer.concat(out);
  return { key: all.subarray(0, 32), iv: all.subarray(32, 48) };
}

function cryptoJsEncryptHex(text, pass, salt) {
  const { key, iv } = evpBytesToKey(pass, salt);
  const c = crypto.createCipheriv('aes-256-cbc', key, iv);
  const ct = Buffer.concat([c.update(text, 'utf8'), c.final()]);
  return Buffer.concat([Buffer.from('Salted__'), salt, ct]).toString('hex');
}

const mnemonic =
  'legal winner thank year wave sausage worth useful legal winner thank yellow';
const bucketId = '1f7a5a8c3e0d9b6f4a2c1e0d';
const index = crypto.createHash('sha256').update('fixed index').digest('hex') +
  crypto.createHash('sha256').update('more').digest('hex').slice(0, 0);

const seed = crypto.pbkdf2Sync(Buffer.from(mnemonic.normalize('NFKD')), Buffer.from('mnemonic'), 2048, 64, 'sha512');
const bucketKey = crypto.createHash('sha512').update(Buffer.concat([seed, Buffer.from(bucketId, 'hex')])).digest();
const indexBuf = Buffer.from(index, 'hex');
const fileKey = crypto.createHash('sha512').update(Buffer.concat([bucketKey.subarray(0, 32), indexBuf])).digest().subarray(0, 32);
const iv = indexBuf.subarray(0, 16);

const plain = Buffer.alloc(70000);
for (let i = 0; i < plain.length; i++) plain[i] = (i * 7 + 3) & 0xff;
const ctr = crypto.createCipheriv('aes-256-ctr', fileKey, iv);
const cipher = Buffer.concat([ctr.update(plain), ctr.final()]);
const contentHash = crypto.createHash('ripemd160').update(crypto.createHash('sha256').update(cipher).digest()).digest('hex');

const salt = Buffer.from('0102030405060708', 'hex');
const passwordSaltHex = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const pwHash = crypto.pbkdf2Sync('correct horse', Buffer.from(passwordSaltHex, 'hex'), 10000, 32, 'sha1').toString('hex');

// @internxt/lib aes.encrypt with fixed salt/iv, as used for share links.
function libAesEncrypt(text, password, salt, iv) {
  const key = crypto.pbkdf2Sync(password, salt, 2145, 32, 'sha512');
  const cipher = crypto.createCipheriv('aes-256-gcm', key, iv);
  const enc = Buffer.concat([cipher.update(text, 'utf8'), cipher.final()]);
  return Buffer.concat([salt, iv, cipher.getAuthTag(), enc]).toString('base64');
}
const shareSalt = Buffer.alloc(64, 7);
const shareIv = Buffer.from('000102030405060708090a0b0c0d0e0f', 'hex');
const shareKeyHex = bucketKey.subarray(0, 32).toString('hex');

console.log(JSON.stringify({
  share: {
    bucket_key_hex: shareKeyHex,
    code: 'Ab3_-x9Q',
    salt_hex: shareSalt.toString('hex'),
    iv_hex: shareIv.toString('hex'),
    encrypted_key: libAesEncrypt(shareKeyHex, 'Ab3_-x9Q', shareSalt, shareIv),
  },
  openssl: {
    secret: APP_CRYPTO_SECRET,
    plain: passwordSaltHex,
    salt_hex: salt.toString('hex'),
    cipher_hex: cryptoJsEncryptHex(passwordSaltHex, APP_CRYPTO_SECRET, salt),
  },
  password: { password: 'correct horse', salt_hex: passwordSaltHex, hash_hex: pwHash },
  keys: {
    mnemonic,
    bucket_id: bucketId,
    index_hex: index,
    seed_hex: seed.toString('hex'),
    file_key_hex: fileKey.toString('hex'),
    plain_len: plain.length,
    cipher_sha256_hex: crypto.createHash('sha256').update(cipher).digest('hex'),
    cipher_window_hex: cipher.subarray(40000, 40100).toString('hex'),
    content_hash_hex: contentHash,
  },
}, null, 2));
