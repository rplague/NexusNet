// NexusNet - OAHD 计划的核心网络层
//
// Copyright (C) 2026 OAHD
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Authentication record layer: TUF-lite over DHT + COSE_Sign1.
//!
//! The DHT is untrusted, overwritable and replayable storage; this module only
//! performs cryptographic and consistency checks on fetched bytes, never
//! touching the network.
//!
//! - **Trusted origin**: records are COSE_Sign1, verified with the authority
//!   ed25519 public key fixed in config.
//! - **Freeze**: index records carry `expires_at`; expired means rejected.
//! - **Rollback**: index and whitelists each carry `version`; lower versions are
//!   rejected (process lifetime only).
//! - **mix-and-match**: the index records each service whitelist's `hash` +
//!   `length`, checked after fetch.
//! - **Replay / cross-location**: COSE `external_aad` binds the record to its DHT
//!   key path.
//!
//! Record structure:
//!
//! ```text
//! key:   /oahd/auth/<net_hash>/service     index
//! key:   /oahd/auth/<net_hash>/<service>   whitelist
//! value: COSE_Sign1(payload = CBOR(doc), external_aad = key path bytes)
//! ```

use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use coset::{
    CborSerializable, CoseSign1, CoseSign1Builder, HeaderBuilder, RegisteredLabelWithPrivate, iana,
};
use libp2p::identity::{self, ed25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum size of a single record, in bytes.
const MAX_DOC_BYTES: usize = 1 << 20;
/// Maximum number of whitelist members.
const MAX_MEMBERS: usize = 100_000;
/// Maximum number of services in the index.
const MAX_SERVICES: usize = 10_000;
/// Reserved last segment of the index key; services may not use this name.
pub const RESERVED_SERVICE: &str = "service";
/// DHT key prefix.
const AUTH_PREFIX: &str = "/oahd/auth";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// COSE envelope encode/decode failed.
    Cose,
    /// Payload structure or fields are invalid.
    Malformed,
    /// The authority public key in config cannot be parsed.
    InvalidAuthority,
    /// Signature verification failed (including algorithm or aad mismatch).
    InvalidSignature,
    /// The index has expired (freeze protection).
    Expired,
    /// Version lower than already seen (rollback protection).
    Rollback,
    /// Whitelist hash/length does not match the index record (mix-and-match protection).
    HashMismatch,
    /// Service not in the index (non-fatal: the caller warns and allows).
    NotInIndex,
    /// Record or entry count exceeds the limit.
    TooLarge,
    /// Network or service name is invalid, or a reserved name was used.
    InvalidName,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Cose => write!(f, "cose envelope error"),
            AuthError::Malformed => write!(f, "malformed auth document"),
            AuthError::InvalidAuthority => write!(f, "invalid authority public key"),
            AuthError::InvalidSignature => write!(f, "signature verification failed"),
            AuthError::Expired => write!(f, "auth document expired"),
            AuthError::Rollback => write!(f, "auth document version rollback"),
            AuthError::HashMismatch => write!(f, "whitelist hash/length mismatch"),
            AuthError::NotInIndex => write!(f, "service not listed in auth index"),
            AuthError::TooLarge => write!(f, "auth document too large"),
            AuthError::InvalidName => write!(f, "invalid network or service name"),
        }
    }
}

impl std::error::Error for AuthError {}

/// An authentication network: the hash used in DHT keys, plus the fixed
/// authority public key.
#[derive(Clone)]
pub struct AuthNetwork {
    /// `base64url_nopad(SHA-256(name))`, used as a key path segment (no `/`).
    pub net_hash: String,
    pub authority: ed25519::PublicKey,
}

impl AuthNetwork {
    /// Build from a network name and a base64 authority public key.
    pub fn new(name: &str, authority_b64: &str) -> Result<Self, AuthError> {
        if !is_valid_network_name(name) {
            return Err(AuthError::InvalidName);
        }
        Ok(Self {
            net_hash: net_hash(name),
            authority: parse_authority(authority_b64)?,
        })
    }

    /// Index record key: `/oahd/auth/<net_hash>/service`.
    pub fn index_key(&self) -> String {
        format!("{}/{}/{}", AUTH_PREFIX, self.net_hash, RESERVED_SERVICE)
    }

    /// Whitelist record key: `/oahd/auth/<net_hash>/<service>`.
    pub fn service_key(&self, service: &str) -> Result<String, AuthError> {
        if !is_valid_service_name(service) {
            return Err(AuthError::InvalidName);
        }
        Ok(format!("{}/{}/{}", AUTH_PREFIX, self.net_hash, service))
    }
}

/// A service entry in the index, binding the byte digest of its whitelist record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEntry {
    pub name: String,
    /// SHA-256 of the whitelist record's full bytes.
    pub hash: Vec<u8>,
    /// Length of the whitelist record's full bytes.
    pub length: u64,
}

/// Index record payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDoc {
    pub version: u64,
    pub expires_at: u64,
    pub services: Vec<ServiceEntry>,
}

/// Whitelist record payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhitelistDoc {
    pub version: u64,
    pub members: Vec<String>,
}

fn cbor_encode<T: Serialize>(value: &T) -> Result<Vec<u8>, AuthError> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).map_err(|_| AuthError::Malformed)?;
    Ok(buf)
}

fn cbor_decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, AuthError> {
    ciborium::de::from_reader(bytes).map_err(|_| AuthError::Malformed)
}

fn eddsa_header() -> coset::Header {
    HeaderBuilder::new()
        .algorithm(iana::Algorithm::EdDSA)
        .build()
}

/// Sign `payload` with `keypair`; `aad` is the bound key path bytes.
fn sign_payload(
    keypair: &identity::Keypair,
    aad: &[u8],
    payload: Vec<u8>,
) -> Result<Vec<u8>, AuthError> {
    let sign1 = CoseSign1Builder::new()
        .protected(eddsa_header())
        .payload(payload)
        .try_create_signature(aad, |data| keypair.sign(data).map_err(|_| AuthError::Cose))?
        .build();
    sign1.to_vec().map_err(|_| AuthError::Cose)
}

/// Verify and return the payload; the raw payload bytes are within the signature
/// scope, so verify before parsing.
fn verify_payload(
    authority: &ed25519::PublicKey,
    aad: &[u8],
    value: &[u8],
) -> Result<Vec<u8>, AuthError> {
    if value.len() > MAX_DOC_BYTES {
        return Err(AuthError::TooLarge);
    }
    let sign1 = CoseSign1::from_slice(value).map_err(|_| AuthError::Cose)?;

    // Enforce the EdDSA algorithm to avoid algorithm confusion.
    let expected = RegisteredLabelWithPrivate::Assigned(iana::Algorithm::EdDSA);
    if sign1.protected.header.alg != Some(expected) {
        return Err(AuthError::InvalidSignature);
    }

    sign1
        .verify_signature(aad, |sig, data| {
            if authority.verify(data, sig) {
                Ok(())
            } else {
                Err(AuthError::InvalidSignature)
            }
        })
        .map_err(|_| AuthError::InvalidSignature)?;

    sign1.payload.ok_or(AuthError::Malformed)
}

/// Build and sign the index record; returns the full bytes to write to the DHT.
pub fn sign_index(
    keypair: &identity::Keypair,
    net: &AuthNetwork,
    doc: &IndexDoc,
) -> Result<Vec<u8>, AuthError> {
    let payload = cbor_encode(doc)?;
    sign_payload(keypair, net.index_key().as_bytes(), payload)
}

/// Build and sign a service whitelist record; returns the full bytes to write to
/// the DHT.
pub fn sign_whitelist(
    keypair: &identity::Keypair,
    net: &AuthNetwork,
    service: &str,
    doc: &WhitelistDoc,
) -> Result<Vec<u8>, AuthError> {
    let key = net.service_key(service)?;
    let payload = cbor_encode(doc)?;
    sign_payload(keypair, key.as_bytes(), payload)
}

/// Verify the index record: signature, structure, service names, and
/// `expires_at` (freeze protection).
pub fn verify_index(net: &AuthNetwork, value: &[u8], now: u64) -> Result<IndexDoc, AuthError> {
    let payload = verify_payload(&net.authority, net.index_key().as_bytes(), value)?;
    let doc: IndexDoc = cbor_decode(&payload)?;

    if doc.services.len() > MAX_SERVICES {
        return Err(AuthError::TooLarge);
    }
    for entry in &doc.services {
        if !is_valid_service_name(&entry.name) {
            return Err(AuthError::InvalidName);
        }
        if entry.hash.len() != 32 {
            return Err(AuthError::Malformed);
        }
    }
    if now > doc.expires_at {
        return Err(AuthError::Expired);
    }
    Ok(doc)
}

/// Verify the whitelist record: first the hash/length binding (mix-and-match),
/// then the signature.
pub fn verify_whitelist(
    net: &AuthNetwork,
    service: &str,
    value: &[u8],
    entry: &ServiceEntry,
) -> Result<WhitelistDoc, AuthError> {
    if value.len() as u64 != entry.length || content_hash(value) != entry.hash {
        return Err(AuthError::HashMismatch);
    }
    let key = net.service_key(service)?;
    let payload = verify_payload(&net.authority, key.as_bytes(), value)?;
    let doc: WhitelistDoc = cbor_decode(&payload)?;
    if doc.members.len() > MAX_MEMBERS {
        return Err(AuthError::TooLarge);
    }
    Ok(doc)
}

/// Rollback guard: highest verified version per record (process lifetime only,
/// never persisted).
#[derive(Debug, Default)]
pub struct AuthVersions {
    index: Option<u64>,
    whitelists: HashMap<String, u64>,
}

impl AuthVersions {
    /// Record an index version; returns [`AuthError::Rollback`] if it is lower
    /// than the highest seen.
    pub fn check_index(&mut self, version: u64) -> Result<(), AuthError> {
        if let Some(cur) = self.index
            && version < cur
        {
            return Err(AuthError::Rollback);
        }
        self.index = Some(version);
        Ok(())
    }

    /// Record a service whitelist version; returns [`AuthError::Rollback`] if it
    /// is lower than the highest seen.
    pub fn check_whitelist(&mut self, service: &str, version: u64) -> Result<(), AuthError> {
        if let Some(cur) = self.whitelists.get(service)
            && version < *cur
        {
            return Err(AuthError::Rollback);
        }
        self.whitelists.insert(service.to_string(), version);
        Ok(())
    }

    /// Highest seen index version, for status queries.
    pub fn index_version(&self) -> Option<u64> {
        self.index
    }

    /// Highest seen whitelist version for a service, for status queries.
    pub fn whitelist_version(&self, service: &str) -> Option<u64> {
        self.whitelists.get(service).copied()
    }

    pub fn clear(&mut self) {
        self.index = None;
        self.whitelists.clear();
    }
}

/// Network name: non-empty, <= 64, `[A-Za-z0-9_-]`.
pub fn is_valid_network_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Service name: same rule as network names, and it may not be the reserved name
/// `service`.
pub fn is_valid_service_name(s: &str) -> bool {
    is_valid_network_name(s) && s != RESERVED_SERVICE
}

/// Parse a base64-encoded ed25519 authority public key.
pub fn parse_authority(b64: &str) -> Result<ed25519::PublicKey, AuthError> {
    let bytes = STANDARD
        .decode(b64)
        .map_err(|_| AuthError::InvalidAuthority)?;
    ed25519::PublicKey::try_from_bytes(&bytes).map_err(|_| AuthError::InvalidAuthority)
}

/// Current Unix time in seconds.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Network name hash (key path segment).
fn net_hash(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(name.as_bytes()))
}

/// SHA-256 of the record content.
fn content_hash(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::PeerId;

    fn keypair() -> identity::Keypair {
        identity::Keypair::generate_ed25519()
    }

    fn network(kp: &identity::Keypair, name: &str) -> AuthNetwork {
        let pub_bytes = kp.public().try_into_ed25519().unwrap().to_bytes();
        AuthNetwork::new(name, &STANDARD.encode(pub_bytes)).unwrap()
    }

    fn entry_for(service: &str, value: &[u8]) -> ServiceEntry {
        ServiceEntry {
            name: service.to_string(),
            hash: content_hash(value),
            length: value.len() as u64,
        }
    }

    fn whitelist(version: u64, members: &[&PeerId]) -> WhitelistDoc {
        WhitelistDoc {
            version,
            members: members.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn index_round_trip() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let doc = IndexDoc {
            version: 1,
            expires_at: now_unix() + 300,
            services: vec![ServiceEntry {
                name: "cmd".into(),
                hash: vec![0u8; 32],
                length: 10,
            }],
        };
        let value = sign_index(&kp, &net, &doc).unwrap();
        let got = verify_index(&net, &value, now_unix()).unwrap();
        assert_eq!(got.version, 1);
        assert_eq!(got.services[0].name, "cmd");
    }

    #[test]
    fn whitelist_round_trip_with_hash_binding() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let peer = keypair().public().to_peer_id();
        let value = sign_whitelist(&kp, &net, "cmd", &whitelist(1, &[&peer])).unwrap();
        let entry = entry_for("cmd", &value);
        let doc = verify_whitelist(&net, "cmd", &value, &entry).unwrap();
        assert_eq!(doc.members, vec![peer.to_string()]);
    }

    #[test]
    fn tampered_payload_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let mut value = sign_index(
            &kp,
            &net,
            &IndexDoc {
                version: 1,
                expires_at: now_unix() + 300,
                services: vec![],
            },
        )
        .unwrap();
        let last = value.len() - 1;
        value[last] ^= 0xff;
        assert!(verify_index(&net, &value, now_unix()).is_err());
    }

    #[test]
    fn wrong_aad_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let peer = keypair().public().to_peer_id();
        let value = sign_whitelist(&kp, &net, "cmd", &whitelist(1, &[&peer])).unwrap();
        // Same value but verified against another service's key path -> aad mismatch.
        let entry = entry_for("ocr", &value);
        assert_eq!(
            verify_whitelist(&net, "ocr", &value, &entry),
            Err(AuthError::InvalidSignature)
        );
    }

    #[test]
    fn expired_index_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let now = now_unix();
        let value = sign_index(
            &kp,
            &net,
            &IndexDoc {
                version: 1,
                expires_at: now - 1,
                services: vec![],
            },
        )
        .unwrap();
        assert_eq!(verify_index(&net, &value, now), Err(AuthError::Expired));
    }

    #[test]
    fn authority_mismatch_rejected() {
        let signer = keypair();
        let net = network(&signer, "myorg");
        let value = sign_index(
            &signer,
            &net,
            &IndexDoc {
                version: 1,
                expires_at: now_unix() + 300,
                services: vec![],
            },
        )
        .unwrap();
        // A same-named network built from another key; verification should fail.
        let other = network(&keypair(), "myorg");
        assert_eq!(
            verify_index(&other, &value, now_unix()),
            Err(AuthError::InvalidSignature)
        );
    }

    #[test]
    fn hash_mismatch_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let peer = keypair().public().to_peer_id();
        let value = sign_whitelist(&kp, &net, "cmd", &whitelist(1, &[&peer])).unwrap();
        let bad = ServiceEntry {
            name: "cmd".into(),
            hash: vec![0u8; 32],
            length: value.len() as u64,
        };
        assert_eq!(
            verify_whitelist(&net, "cmd", &value, &bad),
            Err(AuthError::HashMismatch)
        );
    }

    #[test]
    fn oversized_value_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        let huge = vec![0u8; MAX_DOC_BYTES + 1];
        assert_eq!(
            verify_index(&net, &huge, now_unix()),
            Err(AuthError::TooLarge)
        );
    }

    #[test]
    fn versions_reject_index_rollback() {
        let mut versions = AuthVersions::default();
        versions.check_index(2).unwrap();
        assert_eq!(versions.check_index(1), Err(AuthError::Rollback));
        versions.check_index(3).unwrap();
        assert_eq!(versions.index_version(), Some(3));
    }

    #[test]
    fn versions_reject_whitelist_rollback() {
        let mut versions = AuthVersions::default();
        versions.check_whitelist("cmd", 2).unwrap();
        assert_eq!(versions.check_whitelist("cmd", 1), Err(AuthError::Rollback));
        versions.check_whitelist("cmd", 3).unwrap();
        assert_eq!(versions.whitelist_version("cmd"), Some(3));
        // Other services are unaffected.
        assert_eq!(versions.check_whitelist("ocr", 1), Ok(()));
    }
}
