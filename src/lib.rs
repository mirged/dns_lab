//! Library entry point: key derivation, AEAD helpers and shared randomness.
//!
//! Crypto design (v3, still unaudited):
//!   * ChaCha20-Poly1305 with a fresh 96-bit random nonce per message.
//!   * Keys are derived from the configured passphrase with HKDF-SHA256, not by
//!     padding/truncating the passphrase bytes.
//!   * The tunnel message id travels as AEAD associated data, so a ciphertext is
//!     bound to the DNS QNAME that carries it and chunks cannot be spliced
//!     between messages without failing authentication.

pub mod config;
pub mod db;
pub mod file;
pub mod identity;
pub mod net;
pub mod protocol;
pub mod smaz;

#[cfg(feature = "gui")]
pub mod app;

#[cfg(target_os = "android")]
mod android;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// Default lab-grade shared secret, used as a passphrase.
pub const DEFAULT_SECRET: &str = "an-ultra-secure-secret-key-32a!!";

/// Raw default secret bytes. Kept for backwards-compatible config defaults; it
/// is a *passphrase*, not a key — see [`key_from_str`].
pub const DEFAULT_KEY: &[u8; 32] = b"an-ultra-secure-secret-key-32a!!";

/// HKDF salt/info pair, versioned so a future KDF change cannot silently reuse
/// the same key material.
const KDF_SALT: &[u8] = b"dns_lab/v3";
const KDF_INFO: &[u8] = b"chacha20poly1305-message-key";

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

/// Derive the 32-byte message key from an arbitrary configured passphrase.
///
/// HKDF-SHA256 with a fixed, versioned salt. An empty passphrase falls back to
/// the built-in lab secret so a half-configured node still interoperates with
/// the shipped examples.
pub fn key_from_str(s: &str) -> [u8; 32] {
    let ikm = if s.is_empty() {
        DEFAULT_SECRET.as_bytes()
    } else {
        s.as_bytes()
    };
    let hk = Hkdf::<Sha256>::new(Some(KDF_SALT), ikm);
    let mut key = [0u8; 32];
    // 32 bytes is far below HKDF-SHA256's 255*32 output limit.
    hk.expand(KDF_INFO, &mut key)
        .expect("32-byte HKDF output is always valid");
    key
}

/// 32 random bytes, for seeds and scalars.
pub fn random_array32() -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&random_bytes(32));
    out
}

/// A short, collision-resistant node/message id: 4 random bytes as 8 hex chars.
pub fn random_id() -> String {
    random_bytes(4)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Opaque routing tag for a recipient, derived from the message key.
///
/// The relay routes on this tag instead of a plaintext node id, so it only ever
/// handles `(tag, msg-id, blob)` triples: no keys, no envelopes, nothing to
/// decrypt. Both ends derive the same tag from the same shared passphrase; an
/// observer without the key cannot tell who a tag belongs to, and cannot forge
/// one for a recipient it has not seen traffic for.
///
/// 4 bytes (8 hex chars) keeps the QNAME budget identical to the old node-id
/// label; collisions are irrelevant because the relay only needs the tag to
/// partition mailboxes, and a collision would just merge two inboxes that the
/// recipient can still only decrypt its own messages out of.
pub fn routing_tag(key: &[u8; 32], recipient_id: &str) -> String {
    let hk = Hkdf::<Sha256>::new(Some(b"dns_lab/v4-route"), key);
    let mut out = [0u8; 4];
    hk.expand(recipient_id.as_bytes(), &mut out)
        .expect("4-byte HKDF output is always valid");
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// A human-friendly 32-byte passphrase candidate (base32, 52 chars).
pub fn random_secret() -> String {
    data_encoding::BASE32_NOPAD.encode(&random_bytes(32))
}

fn new_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&random_bytes(12));
    nonce
}

/// Encrypts raw bytes with authenticated additional data.
/// Output format: `[ 12-byte nonce | ciphertext + tag ]`.
pub fn encrypt_with_key_aad(key: &[u8; 32], data: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let raw_nonce = new_nonce();
    let nonce = Nonce::from(raw_nonce);

    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: data, aad })
        .map_err(|_| "encryption failed".to_string())?;

    let mut payload = raw_nonce.to_vec();
    payload.extend_from_slice(&ciphertext);
    Ok(payload)
}

/// Decrypts raw bytes produced by [`encrypt_with_key_aad`].
pub fn decrypt_with_key_aad(key: &[u8; 32], payload: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() < 12 {
        return Err("payload too short: missing nonce".to_string());
    }
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let (nonce_slice, ciphertext) = payload.split_at(12);
    let nonce = Nonce::try_from(nonce_slice).map_err(|_| "invalid nonce length".to_string())?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| "decryption failed: invalid key or tampered ciphertext".to_string())
}

/// Encrypts raw bytes using ChaCha20-Poly1305 with no associated data.
/// Output format: `[ 12-byte nonce | ciphertext + tag ]`.
pub fn encrypt_with_key(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, String> {
    encrypt_with_key_aad(key, data, &[])
}

/// Decrypts raw bytes. Expects `[ 12-byte nonce | ciphertext + tag ]`.
pub fn decrypt_with_key(key: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>, String> {
    decrypt_with_key_aad(key, payload, &[])
}

/// Encrypt with the built-in lab key.
pub fn encrypt_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    encrypt_with_key(&key_from_str(DEFAULT_SECRET), data)
}

/// Decrypt with the built-in lab key.
pub fn decrypt_bytes(payload: &[u8]) -> Result<Vec<u8>, String> {
    decrypt_with_key(&key_from_str(DEFAULT_SECRET), payload)
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

    #[test]
    fn aad_is_authenticated() {
        let key = key_from_str("aad test");
        let ct = encrypt_with_key_aad(&key, b"chunk", b"msg-a").unwrap();
        assert!(decrypt_with_key_aad(&key, &ct, b"msg-a").is_ok());
        assert!(
            decrypt_with_key_aad(&key, &ct, b"msg-b").is_err(),
            "ciphertext must not verify under a different message id"
        );
    }

    #[test]
    fn kdf_is_stable_and_distinct() {
        assert_eq!(key_from_str("secret"), key_from_str("secret"));
        assert_ne!(key_from_str("secret"), key_from_str("secret "));
        assert_ne!(key_from_str("secret"), key_from_str("secreu"));
        assert_eq!(key_from_str(&"x".repeat(200)).len(), 32);
        assert_ne!([0u8; 32], key_from_str("x"));
    }

    #[test]
    fn kdf_hides_short_passphrase_padding() {
        // The old zero-padding scheme made "abc" and "abc\0..." identical.
        assert_ne!(key_from_str("abc"), key_from_str("abc\0"));
    }

    #[test]
    fn random_secret_is_random_and_full_length() {
        assert_eq!(random_secret().len(), 52);
        assert_ne!(random_secret(), random_secret());
    }
}
