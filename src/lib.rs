pub mod config;
pub mod db;
pub mod net;
pub mod protocol;
pub mod smaz;

#[cfg(feature = "gui")]
pub mod app;

#[cfg(target_os = "android")]
mod android;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// Default lab-grade shared secret (exactly 32 bytes).
pub const DEFAULT_KEY: &[u8; 32] = b"an-ultra-secure-secret-key-32a!!";

/// Fill `n` bytes with cryptographically strong randomness.
///
/// Uses `/dev/urandom` on unix and falls back to a time/pid seeded xorshift on
/// other platforms (so the crate still builds anywhere).
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom")
            && f.read_exact(&mut buf).is_ok()
        {
            return buf;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ ((std::process::id() as u64).rotate_left(17));
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    buf
}

/// Derive a 32-byte key from an arbitrary configured string (padded/truncated).
pub fn key_from_str(s: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let bytes = s.as_bytes();
    let n = bytes.len().min(32);
    key[..n].copy_from_slice(&bytes[..n]);
    if bytes.is_empty() {
        key.copy_from_slice(DEFAULT_KEY);
    }
    key
}

/// A short, collision-resistant node/message id: 4 random bytes as 8 hex chars.
pub fn random_id() -> String {
    random_bytes(4)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn new_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&random_bytes(12));
    nonce
}

/// Encrypts raw bytes using ChaCha20-Poly1305.
/// Output format: `[ 12-byte nonce | ciphertext + tag ]`.
pub fn encrypt_with_key(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let raw_nonce = new_nonce();
    let nonce = Nonce::from(raw_nonce);

    let ciphertext = cipher
        .encrypt(&nonce, data)
        .map_err(|_| "encryption failed".to_string())?;

    let mut payload = raw_nonce.to_vec();
    payload.extend_from_slice(&ciphertext);
    Ok(payload)
}

/// Decrypts raw bytes. Expects `[ 12-byte nonce | ciphertext + tag ]`.
pub fn decrypt_with_key(key: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() < 12 {
        return Err("payload too short: missing nonce".to_string());
    }
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let (nonce_slice, ciphertext) = payload.split_at(12);
    let nonce = Nonce::try_from(nonce_slice).map_err(|_| "invalid nonce length".to_string())?;
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| "decryption failed: invalid key or tampered ciphertext".to_string())
}

/// Encrypt with the built-in lab key.
pub fn encrypt_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    encrypt_with_key(DEFAULT_KEY, data)
}

/// Decrypt with the built-in lab key.
pub fn decrypt_bytes(payload: &[u8]) -> Result<Vec<u8>, String> {
    decrypt_with_key(DEFAULT_KEY, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crypto_roundtrip() {
        let original = b"hello how are you";
        let encrypted = encrypt_bytes(original).expect("encryption failed");
        let decrypted = decrypt_bytes(&encrypted).expect("decryption failed");
        assert_eq!(original, decrypted.as_slice());
    }

    #[test]
    fn test_keyed_roundtrip() {
        let key = key_from_str("a different secret");
        let original = b"payload under a derived key";
        let encrypted = encrypt_with_key(&key, original).unwrap();
        let decrypted = decrypt_with_key(&key, &encrypted).unwrap();
        assert_eq!(original.as_slice(), decrypted.as_slice());
    }
}
