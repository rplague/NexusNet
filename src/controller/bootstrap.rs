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

use std::sync::Arc;

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, identify, kad};

use crate::config::ConfigHandle;
use crate::dht_cache::DhtCache;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;

use super::dht_fetchers::ProviderFetcher;

/// DHT provider key: announce/discover bootstrap nodes.
const BOOTSTRAP_PROVIDER_KEY: &[u8] = b"/oahd/bootstrap";

/// Discovers dual-stack bootstrap providers via Identify, and announces this
/// node as one when eligible.
pub struct BootstrapDiscovery {
    config: ConfigHandle,
    swarm: NetworkHandle,
    provider_cache: Arc<DhtCache<kad::RecordKey, Vec<PeerId>, ProviderFetcher>>,
    is_bootstrap_provider: bool,
}

impl BootstrapDiscovery {
    pub fn new(
        config: ConfigHandle,
        swarm: NetworkHandle,
        provider_cache: Arc<DhtCache<kad::RecordKey, Vec<PeerId>, ProviderFetcher>>,
    ) -> Self {
        Self {
            config,
            swarm,
            provider_cache,
            is_bootstrap_provider: false,
        }
    }

    pub fn reset(&mut self) {
        self.is_bootstrap_provider = false;
    }

    /// Publish this node as a bootstrap provider when it is dual-stack and
    /// bootstrap is allowed.
    pub async fn announce_self(&mut self) {
        if self.is_bootstrap_provider || !self.config.allow_bootstrap() {
            return;
        }
        let dual_stack = {
            let cfg = self.config.read();
            cfg.network.ipv4_enabled && cfg.network.ipv6_enabled
        };
        if !dual_stack {
            return;
        }

        let key = kad::RecordKey::new(&BOOTSTRAP_PROVIDER_KEY);
        match self.swarm.start_providing(key).await {
            Ok(_) => {
                self.is_bootstrap_provider = true;
                LogStruct::new(LogLevel::Preset, "已声明为 bootstrap 节点", "").emit();
            }
            Err(e) => {
                LogStruct::new(LogLevel::Warning, "声明 bootstrap 失败", e.to_string()).emit();
            }
        }
    }

    /// On an Identify hit, decide (dual-stack first, then bootstrap) whether to
    /// add the peer to the local bootstrap list.
    ///
    /// Addresses always come from Identify info (no DHT address storage); bootstrap
    /// provider status is queried on demand.
    pub async fn on_identify(&self, info: &identify::Info, peer_id: PeerId) {
        if !self.config.accept_bootstrap_providers() {
            return;
        }
        if !is_dual_stack_addrs(&info.listen_addrs) {
            return;
        }
        let key = kad::RecordKey::new(&BOOTSTRAP_PROVIDER_KEY);
        // Never block the controller on a DHT lookup. A stale or missing
        // cache entry triggers one background refresh.
        self.provider_cache.refresh_if_due(key.clone());
        let providers = match self.provider_cache.peek(&key) {
            Some(read) => read.value.unwrap_or_default(),
            None => return,
        };
        if !providers.contains(&peer_id) {
            return;
        }
        let candidates = bootstrap_addrs_from(&info.listen_addrs, peer_id);
        if candidates.is_empty() {
            return;
        }

        let mut nodes = self.config.bootstrap_nodes();
        let mut changed = false;
        for addr in candidates {
            if !nodes.contains(&addr) {
                nodes.push(addr);
                changed = true;
            }
        }
        if changed {
            self.config.set_bootstrap_nodes(nodes);
            self.config.save_to_default();
            LogStruct::new(
                LogLevel::Preset,
                "配置更新",
                format!("添加 bootstrap 节点: {}", peer_id),
            )
            .emit();
        }
    }
}

/// Whether the given addresses span both IP families (dual-stack).
fn is_dual_stack_addrs(addrs: &[Multiaddr]) -> bool {
    let mut v4 = false;
    let mut v6 = false;
    for addr in addrs {
        for proto in addr.iter() {
            match proto {
                Protocol::Ip4(_) => v4 = true,
                Protocol::Ip6(_) => v6 = true,
                _ => {}
            }
        }
    }
    v4 && v6
}

/// Take one listen address per IP family from Identify info and append
/// `/p2p/<peer_id>`.
fn bootstrap_addrs_from(addrs: &[Multiaddr], peer_id: PeerId) -> Vec<Multiaddr> {
    let mut v4: Option<Multiaddr> = None;
    let mut v6: Option<Multiaddr> = None;
    for addr in addrs {
        let has_p2p = addr.iter().any(|p| matches!(p, Protocol::P2p(_)));
        let full = if has_p2p {
            addr.clone()
        } else {
            let mut a = addr.clone();
            a.push(Protocol::P2p(peer_id));
            a
        };
        let is_v4 = addr.iter().any(|p| matches!(p, Protocol::Ip4(_)));
        let is_v6 = addr.iter().any(|p| matches!(p, Protocol::Ip6(_)));
        if is_v4 && v4.is_none() {
            v4 = Some(full);
        } else if is_v6 && v6.is_none() {
            v6 = Some(full);
        }
    }
    [v4, v6].into_iter().flatten().collect()
}
