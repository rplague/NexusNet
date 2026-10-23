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

//! Minimal COSE_Sign1 helpers shared by auth and service-type records.

use libp2p::identity::{self, ed25519};

use coset::{
    CborSerializable, CoseSign1, CoseSign1Builder, HeaderBuilder, RegisteredLabelWithPrivate, iana,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoseError {
    Cose,
    Malformed,
    InvalidSignature,
    TooLarge,
}

impl std::fmt::Display for CoseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoseError::Cose => write!(f, "cose envelope error"),
            CoseError::Malformed => write!(f, "malformed cose payload"),
            CoseError::InvalidSignature => write!(f, "signature verification failed"),
            CoseError::TooLarge => write!(f, "cose document too large"),
        }
    }
}

impl std::error::Error for CoseError {}

#[allow(dead_code)]
fn eddsa_header() -> coset::Header {
    HeaderBuilder::new()
        .algorithm(iana::Algorithm::EdDSA)
        .build()
}

/// Sign `payload` with an Ed25519 keypair; `aad` binds the record to its DHT key.
#[allow(dead_code)]
pub fn sign_ed25519(
    keypair: &identity::Keypair,
    aad: &[u8],
    payload: Vec<u8>,
) -> Result<Vec<u8>, CoseError> {
    let sign1 = CoseSign1Builder::new()
        .protected(eddsa_header())
        .payload(payload)
        .try_create_signature(aad, |data| keypair.sign(data).map_err(|_| CoseError::Cose))?
        .build();
    sign1.to_vec().map_err(|_| CoseError::Cose)
}

/// Verify a COSE_Sign1 value and return its payload.
pub fn verify_ed25519(
    authority: &ed25519::PublicKey,
    aad: &[u8],
    value: &[u8],
    max_len: usize,
) -> Result<Vec<u8>, CoseError> {
    if value.len() > max_len {
        return Err(CoseError::TooLarge);
    }
    let sign1 = CoseSign1::from_slice(value).map_err(|_| CoseError::Cose)?;

    // Enforce EdDSA to avoid algorithm confusion.
    let expected = RegisteredLabelWithPrivate::Assigned(iana::Algorithm::EdDSA);
    if sign1.protected.header.alg != Some(expected) {
        return Err(CoseError::InvalidSignature);
    }

    sign1
        .verify_signature(aad, |signature, data| {
            if authority.verify(data, signature) {
                Ok(())
            } else {
                Err(CoseError::InvalidSignature)
            }
        })
        .map_err(|_| CoseError::InvalidSignature)?;

    sign1.payload.ok_or(CoseError::Malformed)
}
