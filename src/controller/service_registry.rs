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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use libp2p::kad;

use crate::auth;
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;
use crate::service_dispatcher::BackendStatus;

/// Grace period before withdrawing a service announcement after a backend drops.
const WITHDRAW_GRACE: Duration = Duration::from_secs(3);

/// Manages the DHT lifecycle of the node's local service announcements.
///
/// Renewal of the provider/type records is handled by libp2p's periodic
/// publications; this component only seeds them and withdraws on backend loss.
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
    }

    /// Re-run the announcement pass (e.g. after `reannounce_services`).
    pub async fn reannounce(&mut self) {
        self.announce().await;
    }

    /// Process due withdrawals.
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
    }

    /// Reset DHT-related tracking after a Swarm rebuild.
    pub fn reset(&mut self) {
        self.bootstrapped = false;
        self.announced.clear();
        self.withdraw_deadline.clear();
    }

    /// Announce local services whose backend is ready and which are not yet
    /// announced, and sync the global service-type record.
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
            self.update_service_types().await;
        }
    }

    /// Withdraw a single service's DHT announcement. Does not touch the global
    /// `/oahd/service/types` record.
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

    /// Merge announced service types and write them back to the global DHT record
    /// `/oahd/service/types`.
    async fn update_service_types(&mut self) {
        let types_key = kad::RecordKey::new(b"/oahd/service/types");
        let existing_types = match self.swarm.get_record(types_key.clone()).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                serde_json::from_slice::<Vec<String>>(&peer_record.record.value).unwrap_or_default()
            }
            _ => Vec::new(),
        };

        let mut all_types = existing_types;
        for t in &self.announced {
            if !all_types.contains(t) {
                all_types.push(t.clone());
            }
        }

        if let Ok(types_json) = serde_json::to_vec(&all_types)
            && let Err(e) = self.swarm.put_record(types_key, types_json).await
        {
            LogStruct::new(LogLevel::Warning, "更新服务类型列表失败", e.to_string()).emit();
        }
    }
}
