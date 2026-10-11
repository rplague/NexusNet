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

use std::time::Duration;

use libp2p::{PeerId, identify, ping};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::{AGENT_NAME, NetworkEvent, NetworkHandle, NetworkStart};
use crate::service_dispatcher::{InboundRequest, ToController};
use crate::service_protocol;

use super::auth_gate::{AuthGate, Decision};
use super::bootstrap::BootstrapDiscovery;
use super::peer_tracker::PeerTracker;
use super::relay_manager::RelayManager;
use super::service_registry::ServiceRegistry;

/// Thin orchestration hub: routes between the Swarm actor, the
/// `ServiceDispatcher`, the periodic ticks and the state-machine components.
pub struct NodeController {
    pub(super) config: ConfigHandle,
    pub(super) my_peer_id: PeerId,
    pub(super) swarm: NetworkHandle,
    to_dispatcher_tx: mpsc::UnboundedSender<InboundRequest>,
    to_controller_rx: mpsc::UnboundedReceiver<ToController>,
    event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    shutdown_rx: watch::Receiver<bool>,
    pub(super) peers: PeerTracker,
    pub(super) auth: AuthGate,
    pub(super) services: ServiceRegistry,
    pub(super) relay: RelayManager,
    pub(super) bootstrap: BootstrapDiscovery,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        to_controller_rx: mpsc::UnboundedReceiver<ToController>,
        to_dispatcher_tx: mpsc::UnboundedSender<InboundRequest>,
        network: NetworkStart,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let NetworkStart {
            handle: swarm,
            events: event_rx,
            task: _network_task,
        } = network;

        let peers = PeerTracker::new(my_peer_id);
        let auth = AuthGate::new(config.clone(), swarm.clone());
        let services = ServiceRegistry::new(config.clone(), swarm.clone());
        let relay = RelayManager::new(config.clone(), swarm.clone(), my_peer_id);
        let bootstrap = BootstrapDiscovery::new(config.clone(), swarm.clone());

        Self {
            config,
            my_peer_id,
            swarm,
            to_dispatcher_tx,
            to_controller_rx,
            event_rx,
            shutdown_rx,
            peers,
            auth,
            services,
            relay,
            bootstrap,
        }
    }

    /// Run the node orchestration loop.
    pub async fn run(mut self) {
        // Periodic relay top-up.
        let retry = Duration::from_secs(self.config.relay_retry_interval().max(1) as u64);
        let mut relay_tick = tokio::time::interval_at(tokio::time::Instant::now() + retry, retry);
        relay_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Withdrawal check: fixed 1s, only for processing due withdrawals.
        let mut service_tick = tokio::time::interval(Duration::from_secs(1));
        service_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(event) = self.event_rx.recv() => {
                    self.handle_controller_event(event).await;
                }
                Some(msg) = self.to_controller_rx.recv() => {
                    self.handle_controller_message(msg).await;
                }
                _ = service_tick.tick() => {
                    self.services.tick().await;
                }
                _ = relay_tick.tick() => {
                    self.bootstrap.announce_self().await;
                    self.relay.tick().await;
                }
                _ = self.shutdown_rx.changed() => {
                    self.swarm.shutdown();
                    return;
                }
            }
        }
    }

    async fn handle_controller_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::Ping(event) => self.handle_ping(event),
            NetworkEvent::Identify(event) => self.handle_identify(event).await,
            NetworkEvent::BootstrapCompleted => {
                self.services.on_bootstrap_completed().await;
                self.bootstrap.announce_self().await;
                self.relay.tick().await;
            }
            NetworkEvent::InboundServiceRequest {
                peer,
                request_id,
                service,
                payload,
                response_tx,
            } => match self.auth.authorize(&peer, &service).await {
                Decision::Allow => {
                    self.forward_inbound(request_id, service, payload, response_tx);
                }
                Decision::AllowUnprotected => {
                    if self.config.set_service_require_auth(&service, false) {
                        self.config.save_to_default();
                        LogStruct::new(
                            LogLevel::Warning,
                            "鉴权网络同步",
                            format!("{}在网络中已经无需鉴权，已经进行网络同步", service),
                        )
                        .emit();
                    }
                    self.forward_inbound(request_id, service, payload, response_tx);
                }
                Decision::Deny(reason) => {
                    LogStruct::new(
                        LogLevel::Warning,
                        "鉴权拒绝",
                        format!("{} -> {}: {}", peer, service, reason),
                    )
                    .emit();
                    let _ = response_tx.send(Ok(service_protocol::Response {
                        success: false,
                        data: format!("unauthorized: {}", reason).into_bytes(),
                    }));
                }
            },
            NetworkEvent::Relay(event) => self.relay.on_relay_event(event),
            NetworkEvent::RelayClient(event) => self.relay.on_relay_client_event(event),
            NetworkEvent::ListenerClosed { addresses } => {
                // Relay dropped: try to top up to the target immediately.
                if self.relay.on_listener_closed(addresses).await {
                    self.relay.tick().await;
                }
            }
        }
    }

    fn handle_ping(&mut self, event: ping::Event) {
        if let Some(peer) = self.peers.on_ping(event, self.config.ping_max_failures()) {
            self.swarm.disconnect_peer(peer);
        }
    }

    async fn handle_identify(&mut self, event: identify::Event) {
        match event {
            identify::Event::Received { peer_id, info, .. } => {
                if info.agent_version.starts_with(AGENT_NAME) {
                    self.bootstrap.on_identify(&info, peer_id).await;
                } else {
                    // Non-OAHD node; cannot disconnect directly, just log.
                    LogStruct::new(
                        LogLevel::Debug,
                        "非 OAHD 节点",
                        format!("{}: {}", peer_id, info.agent_version),
                    )
                    .emit();
                }
            }
            identify::Event::Error { peer_id, error, .. } => {
                LogStruct::new(
                    LogLevel::Error,
                    "Identify错误",
                    format!("{}: {}", peer_id, error),
                )
                .emit();
            }
            _ => {}
        }
    }

    /// Forward an inbound request to the ServiceDispatcher.
    fn forward_inbound(
        &self,
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    ) {
        let inbound = InboundRequest {
            service,
            payload,
            response_tx,
        };
        if let Err(e) = self.to_dispatcher_tx.send(inbound) {
            let err_resp = service_protocol::Response {
                success: false,
                data: format!("service unavailable: {}", e).into_bytes(),
            };
            self.swarm.send_response(request_id, err_resp);
        }
    }
}
