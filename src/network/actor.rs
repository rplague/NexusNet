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

//! Swarm actor: a single event loop owning the `Swarm`.
//!
//! # Why an actor is needed
//!
//! libp2p is event-driven: starting a query returns a `QueryId` immediately, and
//! the actual result only appears on some future poll of the `Swarm`. If a
//! caller both polls the Swarm and waits for the query result, they block each
//! other, forming a re-entrant deadlock. So polling the Swarm and waiting for
//! results are split into two execution contexts:

use crate::config::ConfigHandle;
use crate::network::NetworkError;
use crate::network::behaviour::{AGENT_NAME, NetBehaviour, NetBehaviourEvent};
use crate::network::builder::build_swarm;
use crate::network::pq::{self, PqIdentity, PqKeys, PqRequest, PqResponse};
use crate::service_protocol;
use crate::{LogLevel, LogStruct};

use libp2p::futures::StreamExt;
use libp2p::swarm::{ConnectionId, SwarmEvent};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, identify, identity, kad, ping, relay,
    request_response,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Commands sent to the actor by `NodeController` through [`NetworkHandle`].
pub enum SwarmCommand {
    KadGetRecord {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<kad::GetRecordOk, NetworkError>>,
    },
    /// Like `KadGetRecord`, but ignores the local store copy and the record
    /// cache so the freshest value held by peers is returned. Used to build the
    /// merged global service-type index without a node's own publisher record
    /// shadowing remote updates.
    KadGetRecordNetwork {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<kad::GetRecordOk, NetworkError>>,
    },
    KadGetProviders {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<Vec<PeerId>, NetworkError>>,
    },
    KadPutRecord {
        key: kad::RecordKey,
        value: Vec<u8>,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    KadStartProviding {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    KadStopProviding {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    /// Remove a locally stored record (only if this node is its publisher).
    KadRemoveRecord {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    /// Clear the `get_record` result cache; replies with the number of entries
    /// removed.
    KadCacheRefresh {
        resp: oneshot::Sender<Result<usize, NetworkError>>,
    },
    ServiceSendRequest {
        peer: PeerId,
        request: service_protocol::Request,
        resp: oneshot::Sender<Result<service_protocol::Response, NetworkError>>,
    },
    ServiceSendResponse {
        request_id: String,
        response: service_protocol::Response,
    },
    ListenOn {
        addr: Multiaddr,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    Dial {
        addr: Multiaddr,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    DisconnectPeer {
        peer: PeerId,
    },
    /// Rebuild the whole Swarm from the current config, for hot reload.
    Reload {
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    Shutdown,
}

/// Events forwarded from the actor to `NodeController`.
#[allow(clippy::large_enum_variant)]
pub enum NetworkEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    /// First successful bootstrap within this Swarm's lifetime.
    BootstrapCompleted,
    InboundServiceRequest {
        peer: PeerId,
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    },
    Relay(relay::Event),
    RelayClient(relay::client::Event),
    /// A listener closed. `addresses` holds all addresses it ever listened on;
    /// failed or dropped relay reservations show up here with a `p2p-circuit`
    /// address.
    ListenerClosed {
        addresses: Vec<Multiaddr>,
    },
}

/// Async facade. All methods talk to the actor via a command channel, using
/// oneshot channels for replies internally.
#[derive(Clone)]
pub struct NetworkHandle {
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
}

impl NetworkHandle {
    fn new(cmd_tx: mpsc::UnboundedSender<SwarmCommand>) -> Self {
        Self { cmd_tx }
    }

    /// Generic "send command + await reply" helper, removing per-method oneshot
    /// boilerplate.
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> SwarmCommand,
    ) -> Result<T, NetworkError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(make(tx))
            .map_err(|_| NetworkError::ActorGone)?;
        rx.await.map_err(|_| NetworkError::ActorGone)
    }

    pub async fn get_record(&self, key: kad::RecordKey) -> Result<kad::GetRecordOk, NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadGetRecord { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// Fetch a record from the DHT peers, ignoring the local store copy and the
    /// 60s result cache. See `SwarmCommand::KadGetRecordNetwork`.
    pub async fn get_record_network(
        &self,
        key: kad::RecordKey,
    ) -> Result<kad::GetRecordOk, NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadGetRecordNetwork { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn get_providers(&self, key: kad::RecordKey) -> Result<Vec<PeerId>, NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadGetProviders { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// Clear the `get_record` result cache; returns the number of entries
    /// removed.
    pub async fn refresh_record_cache(&self) -> Result<usize, NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadCacheRefresh { resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn put_record(
        &self,
        key: kad::RecordKey,
        value: Vec<u8>,
    ) -> Result<(), NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadPutRecord { key, value, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn start_providing(&self, key: kad::RecordKey) -> Result<(), NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadStartProviding { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn stop_providing(&self, key: kad::RecordKey) -> Result<(), NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadStopProviding { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// Remove a locally stored record. libp2p only removes it if this node is the
    /// record's publisher; otherwise it is a no-op. Clears the record cache for
    /// the key so later reads go back to the network.
    pub async fn remove_record(&self, key: kad::RecordKey) -> Result<(), NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadRemoveRecord { key, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn send_request(
        &self,
        peer: &PeerId,
        request: service_protocol::Request,
    ) -> Result<service_protocol::Response, NetworkError> {
        match self
            .request(|resp| SwarmCommand::ServiceSendRequest {
                peer: *peer,
                request,
                resp,
            })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub fn send_response(&self, request_id: String, response: service_protocol::Response) {
        let _ = self.cmd_tx.send(SwarmCommand::ServiceSendResponse {
            request_id,
            response,
        });
    }

    pub async fn listen_on(&self, addr: Multiaddr) -> Result<(), NetworkError> {
        match self
            .request(|resp| SwarmCommand::ListenOn { addr, resp })
            .await
        {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<(), NetworkError> {
        match self.request(|resp| SwarmCommand::Dial { addr, resp }).await {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// Disconnect a peer, without waiting for a reply.
    pub fn disconnect_peer(&self, peer: PeerId) {
        let _ = self.cmd_tx.send(SwarmCommand::DisconnectPeer { peer });
    }

    /// Rebuild the Swarm from the current `ConfigHandle`. Drops all connections
    /// and clears the DHT local store.
    pub async fn reload(&self) -> Result<(), NetworkError> {
        match self.request(|resp| SwarmCommand::Reload { resp }).await {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// Tell the actor to exit its loop.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(SwarmCommand::Shutdown);
    }
}

/// A pending Kademlia query entry.
enum KadPending {
    GetRecord {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<kad::GetRecordOk, NetworkError>>,
        /// When set, the local store copy (`peer == None`) is ignored and only a
        /// record obtained from the DHT finalizes the query.
        network_only: bool,
    },
    GetProviders {
        resp: oneshot::Sender<Result<Vec<PeerId>, NetworkError>>,
        /// Accumulated provider set. libp2p reports in batches, so it is
        /// accumulated until the query finalizes.
        providers: HashSet<PeerId>,
    },
    PutRecord {
        key: kad::RecordKey,
        value: Vec<u8>,
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    StartProviding(oneshot::Sender<Result<(), NetworkError>>),
}

impl KadPending {
    /// Backfill an error when the query is cancelled, e.g. on reload.
    fn fail(self, err: NetworkError) {
        match self {
            KadPending::GetRecord { resp, .. } => {
                let _ = resp.send(Err(err));
            }
            KadPending::GetProviders { resp, .. } => {
                let _ = resp.send(Err(err));
            }
            KadPending::PutRecord { resp, .. } => {
                let _ = resp.send(Err(err));
            }
            KadPending::StartProviding(tx) => {
                let _ = tx.send(Err(err));
            }
        }
    }
}

/// TTL of the in-memory `get_record` result cache.
const RECORD_CACHE_TTL: Duration = Duration::from_secs(60);
/// Maximum number of cached records; when full a random third is evicted.
const RECORD_CACHE_CAPACITY: usize = 1024;

/// A cached `get_record` result. `value == None` is a negative cache entry.
struct CachedRecord {
    value: Option<Vec<u8>>,
    fetched_at: Instant,
}

/// Accumulate progress events for a `GetProviders` query.
///
/// libp2p reports providers whose records hit the local store first via
/// `FoundProviders`, then more batches as the DHT query proceeds, and finally
/// terminates with `FinishedWithNoAdditionalRecord` (or `Err`). All batches
/// must be accumulated until the terminal event, or providers discovered by the
/// DHT would be lost.
///
/// Returns `None` once the query finalized and the result was sent via `resp`;
/// `Some` means more events are needed (the pending entry is returned as-is).
/// Non-`GetProviders` entries are returned as-is.
fn cacheable_record(
    result: &Result<kad::GetRecordOk, kad::GetRecordError>,
    network_only: bool,
) -> Option<Option<Vec<u8>>> {
    match result {
        Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
            Some(Some(peer_record.record.value.clone()))
        }
        Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. }) if !network_only => Some(None),
        _ => None,
    }
}

fn apply_get_providers(
    pending: KadPending,
    result: Result<kad::GetProvidersOk, kad::GetProvidersError>,
) -> Option<KadPending> {
    match (pending, result) {
        (
            KadPending::GetProviders {
                resp,
                mut providers,
            },
            Ok(kad::GetProvidersOk::FoundProviders {
                providers: batch, ..
            }),
        ) => {
            providers.extend(batch);
            Some(KadPending::GetProviders { resp, providers })
        }
        (
            KadPending::GetProviders { resp, providers },
            Ok(kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. }),
        ) => {
            let _ = resp.send(Ok(providers.into_iter().collect()));
            None
        }
        (KadPending::GetProviders { resp, .. }, Err(e)) => {
            let _ = resp.send(Err(NetworkError::Kad(format!("{e:?}"))));
            None
        }
        (other, _) => Some(other),
    }
}

/// Start result: facade + event stream + task handle.
pub struct NetworkStart {
    pub handle: NetworkHandle,
    pub events: mpsc::UnboundedReceiver<NetworkEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Reply context for an inbound request: distinguishes plain and PQ.
enum PendingInbound {
    Plain(request_response::ResponseChannel<service_protocol::Response>),
    Pq {
        channel: request_response::ResponseChannel<PqResponse>,
        inbound: pq::PqInbound,
    },
}

/// An inbound request awaiting an application response.
struct PendingInboundEntry {
    connection_id: ConnectionId,
    peer: PeerId,
    pending: PendingInbound,
}

/// Waiter and wire size for an outbound plaintext request.
struct PendingOutbound {
    resp: oneshot::Sender<Result<service_protocol::Response, NetworkError>>,
    wire_len: u64,
}

/// Waiter and response key for an outbound PQ request.
struct PqOutboundPending {
    resp: oneshot::Sender<Result<service_protocol::Response, NetworkError>>,
    out: pq::PqOutbound,
    wire_len: u64,
}

/// Rejection returned to requests that arrive during a hot reload.
const RELOAD_REJECTION: &[u8] = b"node is reloading; retry shortly";

fn reload_rejection_response() -> service_protocol::Response {
    service_protocol::Response {
        success: false,
        data: RELOAD_REJECTION.to_vec(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendSecurity {
    Pq,
    Plaintext,
    WaitIdentity,
    MissingKeys,
    MissingTransport,
    Unsupported,
}

fn send_security(
    transport: bool,
    required: bool,
    keys_available: bool,
    identity_available: bool,
    supports_pq: Option<bool>,
) -> SendSecurity {
    if required && !transport {
        return SendSecurity::MissingTransport;
    }
    if required && !identity_available {
        return match supports_pq {
            Some(false) => SendSecurity::Unsupported,
            _ => SendSecurity::WaitIdentity,
        };
    }
    if !transport {
        return SendSecurity::Plaintext;
    }
    if !keys_available {
        return SendSecurity::MissingKeys;
    }
    if identity_available {
        return SendSecurity::Pq;
    }
    match supports_pq {
        Some(true) | None => SendSecurity::WaitIdentity,
        Some(false) => SendSecurity::Plaintext,
    }
}

pub(crate) struct SwarmActor {
    swarm: Swarm<NetBehaviour>,
    config: ConfigHandle,
    keypair: identity::Keypair,
    pq_keys: Option<PqKeys>,
    cmd_rx: mpsc::UnboundedReceiver<SwarmCommand>,
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    bootstrap_triggered: bool,
    /// Protocols announced by the peer (from Identify), used to decide PQ
    /// capability.
    peer_protocols: HashMap<PeerId, HashSet<StreamProtocol>>,
    /// Verified and cached peer PQ identities.
    pq_identities: HashMap<PeerId, PqIdentity>,
    pending_kad: HashMap<kad::QueryId, KadPending>,
    pending_outbound: HashMap<request_response::OutboundRequestId, PendingOutbound>,
    pending_outbound_pq: HashMap<request_response::OutboundRequestId, PqOutboundPending>,
    pending_pq_identity: HashMap<request_response::OutboundRequestId, PeerId>,
    pending_inbound: HashMap<String, PendingInboundEntry>,
    /// `get_record` result cache (including negative entries).
    record_cache: HashMap<kad::RecordKey, CachedRecord>,
}

/// Build the Swarm and start the actor task.
pub fn spawn(
    config: ConfigHandle,
    keypair: identity::Keypair,
    pq_keys: Option<PqKeys>,
) -> Result<NetworkStart, NetworkError> {
    let swarm = build_swarm(&config, &keypair)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let handle = NetworkHandle::new(cmd_tx.clone());

    let mut actor = SwarmActor {
        swarm,
        config,
        keypair,
        pq_keys,
        cmd_rx,
        cmd_tx,
        event_tx,
        bootstrap_triggered: false,
        peer_protocols: HashMap::new(),
        pq_identities: HashMap::new(),
        pending_kad: HashMap::new(),
        pending_outbound: HashMap::new(),
        pending_outbound_pq: HashMap::new(),
        pending_pq_identity: HashMap::new(),
        pending_inbound: HashMap::new(),
        record_cache: HashMap::new(),
    };

    let task = tokio::spawn(async move {
        actor.run().await;
    });

    Ok(NetworkStart {
        handle,
        events: event_rx,
        task,
    })
}

impl SwarmActor {
    async fn run(&mut self) {
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => {
                    self.handle_event(event).await;
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if !self.handle_command(cmd).await {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Get a mutable Kademlia reference; returns `Disabled` if toggled off.
    fn kad(&mut self) -> Result<&mut kad::Behaviour<kad::store::MemoryStore>, NetworkError> {
        self.swarm
            .behaviour_mut()
            .kademlia
            .as_mut()
            .ok_or(NetworkError::Disabled)
    }

    /// Return a fresh cached `get_record` result, if any.
    ///
    /// Positive entries rebuild a `FoundRecord`; negative entries report
    /// `FinishedWithNoAdditionalRecord` (i.e. not found).
    fn cached_record(&self, key: &kad::RecordKey) -> Option<kad::GetRecordOk> {
        let entry = self.record_cache.get(key)?;
        if entry.fetched_at.elapsed() >= RECORD_CACHE_TTL {
            return None;
        }
        Some(match &entry.value {
            Some(value) => kad::GetRecordOk::FoundRecord(kad::PeerRecord {
                peer: None,
                record: kad::Record::new(key.clone(), value.clone()),
            }),
            None => kad::GetRecordOk::FinishedWithNoAdditionalRecord {
                cache_candidates: Default::default(),
            },
        })
    }

    /// Insert a record into the cache, evicting entries when at capacity.
    fn insert_cached(&mut self, key: kad::RecordKey, value: Option<Vec<u8>>) {
        if self.record_cache.len() >= RECORD_CACHE_CAPACITY && !self.record_cache.contains_key(&key)
        {
            // Purge expired entries first; if still full, drop a random third.
            self.record_cache
                .retain(|_, entry| entry.fetched_at.elapsed() < RECORD_CACHE_TTL);
            if self.record_cache.len() >= RECORD_CACHE_CAPACITY {
                let victims: Vec<kad::RecordKey> = self
                    .record_cache
                    .keys()
                    .take(self.record_cache.len() / 3)
                    .cloned()
                    .collect();
                for victim in victims {
                    self.record_cache.remove(&victim);
                }
            }
        }
        self.record_cache.insert(
            key,
            CachedRecord {
                value,
                fetched_at: Instant::now(),
            },
        );
    }

    async fn handle_event(&mut self, event: SwarmEvent<NetBehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(NetBehaviourEvent::Kademlia(kad_event)) => {
                self.handle_kad_event(kad_event);
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReq(svc_event)) => {
                self.handle_service_req(svc_event).await;
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReqPq(svc_event)) => {
                self.handle_service_req_pq(svc_event).await;
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::PqIdentity(svc_event)) => {
                self.handle_pq_identity(svc_event).await;
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Ping(event)) => {
                let _ = self.event_tx.send(NetworkEvent::Ping(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Identify(event)) => {
                if let identify::Event::Received { peer_id, info, .. } = &event {
                    self.on_identify_peer(*peer_id, info);
                }
                let _ = self.event_tx.send(NetworkEvent::Identify(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Relay(event)) => {
                let _ = self.event_tx.send(NetworkEvent::Relay(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::RelayClient(event)) => {
                let _ = self.event_tx.send(NetworkEvent::RelayClient(event));
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                connection_id,
                num_established,
                ..
            } => {
                // After a connection closes its pending inbound requests can no
                // longer be replied to; clean them up to avoid a leak.
                self.pending_inbound
                    .retain(|_, entry| entry.connection_id != connection_id);
                if num_established == 0 {
                    self.peer_protocols.remove(&peer_id);
                    self.pq_identities.remove(&peer_id);
                    self.pending_pq_identity
                        .retain(|_, pending_peer| *pending_peer != peer_id);
                }
            }
            SwarmEvent::ListenerClosed { addresses, .. } => {
                let _ = self
                    .event_tx
                    .send(NetworkEvent::ListenerClosed { addresses });
            }
            _ => {}
        }
    }

    /// Identify -> Kademlia address bridge.
    ///
    /// Writes the OAHD peer's announced listen addresses into the DHT routing
    /// table; `add_address` triggers an auto-bootstrap (throttled ~500ms) when a
    /// new node is inserted, eventually emitting `BootstrapCompleted`.
    fn on_identify_peer(&mut self, peer_id: PeerId, info: &identify::Info) {
        // Cache the peer's protocol table for PQ capability checks.
        self.peer_protocols
            .insert(peer_id, info.protocols.iter().cloned().collect());

        if !info.agent_version.starts_with(AGENT_NAME) {
            return;
        }
        // Identify -> Kademlia address bridge
        if let Ok(kad) = self.kad() {
            for addr in &info.listen_addrs {
                kad.add_address(&peer_id, addr.clone());
            }
        }
        // Peer supports PQ and its identity is uncached -> fetch and verify it.
        self.maybe_fetch_pq_identity(peer_id);
    }

    /// If PQ is enabled and the peer advertises support, request its PQ identity
    /// (once).
    fn maybe_fetch_pq_identity(&mut self, peer_id: PeerId) {
        if self.pq_keys.is_none() || !self.config.pq_enabled() {
            return;
        }
        if self.pq_identities.contains_key(&peer_id) {
            return;
        }
        let supports = self
            .peer_protocols
            .get(&peer_id)
            .is_some_and(|p| p.contains(&StreamProtocol::new(pq::SERVICE_REQ_PQ_PROTOCOL)));
        if !supports {
            return;
        }
        if let Some(behaviour) = self.swarm.behaviour_mut().pq_identity.as_mut() {
            let id = behaviour.send_request(&peer_id, ());
            self.pending_pq_identity.insert(id, peer_id);
        }
    }

    /// Handle Kademlia query progress events, waking the matching pending entry
    /// by `QueryId`.
    fn handle_kad_event(&mut self, event: kad::Event) {
        let kad::Event::OutboundQueryProgressed { result, id, .. } = event else {
            return;
        };

        match result {
            kad::QueryResult::Bootstrap(result) => {
                if result.is_ok() && !self.bootstrap_triggered {
                    self.bootstrap_triggered = true;
                    let _ = self.event_tx.send(NetworkEvent::BootstrapCompleted);
                }
            }
            kad::QueryResult::GetRecord(result) => {
                // A network-only read must not be satisfied by the local store
                // copy; keep waiting for a peer record or the terminal event.
                let network_only = matches!(
                    self.pending_kad.get(&id),
                    Some(KadPending::GetRecord {
                        network_only: true,
                        ..
                    })
                );
                if network_only
                    && let Ok(kad::GetRecordOk::FoundRecord(peer_record)) = &result
                    && peer_record.peer.is_none()
                {
                    return;
                }
                if let Some(KadPending::GetRecord {
                    key,
                    resp,
                    network_only,
                }) = self.pending_kad.remove(&id)
                {
                    if let Some(value) = cacheable_record(&result, network_only) {
                        self.insert_cached(key, value);
                    }
                    let _ = resp.send(result.map_err(|e| NetworkError::Kad(format!("{e:?}"))));
                }
            }
            kad::QueryResult::GetProviders(result) => {
                if let Some(pending) = self.pending_kad.remove(&id)
                    && let Some(pending) = apply_get_providers(pending, result)
                {
                    // Query not finalized yet; put it back to keep accumulating.
                    self.pending_kad.insert(id, pending);
                }
            }
            kad::QueryResult::PutRecord(result) => {
                if let Some(KadPending::PutRecord { key, value, resp }) =
                    self.pending_kad.remove(&id)
                {
                    if result.is_ok() {
                        self.insert_cached(key, Some(value));
                    }
                    let _ = resp.send(
                        result
                            .map(|_| ())
                            .map_err(|e| NetworkError::Kad(format!("{e:?}"))),
                    );
                }
            }
            kad::QueryResult::StartProviding(result) => {
                if let Some(KadPending::StartProviding(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(
                        result
                            .map(|_| ())
                            .map_err(|e| NetworkError::Kad(format!("{e:?}"))),
                    );
                }
            }
            _ => {}
        }
    }

    async fn handle_service_req(
        &mut self,
        event: request_response::Event<service_protocol::Request, service_protocol::Response>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                connection_id,
                message,
                ..
            } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    // pq_required: reject plaintext requests.
                    if self.config.pq_required() {
                        let _ = self.swarm.behaviour_mut().service_req.send_response(
                            channel,
                            service_protocol::Response {
                                success: false,
                                data: b"PQ required".to_vec(),
                            },
                        );
                        return;
                    }
                    let request_id = Uuid::new_v4().to_string();
                    let (resp_tx, resp_rx) = oneshot::channel();

                    self.pending_inbound.insert(
                        request_id.clone(),
                        PendingInboundEntry {
                            connection_id,
                            peer,
                            pending: PendingInbound::Plain(channel),
                        },
                    );

                    let _ = self.event_tx.send(NetworkEvent::InboundServiceRequest {
                        peer,
                        request_id: request_id.clone(),
                        service: request.service,
                        payload: request.payload,
                        response_tx: resp_tx,
                    });

                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        // Whatever the outcome (ok/error/channel closed), a response
                        // must be sent back, so the peer never hangs forever.
                        let response = match resp_rx.await {
                            Ok(Ok(response)) => response,
                            Ok(Err(e)) => service_protocol::Response {
                                success: false,
                                data: e.into_bytes(),
                            },
                            Err(_) => service_protocol::Response {
                                success: false,
                                data: b"request handler dropped".to_vec(),
                            },
                        };
                        let _ = cmd_tx.send(SwarmCommand::ServiceSendResponse {
                            request_id,
                            response,
                        });
                    });
                }
                request_response::Message::Response {
                    request_id,
                    response,
                    ..
                } => {
                    if let Some(pending) = self.pending_outbound.remove(&request_id) {
                        let _ = pending.resp.send(Ok(response));
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(pending) = self.pending_outbound.remove(&request_id) {
                    let message = describe_outbound_failure(
                        false,
                        peer,
                        service_protocol::SERVICE_REQ_PROTOCOL,
                        Some(pending.wire_len),
                        &error,
                    );
                    let _ = pending.resp.send(Err(NetworkError::Request(message)));
                }
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                let detail = describe_inbound_failure(
                    peer,
                    self.config.dispatcher_max_request_bytes(),
                    &error,
                );
                LogStruct::new(LogLevel::Warning, "入站服务请求失败", detail).emit();
            }
            _ => {}
        }
    }

    /// Handle PQ-encrypted service requests/responses.
    async fn handle_service_req_pq(
        &mut self,
        event: request_response::Event<PqRequest, PqResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                connection_id,
                message,
                ..
            } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let Some(keys) = self.pq_keys.as_ref() else {
                        LogStruct::new(
                            LogLevel::Warning,
                            "PQ 请求中止",
                            format!(
                                "{peer}: 本地未加载 PQ 密钥；请检查 keypair.pq.bin 与 crypto.pq_* 配置"
                            ),
                        )
                        .emit();
                        return;
                    };
                    let inbound = match pq::open_request(keys, &peer, &request) {
                        Ok(v) => v,
                        Err(e) => {
                            LogStruct::new(
                                LogLevel::Warning,
                                "PQ 请求解密失败",
                                format!(
                                    "{peer}: {e}；可能对端缓存的 identity 已过期或两端 PQ 密钥不一致"
                                ),
                            )
                            .emit();
                            return;
                        }
                    };
                    if self.config.pq_identity_enabled() && request.identity.is_none() {
                        LogStruct::new(LogLevel::Warning, "PQ 请求缺少签名", peer.to_string())
                            .emit();
                        if let Ok(response) =
                            inbound.seal_response(&encode_response(&service_protocol::Response {
                                success: false,
                                data: b"PQ signature required".to_vec(),
                            }))
                            && let Some(behaviour) =
                                self.swarm.behaviour_mut().service_req_pq.as_mut()
                        {
                            let _ = behaviour.send_response(channel, response);
                        }
                        return;
                    }
                    let Some(req) = decode_request(&inbound.plaintext) else {
                        LogStruct::new(
                            LogLevel::Warning,
                            "PQ 请求格式错误",
                            format!("{peer}: 解密后的请求 payload 无法解析"),
                        )
                        .emit();
                        if let Ok(response) =
                            inbound.seal_response(&encode_response(&service_protocol::Response {
                                success: false,
                                data: b"invalid PQ request payload".to_vec(),
                            }))
                            && let Some(behaviour) =
                                self.swarm.behaviour_mut().service_req_pq.as_mut()
                        {
                            let _ = behaviour.send_response(channel, response);
                        }
                        return;
                    };
                    let request_id = Uuid::new_v4().to_string();
                    let (resp_tx, resp_rx) = oneshot::channel();
                    self.pending_inbound.insert(
                        request_id.clone(),
                        PendingInboundEntry {
                            connection_id,
                            peer,
                            pending: PendingInbound::Pq { channel, inbound },
                        },
                    );
                    let _ = self.event_tx.send(NetworkEvent::InboundServiceRequest {
                        peer,
                        request_id: request_id.clone(),
                        service: req.service,
                        payload: req.payload,
                        response_tx: resp_tx,
                    });
                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        let response = match resp_rx.await {
                            Ok(Ok(response)) => response,
                            Ok(Err(e)) => service_protocol::Response {
                                success: false,
                                data: e.into_bytes(),
                            },
                            Err(_) => service_protocol::Response {
                                success: false,
                                data: b"request handler dropped".to_vec(),
                            },
                        };
                        let _ = cmd_tx.send(SwarmCommand::ServiceSendResponse {
                            request_id,
                            response,
                        });
                    });
                }
                request_response::Message::Response {
                    request_id,
                    response,
                    ..
                } => {
                    if let Some(pending) = self.pending_outbound_pq.remove(&request_id) {
                        let result = match pending.out.open_response(&response) {
                            Ok(bytes) => match decode_response(&bytes) {
                                Some(resp) => Ok(resp),
                                None => Err(NetworkError::Request(
                                    "PQ 响应解码失败（响应可能被截断或超过 max_response_bytes）"
                                        .into(),
                                )),
                            },
                            Err(e) => Err(NetworkError::Request(format!("PQ 响应解密失败: {e}"))),
                        };
                        let _ = pending.resp.send(result);
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(pending) = self.pending_outbound_pq.remove(&request_id) {
                    let message = describe_outbound_failure(
                        true,
                        peer,
                        pq::SERVICE_REQ_PQ_PROTOCOL,
                        Some(pending.wire_len),
                        &error,
                    );
                    let _ = pending.resp.send(Err(NetworkError::Request(message)));
                }
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                let detail = describe_inbound_failure(
                    peer,
                    self.config.dispatcher_max_request_bytes(),
                    &error,
                );
                LogStruct::new(LogLevel::Warning, "入站 PQ 请求失败", detail).emit();
            }
            _ => {}
        }
    }

    /// Handle PQ identity exchange (request reply + cached response
    /// verification).
    async fn handle_pq_identity(&mut self, event: request_response::Event<(), PqIdentity>) {
        match event {
            request_response::Event::Message { message, .. } => match message {
                request_response::Message::Request { channel, .. } => {
                    let identity = self
                        .pq_keys
                        .as_ref()
                        .and_then(|k| k.identity(&self.keypair).ok());
                    if let Some(identity) = identity
                        && let Some(b) = self.swarm.behaviour_mut().pq_identity.as_mut()
                    {
                        let _ = b.send_response(channel, identity);
                    }
                }
                request_response::Message::Response {
                    request_id,
                    response,
                    ..
                } => {
                    if let Some(expected) = self.pending_pq_identity.remove(&request_id) {
                        if response.verify(&expected).is_ok() {
                            self.pq_identities.insert(expected, response);
                        } else {
                            LogStruct::new(
                                LogLevel::Warning,
                                "PQ 身份校验失败",
                                expected.to_string(),
                            )
                            .emit();
                        }
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                self.pending_pq_identity.remove(&request_id);
                LogStruct::new(
                    LogLevel::Debug,
                    "PQ 身份请求失败",
                    format!("{peer}: {error}"),
                )
                .emit();
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                let detail = describe_inbound_failure(
                    peer,
                    self.config.dispatcher_max_request_bytes(),
                    &error,
                );
                LogStruct::new(LogLevel::Debug, "入站 PQ 身份请求失败", detail).emit();
            }
            _ => {}
        }
    }

    /// Handle one command; returns `false` to ask the actor to exit.
    async fn handle_command(&mut self, cmd: SwarmCommand) -> bool {
        match cmd {
            SwarmCommand::KadGetRecord { key, resp } => {
                if let Some(cached) = self.cached_record(&key) {
                    let _ = resp.send(Ok(cached));
                    return true;
                }
                let query_id = match self.kad() {
                    Ok(kad) => kad.get_record(key.clone()),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                self.pending_kad.insert(
                    query_id,
                    KadPending::GetRecord {
                        key,
                        resp,
                        network_only: false,
                    },
                );
            }
            SwarmCommand::KadGetRecordNetwork { key, resp } => {
                // Drop the cached entry so it cannot shadow the network value,
                // then ignore the local store copy while the query runs.
                self.record_cache.remove(&key);
                let query_id = match self.kad() {
                    Ok(kad) => kad.get_record(key.clone()),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                self.pending_kad.insert(
                    query_id,
                    KadPending::GetRecord {
                        key,
                        resp,
                        network_only: true,
                    },
                );
            }
            SwarmCommand::KadGetProviders { key, resp } => {
                let query_id = match self.kad() {
                    Ok(kad) => kad.get_providers(key),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                self.pending_kad.insert(
                    query_id,
                    KadPending::GetProviders {
                        resp,
                        providers: HashSet::new(),
                    },
                );
            }
            SwarmCommand::KadPutRecord { key, value, resp } => {
                let record = kad::Record::new(key.clone(), value.clone());
                let result = match self.kad() {
                    Ok(kad) => kad.put_record(record, kad::Quorum::One),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                match result {
                    Ok(query_id) => {
                        self.pending_kad
                            .insert(query_id, KadPending::PutRecord { key, value, resp });
                    }
                    Err(e) => {
                        let _ = resp.send(Err(NetworkError::Kad(format!("{e:?}"))));
                    }
                }
            }
            SwarmCommand::KadStartProviding { key, resp } => {
                let result = match self.kad() {
                    Ok(kad) => kad.start_providing(key),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                match result {
                    Ok(query_id) => {
                        self.pending_kad
                            .insert(query_id, KadPending::StartProviding(resp));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(NetworkError::Kad(format!("{e:?}"))));
                    }
                }
            }
            SwarmCommand::KadStopProviding { key, resp } => match self.kad() {
                Ok(kad) => {
                    kad.stop_providing(&key);
                    let _ = resp.send(Ok(()));
                }
                Err(e) => {
                    let _ = resp.send(Err(e));
                }
            },
            SwarmCommand::KadRemoveRecord { key, resp } => {
                self.record_cache.remove(&key);
                match self.kad() {
                    Ok(kad) => {
                        // Local, publisher-only operation (see libp2p docs).
                        kad.remove_record(&key);
                        let _ = resp.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(e));
                    }
                }
            }
            SwarmCommand::KadCacheRefresh { resp } => {
                let cleared = self.record_cache.len();
                self.record_cache.clear();
                let _ = resp.send(Ok(cleared));
            }
            SwarmCommand::ServiceSendRequest {
                peer,
                request,
                resp,
            } => {
                let pq_protocol = StreamProtocol::new(pq::SERVICE_REQ_PQ_PROTOCOL);
                let supports_pq = self
                    .peer_protocols
                    .get(&peer)
                    .map(|protocols| protocols.contains(&pq_protocol));
                let security = send_security(
                    self.config.pq_transport_enabled(),
                    self.config.pq_required(),
                    self.pq_keys.is_some(),
                    self.pq_identities.contains_key(&peer),
                    supports_pq,
                );

                let use_pq = match security {
                    SendSecurity::Pq => true,
                    SendSecurity::Plaintext => false,
                    SendSecurity::WaitIdentity => {
                        self.maybe_fetch_pq_identity(peer);
                        let _ = resp.send(Err(NetworkError::Request(
                            "PQ identity not ready; retry after Identify".into(),
                        )));
                        return true;
                    }
                    SendSecurity::MissingKeys => {
                        let _ = resp.send(Err(NetworkError::Request(
                            "PQ transport enabled but local PQ keys are unavailable".into(),
                        )));
                        return true;
                    }
                    SendSecurity::MissingTransport => {
                        let _ = resp.send(Err(NetworkError::Request(
                            "PQ required but transport is disabled".into(),
                        )));
                        return true;
                    }
                    SendSecurity::Unsupported => {
                        let _ = resp.send(Err(NetworkError::Request(
                            "PQ required but peer is not PQ-capable".into(),
                        )));
                        return true;
                    }
                };

                let max_request_bytes = self.config.dispatcher_max_request_bytes();
                let plain_wire_len = match encoded_len(&request) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                if plain_wire_len > max_request_bytes {
                    let _ = resp.send(Err(request_too_large_error(
                        plain_wire_len,
                        max_request_bytes,
                        false,
                    )));
                    return true;
                }

                if use_pq {
                    let recipient = self.pq_identities.get(&peer).cloned().unwrap();
                    let sign = self.config.pq_identity_enabled();
                    let sealed = {
                        let keys = self.pq_keys.as_ref().unwrap();
                        pq::seal_request(
                            keys,
                            &recipient,
                            &encode_request(&request),
                            sign,
                            &self.keypair,
                        )
                    };
                    match sealed {
                        Ok(out) => {
                            let wire_len = match encoded_len(&out.request) {
                                Ok(n) => n,
                                Err(e) => {
                                    let _ = resp.send(Err(e));
                                    return true;
                                }
                            };
                            if wire_len > max_request_bytes {
                                let _ = resp.send(Err(request_too_large_error(
                                    wire_len,
                                    max_request_bytes,
                                    true,
                                )));
                                return true;
                            }
                            if let Some(b) = self.swarm.behaviour_mut().service_req_pq.as_mut() {
                                let id = b.send_request(&peer, out.request.clone());
                                self.pending_outbound_pq.insert(
                                    id,
                                    PqOutboundPending {
                                        resp,
                                        out,
                                        wire_len,
                                    },
                                );
                            } else {
                                let _ = resp.send(Err(NetworkError::Request(
                                    "pq behaviour disabled".into(),
                                )));
                            }
                        }
                        Err(e) => {
                            let _ = resp.send(Err(NetworkError::Request(e.to_string())));
                        }
                    }
                } else {
                    let request_id = self
                        .swarm
                        .behaviour_mut()
                        .service_req
                        .send_request(&peer, request);
                    self.pending_outbound.insert(
                        request_id,
                        PendingOutbound {
                            resp,
                            wire_len: plain_wire_len,
                        },
                    );
                }
            }
            SwarmCommand::ServiceSendResponse {
                request_id,
                response,
            } => {
                if let Some(entry) = self.pending_inbound.remove(&request_id) {
                    let peer = entry.peer;
                    match entry.pending {
                        PendingInbound::Plain(channel) => {
                            let _ = self
                                .swarm
                                .behaviour_mut()
                                .service_req
                                .send_response(channel, response);
                        }
                        PendingInbound::Pq { channel, inbound } => {
                            match inbound.seal_response(&encode_response(&response)) {
                                Ok(resp) => {
                                    if let Some(b) =
                                        self.swarm.behaviour_mut().service_req_pq.as_mut()
                                    {
                                        let _ = b.send_response(channel, resp);
                                    } else {
                                        LogStruct::new(
                                            LogLevel::Warning,
                                            "PQ 响应无法发送",
                                            format!(
                                                "request={request_id} peer={peer}: PQ behaviour 已禁用；该请求将无法回复，对端会收到空响应"
                                            ),
                                        )
                                        .emit();
                                    }
                                }
                                Err(e) => {
                                    LogStruct::new(
                                        LogLevel::Warning,
                                        "PQ 响应加密失败",
                                        format!(
                                            "request={request_id} peer={peer}: {e}；该请求将无法回复，对端会收到空响应"
                                        ),
                                    )
                                    .emit();
                                }
                            }
                        }
                    }
                }
            }
            SwarmCommand::ListenOn { addr, resp } => {
                let result = self
                    .swarm
                    .listen_on(addr)
                    .map(|_| ())
                    .map_err(|e| NetworkError::Transport(e.to_string()));
                let _ = resp.send(result);
            }
            SwarmCommand::Dial { addr, resp } => {
                let result = self
                    .swarm
                    .dial(addr)
                    .map_err(|e| NetworkError::Transport(e.to_string()));
                let _ = resp.send(result);
            }
            SwarmCommand::DisconnectPeer { peer } => {
                let _ = self.swarm.disconnect_peer_id(peer);
            }
            SwarmCommand::Reload { resp } => {
                let result = self.reload().await;
                let _ = resp.send(result);
            }
            SwarmCommand::Shutdown => {
                return false;
            }
        }
        true
    }

    fn reject_inbound(&mut self, pending: PendingInbound) {
        let message = reload_rejection_response();
        match pending {
            PendingInbound::Plain(channel) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .service_req
                    .send_response(channel, message);
            }
            PendingInbound::Pq { channel, inbound } => {
                if let Ok(sealed) = inbound.seal_response(&encode_response(&message))
                    && let Some(behaviour) = self.swarm.behaviour_mut().service_req_pq.as_mut()
                {
                    let _ = behaviour.send_response(channel, sealed);
                }
            }
        }
    }

    /// Reply to inbound requests that are still queued on the old Swarm during
    /// a reload, instead of silently dropping their response channels.
    fn reject_old_swarm_event(&mut self, event: SwarmEvent<NetBehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReq(
                request_response::Event::Message {
                    message: request_response::Message::Request { channel, .. },
                    ..
                },
            )) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .service_req
                    .send_response(channel, reload_rejection_response());
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReqPq(
                request_response::Event::Message {
                    peer,
                    message:
                        request_response::Message::Request {
                            request, channel, ..
                        },
                    ..
                },
            )) => {
                let Some(keys) = self.pq_keys.as_ref() else {
                    LogStruct::new(
                        LogLevel::Warning,
                        "PQ 请求中止",
                        format!(
                            "{peer}: 本地未加载 PQ 密钥；请检查 keypair.pq.bin 与 crypto.pq_* 配置"
                        ),
                    )
                    .emit();
                    return;
                };
                match pq::open_request(keys, &peer, &request) {
                    Ok(inbound) => {
                        if let Ok(sealed) =
                            inbound.seal_response(&encode_response(&reload_rejection_response()))
                            && let Some(behaviour) =
                                self.swarm.behaviour_mut().service_req_pq.as_mut()
                        {
                            let _ = behaviour.send_response(channel, sealed);
                        }
                    }
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "PQ 请求解密失败",
                            format!(
                                "{peer}: {e}；可能对端缓存的 identity 已过期或两端 PQ 密钥不一致"
                            ),
                        )
                        .emit();
                    }
                }
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::PqIdentity(
                request_response::Event::Message {
                    message: request_response::Message::Request { channel, .. },
                    ..
                },
            )) => {
                let identity = self
                    .pq_keys
                    .as_ref()
                    .and_then(|k| k.identity(&self.keypair).ok());
                if let Some(identity) = identity
                    && let Some(behaviour) = self.swarm.behaviour_mut().pq_identity.as_mut()
                {
                    let _ = behaviour.send_response(channel, identity);
                }
            }
            _ => {}
        }
    }

    /// Rebuild the Swarm from the current config. The new Swarm is built first,
    /// so a build failure leaves the old one untouched.
    async fn reload(&mut self) -> Result<(), NetworkError> {
        let new_swarm = build_swarm(&self.config, &self.keypair)?;

        let inbound: Vec<PendingInbound> = self
            .pending_inbound
            .drain()
            .map(|(_, entry)| entry.pending)
            .collect();
        for pending in inbound {
            self.reject_inbound(pending);
        }

        let deadline = tokio::time::sleep(Duration::from_millis(250));
        tokio::pin!(deadline);
        loop {
            let event = tokio::select! {
                _ = &mut deadline => break,
                event = self.swarm.select_next_some() => event,
            };
            self.reject_old_swarm_event(event);
        }

        for (_, pending) in self.pending_kad.drain() {
            pending.fail(NetworkError::Reloaded);
        }
        for (_, pending) in self.pending_outbound.drain() {
            let _ = pending.resp.send(Err(NetworkError::Reloaded));
        }
        for (_, pending) in self.pending_outbound_pq.drain() {
            let _ = pending.resp.send(Err(NetworkError::Reloaded));
        }
        self.pending_pq_identity.clear();
        self.pq_identities.clear();
        self.peer_protocols.clear();
        self.record_cache.clear();

        self.swarm = new_swarm;
        self.bootstrap_triggered = false;

        for addr in self.config.bootstrap_nodes() {
            let _ = self.swarm.dial(addr);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Plaintext encoding (Request/Response inside the PQ envelope)
// ---------------------------------------------------------------------------

/// Request inner encoding: `[u32 service_len][service][payload]`.
fn encode_request(req: &service_protocol::Request) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + req.service.len() + req.payload.len());
    out.extend_from_slice(&(req.service.len() as u32).to_be_bytes());
    out.extend_from_slice(req.service.as_bytes());
    out.extend_from_slice(&req.payload);
    out
}

fn decode_request(bytes: &[u8]) -> Option<service_protocol::Request> {
    let len_bytes = bytes.get(0..4)?;
    let len = u32::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
    let end = 4usize.checked_add(len)?;
    let service = String::from_utf8(bytes.get(4..end)?.to_vec()).ok()?;
    Some(service_protocol::Request {
        service,
        payload: bytes.get(end..)?.to_vec(),
    })
}

/// Response inner encoding: `[u8 success][data]`.
fn encode_response(resp: &service_protocol::Response) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + resp.data.len());
    out.push(resp.success as u8);
    out.extend_from_slice(&resp.data);
    out
}

fn decode_response(bytes: &[u8]) -> Option<service_protocol::Response> {
    let (success, data) = bytes.split_first()?;
    Some(service_protocol::Response {
        success: *success != 0,
        data: data.to_vec(),
    })
}

/// Encode a value with the same codec used on the wire to measure its size.
fn encoded_len<T: serde::Serialize>(value: &T) -> Result<u64, NetworkError> {
    cbor4ii::serde::to_vec(Vec::new(), value)
        .map(|encoded| encoded.len() as u64)
        .map_err(|e| NetworkError::Request(format!("encode request failed: {e}")))
}

fn request_too_large_error(wire_len: u64, limit: u64, pq: bool) -> NetworkError {
    let suffix = if pq {
        "; PQ encrypted envelope overhead is included"
    } else {
        ""
    };
    NetworkError::Request(format!(
        "request too large: {wire_len} bytes > services.dispatcher.max_request_bytes ({limit} bytes){suffix}"
    ))
}

/// Turn a libp2p outbound failure into an actionable error message.
fn describe_outbound_failure(
    pq: bool,
    peer: PeerId,
    protocol: &str,
    wire_len: Option<u64>,
    error: &request_response::OutboundFailure,
) -> String {
    use request_response::OutboundFailure;
    use std::io::ErrorKind;

    let size_hint = wire_len
        .map(|n| format!("；本端请求 {n} 字节"))
        .unwrap_or_default();
    match error {
        OutboundFailure::Io(e) => match e.kind() {
            ErrorKind::UnexpectedEof if pq => format!(
                "对端 {peer} 未返回响应就关闭了 PQ 请求流（可能原因：对端缺少 PQ 密钥、PQ 密钥/身份不匹配、请求超过对端 max_request_bytes 或对端内部错误{size_hint}）"
            ),
            ErrorKind::UnexpectedEof => format!(
                "对端 {peer} 未返回响应就关闭了请求流（可能原因：请求超过对端 max_request_bytes、被对端拒绝或对端内部错误{size_hint}）"
            ),
            ErrorKind::InvalidData | ErrorKind::Unsupported => format!(
                "对端 {peer} 响应解析失败（响应可能被截断、超过 max_response_bytes 或协议版本不兼容）: {e}"
            ),
            _ => format!("对端 {peer} 请求流 I/O 错误: {e}"),
        },
        OutboundFailure::Timeout => format!(
            "对端 {peer} 响应超时（{}s）",
            service_protocol::REQUEST_TIMEOUT.as_secs()
        ),
        OutboundFailure::ConnectionClosed => format!("对端 {peer} 在返回响应前关闭了连接"),
        OutboundFailure::UnsupportedProtocols => format!("对端 {peer} 不支持协议 {protocol}"),
        OutboundFailure::DialFailure => format!("无法与对端 {peer} 建立连接"),
    }
}

/// Turn a libp2p inbound failure into an actionable log message.
fn describe_inbound_failure(
    peer: PeerId,
    max_request_bytes: u64,
    error: &request_response::InboundFailure,
) -> String {
    use request_response::InboundFailure;
    use std::io::ErrorKind;

    match error {
        InboundFailure::Io(e)
            if matches!(
                e.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::InvalidData | ErrorKind::InvalidInput
            ) =>
        {
            format!(
                "{peer}: {error}；请求可能超过 services.dispatcher.max_request_bytes（当前 {max_request_bytes} 字节）"
            )
        }
        InboundFailure::ResponseOmission => {
            format!("{peer}: {error}；响应通道被丢弃，对端会收到空响应")
        }
        _ => format!("{peer}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type ProvidersRx = oneshot::Receiver<Result<Vec<PeerId>, NetworkError>>;

    fn pending_providers() -> (KadPending, ProvidersRx) {
        let (tx, rx) = oneshot::channel();
        (
            KadPending::GetProviders {
                resp: tx,
                providers: HashSet::new(),
            },
            rx,
        )
    }

    fn found(providers: Vec<PeerId>) -> kad::GetProvidersOk {
        kad::GetProvidersOk::FoundProviders {
            key: kad::RecordKey::new(b"k"),
            providers: providers.into_iter().collect(),
        }
    }

    fn finished() -> kad::GetProvidersOk {
        kad::GetProvidersOk::FinishedWithNoAdditionalRecord {
            closest_peers: Vec::new(),
        }
    }

    /// Both the local batch and DHT batches should be merged at finalization,
    /// not just the first batch.
    #[test]
    fn get_providers_accumulates_until_finished() {
        let local = PeerId::random();
        let remote = PeerId::random();
        let (pending, mut rx) = pending_providers();

        let pending =
            apply_get_providers(pending, Ok(found(vec![local]))).expect("本地批次后查询尚未终结");
        assert!(rx.try_recv().is_err(), "终结前不应回执");

        let pending =
            apply_get_providers(pending, Ok(found(vec![remote]))).expect("DHT 批次后查询尚未终结");
        assert!(apply_get_providers(pending, Ok(finished())).is_none());

        let got = rx.try_recv().expect("应已回执").expect("应成功");
        let set: HashSet<PeerId> = got.into_iter().collect();
        assert_eq!(set, HashSet::from([local, remote]));
    }

    /// Duplicate provider reports should be de-duplicated.
    #[test]
    fn get_providers_dedups() {
        let peer = PeerId::random();
        let (pending, mut rx) = pending_providers();
        let pending = apply_get_providers(pending, Ok(found(vec![peer, peer]))).expect("尚未终结");
        assert!(apply_get_providers(pending, Ok(finished())).is_none());

        let got = rx.try_recv().unwrap().unwrap();
        assert_eq!(got, vec![peer]);
    }

    /// With no providers, an empty set should be returned.
    #[test]
    fn get_providers_empty() {
        let (pending, mut rx) = pending_providers();
        assert!(apply_get_providers(pending, Ok(finished())).is_none());
        assert!(rx.try_recv().unwrap().unwrap().is_empty());
    }

    /// A query error should finalize and return the error.
    #[test]
    fn get_providers_error_finalizes() {
        let (pending, mut rx) = pending_providers();
        let err = kad::GetProvidersError::Timeout {
            key: kad::RecordKey::new(b"k"),
            closest_peers: Vec::new(),
        };
        assert!(apply_get_providers(pending, Err(err)).is_none());
        assert!(rx.try_recv().unwrap().is_err());
    }

    #[test]
    fn cacheable_record_ignores_transport_errors() {
        let key = kad::RecordKey::new(b"k");
        let err = kad::GetRecordError::Timeout { key };
        assert_eq!(cacheable_record(&Err(err), false), None);
    }

    #[test]
    fn cacheable_record_positive_on_found() {
        let key = kad::RecordKey::new(b"k");
        let value = b"v".to_vec();
        let ok = kad::GetRecordOk::FoundRecord(kad::PeerRecord {
            peer: None,
            record: kad::Record::new(key, value.clone()),
        });
        assert_eq!(cacheable_record(&Ok(ok), false), Some(Some(value)));
    }

    #[test]
    fn cacheable_record_negative_only_on_regular_miss() {
        let ok = kad::GetRecordOk::FinishedWithNoAdditionalRecord {
            cache_candidates: Default::default(),
        };
        assert_eq!(cacheable_record(&Ok(ok), false), Some(None));

        let ok = kad::GetRecordOk::FinishedWithNoAdditionalRecord {
            cache_candidates: Default::default(),
        };
        assert_eq!(cacheable_record(&Ok(ok), true), None);
    }

    #[test]
    fn send_security_never_downgrades_silently() {
        use SendSecurity::*;

        assert_eq!(send_security(true, false, true, true, Some(true)), Pq);
        assert_eq!(
            send_security(true, false, true, false, Some(true)),
            WaitIdentity
        );
        assert_eq!(send_security(true, false, true, false, None), WaitIdentity);
        assert_eq!(
            send_security(true, false, true, false, Some(false)),
            Plaintext
        );
        assert_eq!(
            send_security(true, false, false, false, Some(true)),
            MissingKeys
        );
        assert_eq!(
            send_security(false, false, true, false, Some(false)),
            Plaintext
        );
        assert_eq!(
            send_security(true, true, true, false, Some(false)),
            Unsupported
        );
        assert_eq!(
            send_security(true, true, true, false, Some(true)),
            WaitIdentity
        );
        assert_eq!(
            send_security(false, true, true, false, Some(true)),
            MissingTransport
        );
        assert_eq!(send_security(true, true, true, true, Some(true)), Pq);
    }

    #[test]
    fn encoded_len_matches_cbor_wire_size() {
        let request = service_protocol::Request {
            service: "cmd".to_string(),
            payload: vec![7; 32],
        };
        let encoded = cbor4ii::serde::to_vec(Vec::new(), &request).unwrap();
        assert_eq!(encoded_len(&request).unwrap() as usize, encoded.len());
    }

    #[test]
    fn request_too_large_error_mentions_config_field() {
        let plain = request_too_large_error(2048, 1024, false).to_string();
        assert!(plain.contains("2048"));
        assert!(plain.contains("services.dispatcher.max_request_bytes"));
        assert!(!plain.contains("PQ"));

        let pq = request_too_large_error(2048, 1024, true).to_string();
        assert!(pq.contains("PQ encrypted envelope overhead"));
    }

    #[test]
    fn outbound_failure_messages_are_actionable() {
        let peer = PeerId::random();
        let eof = request_response::OutboundFailure::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "eof",
        ));

        let plain = describe_outbound_failure(
            false,
            peer,
            service_protocol::SERVICE_REQ_PROTOCOL,
            Some(123),
            &eof,
        );
        assert!(plain.contains("max_request_bytes"));
        assert!(plain.contains("123"));
        assert!(!plain.contains("PQ"));

        let pq =
            describe_outbound_failure(true, peer, pq::SERVICE_REQ_PQ_PROTOCOL, Some(456), &eof);
        assert!(pq.contains("PQ 密钥"));
        assert!(pq.contains("456"));

        let timeout = request_response::OutboundFailure::Timeout;
        assert!(
            describe_outbound_failure(
                false,
                peer,
                service_protocol::SERVICE_REQ_PROTOCOL,
                None,
                &timeout,
            )
            .contains("超时")
        );
    }

    #[test]
    fn inbound_failure_messages_have_hints() {
        let peer = PeerId::random();
        let io = request_response::InboundFailure::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad",
        ));
        assert!(describe_inbound_failure(peer, 4096, &io).contains("max_request_bytes"));

        let omission = request_response::InboundFailure::ResponseOmission;
        assert!(describe_inbound_failure(peer, 4096, &omission).contains("空响应"));
    }
}
