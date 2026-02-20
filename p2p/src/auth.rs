use std::collections::HashMap;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::crypto::{self, PeerIdentity, PublicKeyBytes, Signature};

const CHALLENGE_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("peer not found: {0}")]
    PeerNotFound(String),
    #[error("no pending challenge for peer: {0}")]
    NoPendingChallenge(String),
    #[error("challenge-response verification failed")]
    VerificationFailed,
    #[error("peer already authenticated: {0}")]
    AlreadyAuthenticated(String),
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),
    #[error("base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),
}

/// Step 1: Initiator sends its public key to the responder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthHello {
    pub agent_id: String,
    pub public_key: PublicKeyBytes,
}

/// Step 2: Responder replies with its public key and a random challenge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthChallenge {
    pub agent_id: String,
    pub public_key: PublicKeyBytes,
    /// Base64-encoded 32-byte random nonce.
    pub nonce: String,
}

/// Step 3: Initiator signs the challenge nonce and sends back the signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthResponse {
    pub agent_id: String,
    pub signature: Signature,
}

/// Step 4: Responder confirms authentication succeeded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthConfirm {
    pub agent_id: String,
    /// Responder signs the nonce too, so initiator can verify the responder.
    pub signature: Signature,
}

/// Outcome of a successful mutual authentication.
#[derive(Clone, Debug)]
pub struct AuthenticatedPeer {
    pub agent_id: String,
    pub public_key: PublicKeyBytes,
}

/// Manages the challenge-response authentication handshake for mesh peers.
///
/// Protocol flow (mutual authentication):
/// 1. Initiator → Responder: `AuthHello` (public key)
/// 2. Responder → Initiator: `AuthChallenge` (public key + nonce)
/// 3. Initiator → Responder: `AuthResponse` (signature over nonce)
/// 4. Responder → Initiator: `AuthConfirm` (signature over nonce)
///
/// Both sides verify the other's signature, proving possession of the
/// corresponding private key without ever transmitting it.
#[derive(Clone)]
pub struct PeerAuthenticator {
    identity: Arc<PeerIdentity>,
    agent_id: String,
    /// Pending challenges we issued (keyed by remote agent_id → nonce bytes).
    pending: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    /// Successfully authenticated peers.
    authenticated: Arc<RwLock<HashMap<String, AuthenticatedPeer>>>,
}

impl PeerAuthenticator {
    pub fn new(agent_id: &str, identity: PeerIdentity) -> Self {
        Self {
            identity: Arc::new(identity),
            agent_id: agent_id.to_string(),
            pending: Arc::new(RwLock::new(HashMap::new())),
            authenticated: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create an `AuthHello` to initiate the handshake.
    pub fn make_hello(&self) -> AuthHello {
        AuthHello {
            agent_id: self.agent_id.clone(),
            public_key: self.identity.public_key(),
        }
    }

    /// Handle an incoming `AuthHello`: generate and return a challenge.
    pub async fn handle_hello(&self, hello: AuthHello) -> Result<AuthChallenge, AuthError> {
        let nonce = crypto::random_bytes(CHALLENGE_LEN)?;
        self.pending
            .write()
            .await
            .insert(hello.agent_id.clone(), nonce.clone());

        Ok(AuthChallenge {
            agent_id: self.agent_id.clone(),
            public_key: self.identity.public_key(),
            nonce: BASE64.encode(&nonce),
        })
    }

    /// Sign a received challenge nonce (called by the initiator).
    pub fn sign_challenge(&self, challenge: &AuthChallenge) -> AuthResponse {
        let nonce_bytes = BASE64
            .decode(&challenge.nonce)
            .expect("challenge nonce should be valid base64");
        let signature = self.identity.sign(&nonce_bytes);
        AuthResponse {
            agent_id: self.agent_id.clone(),
            signature,
        }
    }

    /// Verify an `AuthResponse` against the pending challenge for that peer.
    /// On success, marks the peer as authenticated and returns an `AuthConfirm`.
    pub async fn handle_response(
        &self,
        response: AuthResponse,
        remote_public_key: &PublicKeyBytes,
    ) -> Result<AuthConfirm, AuthError> {
        let nonce = self
            .pending
            .write()
            .await
            .remove(&response.agent_id)
            .ok_or_else(|| AuthError::NoPendingChallenge(response.agent_id.clone()))?;

        // Verify the remote peer signed our nonce correctly.
        crypto::verify(remote_public_key, &nonce, &response.signature)
            .map_err(|_| AuthError::VerificationFailed)?;

        // Peer is now authenticated.
        self.authenticated.write().await.insert(
            response.agent_id.clone(),
            AuthenticatedPeer {
                agent_id: response.agent_id,
                public_key: remote_public_key.clone(),
            },
        );

        // Sign the same nonce so the initiator can verify us back.
        let confirm_sig = self.identity.sign(&nonce);
        Ok(AuthConfirm {
            agent_id: self.agent_id.clone(),
            signature: confirm_sig,
        })
    }

    /// Verify an `AuthConfirm` from the responder (called by the initiator).
    pub async fn handle_confirm(
        &self,
        confirm: AuthConfirm,
        challenge_nonce: &str,
        remote_public_key: &PublicKeyBytes,
    ) -> Result<AuthenticatedPeer, AuthError> {
        let nonce_bytes = BASE64.decode(challenge_nonce)?;

        crypto::verify(remote_public_key, &nonce_bytes, &confirm.signature)
            .map_err(|_| AuthError::VerificationFailed)?;

        let peer = AuthenticatedPeer {
            agent_id: confirm.agent_id.clone(),
            public_key: remote_public_key.clone(),
        };
        self.authenticated
            .write()
            .await
            .insert(confirm.agent_id, peer.clone());

        Ok(peer)
    }

    /// Check whether a peer has been authenticated.
    pub async fn is_authenticated(&self, agent_id: &str) -> bool {
        self.authenticated.read().await.contains_key(agent_id)
    }

    /// Get an authenticated peer's record.
    pub async fn get_authenticated(&self, agent_id: &str) -> Option<AuthenticatedPeer> {
        self.authenticated.read().await.get(agent_id).cloned()
    }

    /// List all authenticated peers.
    pub async fn authenticated_peers(&self) -> Vec<AuthenticatedPeer> {
        self.authenticated.read().await.values().cloned().collect()
    }

    /// Remove a peer from the authenticated set (e.g. on disconnect).
    pub async fn remove_peer(&self, agent_id: &str) -> Option<AuthenticatedPeer> {
        self.authenticated.write().await.remove(agent_id)
    }

    /// Return our own public key.
    pub fn public_key(&self) -> PublicKeyBytes {
        self.identity.public_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_authenticator(name: &str) -> PeerAuthenticator {
        let identity = PeerIdentity::generate().unwrap();
        PeerAuthenticator::new(&format!("agent:{name}"), identity)
    }

    #[tokio::test]
    async fn full_mutual_auth_handshake() {
        let alice = make_authenticator("alice");
        let bob = make_authenticator("bob");

        // 1. Alice → Bob: Hello
        let hello = alice.make_hello();
        assert_eq!(hello.agent_id, "agent:alice");

        // 2. Bob → Alice: Challenge
        let challenge = bob.handle_hello(hello.clone()).await.unwrap();
        assert_eq!(challenge.agent_id, "agent:bob");

        // 3. Alice → Bob: Response (Alice signs Bob's nonce)
        let response = alice.sign_challenge(&challenge);
        assert_eq!(response.agent_id, "agent:alice");

        // 4. Bob verifies Alice's response → sends Confirm
        let confirm = bob
            .handle_response(response, &hello.public_key)
            .await
            .unwrap();
        assert_eq!(confirm.agent_id, "agent:bob");
        assert!(bob.is_authenticated("agent:alice").await);

        // 5. Alice verifies Bob's confirm
        let peer = alice
            .handle_confirm(confirm, &challenge.nonce, &challenge.public_key)
            .await
            .unwrap();
        assert_eq!(peer.agent_id, "agent:bob");
        assert!(alice.is_authenticated("agent:bob").await);
    }

    #[tokio::test]
    async fn wrong_signature_rejected() {
        let alice = make_authenticator("alice");
        let bob = make_authenticator("bob");
        let mallory = make_authenticator("mallory");

        let hello = alice.make_hello();
        let challenge = bob.handle_hello(hello.clone()).await.unwrap();

        // Mallory intercepts and tries to respond with her key
        let forged_response = mallory.sign_challenge(&challenge);
        // But provides alice's name
        let forged = AuthResponse {
            agent_id: "agent:alice".into(),
            signature: forged_response.signature,
        };

        // Bob should reject: signature doesn't match alice's public key
        let result = bob.handle_response(forged, &hello.public_key).await;
        assert!(result.is_err());
        assert!(!bob.is_authenticated("agent:alice").await);
    }

    #[tokio::test]
    async fn no_pending_challenge_rejected() {
        let bob = make_authenticator("bob");

        let fake_response = AuthResponse {
            agent_id: "agent:ghost".into(),
            signature: Signature {
                sig: BASE64.encode(b"fake"),
            },
        };
        let pk = PublicKeyBytes {
            key: BASE64.encode(b"fake-key-bytes-32-chars-exactly!"),
        };

        let result = bob.handle_response(fake_response, &pk).await;
        assert!(matches!(result, Err(AuthError::NoPendingChallenge(_))));
    }

    #[tokio::test]
    async fn remove_authenticated_peer() {
        let alice = make_authenticator("alice");
        let bob = make_authenticator("bob");

        // Complete handshake
        let hello = alice.make_hello();
        let challenge = bob.handle_hello(hello.clone()).await.unwrap();
        let response = alice.sign_challenge(&challenge);
        bob.handle_response(response, &hello.public_key)
            .await
            .unwrap();

        assert!(bob.is_authenticated("agent:alice").await);

        bob.remove_peer("agent:alice").await;
        assert!(!bob.is_authenticated("agent:alice").await);
    }

    #[tokio::test]
    async fn authenticated_peers_list() {
        let alice = make_authenticator("alice");
        let bob = make_authenticator("bob");
        let carol = make_authenticator("carol");

        // Auth alice with bob
        let hello = alice.make_hello();
        let challenge = bob.handle_hello(hello.clone()).await.unwrap();
        let response = alice.sign_challenge(&challenge);
        bob.handle_response(response, &hello.public_key)
            .await
            .unwrap();

        // Auth carol with bob
        let hello_c = carol.make_hello();
        let challenge_c = bob.handle_hello(hello_c.clone()).await.unwrap();
        let response_c = carol.sign_challenge(&challenge_c);
        bob.handle_response(response_c, &hello_c.public_key)
            .await
            .unwrap();

        let peers = bob.authenticated_peers().await;
        assert_eq!(peers.len(), 2);
    }
}
