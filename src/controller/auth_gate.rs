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

use libp2p::{PeerId, kad};

use crate::auth::{self, AuthNetwork, AuthVersions};
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;

/// Node authentication runtime state.
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

/// Resolve the authentication state from config.
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

/// Verifies inbound service requests against the DHT-published trust records.
pub struct AuthGate {
    config: ConfigHandle,
    swarm: NetworkHandle,
    state: AuthState,
    versions: AuthVersions,
}

impl AuthGate {
    pub fn new(config: ConfigHandle, swarm: NetworkHandle) -> Self {
        let state = resolve_state(&config);
        if let AuthState::Misconfigured(reason) = &state {
            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason.clone()).emit();
        }
        Self {
            config,
            swarm,
            state,
            versions: AuthVersions::default(),
        }
    }

    /// Recompute the state from config. Version history survives a reload
    /// unless the trust domain (network name or authority key) changed.
    pub fn reload(&mut self) {
        let next = resolve_state(&self.config);
        if !same_trust_domain(&self.state, &next) {
            self.versions.clear();
        }
        self.state = next;
        if let AuthState::Misconfigured(reason) = &self.state {
            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason.clone()).emit();
        }
    }

    /// Authorization: fetch records from the DHT on demand and verify signatures
    /// (raw bytes are cached by the record cache).
    ///
    /// Blocking and inline: first/expired fetches wait on a DHT query.
    pub async fn authorize(&mut self, peer: &PeerId, service: &str) -> Decision {
        let net = match &self.state {
            AuthState::Disabled => return Decision::Allow,
            AuthState::Misconfigured(reason) => return Decision::Deny(reason.clone()),
            AuthState::Active(net) => net.clone(),
        };
        if !self.config.service_requires_auth(service) {
            return Decision::Allow;
        }

        // Index: fetch raw bytes -> verify -> rollback check
        let index_key = kad::RecordKey::new(&net.index_key());
        let index = match self.swarm.get_record(index_key).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                let now = auth::now_unix();
                match auth::verify_index(&net, &peer_record.record.value, now) {
                    Ok(doc) => doc,
                    Err(e) => return Decision::Deny(format!("index verify failed: {e}")),
                }
            }
            Ok(_) => return Decision::Deny("auth index unavailable".to_string()),
            Err(e) => return Decision::Deny(format!("auth index query failed: {e}")),
        };
        if self.versions.check_index(index.version).is_err() {
            return Decision::Deny("auth index rollback".to_string());
        }
        let Some(entry) = index.services.iter().find(|e| e.name == service).cloned() else {
            return Decision::AllowUnprotected;
        };

        // Whitelist: fetch raw bytes -> verify (bound to the index hash/length)
        // -> rollback check
        let service_key = match net.service_key(service) {
            Ok(key) => kad::RecordKey::new(&key),
            Err(_) => return Decision::Deny("invalid service name".to_string()),
        };
        let whitelist = match self.swarm.get_record(service_key).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                match auth::verify_whitelist(&net, service, &peer_record.record.value, &entry) {
                    Ok(doc) => doc,
                    Err(e) => return Decision::Deny(format!("whitelist verify failed: {e}")),
                }
            }
            Ok(_) => return Decision::Deny("whitelist unavailable".to_string()),
            Err(e) => return Decision::Deny(format!("whitelist query failed: {e}")),
        };
        if self
            .versions
            .check_whitelist(service, whitelist.version)
            .is_err()
        {
            return Decision::Deny("whitelist rollback".to_string());
        }
        let target = peer.to_string();
        if whitelist.members.iter().any(|m| m == &target) {
            Decision::Allow
        } else {
            Decision::Deny("peer not authorized".to_string())
        }
    }

    /// `(state, detail)` for the `auth_status` report.
    pub fn state_label(&self) -> (&'static str, Option<String>) {
        match &self.state {
            AuthState::Disabled => ("disabled", None),
            AuthState::Active(_) => ("active", None),
            AuthState::Misconfigured(r) => ("misconfigured", Some(r.clone())),
        }
    }

    pub fn index_version(&self) -> Option<u64> {
        self.versions.index_version()
    }

    pub fn whitelist_version(&self, service: &str) -> Option<u64> {
        self.versions.whitelist_version(service)
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
}
