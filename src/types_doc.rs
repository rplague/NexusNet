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

//! Signed global service-type index (`/oahd/service/authtypes/`).
//!
//! The index is an authority-signed, versioned, expiring document. Nodes never
//! merge or republish their own view; they only accept the highest valid
//! version and replace the previous set. This removes the unauthenticated
//! monotonic union that previously lived in the service registry.

use libp2p::identity::{self, ed25519};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::cose::{self, CoseError};
use crate::limits::DHT_MAX_RECORD_BYTES;

/// DHT key of the signed service-type index.
pub const TYPES_KEY: &[u8] = b"/oahd/service/authtypes/";
/// Legacy (unsigned) service-type index used by older nodes.
pub const LEGACY_TYPES_KEY: &[u8] = b"/oahd/service/types";
/// Maximum number of service names in one document.
pub const MAX_TYPES: usize = 4096;
/// Allowed clock skew for `issued_at`.
pub const MAX_SKEW_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypesDoc {
    pub version: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub services: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypesError {
    Cose(CoseError),
    Malformed,
    InvalidName,
    TooLarge,
    Expired,
    NotYetValid,
    BadTtl,
    Equivocation,
}

impl std::fmt::Display for TypesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypesError::Cose(e) => write!(f, "{e}"),
            TypesError::Malformed => write!(f, "malformed types document"),
            TypesError::InvalidName => write!(f, "invalid service name"),
            TypesError::TooLarge => write!(f, "types document too large"),
            TypesError::Expired => write!(f, "types document expired"),
            TypesError::NotYetValid => write!(f, "types document is not valid yet"),
            TypesError::BadTtl => write!(f, "invalid types document lifetime"),
            TypesError::Equivocation => write!(f, "conflicting types documents"),
        }
    }
}

impl std::error::Error for TypesError {}

impl From<CoseError> for TypesError {
    fn from(error: CoseError) -> Self {
        TypesError::Cose(error)
    }
}

#[allow(dead_code)]
fn cbor_encode(doc: &TypesDoc) -> Result<Vec<u8>, TypesError> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(doc, &mut buf).map_err(|_| TypesError::Malformed)?;
    Ok(buf)
}

/// Build and sign a types document.
#[allow(dead_code)]
pub fn sign_types(keypair: &identity::Keypair, doc: &TypesDoc) -> Result<Vec<u8>, TypesError> {
    let payload = cbor_encode(doc)?;
    cose::sign_ed25519(keypair, TYPES_KEY, payload).map_err(TypesError::Cose)
}

/// Verify a signed types document.
pub fn verify_types(
    authority: &ed25519::PublicKey,
    value: &[u8],
    now: u64,
    max_ttl_seconds: u64,
) -> Result<TypesDoc, TypesError> {
    let payload = cose::verify_ed25519(authority, TYPES_KEY, value, DHT_MAX_RECORD_BYTES)?;
    let mut doc: TypesDoc =
        ciborium::de::from_reader(payload.as_slice()).map_err(|_| TypesError::Malformed)?;

    if doc.services.len() > MAX_TYPES {
        return Err(TypesError::TooLarge);
    }
    if doc.issued_at == 0 || doc.expires_at <= doc.issued_at {
        return Err(TypesError::BadTtl);
    }
    if doc.issued_at > now.saturating_add(MAX_SKEW_SECS) {
        return Err(TypesError::NotYetValid);
    }
    if now > doc.expires_at {
        return Err(TypesError::Expired);
    }
    if doc.expires_at - doc.issued_at > max_ttl_seconds {
        return Err(TypesError::BadTtl);
    }

    doc.services.sort();
    doc.services.dedup();
    if doc
        .services
        .iter()
        .any(|name| !auth::is_valid_service_name(name))
    {
        return Err(TypesError::InvalidName);
    }

    Ok(doc)
}

/// Select the highest-version valid document from a set of DHT records.
pub fn select_types(
    records: &[libp2p::kad::PeerRecord],
    authority: &ed25519::PublicKey,
    now: u64,
    max_ttl_seconds: u64,
) -> Result<Option<(TypesDoc, usize)>, TypesError> {
    let mut best: Option<(TypesDoc, usize)> = None;
    for record in records {
        let Ok(doc) = verify_types(authority, &record.record.value, now, max_ttl_seconds) else {
            continue;
        };
        let size = record.record.value.len();
        match &best {
            None => best = Some((doc, size)),
            Some((current, _)) => {
                if doc.version > current.version {
                    best = Some((doc, size));
                } else if doc.version == current.version && doc != *current {
                    return Err(TypesError::Equivocation);
                }
            }
        }
    }
    Ok(best)
}

/// Parse a legacy unsigned JSON array (`["cmd","ocr"]`).
///
/// Compatibility path only: invalid names are dropped and the list is capped.
pub fn parse_legacy_types(value: &[u8]) -> Option<Vec<String>> {
    let names: Vec<String> = serde_json::from_slice(value).ok()?;
    Some(
        names
            .into_iter()
            .filter(|name| auth::is_valid_service_name(name))
            .take(MAX_TYPES)
            .collect(),
    )
}

/// Encode a legacy unsigned JSON array for the compatibility mirror.
pub fn encode_legacy_types(services: &[String]) -> Option<Vec<u8>> {
    serde_json::to_vec(services).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::kad;

    fn keypair() -> identity::Keypair {
        identity::Keypair::generate_ed25519()
    }

    fn authority(keypair: &identity::Keypair) -> ed25519::PublicKey {
        keypair.public().try_into_ed25519().unwrap()
    }

    fn doc(version: u64, issued_at: u64, expires_at: u64) -> TypesDoc {
        TypesDoc {
            version,
            issued_at,
            expires_at,
            services: vec!["cmd".into(), "ocr".into(), "cmd".into()],
        }
    }

    fn record(value: Vec<u8>) -> kad::PeerRecord {
        kad::PeerRecord {
            peer: None,
            record: kad::Record::new(kad::RecordKey::new(&TYPES_KEY), value),
        }
    }

    #[test]
    fn round_trip_and_dedup() {
        let kp = keypair();
        let now = 1000;
        let value = sign_types(&kp, &doc(1, now, now + 60)).unwrap();
        let got = verify_types(&authority(&kp), &value, now, 300).unwrap();
        assert_eq!(got.services, vec!["cmd", "ocr"]);
    }

    #[test]
    fn wrong_authority_rejected() {
        let kp = keypair();
        let value = sign_types(&kp, &doc(1, 1000, 1060)).unwrap();
        assert!(matches!(
            verify_types(&authority(&keypair()), &value, 1000, 300),
            Err(TypesError::Cose(CoseError::InvalidSignature))
        ));
    }

    #[test]
    fn expired_rejected() {
        let kp = keypair();
        let value = sign_types(&kp, &doc(1, 1000, 1060)).unwrap();
        assert_eq!(
            verify_types(&authority(&kp), &value, 1061, 300),
            Err(TypesError::Expired)
        );
    }

    #[test]
    fn bad_ttl_rejected() {
        let kp = keypair();
        let value = sign_types(&kp, &doc(1, 1000, 2000)).unwrap();
        assert_eq!(
            verify_types(&authority(&kp), &value, 1000, 300),
            Err(TypesError::BadTtl)
        );
    }

    #[test]
    fn legacy_json_round_trip_filters_invalid_names() {
        let value = br#"["cmd","ocr","bad name","service","cmd"]"#;
        let names = parse_legacy_types(value).unwrap();
        assert_eq!(names, vec!["cmd", "ocr", "cmd"]);
        let encoded = encode_legacy_types(&["cmd".to_string(), "ocr".to_string()]).unwrap();
        assert_eq!(parse_legacy_types(&encoded).unwrap(), vec!["cmd", "ocr"]);
    }

    #[test]
    fn highest_version_wins_and_conflict_rejected() {
        let kp = keypair();
        let auth = authority(&kp);
        let older = record(sign_types(&kp, &doc(1, 1000, 1060)).unwrap());
        let newer_doc = doc(2, 1000, 1060);
        let newer = record(sign_types(&kp, &newer_doc).unwrap());
        let selected = select_types(&[older.clone(), newer.clone()], &auth, 1000, 300)
            .unwrap()
            .unwrap();
        assert_eq!(selected.0.version, 2);

        let mut other = doc(2, 1000, 1060);
        other.services = vec!["other".into()];
        let conflicting = record(sign_types(&kp, &other).unwrap());
        assert_eq!(
            select_types(&[older, newer, conflicting], &auth, 1000, 300),
            Err(TypesError::Equivocation)
        );
    }
}
