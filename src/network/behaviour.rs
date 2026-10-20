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

//! libp2p behaviour aggregation: combines ping / identify / kademlia /
//! request-response / relay into a single `NetBehaviour`, with a hand-written
//! event enum as `out_event`.
//!
//! Configuration is read once at construction and baked into each behaviour.
//! libp2p behaviours have no runtime setters.
//!
//! The two PQ behaviours (`service_req_pq`, `pq_identity`) are registered only
//! when PQ is enabled; when disabled their protocols never appear in the
//! Identify protocol table, avoiding a false claim of support.

use crate::config::ConfigHandle;
use crate::network::pq::{self, PqIdentity, PqRequest, PqResponse};
use crate::service_protocol;
use libp2p::request_response::{self, ProtocolSupport, cbor};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::{StreamProtocol, identify, identity, kad, ping, relay, swarm::NetworkBehaviour};
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::{LogLevel, LogStruct};

/// Identify agent name reported by NexusNet nodes.
pub const AGENT_NAME: &str = "/nexusnet";

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NetBehaviourEvent")]
pub struct NetBehaviour {
    pub ping: Toggle<ping::Behaviour>,
    pub identify: identify::Behaviour,
    pub kademlia: Toggle<kad::Behaviour<kad::store::MemoryStore>>,
    pub service_req: cbor::Behaviour<service_protocol::Request, service_protocol::Response>,
    pub service_req_pq: Toggle<cbor::Behaviour<PqRequest, PqResponse>>,
    pub pq_identity: Toggle<cbor::Behaviour<(), PqIdentity>>,
    pub relay_server: relay::Behaviour,
    pub relay_client: relay::client::Behaviour,
}

/// Events emitted by the composite behaviour.
///
/// Each variant carries the wrapped behaviour's raw event; `Toggle<T>` has the
/// same `ToSwarm` as `T`, so `Ping`/`Kademlia` still use `ping::Event` /
/// `kad::Event`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum NetBehaviourEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kademlia(kad::Event),
    ServiceReq(request_response::Event<service_protocol::Request, service_protocol::Response>),
    ServiceReqPq(request_response::Event<PqRequest, PqResponse>),
    PqIdentity(request_response::Event<(), PqIdentity>),
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
impl From<request_response::Event<PqRequest, PqResponse>> for NetBehaviourEvent {
    fn from(event: request_response::Event<PqRequest, PqResponse>) -> Self {
        NetBehaviourEvent::ServiceReqPq(event)
    }
}
impl From<request_response::Event<(), PqIdentity>> for NetBehaviourEvent {
    fn from(event: request_response::Event<(), PqIdentity>) -> Self {
        NetBehaviourEvent::PqIdentity(event)
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

fn request_response_config() -> request_response::Config {
    request_response::Config::default().with_request_timeout(service_protocol::REQUEST_TIMEOUT)
}

impl NetBehaviour {
    pub fn new(
        config: &ConfigHandle,
        keypair: &identity::Keypair,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        let peer_id = keypair.public().to_peer_id();

        // Ping: fully disabled via Toggle when enabled = false.
        let ping_config = ping::Config::new()
            .with_interval(Duration::from_secs(config.ping_interval().into()))
            .with_timeout(Duration::from_secs(config.ping_timeout().into()));
        let ping: Toggle<ping::Behaviour> = if config.ping_enabled() {
            Toggle::from(Some(ping::Behaviour::new(ping_config)))
        } else {
            Toggle::from(None)
        };

        // Identify configuration.
        let identify_config = identify::Config::new(AGENT_NAME.to_string(), keypair.public())
            .with_agent_version(AGENT_NAME.to_string())
            .with_push_listen_addr_updates(true);
        let identify = identify::Behaviour::new(identify_config);

        // Kademlia configuration.
        let store = kad::store::MemoryStore::new(peer_id);
        let mut kad_config = kad::Config::new(StreamProtocol::new("/ipfs/kad/1.0.0"));
        let record_ttl = Duration::from_secs(config.kademlia_record_ttl().into());
        kad_config.set_record_ttl(Some(record_ttl));
        // Republish well before expiry so remote copies never lapse.
        kad_config.set_publication_interval(Some(record_ttl / 2));
        // Provider records are renewed by libp2p's periodic provider job.
        kad_config.set_provider_record_ttl(Some(Duration::from_secs(48 * 60 * 60)));
        kad_config.set_provider_publication_interval(Some(Duration::from_secs(60 * 60)));
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

        let max_request_bytes = config.dispatcher_max_request_bytes();
        let max_response_bytes = config.dispatcher_max_response_bytes();
        let service_req =
            service_protocol::new_service_req_behaviour(max_request_bytes, max_response_bytes);

        // PQ behaviours: registered only when enabled.
        let pq_active = config.pq_enabled();
        let service_req_pq: Toggle<cbor::Behaviour<PqRequest, PqResponse>> = if pq_active {
            let codec = cbor::codec::Codec::<PqRequest, PqResponse>::default()
                .set_request_size_maximum(max_request_bytes)
                .set_response_size_maximum(max_response_bytes);
            Toggle::from(Some(cbor::Behaviour::with_codec(
                codec,
                vec![(
                    StreamProtocol::new(pq::SERVICE_REQ_PQ_PROTOCOL),
                    ProtocolSupport::Full,
                )],
                request_response_config(),
            )))
        } else {
            Toggle::from(None)
        };
        let pq_identity: Toggle<cbor::Behaviour<(), PqIdentity>> = if pq_active {
            Toggle::from(Some(cbor::Behaviour::new(
                vec![(
                    StreamProtocol::new(pq::PQ_IDENTITY_PROTOCOL),
                    ProtocolSupport::Full,
                )],
                request_response_config(),
            )))
        } else {
            Toggle::from(None)
        };
        if pq_active {
            LogStruct::new(LogLevel::Preset, "PQ 已启用", "crypto.pq_* = true").emit();
        }

        // Relay server: dual-stack nodes accept reservations, single-stack
        // nodes reject all.
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
            service_req_pq,
            pq_identity,
            relay_server,
            relay_client,
        }
    }
}
