//! Encryption at rest: XChaCha20-Poly1305 with a key ring, plus the HMAC / HKDF helpers it needs.
//!
//! What it protects: the commit log and checkpoints (file backend) and the `commits` /
//! `checkpoint_chunks` payloads (PostgreSQL backend). Someone who copies the disk, a backup or the
//! PostgreSQL data directory gets ciphertext. Each record is sealed with a fresh random 192-bit nonce
//! and an *associated data* string that names where the record belongs (store, kind, commit timestamp,
//! chunk number), so a record cannot be moved to another position, store or tenant without detection.
//!
//! What it does not protect: data in process memory, a running process's key, the relational mirror
//! (so encryption refuses to start with the mirror on), and it cannot detect rollback to an older
//! complete copy or truncation of the log's tail (a torn write looks the same). Key custody is the
//! caller's job; use a KMS or an HSM-backed file and rotate with [`Keyring::with_previous`].
//!
//! Record layout: `0xE1 | key id (u32 LE) | nonce (24) | ciphertext || tag (16)`.

use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

pub const KEY_LEN: usize = 32;
const MAGIC: u8 = 0xE1;
const NONCE_LEN: usize = 24;
const HEADER: usize = 1 + 4 + NONCE_LEN;
const TAG: usize = 16;

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut h = Sha256::new();
    h.update(ipad);
    h.update(msg);
    let inner = h.finalize();
    let mut h = Sha256::new();
    h.update(opad);
    h.update(inner);
    h.finalize().into()
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// HKDF-SHA256 (RFC 5869), one output block (32 bytes).
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hmac_sha256(salt, ikm);
    let mut t = info.to_vec();
    t.push(1);
    hmac_sha256(&prk, &t)
}

fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // volatile so the compiler cannot drop the writes
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct Key {
    id: u32,
    bytes: [u8; KEY_LEN],
}

impl Drop for Key {
    fn drop(&mut self) {
        wipe(&mut self.bytes);
    }
}

/// A secret byte string whose `Debug` never shows it and which is wiped when dropped.
#[derive(Clone)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes, hidden)", self.0.len())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// An active key (used to seal) and any number of previous keys (still accepted to open).
pub struct Keyring {
    active: Key,
    previous: Vec<Key>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyring(active id {}, {} previous; key material hidden)", self.active.id, self.previous.len())
    }
}

impl Keyring {
    /// A ring whose active key is `key` with identifier `id`.
    pub fn new(id: u32, key: [u8; KEY_LEN]) -> Keyring {
        Keyring { active: Key { id, bytes: key }, previous: vec![] }
    }

    /// Derive a per-store key from a master secret: `HKDF(salt="nexgraph-at-rest-v1", ikm=master,
    /// info=context || id)`. Use the store (schema / directory name) as `context` so two tenants never
    /// share a key. `master` must be at least 32 bytes of high-entropy secret (not a password).
    pub fn from_master(master: &[u8], id: u32, context: &str) -> Result<Keyring> {
        if master.len() < 32 {
            return Err(Error::Invalid("encryption master key must be at least 32 bytes".into()));
        }
        let mut info = context.as_bytes().to_vec();
        info.extend_from_slice(&id.to_le_bytes());
        Ok(Keyring::new(id, hkdf_sha256(b"nexgraph-at-rest-v1", master, &info)))
    }

    /// Per-store ring from a master secret and any previous master secrets (for rotation).
    pub fn from_masters(active: (u32, &[u8]), previous: &[(u32, &[u8])], context: &str) -> Result<Keyring> {
        let mut ring = Keyring::from_master(active.1, active.0, context)?;
        for (id, m) in previous {
            let k = Keyring::from_master(m, *id, context)?;
            ring.previous.push(Key { id: *id, bytes: k.active.bytes });
        }
        Ok(ring)
    }

    /// Keep an older key so records sealed under it still open (key rotation: make a new ring with a
    /// new active key, add the old ones here, then `checkpoint()` to re-seal everything under the new key).
    pub fn with_previous(mut self, id: u32, key: [u8; KEY_LEN]) -> Keyring {
        self.previous.push(Key { id, bytes: key });
        self
    }

    pub fn active_id(&self) -> u32 {
        self.active.id
    }

    fn find(&self, id: u32) -> Option<&Key> {
        if self.active.id == id {
            return Some(&self.active);
        }
        self.previous.iter().find(|k| k.id == id)
    }

    /// Encrypt `plain`, binding it to `aad`.
    pub fn seal(&self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| Error::Backend(format!("no secure randomness available: {e}")))?;
        let c = XChaCha20Poly1305::new((&self.active.bytes).into());
        let ct = c.encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad }).map_err(|_| Error::Backend("encryption failed".into()))?;
        let mut out = Vec::with_capacity(HEADER + ct.len());
        out.push(MAGIC);
        out.extend_from_slice(&self.active.id.to_le_bytes());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt and authenticate a record sealed for `aad`. One uniform error for every failure
    /// (wrong key, wrong position, altered bytes) so it is not an oracle.
    pub fn open(&self, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
        let bad = || Error::Corrupt("encrypted record failed authentication (wrong key, altered, moved, or not encrypted)".into());
        if blob.len() < HEADER + TAG || blob[0] != MAGIC {
            return Err(bad());
        }
        let id = u32::from_le_bytes(blob[1..5].try_into().unwrap());
        let key = self.find(id).ok_or_else(|| Error::Corrupt(format!("record was sealed with key id {id}, which is not in the key ring")))?;
        let c = XChaCha20Poly1305::new((&key.bytes).into());
        c.decrypt(XNonce::from_slice(&blob[5..5 + NONCE_LEN]), Payload { msg: &blob[HEADER..], aad }).map_err(|_| bad())
    }
}

/// Parse a 64-hex-digit key.
pub fn key_from_hex(h: &str) -> Result<[u8; KEY_LEN]> {
    let h = h.trim();
    if h.len() != KEY_LEN * 2 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("key must be 64 hexadecimal digits (32 bytes)".into()));
    }
    let mut k = [0u8; KEY_LEN];
    for i in 0..KEY_LEN {
        k[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn hmac_matches_rfc4231_vectors() {
        // test case 1
        let k = [0x0bu8; 20];
        assert_eq!(hmac_sha256(&k, b"Hi There").to_vec(), unhex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"));
        // test case 2
        assert_eq!(hmac_sha256(b"Jefe", b"what do ya want for nothing?").to_vec(), unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"));
        // test case 6: key longer than the block size
        let k = [0xaau8; 131];
        assert_eq!(
            hmac_sha256(&k, b"Test Using Larger Than Block-Size Key - Hash Key First").to_vec(),
            unhex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn hkdf_matches_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        // first 32 bytes of the OKM (T(1)) from RFC 5869 appendix A.1
        assert_eq!(hkdf_sha256(&salt, &ikm, &info).to_vec(), unhex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"));
    }

    #[test]
    fn seal_open_roundtrip_and_every_tamper_fails() {
        let r = Keyring::new(7, [9u8; 32]);
        let blob = r.seal(b"pos-1", b"patient 002-10665").unwrap();
        assert_eq!(r.open(b"pos-1", &blob).unwrap(), b"patient 002-10665");
        assert!(!blob.windows(7).any(|w| w == b"patient"), "plaintext must not appear");
        // wrong position
        assert!(r.open(b"pos-2", &blob).is_err());
        // every single-byte flip is caught
        for i in 0..blob.len() {
            let mut b = blob.clone();
            b[i] ^= 1;
            assert!(r.open(b"pos-1", &b).is_err(), "flip at byte {i} went unnoticed");
        }
        // truncation
        assert!(r.open(b"pos-1", &blob[..blob.len() - 1]).is_err());
        // wrong key
        assert!(Keyring::new(7, [8u8; 32]).open(b"pos-1", &blob).is_err());
        // nonces are fresh
        assert_ne!(r.seal(b"a", b"x").unwrap(), r.seal(b"a", b"x").unwrap());
    }

    #[test]
    fn rotation_and_derivation() {
        let old = Keyring::new(1, [1u8; 32]);
        let blob = old.seal(b"a", b"secret").unwrap();
        let new = Keyring::new(2, [2u8; 32]).with_previous(1, [1u8; 32]);
        assert_eq!(new.open(b"a", &blob).unwrap(), b"secret");
        let blob2 = new.seal(b"a", b"secret").unwrap();
        assert!(old.open(b"a", &blob2).is_err(), "the old ring has no key 2");
        let m = [5u8; 32];
        let a = Keyring::from_master(&m, 1, "tenant_a").unwrap();
        let b = Keyring::from_master(&m, 1, "tenant_b").unwrap();
        let blob = a.seal(b"x", b"v").unwrap();
        assert!(b.open(b"x", &blob).is_err(), "tenants must not share a key");
        assert!(Keyring::from_master(&[0u8; 16], 1, "t").is_err());
        assert!(format!("{a:?}").contains("hidden"));
        assert!(key_from_hex("zz").is_err() && key_from_hex(&"ab".repeat(32)).is_ok());
    }
}
