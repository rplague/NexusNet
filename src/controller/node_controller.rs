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

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use libp2p::{PeerId, identify, kad, ping};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::ConfigHandle;
use crate::dht_cache::{CachePolicy, DhtCache};
use crate::log::{LogLevel, LogStruct};
use crate::network::{AGENT_NAME, NetworkEvent, NetworkHandle, NetworkStart};
use crate::service_dispatcher::{BackendReadiness, InboundRequest, ToController};
use crate::service_protocol;

use super::auth_gate::{AuthGate, Decision};
use super::bootstrap::BootstrapDiscovery;
use super::dht_fetchers::{
    GenericRecordFetcher, LegacyTypesFetcher, ProviderFetcher, TypesFetcher,
};
use super::legacy_types::LegacyTypesPublisher;
use super::peer_tracker::PeerTracker;
use super::relay_manager::RelayManager;
use super::service_registry::{self, ServiceCommand, ServiceSnapshot};

const AUTH_WORKERS: usize = 4;
const AUTH_QUEUE_CAPACITY: usize = 32;

/// Thin orchestration hub: routes between the Swarm actor, the
/// `ServiceDispatcher`, the periodic ticks and the state-machine components.
pub struct NodeController {
    pub(super) config: ConfigHandle,
    pub(super) my_peer_id: PeerId,
    pub(super) swarm: NetworkHandle,
    to_controller_rx: mpsc::Receiver<ToController>,
    event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    shutdown_rx: watch::Receiver<bool>,
    pub(super) peers: PeerTracker,
    pub(super) auth: AuthGate,
    auth_txs: Vec<mpsc::Sender<AuthJob>>,
    auth_next: usize,
    pub(super) dht_permits: Arc<tokio::sync::Semaphore>,
    pub(super) services_tx: mpsc::Sender<ServiceCommand>,
    pub(super) services_snapshot: watch::Receiver<Arc<ServiceSnapshot>>,
    pub(super) relay: RelayManager,
    pub(super) bootstrap: BootstrapDiscovery,
    pub(super) record_cache: Arc<DhtCache<kad::RecordKey, Vec<u8>, GenericRecordFetcher>>,
    pub(super) provider_cache: Arc<DhtCache<kad::RecordKey, Vec<PeerId>, ProviderFetcher>>,
    pub(super) signed_types_cache: Option<Arc<DhtCache<(), BTreeSet<String>, TypesFetcher>>>,
    pub(super) legacy_types_cache: Arc<DhtCache<(), BTreeSet<String>, LegacyTypesFetcher>>,
    pub(super) legacy_types_publisher: Option<Arc<LegacyTypesPublisher>>,
    pub(super) legacy_types_merge: bool,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        to_controller_rx: mpsc::Receiver<ToController>,
        to_dispatcher_tx: mpsc::Sender<InboundRequest>,
        readiness_rx: watch::Receiver<Arc<BackendReadiness>>,
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
        let mut auth_txs = Vec::with_capacity(AUTH_WORKERS);
        for _ in 0..AUTH_WORKERS {
            let (tx, rx) = mpsc::channel(AUTH_QUEUE_CAPACITY);
            auth_txs.push(tx);
            tokio::spawn(auth_worker(
                auth.clone(),
                rx,
                to_dispatcher_tx.clone(),
                swarm.clone(),
            ));
        }
        let dht_permits = Arc::new(tokio::sync::Semaphore::new(
            config.dispatcher_max_inflight_dht().clamp(1, 16),
        ));
        let record_cache = DhtCache::new(
            Arc::new(GenericRecordFetcher {
                swarm: swarm.clone(),
            }),
            CachePolicy::generic(),
            4,
        );
        let provider_cache = DhtCache::new(
            Arc::new(ProviderFetcher {
                swarm: swarm.clone(),
            }),
            CachePolicy::providers(),
            4,
        );
        let types_interval = Duration::from_secs(config.dispatcher_types_sync_interval() as u64);
        let legacy_types_cache = DhtCache::new(
            Arc::new(LegacyTypesFetcher {
                swarm: swarm.clone(),
            }),
            CachePolicy::types(types_interval),
            2,
        );
        let signed_types_cache = config.types_authority_key().map(|authority| {
            DhtCache::new(
                Arc::new(TypesFetcher {
                    swarm: swarm.clone(),
                    authority,
                    max_ttl_seconds: config.types_ttl_seconds(),
                }),
                CachePolicy::types(types_interval),
                2,
            )
        });
        let legacy_types_publisher = if signed_types_cache.is_none() && config.kademlia_enabled() {
            LogStruct::new(
                LogLevel::Warning,
                "类型索引兼容模式",
                "未配置 services.registry.types_authority，使用旧版未签名索引",
            )
            .emit();
            Some(LegacyTypesPublisher::new(
                swarm.clone(),
                types_interval,
                config.compat_max_record_bytes() as usize,
            ))
        } else {
            None
        };
        let services = service_registry::spawn(
            config.clone(),
            swarm.clone(),
            readiness_rx,
            shutdown_rx.clone(),
        );
        let relay = RelayManager::new(config.clone(), swarm.clone(), my_peer_id);
        let bootstrap =
            BootstrapDiscovery::new(config.clone(), swarm.clone(), provider_cache.clone());
        let legacy_types_merge = config.legacy_types_merge();

        Self {
            config,
            my_peer_id,
            swarm,
            to_controller_rx,
            event_rx,
            shutdown_rx,
            peers,
            auth,
            auth_txs,
            auth_next: 0,
            dht_permits,
            services_tx: services.tx,
            services_snapshot: services.snapshot,
            relay,
            bootstrap,
            record_cache,
            provider_cache,
            signed_types_cache,
            legacy_types_cache,
            legacy_types_publisher,
            legacy_types_merge,
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
                    let _ = self.services_tx.try_send(ServiceCommand::Tick);
                    let signed_present = self
                        .signed_types_cache
                        .as_ref()
                        .and_then(|cache| cache.peek(&()).and_then(|read| read.value))
                        .is_some();
                    if let Some(cache) = &self.signed_types_cache {
                        cache.refresh_if_due(());
                    }
                    if self.legacy_types_merge || !signed_present {
                        self.legacy_types_cache.refresh_if_due(());
                    }
                    if let Some(publisher) = &self.legacy_types_publisher {
                        let mut types = self
                            .legacy_types_cache
                            .peek(&())
                            .and_then(|read| read.value)
                            .unwrap_or_default();
                        types.extend(self.services_snapshot.borrow().announced.iter().cloned());
                        publisher.maybe_publish(types);
                    }
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
                self.mark_caches_stale();
                self.bootstrap.reset();
                self.send_service(ServiceCommand::BootstrapCompleted).await;
                self.bootstrap.announce_self().await;
                self.relay.tick().await;
            }
            NetworkEvent::InboundServiceRequest {
                peer,
                request_id,
                service,
                payload,
                response_tx,
            } => self.enqueue_auth(peer, request_id, service, payload, response_tx),
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

    pub(super) fn mark_caches_stale(&self) {
        self.record_cache.mark_all_stale();
        self.provider_cache.mark_all_stale();
        if let Some(cache) = &self.signed_types_cache {
            cache.mark_all_stale();
        }
        self.legacy_types_cache.mark_all_stale();
        self.auth.mark_all_stale();
    }

    pub(super) fn invalidate_caches(&self) {
        self.record_cache.invalidate_all();
        self.provider_cache.invalidate_all();
        if let Some(cache) = &self.signed_types_cache {
            cache.invalidate_all();
        }
        self.legacy_types_cache.invalidate_all();
        self.auth.invalidate_all();
    }
    pub(super) fn invalidate_record_key(&self, key: &str) {
        let record_key = kad::RecordKey::new(&key);
        self.record_cache.invalidate(&record_key);
        if key.starts_with("/oahd/auth/") {
            self.auth.invalidate_all();
        }
        if key.starts_with("/oahd/service/") {
            self.provider_cache.invalidate(&record_key);
        }
        if key == "/oahd/service/authtypes/"
            && let Some(cache) = &self.signed_types_cache
        {
            cache.mark_stale(&());
        }
        if key == "/oahd/service/types" {
            self.legacy_types_cache.mark_stale(&());
        }
    }

    pub(super) async fn send_service(&self, command: ServiceCommand) {
        if self.services_tx.send(command).await.is_err() {
            LogStruct::new(LogLevel::Critical, "服务注册任务不可用", "").emit();
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

    fn enqueue_auth(
        &mut self,
        peer: PeerId,
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    ) {
        let job = AuthJob {
            peer,
            request_id,
            service,
            payload,
            response_tx,
        };
        let index = self.auth_next % self.auth_txs.len();
        self.auth_next = self.auth_next.wrapping_add(1);
        match self.auth_txs[index].try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(job)) => {
                LogStruct::new(LogLevel::Warning, "鉴权队列已满", job.service.clone()).emit();
                self.swarm.send_response(
                    job.request_id,
                    service_protocol::Response {
                        success: false,
                        data: b"authentication overloaded".to_vec(),
                    },
                );
            }
            Err(mpsc::error::TrySendError::Closed(job)) => {
                LogStruct::new(LogLevel::Error, "鉴权队列不可用", job.service.clone()).emit();
                self.swarm.send_response(
                    job.request_id,
                    service_protocol::Response {
                        success: false,
                        data: b"authentication unavailable".to_vec(),
                    },
                );
            }
        }
    }
}

struct AuthJob {
    peer: PeerId,
    request_id: String,
    service: String,
    payload: Vec<u8>,
    response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
}

async fn auth_worker(
    gate: AuthGate,
    mut rx: mpsc::Receiver<AuthJob>,
    to_dispatcher_tx: mpsc::Sender<InboundRequest>,
    swarm: NetworkHandle,
) {
    while let Some(job) = rx.recv().await {
        match gate.authorize(&job.peer, &job.service).await {
            Decision::Allow => forward_auth_job(&to_dispatcher_tx, &swarm, job).await,
            Decision::AllowUnprotected => {
                LogStruct::new(
                    LogLevel::Warning,
                    "鉴权网络同步",
                    format!(
                        "{} 不在鉴权索引中，本次放行；本地 require_auth 保持不变",
                        job.service
                    ),
                )
                .emit();
                forward_auth_job(&to_dispatcher_tx, &swarm, job).await;
            }
            Decision::Deny(reason) => {
                LogStruct::new(
                    LogLevel::Warning,
                    "鉴权拒绝",
                    format!("{} -> {}: {}", job.peer, job.service, reason),
                )
                .emit();
                swarm.send_response(
                    job.request_id,
                    service_protocol::Response {
                        success: false,
                        data: format!("unauthorized: {}", reason).into_bytes(),
                    },
                );
            }
        }
    }
}

async fn forward_auth_job(
    to_dispatcher_tx: &mpsc::Sender<InboundRequest>,
    swarm: &NetworkHandle,
    job: AuthJob,
) {
    let AuthJob {
        request_id,
        service,
        payload,
        response_tx,
        ..
    } = job;
    let inbound = InboundRequest {
        service,
        payload,
        response_tx,
    };
    if let Err(e) = to_dispatcher_tx.send(inbound).await {
        let err_resp = service_protocol::Response {
            success: false,
            data: format!("service unavailable: {}", e).into_bytes(),
        };
        swarm.send_response(request_id, err_resp);
    }
}
