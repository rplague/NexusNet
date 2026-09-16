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

//! libp2p 行为聚合：把 ping / identify / kademlia / request-response / relay
//! 组合成单一的 `NetBehaviour`，并把手写事件枚举作为 `out_event`。
//!
//! 配置在构造时读取一次并固化进各行为实例。libp2p 各行为没有运行时 setter。

use crate::config::ConfigHandle;
use crate::service_protocol;
use libp2p::request_response::{self, cbor};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::{StreamProtocol, identify, identity, kad, ping, relay, swarm::NetworkBehaviour};
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::{LogLevel, LogStruct};

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NetBehaviourEvent")]
pub struct NetBehaviour {
    pub ping: Toggle<ping::Behaviour>,
    pub identify: identify::Behaviour,
    pub kademlia: Toggle<kad::Behaviour<kad::store::MemoryStore>>,
    pub service_req: cbor::Behaviour<service_protocol::Request, service_protocol::Response>,
    pub relay_server: relay::Behaviour,
    pub relay_client: relay::client::Behaviour,
}

/// 复合行为对外发出的事件。
///
/// 每个变体承载对应子行为的原始事件；`Toggle<T>` 的 `ToSwarm` 与 `T` 相同，
/// 因此 `Ping`/`Kademlia` 变体仍使用 `ping::Event` / `kad::Event`。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum NetBehaviourEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kademlia(kad::Event),
    ServiceReq(request_response::Event<service_protocol::Request, service_protocol::Response>),
    Relay(relay::Event),
    RelayClient(relay::client::Event),
}

impl From<ping::Event> for NetBehaviourEvent {
    fn from(event: ping::Event) -> Self {
        NetBehaviourEvent::Ping(event)
    }
}
impl From<identify::Event> for NetBehaviourEvent {
    fn from(event: identify::Event) -> Self {
        NetBehaviourEvent::Identify(event)
    }
}
impl From<kad::Event> for NetBehaviourEvent {
    fn from(event: kad::Event) -> Self {
        NetBehaviourEvent::Kademlia(event)
    }
}
impl From<request_response::Event<service_protocol::Request, service_protocol::Response>>
    for NetBehaviourEvent
{
    fn from(
        event: request_response::Event<service_protocol::Request, service_protocol::Response>,
    ) -> Self {
        NetBehaviourEvent::ServiceReq(event)
    }
}
impl From<relay::Event> for NetBehaviourEvent {
    fn from(event: relay::Event) -> Self {
        NetBehaviourEvent::Relay(event)
    }
}
impl From<relay::client::Event> for NetBehaviourEvent {
    fn from(event: relay::client::Event) -> Self {
        NetBehaviourEvent::RelayClient(event)
    }
}

impl NetBehaviour {
    pub fn new(
        config: &ConfigHandle,
        keypair: &identity::Keypair,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        let peer_id = keypair.public().to_peer_id();

        // Ping —— enabled=false 时用 Toggle 彻底关闭
        let ping_config = ping::Config::new()
            .with_interval(Duration::from_secs(config.ping_interval().into()))
            .with_timeout(Duration::from_secs(config.ping_timeout().into()));
        let ping: Toggle<ping::Behaviour> = if config.ping_enabled() {
            Toggle::from(Some(ping::Behaviour::new(ping_config)))
        } else {
            LogStruct::new(
                LogLevel::Preset,
                "Ping 已禁用",
                "services.ping.enabled = false",
            )
            .emit();
            Toggle::from(None)
        };

        // Identify 配置
        let protocol_name = "/oahd";
        let identify_config = identify::Config::new(
            format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")),
            keypair.public(),
        )
        .with_agent_version(format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")))
        .with_push_listen_addr_updates(true);
        let identify = identify::Behaviour::new(identify_config);

        // Kademlia 配置
        let store = kad::store::MemoryStore::new(peer_id);
        let mut kad_config = kad::Config::new(StreamProtocol::new("/ipfs/kad/1.0.0"));
        kad_config.set_record_ttl(Some(Duration::from_secs(
            config.kademlia_record_ttl().into(),
        )));
        kad_config.set_query_timeout(Duration::from_secs(config.kademlia_query_timeout().into()));
        if let Some(rf) = NonZeroUsize::new(config.kademlia_replication_factor() as usize) {
            kad_config.set_replication_factor(rf);
        }
        let mut kad_behaviour = kad::Behaviour::with_config(peer_id, store, kad_config);
        kad_behaviour.set_mode(Some(kad::Mode::Server));
        let kademlia: Toggle<kad::Behaviour<kad::store::MemoryStore>> = if config.kademlia_enabled()
        {
            Toggle::from(Some(kad_behaviour))
        } else {
            LogStruct::new(
                LogLevel::Preset,
                "Kademlia 已禁用",
                "services.kademlia.enabled = false",
            )
            .emit();
            Toggle::from(None)
        };

        let service_req = service_protocol::new_service_req_behaviour();

        // Relay server — 双栈节点接受 reservation，单栈节点拒绝所有
        let is_dual_stack = {
            let cfg = config.read();
            cfg.network.ipv4_enabled && cfg.network.ipv6_enabled
        };
        let mut relay_cfg = relay::Config::default();
        if is_dual_stack {
            relay_cfg.max_reservations = 16;
            LogStruct::new(
                LogLevel::Preset,
                "双栈节点",
                "启用中继服务器模式 (max 16 reservations)",
            )
            .emit();
        } else {
            relay_cfg.max_reservations = 0;
            relay_cfg.max_reservations_per_peer = 0;
        }
        let relay_server = relay::Behaviour::new(peer_id, relay_cfg);

        Self {
            ping,
            identify,
            kademlia,
            service_req,
            relay_server,
            relay_client,
        }
    }
}
