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

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, RwLock};

use libp2p::futures::future::BoxFuture;
use libp2p::{PeerId, kad};

use crate::auth::{self, AuthNetwork, AuthVersions, IndexDoc, ServiceEntry, WhitelistDoc};
use crate::config::ConfigHandle;
use crate::dht_cache::{DhtCache, Fetched, Fetcher};
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;

/// Node authentication runtime state.
#[derive(Clone)]
enum AuthState {
    /// `auth.network == "none"`: authentication disabled.
    Disabled,
    /// Authentication enabled and the authority public key is valid.
    Active(AuthNetwork),
    /// Network set but the authority key is missing/invalid: fail-closed (reject).
    Misconfigured(String),
}

/// Authorization verdict for a single inbound request.
pub enum Decision {
    Allow,
    /// Service not in the auth index: allow this request without changing local config.
    AllowUnprotected,
    Deny(String),
}

fn resolve_state(config: &ConfigHandle) -> AuthState {
    let network = config.auth_network();
    if network == "none" {
        return AuthState::Disabled;
    }
    match config.auth_authority(&network) {
        Some(authority) => match AuthNetwork::new(&network, &authority) {
            Ok(net) => AuthState::Active(net),
            Err(e) => AuthState::Misconfigured(format!("鉴权网络 '{network}' 权威公钥无效: {e}")),
        },
        None => AuthState::Misconfigured(format!("鉴权网络 '{network}' 未配置权威公钥")),
    }
}

fn same_trust_domain(a: &AuthState, b: &AuthState) -> bool {
    match (a, b) {
        (AuthState::Active(a), AuthState::Active(b)) => {
            a.net_hash == b.net_hash && a.authority.to_bytes() == b.authority.to_bytes()
        }
        (AuthState::Disabled, AuthState::Disabled) => true,
        _ => false,
    }
}

#[derive(Clone, Debug, Eq)]
struct WhitelistKey {
    service: String,
    hash: Vec<u8>,
    length: u64,
    index_expires_at: u64,
}

impl PartialEq for WhitelistKey {
    fn eq(&self, other: &Self) -> bool {
        self.service == other.service
            && self.hash == other.hash
            && self.length == other.length
            && self.index_expires_at == other.index_expires_at
    }
}

impl Hash for WhitelistKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.service.hash(state);
        self.hash.hash(state);
        self.length.hash(state);
        self.index_expires_at.hash(state);
    }
}

struct AuthIndexFetcher {
    swarm: NetworkHandle,
    net: AuthNetwork,
}

impl Fetcher<kad::RecordKey, IndexDoc> for AuthIndexFetcher {
    fn fetch<'a>(
        &'a self,
        key: &'a kad::RecordKey,
    ) -> BoxFuture<'a, Result<Option<Fetched<IndexDoc>>, String>> {
        Box::pin(async move {
            let records = self
                .swarm
                .get_record_all(key.clone())
                .await
                .map_err(|e| e.to_string())?;
            select_index(&records, &self.net, auth::now_unix()).map(|selected| {
                selected.map(|(doc, size)| Fetched {
                    version: Some(doc.version),
                    value: doc,
                    size,
                })
            })
        })
    }
}

struct AuthWhitelistFetcher {
    swarm: NetworkHandle,
    net: AuthNetwork,
}

impl Fetcher<WhitelistKey, WhitelistDoc> for AuthWhitelistFetcher {
    fn fetch<'a>(
        &'a self,
        key: &'a WhitelistKey,
    ) -> BoxFuture<'a, Result<Option<Fetched<WhitelistDoc>>, String>> {
        Box::pin(async move {
            let record_key = self
                .net
                .service_key(&key.service)
                .map_err(|e| e.to_string())?;
            let records = self
                .swarm
                .get_record_all(kad::RecordKey::new(&record_key))
                .await
                .map_err(|e| e.to_string())?;
            let entry = ServiceEntry {
                name: key.service.clone(),
                hash: key.hash.clone(),
                length: key.length,
            };
            select_whitelist(&records, &self.net, key, &entry).map(|selected| {
                selected.map(|(doc, size)| Fetched {
                    version: Some(doc.version),
                    value: doc,
                    size,
                })
            })
        })
    }
}

fn select_index(
    records: &[kad::PeerRecord],
    net: &AuthNetwork,
    now: u64,
) -> Result<Option<(IndexDoc, usize)>, String> {
    let mut best: Option<(IndexDoc, usize)> = None;
    for record in records {
        let Ok(doc) = auth::verify_index(net, &record.record.value, now) else {
            continue;
        };
        let size = record.record.value.len();
        match &best {
            None => best = Some((doc, size)),
            Some((current, _)) => {
                if doc.version > current.version {
                    best = Some((doc, size));
                } else if doc.version == current.version && doc != *current {
                    return Err("conflicting auth index content for the same version".into());
                }
            }
        }
    }
    Ok(best)
}

fn select_whitelist(
    records: &[kad::PeerRecord],
    net: &AuthNetwork,
    key: &WhitelistKey,
    entry: &ServiceEntry,
) -> Result<Option<(WhitelistDoc, usize)>, String> {
    let mut best: Option<(WhitelistDoc, usize)> = None;
    for record in records {
        let Ok(doc) = auth::verify_whitelist(net, &key.service, &record.record.value, entry) else {
            continue;
        };
        let size = record.record.value.len();
        match &best {
            None => best = Some((doc, size)),
            Some((current, _)) => {
                if doc.version > current.version {
                    best = Some((doc, size));
                } else if doc.version == current.version && doc != *current {
                    return Err("conflicting whitelist content for the same version".to_string());
                }
            }
        }
    }
    Ok(best)
}

struct AuthCaches {
    index: Arc<DhtCache<kad::RecordKey, IndexDoc, AuthIndexFetcher>>,
    whitelist: Arc<DhtCache<WhitelistKey, WhitelistDoc, AuthWhitelistFetcher>>,
}

fn build_caches(net: &AuthNetwork, swarm: &NetworkHandle) -> Arc<AuthCaches> {
    let index_fetcher = Arc::new(AuthIndexFetcher {
        swarm: swarm.clone(),
        net: net.clone(),
    });
    let whitelist_fetcher = Arc::new(AuthWhitelistFetcher {
        swarm: swarm.clone(),
        net: net.clone(),
    });
    Arc::new(AuthCaches {
        index: DhtCache::new(index_fetcher, 1024, 64 << 20),
        whitelist: DhtCache::new(whitelist_fetcher, 1024, 64 << 20),
    })
}

/// Verifies inbound service requests against the DHT-published trust records.
#[derive(Clone)]
pub struct AuthGate {
    config: ConfigHandle,
    swarm: NetworkHandle,
    state: Arc<RwLock<AuthState>>,
    caches: Arc<RwLock<Option<Arc<AuthCaches>>>>,
    versions: Arc<Mutex<AuthVersions>>,
}

impl AuthGate {
    pub fn new(config: ConfigHandle, swarm: NetworkHandle) -> Self {
        let state = resolve_state(&config);
        if let AuthState::Misconfigured(reason) = &state {
            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason.clone()).emit();
        }
        let caches = match &state {
            AuthState::Active(net) => Some(build_caches(net, &swarm)),
            _ => None,
        };
        Self {
            config,
            swarm,
            state: Arc::new(RwLock::new(state)),
            caches: Arc::new(RwLock::new(caches)),
            versions: Arc::new(Mutex::new(AuthVersions::default())),
        }
    }

    /// Recompute the state from config. Version history survives a reload
    /// unless the trust domain (network name or authority key) changed.
    pub fn reload(&self) {
        let next = resolve_state(&self.config);
        let misconfigured = match &next {
            AuthState::Misconfigured(reason) => Some(reason.clone()),
            _ => None,
        };
        let mut state = self.state.write().expect("auth state poisoned");
        let same_domain = same_trust_domain(&state, &next);
        if !same_domain {
            self.versions
                .lock()
                .expect("auth versions poisoned")
                .clear();
        }
        if let (AuthState::Active(net), true) = (&next, same_domain) {
            if self.caches.read().expect("auth caches poisoned").is_none() {
                *self.caches.write().expect("auth caches poisoned") =
                    Some(build_caches(net, &self.swarm));
            }
        } else {
            let caches = match &next {
                AuthState::Active(net) => Some(build_caches(net, &self.swarm)),
                _ => None,
            };
            *self.caches.write().expect("auth caches poisoned") = caches;
        }
        *state = next;
        drop(state);
        if let Some(reason) = misconfigured {
            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason).emit();
        }
    }

    /// Authorization: return the last verified record immediately and refresh
    /// it in the background. Expired documents are still rejected.
    pub async fn authorize(&self, peer: &PeerId, service: &str) -> Decision {
        let state = self.state.read().expect("auth state poisoned").clone();
        let net = match &state {
            AuthState::Disabled => return Decision::Allow,
            AuthState::Misconfigured(reason) => return Decision::Deny(reason.clone()),
            AuthState::Active(net) => net.clone(),
        };
        if !self.config.service_requires_auth(service) {
            return Decision::Allow;
        }
        let Some(caches) = self
            .caches
            .read()
            .expect("auth caches poisoned")
            .as_ref()
            .cloned()
        else {
            return Decision::Deny("auth caches unavailable".to_string());
        };

        let index_key = kad::RecordKey::new(&net.index_key());
        let index = match caches.index.get(index_key.clone()).await {
            Ok(read) => read,
            Err(e) => return Decision::Deny(format!("auth index query failed: {e}")),
        };
        let Some(index) = index.value else {
            return Decision::Deny("auth index unavailable".to_string());
        };
        if auth::now_unix() > index.expires_at {
            return Decision::Deny("auth index expired".to_string());
        }
        if self
            .versions
            .lock()
            .expect("auth versions poisoned")
            .check_index(index.version)
            .is_err()
        {
            caches.index.invalidate(&index_key);
            return Decision::Deny("auth index rollback".to_string());
        }
        let Some(entry) = index.services.iter().find(|e| e.name == service).cloned() else {
            return Decision::AllowUnprotected;
        };

        let key = WhitelistKey {
            service: service.to_string(),
            hash: entry.hash.clone(),
            length: entry.length,
            index_expires_at: index.expires_at,
        };
        let whitelist = match caches.whitelist.get(key.clone()).await {
            Ok(read) => read,
            Err(e) => return Decision::Deny(format!("whitelist query failed: {e}")),
        };
        let Some(whitelist) = whitelist.value else {
            return Decision::Deny("whitelist unavailable".to_string());
        };
        if self
            .versions
            .lock()
            .expect("auth versions poisoned")
            .check_whitelist(service, whitelist.version)
            .is_err()
        {
            caches.whitelist.invalidate(&key);
            return Decision::Deny("whitelist rollback".to_string());
        }
        let target = peer.to_string();
        if whitelist.members.iter().any(|m| m == &target) {
            Decision::Allow
        } else {
            Decision::Deny("peer not authorized".to_string())
        }
    }

    /// Drop all cached trust records.
    pub fn invalidate_all(&self) {
        if let Some(caches) = self
            .caches
            .read()
            .expect("auth caches poisoned")
            .as_ref()
            .cloned()
        {
            caches.index.invalidate_all();
            caches.whitelist.invalidate_all();
        }
    }

    /// `(state, detail)` for the `auth_status` report.
    pub fn state_label(&self) -> (&'static str, Option<String>) {
        let state = self.state.read().expect("auth state poisoned");
        match &*state {
            AuthState::Disabled => ("disabled", None),
            AuthState::Active(_) => ("active", None),
            AuthState::Misconfigured(r) => ("misconfigured", Some(r.clone())),
        }
    }

    pub fn index_version(&self) -> Option<u64> {
        self.versions
            .lock()
            .expect("auth versions poisoned")
            .index_version()
    }

    pub fn whitelist_version(&self, service: &str) -> Option<u64> {
        self.versions
            .lock()
            .expect("auth versions poisoned")
            .whitelist_version(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use libp2p::identity;

    fn net(name: &str, keypair: &identity::Keypair) -> AuthNetwork {
        let public = keypair.public().try_into_ed25519().unwrap().to_bytes();
        AuthNetwork::new(name, &STANDARD.encode(public)).unwrap()
    }

    #[test]
    fn same_network_and_authority_keeps_versions() {
        let keypair = identity::Keypair::generate_ed25519();
        let network = net("myorg", &keypair);
        assert!(same_trust_domain(
            &AuthState::Active(network.clone()),
            &AuthState::Active(network)
        ));
    }

    #[test]
    fn changed_network_or_authority_resets_versions() {
        let keypair = identity::Keypair::generate_ed25519();
        let other_keypair = identity::Keypair::generate_ed25519();
        assert!(!same_trust_domain(
            &AuthState::Active(net("myorg", &keypair)),
            &AuthState::Active(net("other", &keypair))
        ));
        assert!(!same_trust_domain(
            &AuthState::Active(net("myorg", &keypair)),
            &AuthState::Active(net("myorg", &other_keypair))
        ));
    }

    #[test]
    fn disabled_state_is_stable() {
        assert!(same_trust_domain(
            &AuthState::Disabled,
            &AuthState::Disabled
        ));
        assert!(!same_trust_domain(
            &AuthState::Disabled,
            &AuthState::Misconfigured("missing".to_string())
        ));
    }

    fn record(key: &str, value: Vec<u8>) -> kad::PeerRecord {
        kad::PeerRecord {
            peer: None,
            record: kad::Record::new(kad::RecordKey::new(&key.to_string()), value),
        }
    }

    #[test]
    fn select_index_uses_highest_version() {
        let keypair = identity::Keypair::generate_ed25519();
        let net = net("myorg", &keypair);
        let now = auth::now_unix();
        let v1 = auth::sign_index(
            &keypair,
            &net,
            &IndexDoc {
                version: 1,
                expires_at: now + 60,
                services: vec![],
            },
        )
        .unwrap();
        let v2 = auth::sign_index(
            &keypair,
            &net,
            &IndexDoc {
                version: 2,
                expires_at: now + 60,
                services: vec![],
            },
        )
        .unwrap();
        let key = net.index_key();
        let selected = select_index(&[record(&key, v1), record(&key, v2)], &net, now)
            .unwrap()
            .unwrap();
        assert_eq!(selected.0.version, 2);
    }

    #[test]
    fn select_index_rejects_same_version_conflict() {
        let keypair = identity::Keypair::generate_ed25519();
        let net = net("myorg", &keypair);
        let now = auth::now_unix();
        let a = auth::sign_index(
            &keypair,
            &net,
            &IndexDoc {
                version: 2,
                expires_at: now + 60,
                services: vec![],
            },
        )
        .unwrap();
        let b = auth::sign_index(
            &keypair,
            &net,
            &IndexDoc {
                version: 2,
                expires_at: now + 60,
                services: vec![ServiceEntry {
                    name: "cmd".into(),
                    hash: vec![0; 32],
                    length: 1,
                }],
            },
        )
        .unwrap();
        let key = net.index_key();
        assert!(select_index(&[record(&key, a), record(&key, b)], &net, now).is_err());
    }

    #[test]
    fn auth_gate_is_clone_send_sync() {
        fn assert_traits<T: Clone + Send + Sync>() {}
        assert_traits::<AuthGate>();
    }
}
