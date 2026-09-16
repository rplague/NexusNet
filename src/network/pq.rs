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

//! 抗量子（PQ）应用层加密原语。
//!
//! 组合：ML-KEM-768（FIPS 203）+ X25519 混合 KEM、ChaCha20-Poly1305 AEAD、
//! 可选 ML-DSA-65（FIPS 204）签名。用于服务请求/响应的机密性（HNDL 防护）。
//!
//! 关键设计：请求方 encaps 到响应方 KEM 公钥，响应方 decaps 后双方共享同一密钥，
//! 响应直接复用该密钥加密，**无需第二次 KEM、无需在请求中携带请求方公钥**。
//!
//! 注意：PQ 公钥用 ED25519 身份签名绑定到 `PeerId`，因此身份绑定本身仍是经典安全。

use chacha20poly1305::aead::{Aead, KeyInit as _};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use libp2p::{PeerId, identity};
use ml_dsa::{
    Generate as _, KeyExport as _, KeyInit as _, Keypair as _, MlDsa65, Signature as DsaSignature,
    SignatureEncoding as _, Signer as _, SigningKey, Verifier as _, VerifyingKey,
};
use ml_kem::kem::{Decapsulate as _, Encapsulate as _, Kem as _, TryKeyInit as _};
use ml_kem::{Ciphertext, DecapsulationKey, EncapsulationKey, MlKem768};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

const ENVELOPE_VERSION: u8 = 1;
const REQ_INFO: &[u8] = b"oahd/pq/req/v1";
const RES_INFO: &[u8] = b"oahd/pq/res/v1";

/// PQ 加密服务调用协议 ID。
pub const SERVICE_REQ_PQ_PROTOCOL: &str = "/oahd/service_req/2.0.0";
/// PQ 身份交换协议 ID（其存在即表示对端支持 PQ）。
pub const PQ_IDENTITY_PROTOCOL: &str = "/oahd/pq_identity/1.0.0";
const NONCE_LEN: usize = 12;
/// 单个信封密文上限，防止恶意长度触发大分配。
const MAX_CIPHERTEXT: usize = 1 << 20;

/// ML-KEM-768 参数尺寸（FIPS 203）。
const MLKEM_CT_LEN: usize = 1088;
const MLKEM_EK_LEN: usize = 1184;
const MLKEM_DK_LEN: usize = 2400;
/// ML-DSA-65 参数尺寸（FIPS 204）。
const MLDSA_VK_LEN: usize = 1952;
const MLDSA_SK_LEN: usize = 4032;
const X25519_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PqError {
    /// 身份证明（ED25519 绑定）无效。
    InvalidIdentity,
    /// 密钥材料非法或长度不符。
    InvalidKey,
    /// 信封格式/尺寸非法。
    InvalidEnvelope,
    /// 解密或验签失败。
    Crypto,
}

impl std::fmt::Display for PqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PqError::InvalidIdentity => write!(f, "pq identity proof invalid"),
            PqError::InvalidKey => write!(f, "pq key material invalid"),
            PqError::InvalidEnvelope => write!(f, "pq envelope malformed"),
            PqError::Crypto => write!(f, "pq crypto failure"),
        }
    }
}

impl std::error::Error for PqError {}

// ---------------------------------------------------------------------------
// 密钥
// ---------------------------------------------------------------------------

/// 本地 PQ 密钥集合（ML-KEM + X25519 + ML-DSA）。
pub struct PqKeys {
    kem_dk: DecapsulationKey<MlKem768>,
    kem_ek: EncapsulationKey<MlKem768>,
    x_secret: StaticSecret,
    sig_sk: SigningKey<MlDsa65>,
    sig_vk: VerifyingKey<MlDsa65>,
}

impl PqKeys {
    /// 生成全新密钥对。
    pub fn generate() -> Self {
        let (kem_dk, kem_ek) = MlKem768::generate_keypair();
        let sig_sk = SigningKey::<MlDsa65>::generate();
        let sig_vk = sig_sk.verifying_key().clone();
        let x_secret = StaticSecret::random_from_rng(OsRng);
        Self {
            kem_dk,
            kem_ek,
            x_secret,
            sig_sk,
            sig_vk,
        }
    }

    /// 序列化为版本化的长度前缀格式。
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(ENVELOPE_VERSION);
        push_field(&mut out, self.kem_dk.to_bytes().as_slice());
        push_field(&mut out, self.kem_ek.to_bytes().as_slice());
        push_field(&mut out, &self.x_secret.to_bytes());
        push_field(&mut out, self.sig_sk.to_bytes().as_slice());
        push_field(&mut out, self.sig_vk.to_bytes().as_slice());
        out
    }

    /// 从序列化字节恢复。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PqError> {
        let mut r = Reader::new(bytes);
        if r.u8()? != ENVELOPE_VERSION {
            return Err(PqError::InvalidKey);
        }
        let kem_dk_bytes = r.field()?;
        let kem_ek_bytes = r.field()?;
        let x_bytes = r.field()?;
        let sig_sk_bytes = r.field()?;
        let sig_vk_bytes = r.field()?;

        let kem_dk = DecapsulationKey::<MlKem768>::new_from_slice(kem_dk_bytes)
            .map_err(|_| PqError::InvalidKey)?;
        let kem_ek = EncapsulationKey::<MlKem768>::new_from_slice(kem_ek_bytes)
            .map_err(|_| PqError::InvalidKey)?;
        let x_arr: [u8; X25519_LEN] = x_bytes.try_into().map_err(|_| PqError::InvalidKey)?;
        let sig_sk =
            SigningKey::<MlDsa65>::new_from_slice(sig_sk_bytes).map_err(|_| PqError::InvalidKey)?;
        let sig_vk = VerifyingKey::<MlDsa65>::new_from_slice(sig_vk_bytes)
            .map_err(|_| PqError::InvalidKey)?;

        Ok(Self {
            kem_dk,
            kem_ek,
            x_secret: StaticSecret::from(x_arr),
            sig_sk,
            sig_vk,
        })
    }

    /// 构造带 ED25519 身份证明的公开身份。
    pub fn identity(&self, id_keypair: &identity::Keypair) -> Result<PqIdentity, PqError> {
        let kem_pub = self.kem_ek.to_bytes().as_slice().to_vec();
        let x_pub = X25519PublicKey::from(&self.x_secret).to_bytes().to_vec();
        let sig_pub = self.sig_vk.to_bytes().as_slice().to_vec();

        let proof = id_keypair
            .sign(&identity_message(&kem_pub, &x_pub, &sig_pub))
            .map_err(|_| PqError::Crypto)?;
        let ed_pub = id_keypair
            .public()
            .clone()
            .try_into_ed25519()
            .map_err(|_| PqError::InvalidIdentity)?
            .to_bytes()
            .to_vec();

        Ok(PqIdentity {
            ed25519_pub: ed_pub,
            kem_pub,
            x25519_pub: x_pub,
            sig_pub,
            proof,
        })
    }
}

// ---------------------------------------------------------------------------
// 身份
// ---------------------------------------------------------------------------

/// 对端的公开 PQ 身份（含 ED25519 绑定证明）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqIdentity {
    pub ed25519_pub: Vec<u8>,
    pub kem_pub: Vec<u8>,
    pub x25519_pub: Vec<u8>,
    pub sig_pub: Vec<u8>,
    pub proof: Vec<u8>,
}

impl PqIdentity {
    /// 校验身份证明并把 PQ 公钥绑定到 `expected` PeerId。
    pub fn verify(&self, expected: &PeerId) -> Result<(), PqError> {
        let ed = identity::ed25519::PublicKey::try_from_bytes(&self.ed25519_pub)
            .map_err(|_| PqError::InvalidIdentity)?;
        if identity::PublicKey::from(ed.clone()).to_peer_id() != *expected {
            return Err(PqError::InvalidIdentity);
        }
        if !ed.verify(
            &identity_message(&self.kem_pub, &self.x25519_pub, &self.sig_pub),
            &self.proof,
        ) {
            return Err(PqError::InvalidIdentity);
        }
        Ok(())
    }

    fn kem_ek(&self) -> Result<EncapsulationKey<MlKem768>, PqError> {
        EncapsulationKey::<MlKem768>::new_from_slice(&self.kem_pub).map_err(|_| PqError::InvalidKey)
    }

    fn x_pub(&self) -> Result<X25519PublicKey, PqError> {
        let arr: [u8; X25519_LEN] = self
            .x25519_pub
            .as_slice()
            .try_into()
            .map_err(|_| PqError::InvalidKey)?;
        Ok(X25519PublicKey::from(arr))
    }

    fn sig_vk(&self) -> Result<VerifyingKey<MlDsa65>, PqError> {
        VerifyingKey::<MlDsa65>::new_from_slice(&self.sig_pub).map_err(|_| PqError::InvalidKey)
    }
}

// ---------------------------------------------------------------------------
// 信封
// ---------------------------------------------------------------------------

/// 加密请求：携带 KEM 密文与（可选）发送方身份/签名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqRequest {
    pub version: u8,
    pub kem: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub identity: Option<PqIdentity>,
    pub signature: Option<Vec<u8>>,
}

/// 加密响应：复用请求建立的共享密钥，无需 KEM。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqResponse {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// 请求方发出后保留的响应密钥。
pub struct PqOutbound {
    pub request: PqRequest,
    res_key: [u8; 32],
}

impl PqOutbound {
    /// 解密响应。
    pub fn open_response(&self, response: &PqResponse) -> Result<Vec<u8>, PqError> {
        aead_open(&self.res_key, &response.nonce, &response.ciphertext)
    }
}

/// 响应方处理请求后得到的明文与响应密钥。
pub struct PqInbound {
    pub plaintext: Vec<u8>,
    res_key: [u8; 32],
}

impl PqInbound {
    /// 加密响应。
    pub fn seal_response(&self, plaintext: &[u8]) -> Result<PqResponse, PqError> {
        let (nonce, ciphertext) = aead_seal(&self.res_key, plaintext)?;
        Ok(PqResponse { nonce, ciphertext })
    }
}

/// 请求方：加密请求。`sign` 为真时附带 ML-DSA 签名与身份。
pub fn seal_request(
    keys: &PqKeys,
    recipient: &PqIdentity,
    plaintext: &[u8],
    sign: bool,
    id_keypair: &identity::Keypair,
) -> Result<PqOutbound, PqError> {
    let ek = recipient.kem_ek()?;
    let (kem_ct, kem_ss) = ek.encapsulate();

    let eph_secret = StaticSecret::random_from_rng(OsRng);
    let eph_pub = X25519PublicKey::from(&eph_secret);
    let x_ss = eph_secret.diffie_hellman(&recipient.x_pub()?);

    let shared = combine(&kem_ss, x_ss.as_bytes());
    let req_key = derive(&shared, REQ_INFO);
    let res_key = derive(&shared, RES_INFO);

    let (nonce, ciphertext) = aead_seal(&req_key, plaintext)?;

    let mut kem = kem_ct.as_slice().to_vec();
    kem.extend_from_slice(eph_pub.as_bytes());

    let (identity, signature) = if sign {
        let id = keys.identity(id_keypair)?;
        let sig = keys
            .sig_sk
            .sign(&signed_message(&kem, &nonce, &ciphertext))
            .to_vec();
        (Some(id), Some(sig))
    } else {
        (None, None)
    };

    Ok(PqOutbound {
        request: PqRequest {
            version: ENVELOPE_VERSION,
            kem,
            nonce,
            ciphertext,
            identity,
            signature,
        },
        res_key,
    })
}

/// 响应方：解密请求。
pub fn open_request(keys: &PqKeys, request: &PqRequest) -> Result<PqInbound, PqError> {
    if request.version != ENVELOPE_VERSION
        || request.kem.len() != MLKEM_CT_LEN + X25519_LEN
        || request.nonce.len() != NONCE_LEN
        || request.ciphertext.len() > MAX_CIPHERTEXT
    {
        return Err(PqError::InvalidEnvelope);
    }

    // 可选签名校验
    if let (Some(id), Some(sig)) = (&request.identity, &request.signature) {
        id.verify(&id_peer_id(id)?)?;
        let vk = id.sig_vk()?;
        let sig = DsaSignature::<MlDsa65>::try_from(sig.as_slice())
            .map_err(|_| PqError::InvalidIdentity)?;
        if vk
            .verify(
                &signed_message(&request.kem, &request.nonce, &request.ciphertext),
                &sig,
            )
            .is_err()
        {
            return Err(PqError::InvalidIdentity);
        }
    }

    let (kem_bytes, x_bytes) = request.kem.split_at(MLKEM_CT_LEN);
    let kem_ct =
        Ciphertext::<MlKem768>::try_from(kem_bytes).map_err(|_| PqError::InvalidEnvelope)?;
    let kem_ss = keys.kem_dk.decapsulate(&kem_ct);

    let eph_arr: [u8; X25519_LEN] = x_bytes.try_into().map_err(|_| PqError::InvalidEnvelope)?;
    let x_ss = keys
        .x_secret
        .diffie_hellman(&X25519PublicKey::from(eph_arr));

    let shared = combine(&kem_ss, x_ss.as_bytes());
    let req_key = derive(&shared, REQ_INFO);
    let res_key = derive(&shared, RES_INFO);

    let plaintext = aead_open(&req_key, &request.nonce, &request.ciphertext)?;
    Ok(PqInbound { plaintext, res_key })
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

fn identity_message(kem_pub: &[u8], x25519_pub: &[u8], sig_pub: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(kem_pub.len() + x25519_pub.len() + sig_pub.len() + 16);
    m.extend_from_slice(b"oahd/pq/identity/v1");
    m.extend_from_slice(kem_pub);
    m.extend_from_slice(x25519_pub);
    m.extend_from_slice(sig_pub);
    m
}

fn signed_message(kem: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(kem.len() + nonce.len() + ciphertext.len() + 16);
    m.extend_from_slice(b"oahd/pq/request/v1");
    m.extend_from_slice(kem);
    m.extend_from_slice(nonce);
    m.extend_from_slice(ciphertext);
    m
}

fn id_peer_id(id: &PqIdentity) -> Result<PeerId, PqError> {
    let ed = identity::ed25519::PublicKey::try_from_bytes(&id.ed25519_pub)
        .map_err(|_| PqError::InvalidIdentity)?;
    Ok(identity::PublicKey::from(ed).to_peer_id())
}

fn combine(kem_ss: &[u8], x_ss: &[u8]) -> [u8; 32] {
    let mut ikm = Vec::with_capacity(kem_ss.len() + x_ss.len());
    ikm.extend_from_slice(kem_ss);
    ikm.extend_from_slice(x_ss);
    let hk = Hkdf::<Sha256>::new(None, &ikm);
    let mut out = [0u8; 32];
    hk.expand(b"oahd/pq/combine/v1", &mut out)
        .expect("32 bytes is a valid HKDF output length");
    out
}

fn derive(shared: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shared);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 bytes is a valid HKDF output length");
    out
}

fn aead_seal(key: &[u8; 32], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), PqError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| PqError::Crypto)?;
    Ok((nonce.to_vec(), ciphertext))
}

fn aead_open(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, PqError> {
    if nonce.len() != NONCE_LEN || ciphertext.len() > MAX_CIPHERTEXT {
        return Err(PqError::InvalidEnvelope);
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| PqError::Crypto)
}

fn push_field(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, PqError> {
        let b = *self.buf.get(self.pos).ok_or(PqError::InvalidKey)?;
        self.pos += 1;
        Ok(b)
    }

    fn field(&mut self) -> Result<&'a [u8], PqError> {
        let end = self.pos + 4;
        let len_bytes = self.buf.get(self.pos..end).ok_or(PqError::InvalidKey)?;
        let len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
        self.pos = end;
        let field = self
            .buf
            .get(self.pos..self.pos + len)
            .ok_or(PqError::InvalidKey)?;
        self.pos += len;
        Ok(field)
    }
}

// 编译期尺寸自检：确保常量与算法实现一致。
const _: () = assert!(MLKEM_CT_LEN == 1088 && MLKEM_EK_LEN == 1184 && MLKEM_DK_LEN == 2400);
const _: () = assert!(MLDSA_VK_LEN == 1952 && MLDSA_SK_LEN == 4032);

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity;

    fn id_keypair() -> identity::Keypair {
        identity::Keypair::generate_ed25519()
    }

    #[test]
    fn request_response_round_trip() {
        let responder = PqKeys::generate();
        let responder_id_kp = id_keypair();
        let responder_peer = responder_id_kp.public().to_peer_id();
        let responder_identity = responder.identity(&responder_id_kp).unwrap();
        responder_identity.verify(&responder_peer).unwrap();

        let requester = PqKeys::generate();
        let requester_id_kp = id_keypair();

        let out = seal_request(
            &requester,
            &responder_identity,
            b"hello pq",
            false,
            &requester_id_kp,
        )
        .unwrap();

        let inbound = open_request(&responder, &out.request).unwrap();
        assert_eq!(inbound.plaintext, b"hello pq");

        let resp = inbound.seal_response(b"world pq").unwrap();
        let got = out.open_response(&resp).unwrap();
        assert_eq!(got, b"world pq");
    }

    #[test]
    fn signed_request_round_trip_and_verification() {
        let responder = PqKeys::generate();
        let responder_id_kp = id_keypair();
        let responder_identity = responder.identity(&responder_id_kp).unwrap();

        let requester = PqKeys::generate();
        let requester_id_kp = id_keypair();

        let out = seal_request(
            &requester,
            &responder_identity,
            b"signed",
            true,
            &requester_id_kp,
        )
        .unwrap();
        assert!(out.request.signature.is_some());
        assert!(out.request.identity.is_some());

        let inbound = open_request(&responder, &out.request).unwrap();
        assert_eq!(inbound.plaintext, b"signed");
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let responder = PqKeys::generate();
        let responder_id_kp = id_keypair();
        let responder_identity = responder.identity(&responder_id_kp).unwrap();

        let requester = PqKeys::generate();
        let requester_id_kp = id_keypair();
        let mut out = seal_request(
            &requester,
            &responder_identity,
            b"secret",
            false,
            &requester_id_kp,
        )
        .unwrap();

        out.request.ciphertext[0] ^= 0xff;
        assert!(matches!(
            open_request(&responder, &out.request),
            Err(PqError::Crypto)
        ));
    }

    #[test]
    fn identity_bound_to_peer_id() {
        let kp = id_keypair();
        let keys = PqKeys::generate();
        let id = keys.identity(&kp).unwrap();
        let other = id_keypair().public().to_peer_id();
        assert_eq!(id.verify(&other), Err(PqError::InvalidIdentity));
    }

    #[test]
    fn key_serialization_round_trip() {
        let keys = PqKeys::generate();
        let bytes = keys.to_bytes();
        let restored = PqKeys::from_bytes(&bytes).unwrap();

        let id_kp = id_keypair();
        let id = restored.identity(&id_kp).unwrap();
        id.verify(&id_kp.public().to_peer_id()).unwrap();
    }
}
