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

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, relay};

use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;

/// Target number of relay reservations.
pub const RELAY_TARGET: usize = 3;

/// Consumer-side relay reservation state machine.
pub struct RelayManager {
    config: ConfigHandle,
    swarm: NetworkHandle,
    my_peer_id: PeerId,
    /// Relays confirmed usable.
    active: HashSet<PeerId>,
    /// Reservations started and awaiting confirmation; value is the start time.
    pending: HashMap<PeerId, Instant>,
    /// Consecutive relay failure counts.
    failures: HashMap<PeerId, u32>,
    /// Relay backoff deadlines.
    backoff: HashMap<PeerId, Instant>,
}

impl RelayManager {
    pub fn new(config: ConfigHandle, swarm: NetworkHandle, my_peer_id: PeerId) -> Self {
        Self {
            config,
            swarm,
            my_peer_id,
            active: HashSet::new(),
            pending: HashMap::new(),
            failures: HashMap::new(),
            backoff: HashMap::new(),
        }
    }

    pub fn reset(&mut self) {
        self.active.clear();
        self.pending.clear();
        self.failures.clear();
        self.backoff.clear();
    }

    /// Whether this node needs relay reservations (not dual-stack).
    pub fn need_relay(&self) -> bool {
        let cfg = self.config.read();
        !(cfg.network.ipv4_enabled && cfg.network.ipv6_enabled)
    }

    pub fn active(&self) -> Vec<String> {
        self.active.iter().map(|p| p.to_string()).collect()
    }

    pub fn pending(&self) -> Vec<String> {
        self.pending.keys().map(|p| p.to_string()).collect()
    }

    /// Relay server (inbound reservation) events: informational only.
    pub fn on_relay_event(&self, event: relay::Event) {
        match event {
            relay::Event::ReservationReqAccepted { src_peer_id, .. } => {
                LogStruct::new(
                    LogLevel::Debug,
                    "中继预约已接受",
                    format!("对端: {}", src_peer_id),
                )
                .emit();
            }
            relay::Event::ReservationReqDenied {
                src_peer_id,
                status,
                ..
            } => {
                LogStruct::new(
                    LogLevel::Debug,
                    "中继预约被拒绝",
                    format!("对端: {}, 状态: {:?}", src_peer_id, status),
                )
                .emit();
            }
            relay::Event::CircuitReqAccepted {
                src_peer_id,
                dst_peer_id,
                ..
            } => {
                LogStruct::new(
                    LogLevel::Debug,
                    "中继电路已建立",
                    format!("来源: {}, 目标: {}", src_peer_id, dst_peer_id),
                )
                .emit();
            }
            _ => {}
        }
    }

    /// Relay client (outbound) events: track reservation successes.
    pub fn on_relay_client_event(&mut self, event: relay::client::Event) {
        match event {
            relay::client::Event::ReservationReqAccepted {
                relay_peer_id,
                renewal,
                ..
            } => {
                self.pending.remove(&relay_peer_id);
                self.active.insert(relay_peer_id);
                self.failures.remove(&relay_peer_id);
                self.backoff.remove(&relay_peer_id);
                LogStruct::new(
                    LogLevel::Preset,
                    if renewal {
                        "中继预约续期"
                    } else {
                        "中继预约成功"
                    },
                    format!("{}/{}: {}", self.active.len(), RELAY_TARGET, relay_peer_id),
                )
                .emit();
            }
            relay::client::Event::OutboundCircuitEstablished { relay_peer_id, .. } => {
                LogStruct::new(
                    LogLevel::Debug,
                    "出站中继电路已建立",
                    format!("中继节点: {}", relay_peer_id),
                )
                .emit();
            }
            relay::client::Event::InboundCircuitEstablished { src_peer_id, .. } => {
                LogStruct::new(
                    LogLevel::Debug,
                    "入站中继电路已建立",
                    format!("来源: {}", src_peer_id),
                )
                .emit();
            }
        }
    }

    /// Handle a closed relay listener. Returns whether a tracked relay was lost.
    pub async fn on_listener_closed(&mut self, addresses: Vec<Multiaddr>) -> bool {
        let mut lost = false;
        for addr in addresses {
            if !addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                continue;
            }
            let Some(peer_id) = extract_peer_id_from_multiaddr(&addr) else {
                continue;
            };
            let was_active = self.active.remove(&peer_id);
            let was_pending = self.pending.remove(&peer_id).is_some();
            if !was_active && !was_pending {
                // Not a tracked reservation; ignore.
                continue;
            }
            lost = true;
            if was_active {
                LogStruct::new(LogLevel::Warning, "中继连接断开", peer_id.to_string()).emit();
            }
            self.record_failure(peer_id, Instant::now());
        }
        lost
    }

    /// Consumer-side relay top-up: best-effort reach the target number of usable
    /// relays.
    pub async fn tick(&mut self) {
        if !self.need_relay() {
            return;
        }
        let target = RELAY_TARGET;
        if target == 0 {
            return;
        }

        let now = Instant::now();
        self.expire_pending(now);

        // Deduplicated candidates: peer_id -> first address.
        let mut candidates: Vec<(PeerId, Multiaddr)> = Vec::new();
        let mut seen = HashSet::new();
        for addr in self.config.bootstrap_nodes() {
            if let Some(peer_id) = extract_peer_id_from_multiaddr(&addr) {
                if peer_id == self.my_peer_id || !seen.insert(peer_id) {
                    continue;
                }
                candidates.push((peer_id, addr));
            }
        }

        let desired = target.min(candidates.len());
        for (peer_id, addr) in candidates {
            if self.active.len() + self.pending.len() >= desired {
                break;
            }
            if self.active.contains(&peer_id) || self.pending.contains_key(&peer_id) {
                continue;
            }
            if self.backoff.get(&peer_id).is_some_and(|until| *until > now) {
                continue;
            }

            let mut relay_listen_addr = addr.clone();
            relay_listen_addr.push(Protocol::P2pCircuit);
            match self.swarm.listen_on(relay_listen_addr).await {
                Ok(_) => {
                    self.pending.insert(peer_id, now);
                }
                Err(e) => {
                    LogStruct::new(
                        LogLevel::Warning,
                        "中继预约请求失败",
                        format!("{}: {}", peer_id, e),
                    )
                    .emit();
                    self.record_failure(peer_id, now);
                }
            }
        }
    }

    /// Reservations unconfirmed for too long are treated as failed.
    fn expire_pending(&mut self, now: Instant) {
        let timeout = Duration::from_secs(
            (self.config.relay_retry_interval() as u64)
                .saturating_mul(3)
                .max(180),
        );
        let expired: Vec<PeerId> = self
            .pending
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= timeout)
            .map(|(peer, _)| *peer)
            .collect();
        for peer_id in expired {
            self.pending.remove(&peer_id);
            self.record_failure(peer_id, now);
        }
    }

    /// Record a relay failure and set exponential backoff. Bootstrap
    /// configuration is never modified; a failing relay is only retried later.
    fn record_failure(&mut self, peer_id: PeerId, now: Instant) {
        let count = {
            let c = self.failures.entry(peer_id).or_insert(0);
            *c += 1;
            *c
        };

        let max = self.config.relay_max_failures();
        let delay = if max > 0 && count >= max {
            Duration::from_secs(3600)
        } else {
            Duration::from_secs(relay_backoff_secs(count))
        };
        self.backoff.insert(peer_id, now + delay);
    }
}

pub(crate) fn extract_peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    for proto in addr.iter() {
        if let Protocol::P2p(id) = proto {
            return Some(id);
        }
    }
    None
}

/// Consecutive failures -> backoff seconds.
fn relay_backoff_secs(count: u32) -> u64 {
    let shift = count.saturating_sub(1).min(5);
    (30u64 << shift).min(600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(relay_backoff_secs(1), 30);
        assert_eq!(relay_backoff_secs(2), 60);
        assert_eq!(relay_backoff_secs(10), 600);
    }
}
