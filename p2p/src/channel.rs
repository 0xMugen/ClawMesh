use ring::aead::{
    Aad, BoundKey, Nonce, NonceSequence, OpeningKey, SealingKey, UnboundKey, CHACHA20_POLY1305,
};
use ring::agreement::{self, EphemeralPrivateKey, PublicKey, UnparsedPublicKey, X25519};
use ring::rand::SystemRandom;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("key agreement failed")]
    KeyAgreement,
    #[error("encryption failed")]
    SealFailed,
    #[error("decryption failed")]
    OpenFailed,
    #[error("nonce overflow")]
    NonceOverflow,
    #[error("invalid key material")]
    InvalidKey,
}

/// Counter-based nonce sequence for AEAD operations.
///
/// Each direction of an encrypted channel gets its own `CounterNonce`.
/// The 12-byte nonce is built from a 4-byte fixed prefix (to distinguish
/// directions) plus an 8-byte little-endian counter.
struct CounterNonce {
    prefix: [u8; 4],
    counter: u64,
}

impl CounterNonce {
    fn new(prefix: [u8; 4]) -> Self {
        Self { prefix, counter: 0 }
    }
}

impl NonceSequence for CounterNonce {
    fn advance(&mut self) -> Result<Nonce, ring::error::Unspecified> {
        let c = self.counter;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(ring::error::Unspecified)?;
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[..4].copy_from_slice(&self.prefix);
        nonce_bytes[4..].copy_from_slice(&c.to_le_bytes());
        Nonce::try_assume_unique_for_key(&nonce_bytes)
    }
}

/// A sealed (encrypted) message with its nonce for transmission.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedMessage {
    /// Nonce used for this message (base64-encoded, 12 bytes).
    pub nonce: Vec<u8>,
    /// Ciphertext including the authentication tag (base64-encoded).
    pub ciphertext: Vec<u8>,
}

/// Result of the X25519 key agreement: the shared secret and our ephemeral public key.
pub struct KeyAgreementResult {
    /// Shared secret derived from ECDH (32 bytes), used as AEAD key material.
    pub shared_secret: Vec<u8>,
    /// Our ephemeral public key to send to the remote peer.
    pub our_public_key: Vec<u8>,
}

/// Perform the initiator side of X25519 key agreement.
///
/// Returns the ephemeral public key (to send to the responder) and the
/// private key (to complete the agreement once we receive the responder's
/// public key).
pub fn generate_ephemeral_key() -> Result<(EphemeralPrivateKey, PublicKey), ChannelError> {
    let rng = SystemRandom::new();
    let private_key =
        EphemeralPrivateKey::generate(&X25519, &rng).map_err(|_| ChannelError::KeyAgreement)?;
    let public_key = private_key
        .compute_public_key()
        .map_err(|_| ChannelError::KeyAgreement)?;
    Ok((private_key, public_key))
}

/// Complete the X25519 key agreement given our ephemeral private key and the
/// remote peer's public key bytes.
///
/// Returns the 32-byte shared secret suitable for use as a ChaCha20-Poly1305 key.
pub fn agree(
    our_private: EphemeralPrivateKey,
    remote_public_bytes: &[u8],
) -> Result<Vec<u8>, ChannelError> {
    let remote_public = UnparsedPublicKey::new(&X25519, remote_public_bytes);
    agreement::agree_ephemeral(our_private, &remote_public, |shared| shared.to_vec())
        .map_err(|_| ChannelError::KeyAgreement)
}

/// An encrypted bidirectional channel between two authenticated peers.
///
/// Uses ChaCha20-Poly1305 AEAD with a shared secret derived from X25519 ECDH.
/// Each direction has its own counter-based nonce to prevent nonce reuse.
pub struct EncryptedChannel {
    sealing_key: SealingKey<CounterNonce>,
    opening_key: OpeningKey<CounterNonce>,
}

impl EncryptedChannel {
    /// Create an encrypted channel from a shared secret.
    ///
    /// `is_initiator` determines the nonce prefix so that each direction
    /// uses a distinct nonce space (initiator seals with prefix `[0,0,0,1]`,
    /// responder seals with `[0,0,0,2]`, and vice versa for opening).
    pub fn new(shared_secret: &[u8], is_initiator: bool) -> Result<Self, ChannelError> {
        let (seal_prefix, open_prefix) = if is_initiator {
            ([0u8, 0, 0, 1], [0u8, 0, 0, 2])
        } else {
            ([0u8, 0, 0, 2], [0u8, 0, 0, 1])
        };

        let seal_key = UnboundKey::new(&CHACHA20_POLY1305, shared_secret)
            .map_err(|_| ChannelError::InvalidKey)?;
        let open_key = UnboundKey::new(&CHACHA20_POLY1305, shared_secret)
            .map_err(|_| ChannelError::InvalidKey)?;

        Ok(Self {
            sealing_key: SealingKey::new(seal_key, CounterNonce::new(seal_prefix)),
            opening_key: OpeningKey::new(open_key, CounterNonce::new(open_prefix)),
        })
    }

    /// Encrypt a plaintext message. Returns the ciphertext with appended auth tag.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut in_out = plaintext.to_vec();
        self.sealing_key
            .seal_in_place_append_tag(Aad::empty(), &mut in_out)
            .map_err(|_| ChannelError::SealFailed)?;
        Ok(in_out)
    }

    /// Decrypt a ciphertext (with appended auth tag). Returns the plaintext.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut in_out = ciphertext.to_vec();
        let plaintext = self
            .opening_key
            .open_in_place(Aad::empty(), &mut in_out)
            .map_err(|_| ChannelError::OpenFailed)?;
        Ok(plaintext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel_pair() -> (EncryptedChannel, EncryptedChannel) {
        // Simulate X25519 key agreement between two peers.
        let (priv_a, pub_a) = generate_ephemeral_key().unwrap();
        let (priv_b, pub_b) = generate_ephemeral_key().unwrap();

        let secret_a = agree(priv_a, pub_b.as_ref()).unwrap();
        let secret_b = agree(priv_b, pub_a.as_ref()).unwrap();

        // Both sides derive the same shared secret.
        assert_eq!(secret_a, secret_b);

        let chan_a = EncryptedChannel::new(&secret_a, true).unwrap();
        let chan_b = EncryptedChannel::new(&secret_b, false).unwrap();
        (chan_a, chan_b)
    }

    #[test]
    fn seal_and_open_roundtrip() {
        let (mut alice, mut bob) = make_channel_pair();

        let plaintext = b"hello from alice";
        let ciphertext = alice.seal(plaintext).unwrap();

        // Ciphertext should differ from plaintext
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);

        let decrypted = bob.open(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn bidirectional_communication() {
        let (mut alice, mut bob) = make_channel_pair();

        // Alice → Bob
        let ct1 = alice.seal(b"ping").unwrap();
        assert_eq!(bob.open(&ct1).unwrap(), b"ping");

        // Bob → Alice
        let ct2 = bob.seal(b"pong").unwrap();
        assert_eq!(alice.open(&ct2).unwrap(), b"pong");

        // Multiple messages
        let ct3 = alice.seal(b"message 2").unwrap();
        assert_eq!(bob.open(&ct3).unwrap(), b"message 2");
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let (mut alice, mut bob) = make_channel_pair();

        let mut ciphertext = alice.seal(b"secret data").unwrap();
        // Flip a byte
        ciphertext[0] ^= 0xff;

        assert!(bob.open(&ciphertext).is_err());
    }

    #[test]
    fn wrong_direction_rejected() {
        let (mut alice, mut _bob) = make_channel_pair();

        // Alice seals a message, then tries to open it herself
        // (wrong nonce prefix → decryption fails)
        let ciphertext = alice.seal(b"self-talk").unwrap();
        assert!(alice.open(&ciphertext).is_err());
    }

    #[test]
    fn key_agreement_roundtrip() {
        let (priv_a, pub_a) = generate_ephemeral_key().unwrap();
        let (priv_b, pub_b) = generate_ephemeral_key().unwrap();

        let secret_a = agree(priv_a, pub_b.as_ref()).unwrap();
        let secret_b = agree(priv_b, pub_a.as_ref()).unwrap();

        assert_eq!(secret_a.len(), 32);
        assert_eq!(secret_a, secret_b);
    }

    #[test]
    fn different_keys_different_secrets() {
        let (priv_a, _pub_a) = generate_ephemeral_key().unwrap();
        let (_, pub_c) = generate_ephemeral_key().unwrap();

        // A agrees with C instead of B → different secret
        let secret_ac = agree(priv_a, pub_c.as_ref()).unwrap();

        let (_priv_b, pub_b) = generate_ephemeral_key().unwrap();
        let (priv_d, _pub_d) = generate_ephemeral_key().unwrap();
        let secret_bd = agree(priv_d, pub_b.as_ref()).unwrap();

        // Random independent agreements should differ
        // (probabilistically guaranteed for 32-byte secrets)
        assert_ne!(secret_ac, secret_bd);
    }

    #[test]
    fn empty_plaintext() {
        let (mut alice, mut bob) = make_channel_pair();

        let ct = alice.seal(b"").unwrap();
        let pt = bob.open(&ct).unwrap();
        assert!(pt.is_empty());
    }

    #[test]
    fn large_message() {
        let (mut alice, mut bob) = make_channel_pair();

        let big = vec![0xABu8; 64 * 1024]; // 64 KiB
        let ct = alice.seal(&big).unwrap();
        let pt = bob.open(&ct).unwrap();
        assert_eq!(pt, big);
    }
}
