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

//! 鉴权记录层：TUF-lite over DHT + COSE_Sign1
//!
//! DHT 是不可信、可覆盖、可重放的存储；本模块只对取回字节做密码学与一致性判定，
//! 不触碰网络
//!
//! - **来源可信**：记录为 COSE_Sign1，用配置固定的权威 ed25519 公钥验签
//! - **冻结**：索引记录带 `expires_at`，过期即拒
//! - **回滚**：索引与白名单各带 `version`，缓存拒绝更低版本（仅存活于进程内）
//! - **mix-and-match**：索引记录每个服务白名单的 `hash`+`length`，取回后校验
//! - **重放/跨位置**：COSE `external_aad` 绑定记录所在的 DHT key 路径
//!
//! 记录结构：
//!
//! ```text
//! key:   /oahd/auth/<net_hash>/service     索引
//! key:   /oahd/auth/<net_hash>/<service>   白名单
//! value: COSE_Sign1(payload = CBOR(doc), external_aad = key 路径字节)
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use coset::{
    CborSerializable, CoseSign1, CoseSign1Builder, HeaderBuilder, RegisteredLabelWithPrivate, iana,
};
use libp2p::PeerId;
use libp2p::identity::{self, ed25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个记录的字节上限
const MAX_DOC_BYTES: usize = 1 << 20;
/// 白名单成员数上限
const MAX_MEMBERS: usize = 100_000;
/// 索引服务数上限
const MAX_SERVICES: usize = 10_000;
/// 索引 key 的保留末段，禁止服务使用该名
pub const RESERVED_SERVICE: &str = "service";
/// DHT key 前缀
const AUTH_PREFIX: &str = "/oahd/auth";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// COSE 信封编解码失败
    Cose,
    /// payload 结构或字段非法
    Malformed,
    /// 配置中的权威公钥无法解析
    InvalidAuthority,
    /// 签名验证失败（含算法不符、aad 不符）
    InvalidSignature,
    /// 索引已过期（冻结防护）
    Expired,
    /// 版本低于已见（回滚防护）
    Rollback,
    /// 白名单与索引记录的 hash/length 不符（mix-and-match 防护）
    HashMismatch,
    /// 服务不在索引中（非致命：调用方警告并放行）
    NotInIndex,
    /// 记录或条目数超过上限
    TooLarge,
    /// 网络名/服务名非法，或使用了保留名
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

/// 一个鉴权网络：明文名、DHT key 用的哈希、以及固定的权威公钥
#[derive(Clone)]
pub struct AuthNetwork {
    pub name: String,
    /// `base64url_nopad(SHA-256(name))`，用作 key 路径段（不含 `/`）
    pub net_hash: String,
    pub authority: ed25519::PublicKey,
}

impl AuthNetwork {
    /// 由网络名与 base64 权威公钥构造
    pub fn new(name: &str, authority_b64: &str) -> Result<Self, AuthError> {
        if !is_valid_network_name(name) {
            return Err(AuthError::InvalidName);
        }
        Ok(Self {
            name: name.to_string(),
            net_hash: net_hash(name),
            authority: parse_authority(authority_b64)?,
        })
    }

    /// 索引记录 key：`/oahd/auth/<net_hash>/service`
    pub fn index_key(&self) -> String {
        format!("{}/{}/{}", AUTH_PREFIX, self.net_hash, RESERVED_SERVICE)
    }

    /// 白名单记录 key：`/oahd/auth/<net_hash>/<service>`
    pub fn service_key(&self, service: &str) -> Result<String, AuthError> {
        if !is_valid_service_name(service) {
            return Err(AuthError::InvalidName);
        }
        Ok(format!("{}/{}/{}", AUTH_PREFIX, self.net_hash, service))
    }
}

/// 索引中的一条服务项，绑定对应白名单记录的字节摘要
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEntry {
    pub name: String,
    /// 白名单记录完整字节的 SHA-256
    pub hash: Vec<u8>,
    /// 白名单记录完整字节的长度
    pub length: u64,
}

/// 索引记录 payload
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDoc {
    pub version: u64,
    pub expires_at: u64,
    pub services: Vec<ServiceEntry>,
}

/// 白名单记录 payload
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

/// 用 `keypair` 对 `payload` 签名，`aad` 为绑定的 key 路径字节
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

/// 验签并返回 payload；payload 原始字节在签名范围内，先验签再解析
fn verify_payload(
    authority: &ed25519::PublicKey,
    aad: &[u8],
    value: &[u8],
) -> Result<Vec<u8>, AuthError> {
    if value.len() > MAX_DOC_BYTES {
        return Err(AuthError::TooLarge);
    }
    let sign1 = CoseSign1::from_slice(value).map_err(|_| AuthError::Cose)?;

    // 强制 EdDSA 算法，避免算法混淆
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

/// 构造并签名索引记录，返回可写入 DHT 的完整字节
pub fn sign_index(
    keypair: &identity::Keypair,
    net: &AuthNetwork,
    doc: &IndexDoc,
) -> Result<Vec<u8>, AuthError> {
    let payload = cbor_encode(doc)?;
    sign_payload(keypair, net.index_key().as_bytes(), payload)
}

/// 构造并签名某服务的白名单记录，返回可写入 DHT 的完整字节
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

/// 校验索引记录：签名、结构、服务名、以及 `expires_at`（冻结防护）
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

/// 校验白名单记录：先做 hash/length 绑定（mix-and-match），再验签
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

/// 已校验的索引缓存
#[derive(Debug, Clone)]
pub struct CachedIndex {
    pub services: HashMap<String, ServiceEntry>,
    pub version: u64,
    pub expires_at: u64,
    pub fetched_at: Instant,
}

impl CachedIndex {
    /// 新鲜 = 未过文档期 且 未过本地 TTL
    pub fn is_fresh(&self, now: u64, ttl: Duration) -> bool {
        now <= self.expires_at && self.fetched_at.elapsed() < ttl
    }
}

/// 已校验的白名单缓存
#[derive(Debug, Clone)]
pub struct CachedWhitelist {
    pub members: HashSet<PeerId>,
    pub version: u64,
    pub fetched_at: Instant,
}

impl CachedWhitelist {
    /// 白名单记录本身无过期时间，缓存新鲜度仅由本地 TTL 决定；是否生效还需索引新鲜
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }

    pub fn is_member(&self, peer: &PeerId) -> bool {
        self.members.contains(peer)
    }
}

/// 鉴权缓存；版本回滚防护仅存活于进程生命周期（不落盘）
#[derive(Debug, Default)]
pub struct AuthCache {
    index: Option<CachedIndex>,
    whitelists: HashMap<String, CachedWhitelist>,
}

impl AuthCache {
    /// 写入索引，拒绝更低版本
    pub fn insert_index(&mut self, doc: IndexDoc) -> Result<(), AuthError> {
        if let Some(cur) = &self.index
            && doc.version < cur.version
        {
            return Err(AuthError::Rollback);
        }
        let services = doc
            .services
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect();
        self.index = Some(CachedIndex {
            services,
            version: doc.version,
            expires_at: doc.expires_at,
            fetched_at: Instant::now(),
        });
        Ok(())
    }

    /// 写入白名单，解析成员并拒绝更低版本
    pub fn insert_whitelist(&mut self, service: &str, doc: WhitelistDoc) -> Result<(), AuthError> {
        if !is_valid_service_name(service) {
            return Err(AuthError::InvalidName);
        }
        if let Some(cur) = self.whitelists.get(service)
            && doc.version < cur.version
        {
            return Err(AuthError::Rollback);
        }
        let mut members = HashSet::with_capacity(doc.members.len());
        for m in &doc.members {
            let peer = m.parse::<PeerId>().map_err(|_| AuthError::Malformed)?;
            members.insert(peer);
        }
        self.whitelists.insert(
            service.to_string(),
            CachedWhitelist {
                members,
                version: doc.version,
                fetched_at: Instant::now(),
            },
        );
        Ok(())
    }

    pub fn fresh_index(&self, now: u64, ttl: Duration) -> Option<&CachedIndex> {
        self.index.as_ref().filter(|i| i.is_fresh(now, ttl))
    }

    pub fn fresh_whitelist(&self, service: &str, ttl: Duration) -> Option<&CachedWhitelist> {
        self.whitelists.get(service).filter(|w| w.is_fresh(ttl))
    }

    /// 当前索引缓存（无论新鲜与否），供状态查询
    pub fn index(&self) -> Option<&CachedIndex> {
        self.index.as_ref()
    }

    /// 指定服务的白名单缓存（无论新鲜与否），供状态查询
    pub fn whitelist(&self, service: &str) -> Option<&CachedWhitelist> {
        self.whitelists.get(service)
    }

    pub fn clear(&mut self) {
        self.index = None;
        self.whitelists.clear();
    }
}

/// 网络名：非空、≤64、`[A-Za-z0-9_-]`
pub fn is_valid_network_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 服务名：同网络名规则，且不得为保留名 `service`
pub fn is_valid_service_name(s: &str) -> bool {
    is_valid_network_name(s) && s != RESERVED_SERVICE
}

/// 解析 base64 编码的 ed25519 权威公钥
pub fn parse_authority(b64: &str) -> Result<ed25519::PublicKey, AuthError> {
    let bytes = STANDARD
        .decode(b64)
        .map_err(|_| AuthError::InvalidAuthority)?;
    ed25519::PublicKey::try_from_bytes(&bytes).map_err(|_| AuthError::InvalidAuthority)
}

/// 当前 Unix 时间（秒）
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 网络名哈希（key 路径段）
fn net_hash(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(name.as_bytes()))
}

/// 记录内容的 SHA-256
fn content_hash(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 用同一 value、但以另一个服务的 key 路径验签 → aad 不符
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
        // 另一把密钥构成的同名网络，验签应失败
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
    fn cache_rejects_rollback() {
        let mut cache = AuthCache::default();
        let doc = |v| IndexDoc {
            version: v,
            expires_at: now_unix() + 300,
            services: vec![],
        };
        cache.insert_index(doc(2)).unwrap();
        assert_eq!(cache.insert_index(doc(1)), Err(AuthError::Rollback));
        cache.insert_index(doc(3)).unwrap();
    }

    #[test]
    fn cache_whitelist_rejects_rollback_and_bad_member() {
        let mut cache = AuthCache::default();
        let peer = keypair().public().to_peer_id();
        cache
            .insert_whitelist("cmd", whitelist(2, &[&peer]))
            .unwrap();
        assert_eq!(
            cache.insert_whitelist("cmd", whitelist(1, &[&peer])),
            Err(AuthError::Rollback)
        );
        let bad = WhitelistDoc {
            version: 3,
            members: vec!["not-a-peer-id".into()],
        };
        assert_eq!(
            cache.insert_whitelist("cmd", bad),
            Err(AuthError::Malformed)
        );
    }

    #[test]
    fn freshness_conditions() {
        let mut cache = AuthCache::default();
        let now = now_unix();
        cache
            .insert_index(IndexDoc {
                version: 1,
                expires_at: now + 300,
                services: vec![],
            })
            .unwrap();
        assert!(cache.fresh_index(now, Duration::from_secs(300)).is_some());
        // TTL 为零 → 视为不新鲜
        assert!(cache.fresh_index(now, Duration::ZERO).is_none());
        // 已过文档期 → 不新鲜
        assert!(
            cache
                .fresh_index(now + 301, Duration::from_secs(300))
                .is_none()
        );
    }

    #[test]
    fn name_validation() {
        assert!(is_valid_network_name("myorg"));
        assert!(is_valid_network_name("my_org-1"));
        assert!(!is_valid_network_name(""));
        assert!(!is_valid_network_name("a/b"));
        assert!(is_valid_service_name("cmd"));
        assert!(!is_valid_service_name(RESERVED_SERVICE));
        assert!(!is_valid_service_name(""));
    }

    #[test]
    fn net_hash_is_stable_and_path_safe() {
        let h1 = net_hash("myorg");
        let h2 = net_hash("myorg");
        let h3 = net_hash("other");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert!(!h1.contains('/'));
        assert!(!h1.contains('+'));
    }

    #[test]
    fn invalid_network_name_rejected_at_construction() {
        let kp = keypair();
        let pub_bytes = kp.public().try_into_ed25519().unwrap().to_bytes();
        let b64 = STANDARD.encode(pub_bytes);
        assert_eq!(
            AuthNetwork::new("bad/name", &b64).err(),
            Some(AuthError::InvalidName)
        );
    }

    #[test]
    fn reserved_service_key_rejected() {
        let kp = keypair();
        let net = network(&kp, "myorg");
        assert_eq!(
            net.service_key(RESERVED_SERVICE),
            Err(AuthError::InvalidName)
        );
    }
}
