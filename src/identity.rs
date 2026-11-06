//! Per-node identity: Ed25519 signing keys and X25519 agreement keys.
//!
//! The shared passphrase gives confidentiality against the network (and keeps
//! relays blind), but on its own it proves nothing about *who* wrote a message:
//! any key holder could put someone else's id in `from`. Each node therefore
//! mints a signing key on first run, and every envelope carries a signature
//! that the recipient checks against the sender's pinned public key.
//!
//! An X25519 key is minted alongside it so that, once two peers have exchanged
//! public bundles, payloads can be encrypted to that recipient alone instead of
//! to the whole group. Until then the group key is used (see `Alg` in
//! [`crate::protocol`]).
//!
//! Both secrets are 32 random bytes stored in the identity table; nothing is
//! derived from the passphrase, so rotating the passphrase does not change a
//! node's identity.

use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// Length of an Ed25519 public key / X25519 public key.
pub const PUB_LEN: usize = 32;
/// Length of an Ed25519 signature.
pub const SIG_LEN: usize = 64;
/// Length of a public bundle: `ed25519_pub || x25519_pub`.
pub const BUNDLE_LEN: usize = PUB_LEN * 2;

/// The X25519 base point (u = 9, little endian).
const BASE_POINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

pub type PublicKey = [u8; PUB_LEN];
pub type Signature64 = [u8; SIG_LEN];

/// Local identity material.
#[derive(Clone)]
pub struct Identity {
    ed_secret: [u8; 32],
    x_secret: [u8; 32],
    pub ed_public: PublicKey,
    pub x_public: PublicKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secrets, even by accident.
        f.debug_struct("Identity")
            .field("ed_public", &hex(&self.ed_public))
            .field("x_public", &hex(&self.x_public))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Mint a fresh identity from the OS RNG.
    pub fn generate() -> Identity {
        Identity::from_secrets(crate::random_array32(), crate::random_array32())
    }

    /// Rebuild an identity from stored secrets (validating the Ed25519 seed).
    pub fn from_secrets(ed_secret: [u8; 32], x_secret: [u8; 32]) -> Identity {
        let signing = SigningKey::from_bytes(&ed_secret);
        let ed_public = signing.verifying_key().to_bytes();
        let x_public = MontgomeryPoint(BASE_POINT).mul_clamped(x_secret).to_bytes();
        Identity {
            ed_secret,
            x_secret,
            ed_public,
            x_public,
        }
    }

    /// Load or create the identity stored under `identity` in `db`.
    pub fn load_or_create(db: &crate::db::Db) -> Result<Identity, String> {
        if let Some((ed, x)) = db.identity_secrets()? {
            return Ok(Identity::from_secrets(ed, x));
        }
        let identity = Identity::generate();
        db.store_identity(&identity.ed_secret, &identity.x_secret)?;
        Ok(identity)
    }

    /// Secrets as stored on disk.
    pub fn secrets(&self) -> ([u8; 32], [u8; 32]) {
        (self.ed_secret, self.x_secret)
    }

    /// `ed25519_pub || x25519_pub`, the 64 bytes a peer needs to pin us.
    pub fn bundle(&self) -> [u8; BUNDLE_LEN] {
        let mut out = [0u8; BUNDLE_LEN];
        out[..PUB_LEN].copy_from_slice(&self.ed_public);
        out[PUB_LEN..].copy_from_slice(&self.x_public);
        out
    }

    pub fn sign(&self, message: &[u8]) -> Signature64 {
        SigningKey::from_bytes(&self.ed_secret)
            .sign(message)
            .to_bytes()
    }

    /// X25519 shared secret with a peer's public key.
    ///
    /// Rejects the all-zero output, which is what a low-order (malicious) public
    /// key produces.
    pub fn agree(&self, peer_x_public: &PublicKey) -> Result<[u8; 32], String> {
        let shared = MontgomeryPoint(*peer_x_public)
            .mul_clamped(self.x_secret)
            .to_bytes();
        if shared.iter().all(|b| *b == 0) {
            return Err("X25519 produced an all-zero secret (low-order public key)".into());
        }
        Ok(shared)
    }
}

/// Verify an Ed25519 signature.
pub fn verify(ed_public: &PublicKey, message: &[u8], sig: &Signature64) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(ed_public) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(sig)).is_ok()
}

/// Split a `ed||x` bundle.
pub fn split_bundle(bundle: &[u8]) -> Option<(PublicKey, PublicKey)> {
    if bundle.len() != BUNDLE_LEN {
        return None;
    }
    let mut ed = [0u8; PUB_LEN];
    let mut x = [0u8; PUB_LEN];
    ed.copy_from_slice(&bundle[..PUB_LEN]);
    x.copy_from_slice(&bundle[PUB_LEN..]);
    Some((ed, x))
}

/// Derive a 32-byte key from a shared secret with a context string.
pub fn kdf(shared: &[u8; 32], context: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(b"dns_lab/v5-payload"), shared);
    let mut out = [0u8; 32];
    hk.expand(context, &mut out)
        .expect("32-byte HKDF output is always valid");
    out
}

/// Short fingerprint of a public key, for logs and the UI.
pub fn fingerprint(public: &PublicKey) -> String {
    let digest = Sha256::digest(public);
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn ed25519_matches_rfc8032_vector_1() {
        let seed: [u8; 32] =
            from_hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .try_into()
                .unwrap();
        let id = Identity::from_secrets(seed, [7u8; 32]);
        assert_eq!(
            hex(&id.ed_public),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );

        let sig = id.sign(b"");
        assert_eq!(
            hex(&sig),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555f\
             b8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
                .replace(['\n', ' '], "")
        );
        assert!(verify(&id.ed_public, b"", &sig));
        assert!(!verify(&id.ed_public, b"x", &sig));
    }

    #[test]
    fn x25519_matches_rfc7748_vector() {
        let alice_secret: [u8; 32] =
            from_hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a")
                .try_into()
                .unwrap();
        let bob_secret: [u8; 32] =
            from_hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb")
                .try_into()
                .unwrap();
        let alice = Identity::from_secrets([1u8; 32], alice_secret);
        let bob = Identity::from_secrets([2u8; 32], bob_secret);

        assert_eq!(
            hex(&alice.x_public),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            hex(&bob.x_public),
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
        );
        let ab = alice.agree(&bob.x_public).unwrap();
        let ba = bob.agree(&alice.x_public).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(
            hex(&ab),
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
        );
    }

    #[test]
    fn low_order_public_key_is_rejected() {
        let id = Identity::generate();
        // The identity element: a low-order point whose DH output is all zero.
        let mut low = [0u8; 32];
        low[0] = 1;
        assert!(id.agree(&low).is_err());
    }

    #[test]
    fn bundle_roundtrip_and_fingerprint() {
        let id = Identity::generate();
        let (ed, x) = split_bundle(&id.bundle()).unwrap();
        assert_eq!(ed, id.ed_public);
        assert_eq!(x, id.x_public);
        assert_eq!(fingerprint(&id.ed_public).len(), 8);
        assert!(split_bundle(&[0u8; 10]).is_none());
    }

    #[test]
    fn two_identities_agree_on_the_same_key() {
        let a = Identity::generate();
        let b = Identity::generate();
        assert_eq!(a.agree(&b.x_public).unwrap(), b.agree(&a.x_public).unwrap());
        assert_eq!(a.sign(b"hello").len(), SIG_LEN);
        assert!(verify(&a.ed_public, b"hello", &a.sign(b"hello")));
    }
}
