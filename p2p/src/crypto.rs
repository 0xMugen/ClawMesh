use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("key generation failed")]
    KeyGeneration,
    #[error("signing failed")]
    SigningFailed,
    #[error("signature verification failed")]
    VerificationFailed,
    #[error("invalid key material")]
    InvalidKey,
    #[error("base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),
}

/// An Ed25519 identity key pair for a mesh peer.
///
/// Wraps `ring::signature::Ed25519KeyPair` and provides convenience methods
/// for signing messages and exporting the public key.
pub struct PeerIdentity {
    key_pair: Ed25519KeyPair,
    /// Retained PKCS#8 bytes for reconstructing additional instances.
    pkcs8_der: Vec<u8>,
}

/// Serialisable representation of a peer's public key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyBytes {
    /// Base64-encoded Ed25519 public key (32 bytes raw).
    pub key: String,
}

/// A cryptographic signature over an arbitrary message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signature {
    /// Base64-encoded Ed25519 signature (64 bytes raw).
    pub sig: String,
}

impl PeerIdentity {
    /// Generate a fresh Ed25519 key pair using the OS CSPRNG.
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let pkcs8_bytes =
            Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| CryptoError::KeyGeneration)?;
        let der = pkcs8_bytes.as_ref().to_vec();
        let key_pair =
            Ed25519KeyPair::from_pkcs8(&der).map_err(|_| CryptoError::KeyGeneration)?;
        Ok(Self {
            key_pair,
            pkcs8_der: der,
        })
    }

    /// Reconstruct from a PKCS#8 v2 document (base64-encoded).
    pub fn from_pkcs8_base64(encoded: &str) -> Result<Self, CryptoError> {
        let der = BASE64.decode(encoded)?;
        let key_pair = Ed25519KeyPair::from_pkcs8(&der).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self {
            key_pair,
            pkcs8_der: der,
        })
    }

    /// Create a second `PeerIdentity` from the same key material.
    ///
    /// This is used when multiple subsystems (e.g. `MeshNode` and
    /// `PeerAuthenticator`) each need their own owned identity handle.
    pub fn clone_for_auth(&self) -> Self {
        let key_pair = Ed25519KeyPair::from_pkcs8(&self.pkcs8_der)
            .expect("pkcs8 bytes should remain valid");
        Self {
            key_pair,
            pkcs8_der: self.pkcs8_der.clone(),
        }
    }

    /// Return the public key in serialisable form.
    pub fn public_key(&self) -> PublicKeyBytes {
        PublicKeyBytes {
            key: BASE64.encode(self.key_pair.public_key().as_ref()),
        }
    }

    /// Sign an arbitrary byte message.
    pub fn sign(&self, message: &[u8]) -> Signature {
        let sig = self.key_pair.sign(message);
        Signature {
            sig: BASE64.encode(sig.as_ref()),
        }
    }
}

/// Verify an Ed25519 signature against a public key and message.
pub fn verify(
    public_key: &PublicKeyBytes,
    message: &[u8],
    signature: &Signature,
) -> Result<(), CryptoError> {
    let key_bytes = BASE64.decode(&public_key.key)?;
    let sig_bytes = BASE64.decode(&signature.sig)?;

    let peer_public_key =
        UnparsedPublicKey::new(&signature::ED25519, &key_bytes);

    peer_public_key
        .verify(message, &sig_bytes)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Generate `len` cryptographically-secure random bytes (e.g. for challenges).
pub fn random_bytes(len: usize) -> Result<Vec<u8>, CryptoError> {
    let rng = SystemRandom::new();
    let mut buf = vec![0u8; len];
    ring::rand::SecureRandom::fill(&rng, &mut buf).map_err(|_| CryptoError::KeyGeneration)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_sign_verify() {
        let id = PeerIdentity::generate().unwrap();
        let msg = b"hello mesh";

        let sig = id.sign(msg);
        let pk = id.public_key();

        assert!(verify(&pk, msg, &sig).is_ok());
    }

    #[test]
    fn verify_rejects_wrong_message() {
        let id = PeerIdentity::generate().unwrap();
        let sig = id.sign(b"correct message");
        let pk = id.public_key();

        assert!(verify(&pk, b"wrong message", &sig).is_err());
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let id_a = PeerIdentity::generate().unwrap();
        let id_b = PeerIdentity::generate().unwrap();
        let msg = b"hello";
        let sig = id_a.sign(msg);

        // Verify with B's public key should fail
        assert!(verify(&id_b.public_key(), msg, &sig).is_err());
    }

    #[test]
    fn public_key_serialization_roundtrip() {
        let id = PeerIdentity::generate().unwrap();
        let pk = id.public_key();

        let json = serde_json::to_string(&pk).unwrap();
        let deserialized: PublicKeyBytes = serde_json::from_str(&json).unwrap();
        assert_eq!(pk, deserialized);
    }

    #[test]
    fn random_bytes_returns_requested_length() {
        let bytes = random_bytes(32).unwrap();
        assert_eq!(bytes.len(), 32);

        // Two calls should produce different output
        let bytes2 = random_bytes(32).unwrap();
        assert_ne!(bytes, bytes2);
    }

    #[test]
    fn signature_serialization_roundtrip() {
        let id = PeerIdentity::generate().unwrap();
        let sig = id.sign(b"test");

        let json = serde_json::to_string(&sig).unwrap();
        let deserialized: Signature = serde_json::from_str(&json).unwrap();
        assert_eq!(sig.sig, deserialized.sig);
    }
}
