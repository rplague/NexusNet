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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use libp2p::kad;

use crate::auth;
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;
use crate::service_dispatcher::BackendStatus;

/// Grace period before withdrawing a service announcement after a backend drops.
const WITHDRAW_GRACE: Duration = Duration::from_secs(3);

/// Key of the global, append-only service-type index.
const TYPES_KEY: &[u8] = b"/oahd/service/types";

/// Upper bound on the locally tracked service-type union.
const MAX_TYPES: usize = 4096;

fn types_key() -> kad::RecordKey {
    kad::RecordKey::new(&TYPES_KEY)
}

fn merge_type_set(known: &mut BTreeSet<String>, incoming: impl IntoIterator<Item = String>) {
    for name in incoming {
        if known.len() >= MAX_TYPES {
            break;
        }
        if auth::is_valid_service_name(&name) {
            known.insert(name);
        }
    }
}

/// Manages the DHT lifecycle of the node's local service announcements and the
/// global service-type index.
///
/// Renewal of provider records is handled by libp2p's periodic publications;
/// this component only seeds them and withdraws on backend loss.
///
/// The type index (`/oahd/service/types`) is a last-writer-wins DHT record with
/// no compare-and-swap, so we treat it as a best-effort replicated set and make
/// it converge by keeping a **monotonic** local union (`known_types`), reading
/// it back from the DHT peers (bypassing the local store copy, which would
/// otherwise shadow remote updates), and periodically re-publishing the union.
/// Because the union never shrinks, a node can never drop a type it has already
/// observed, even if a concurrent writer temporarily clobbers the record.
pub struct ServiceRegistry {
    config: ConfigHandle,
    swarm: NetworkHandle,
    /// Whether bootstrap has completed in the current Swarm lifetime.
    bootstrapped: bool,
    /// Local service names whose backend handshake succeeded.
    backend_ready: HashSet<String>,
    /// Service names currently announced to the DHT.
    announced: HashSet<String>,
    /// Deadlines awaiting withdrawal after a backend drops.
    withdraw_deadline: HashMap<String, Instant>,
    /// Monotonic union of every service type seen locally or read from the DHT.
    known_types: BTreeSet<String>,
    /// Next scheduled reconciliation with the DHT.
    next_types_sync: Option<Instant>,
}

impl ServiceRegistry {
    pub fn new(config: ConfigHandle, swarm: NetworkHandle) -> Self {
        Self {
            config,
            swarm,
            bootstrapped: false,
            backend_ready: HashSet::new(),
            announced: HashSet::new(),
            withdraw_deadline: HashMap::new(),
            known_types: BTreeSet::new(),
            next_types_sync: None,
        }
    }

    /// Handle a backend readiness change.
    pub async fn on_backend_status(&mut self, status: BackendStatus) {
        if status.ready {
            self.backend_ready.insert(status.service.clone());
            self.withdraw_deadline.remove(&status.service);
            if self.bootstrapped {
                self.announce().await;
            }
        } else {
            self.backend_ready.remove(&status.service);
            // An announced service that drops enters the grace period; if it
            // reconnects within 3s it is not withdrawn.
            if self.announced.contains(&status.service) {
                self.withdraw_deadline
                    .entry(status.service)
                    .or_insert_with(|| Instant::now() + WITHDRAW_GRACE);
            }
        }
    }

    /// Mark bootstrap complete and announce ready services.
    pub async fn on_bootstrap_completed(&mut self) {
        self.bootstrapped = true;
        self.announce().await;
        self.reconcile_types().await;
    }

    /// Re-run the announcement pass (e.g. after `reannounce_services`).
    pub async fn reannounce(&mut self) {
        self.announce().await;
        self.reconcile_types().await;
    }

    /// Process due withdrawals and the periodic type-index reconciliation.
    pub async fn tick(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .withdraw_deadline
            .iter()
            .filter(|(svc, dl)| **dl <= now && !self.backend_ready.contains(*svc))
            .map(|(svc, _)| svc.clone())
            .collect();
        for service in due {
            self.withdraw_deadline.remove(&service);
            self.withdraw_service(&service).await;
        }

        if self.next_types_sync.is_some_and(|deadline| now >= deadline) {
            self.reconcile_types().await;
        }
    }

    /// Reset DHT-related tracking after a Swarm rebuild.
    pub fn reset(&mut self) {
        self.bootstrapped = false;
        self.announced.clear();
        self.withdraw_deadline.clear();
        // Keep the monotonic union so known types survive the reload; the
        // periodic reconciliation will re-publish it to the rebuilt DHT.
        self.next_types_sync = None;
    }

    /// Return the current global service-type list, merging in a fresh read
    /// from the DHT peers. Used to serve `list_services`.
    pub async fn list_types(&mut self) -> Vec<String> {
        let network_types = self.read_types_from_network().await;
        merge_type_set(&mut self.known_types, network_types);
        let announced: Vec<String> = self.announced.iter().cloned().collect();
        merge_type_set(&mut self.known_types, announced);
        self.known_types.iter().cloned().collect()
    }

    /// Announce local services whose backend is ready and which are not yet
    /// announced, then sync the global service-type index.
    async fn announce(&mut self) {
        let targets: Vec<String> = self
            .config
            .read()
            .services
            .dispatcher
            .local_services
            .iter()
            .filter(|s| auth::is_valid_service_name(&s.name))
            .filter(|s| self.backend_ready.contains(&s.name))
            .filter(|s| !self.announced.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        if targets.is_empty() {
            return;
        }

        let mut newly = Vec::new();
        for name in &targets {
            let key = kad::RecordKey::new(&format!("/oahd/service/{}", name));
            if let Err(e) = self.swarm.start_providing(key).await {
                LogStruct::new(
                    LogLevel::Warning,
                    "注册服务失败",
                    format!("{}: {}", name, e),
                )
                .emit();
                continue;
            }
            self.announced.insert(name.clone());
            newly.push(name.clone());
            LogStruct::new(LogLevel::Preset, format!("已注册服务: {}", name), "").emit();
        }

        if !newly.is_empty() {
            self.reconcile_types().await;
        }
    }

    /// Withdraw a single service's DHT announcement. Does not touch the global
    /// `/oahd/service/types` record (the type index is append-only).
    async fn withdraw_service(&mut self, service: &str) {
        let key = format!("/oahd/service/{}", service);
        match self.swarm.stop_providing(kad::RecordKey::new(&key)).await {
            Ok(()) => {
                self.announced.remove(service);
                LogStruct::new(LogLevel::Preset, format!("已撤销服务宣告: {}", service), "").emit();
            }
            Err(e) => {
                LogStruct::new(
                    LogLevel::Warning,
                    "撤销服务宣告失败",
                    format!("{}: {}", service, e),
                )
                .emit();
            }
        }
    }

    /// Merge the DHT type index with the local union and re-publish it.
    async fn reconcile_types(&mut self) {
        let interval = Duration::from_secs(self.config.dispatcher_types_sync_interval() as u64);
        self.next_types_sync = Some(Instant::now() + interval);

        let network_types = self.read_types_from_network().await;
        let network_set: BTreeSet<String> = network_types.iter().cloned().collect();
        merge_type_set(&mut self.known_types, network_types);
        let announced: Vec<String> = self.announced.iter().cloned().collect();
        merge_type_set(&mut self.known_types, announced);

        if self.known_types.is_empty() {
            // Never write an empty index: it would clobber types published by
            // other nodes.
            return;
        }
        // Skip the write only when the DHT already reflects our union. If a peer
        // clobbered the record with a smaller set, our union differs and we
        // re-assert the superset.
        if self.known_types == network_set {
            return;
        }

        let list: Vec<String> = self.known_types.iter().cloned().collect();
        let Ok(value) = serde_json::to_vec(&list) else {
            return;
        };
        if let Err(e) = self.swarm.put_record(types_key(), value).await {
            LogStruct::new(LogLevel::Warning, "更新服务类型列表失败", e.to_string()).emit();
        }
    }

    /// Read the type index from the DHT peers, ignoring the local store copy so
    /// remote additions are never shadowed.
    async fn read_types_from_network(&self) -> Vec<String> {
        match self.swarm.get_record_network(types_key()).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                match serde_json::from_slice::<Vec<String>>(&peer_record.record.value) {
                    Ok(types) => types
                        .into_iter()
                        .filter(|name| auth::is_valid_service_name(name))
                        .take(MAX_TYPES)
                        .collect(),
                    Err(e) => {
                        LogStruct::new(LogLevel::Warning, "服务类型索引解析失败", e.to_string())
                            .emit();
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The union must be monotonic and de-duplicated.
    #[test]
    fn known_types_union_is_monotonic() {
        let mut known: BTreeSet<String> = BTreeSet::new();
        known.extend(["a".to_string()]);
        known.extend(["a".to_string(), "b".to_string()]);
        known.extend(["c".to_string()]);
        assert_eq!(
            known.iter().cloned().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn merge_type_set_filters_invalid_names() {
        let mut known = BTreeSet::new();
        merge_type_set(
            &mut known,
            vec![
                "cmd".to_string(),
                "bad/name".to_string(),
                "service".to_string(),
            ],
        );
        assert_eq!(known, BTreeSet::from(["cmd".to_string()]));
    }

    #[test]
    fn merge_type_set_caps_the_union() {
        let mut known = BTreeSet::new();
        let incoming = (0..MAX_TYPES + 10).map(|i| format!("svc{i}"));
        merge_type_set(&mut known, incoming);
        assert_eq!(known.len(), MAX_TYPES);
    }
}
