use crate::auth::{self, AuthVersions};
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::{AGENT_NAME, NetworkEvent, NetworkHandle, NetworkStart};
use crate::service_dispatcher::{
    BackendStatus, ControlCommand, InboundRequest, ToController, ToDispatcher,
};
use crate::service_protocol;
use crate::sidecar_protocol::Message;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, identify, kad, ping, relay};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// DHT provider key: announce/discover bootstrap nodes
const BOOTSTRAP_PROVIDER_KEY: &[u8] = b"/oahd/bootstrap";
/// Target number of relay reservations.
const RELAY_TARGET: usize = 3;
/// Grace period before withdrawing a service announcement after a backend drops.
const WITHDRAW_GRACE: Duration = Duration::from_secs(3);

/// Node authentication runtime state.
enum AuthState {
    /// `auth.network == "none"`: authentication disabled.
    Disabled,
    /// Authentication enabled and the authority public key is valid.
    Active(auth::AuthNetwork),
    /// Network set but the authority key is missing/invalid: fail-closed (reject).
    Misconfigured(String),
}

/// Authorization verdict for a single inbound request.
enum Decision {
    Allow,
    /// Service not in the auth index: allow, and sync the local auth requirement off.
    AllowUnprotected,
    Deny(String),
}

/// Resolve the authentication state from config.
fn resolve_auth_state(config: &ConfigHandle) -> AuthState {
    let network = config.auth_network();
    if network == "none" {
        return AuthState::Disabled;
    }
    match config.auth_authority(&network) {
        Some(authority) => match auth::AuthNetwork::new(&network, &authority) {
            Ok(net) => AuthState::Active(net),
            Err(e) => AuthState::Misconfigured(format!("鉴权网络 '{network}' 权威公钥无效: {e}")),
        },
        None => AuthState::Misconfigured(format!("鉴权网络 '{network}' 未配置权威公钥")),
    }
}

pub struct NodeController {
    config: ConfigHandle,
    my_peer_id: PeerId,
    node_rtts: Arc<RwLock<HashMap<PeerId, Duration>>>,
    node_ping_failures: HashMap<PeerId, u32>,
    /// Relays confirmed usable.
    active_relays: HashSet<PeerId>,
    /// Relay reservations started and awaiting confirmation; value is the start time (for timeout).
    pending_relays: HashMap<PeerId, Instant>,
    /// Consecutive relay failure counts.
    relay_failures: HashMap<PeerId, u32>,
    /// Relay backoff deadlines.
    relay_backoff: HashMap<PeerId, Instant>,
    /// Whether this node has successfully announced itself as a bootstrap provider.
    is_bootstrap_provider: bool,
    /// Whether bootstrap completed within this Swarm's lifetime.
    bootstrap_completed: bool,
    /// Local service names whose backend handshake succeeded and are currently serviceable.
    backend_ready: HashSet<String>,
    /// Service names currently announced to the DHT.
    announced_services: HashSet<String>,
    /// Deadlines awaiting withdrawal after a backend drops.
    withdraw_deadline: HashMap<String, Instant>,
    to_controller_rx: mpsc::UnboundedReceiver<ToController>,
    to_dispatcher_tx: mpsc::UnboundedSender<ToDispatcher>,
    swarm: NetworkHandle,
    event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    shutdown_rx: watch::Receiver<bool>,
    /// Authentication runtime state.
    auth_state: AuthState,
    /// Rollback version guard for auth records.
    auth_versions: AuthVersions,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        to_controller_rx: mpsc::UnboundedReceiver<ToController>,
        to_dispatcher_tx: mpsc::UnboundedSender<ToDispatcher>,
        network: NetworkStart,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let node_rtts = Arc::new(RwLock::new(HashMap::new()));
        if let Ok(mut map) = node_rtts.write() {
            map.insert(my_peer_id, Duration::ZERO);
        }

        let auth_state = resolve_auth_state(&config);
        if let AuthState::Misconfigured(reason) = &auth_state {
            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason.clone()).emit();
        }

        let NetworkStart {
            handle: swarm,
            events: event_rx,
            task: _network_task,
        } = network;

        Self {
            config,
            my_peer_id,
            node_rtts,
            node_ping_failures: HashMap::new(),
            active_relays: HashSet::new(),
            pending_relays: HashMap::new(),
            relay_failures: HashMap::new(),
            relay_backoff: HashMap::new(),
            is_bootstrap_provider: false,
            bootstrap_completed: false,
            backend_ready: HashSet::new(),
            announced_services: HashSet::new(),
            withdraw_deadline: HashMap::new(),
            to_controller_rx,
            to_dispatcher_tx,
            swarm,
            event_rx,
            shutdown_rx,
            auth_state,
            auth_versions: AuthVersions::default(),
        }
    }

    /// Run the node orchestration loop over actor events and backend commands.
    pub async fn run(mut self) -> Result<(), Box<dyn Error>> {
        // Periodic top-up: keep trying to reach the relay target even before
        // bootstrap completes or when relays drop.
        let retry = Duration::from_secs(self.config.relay_retry_interval().max(1) as u64);
        let mut relay_tick = tokio::time::interval_at(tokio::time::Instant::now() + retry, retry);
        relay_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Withdrawal check: fixed 1s, only for processing due withdrawals.
        let mut service_tick = tokio::time::interval(Duration::from_secs(1));
        service_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(event) = self.event_rx.recv() => {
                    self.handle_controller_event(event).await?;
                }
                Some(msg) = self.to_controller_rx.recv() => {
                    self.handle_controller_message(msg).await;
                }
                _ = service_tick.tick() => {
                    self.reconcile_services().await;
                }
                _ = relay_tick.tick() => {
                    self.announce_bootstrap_provider().await;
                    self.reconcile_relays().await;
                }
                _ = self.shutdown_rx.changed() => {
                    self.swarm.shutdown();
                    return Ok(());
                }
            }
        }
    }

    async fn handle_controller_event(&mut self, event: NetworkEvent) -> Result<(), Box<dyn Error>> {
        match event {
            NetworkEvent::Ping(event) => self.handle_ping(event).await?,
            NetworkEvent::Identify(event) => self.handle_identify(event).await?,
            NetworkEvent::BootstrapCompleted => {
                self.bootstrap_completed = true;
                self.announce_local_services().await;
                self.announce_bootstrap_provider().await;
                self.reconcile_relays().await;
            }
            NetworkEvent::InboundServiceRequest {
                peer,
                request_id,
                service,
                payload,
                response_tx,
            } => match self.authorize(&peer, &service).await {
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
            NetworkEvent::Relay(event) => self.handle_relay(event).await?,
            NetworkEvent::RelayClient(event) => self.handle_relay_client(event).await?,
            NetworkEvent::ListenerClosed { addresses } => {
                // Relay dropped: try to top up to the target immediately.
                if self.handle_listener_closed(addresses).await {
                    self.reconcile_relays().await;
                }
            }
        }
        Ok(())
    }

    /// Authorization: fetch records from the DHT on demand and verify signatures
    /// (raw bytes are cached by the record cache).
    ///
    /// Blocking and inline: first/expired fetches wait on a DHT query.
    async fn authorize(&mut self, peer: &PeerId, service: &str) -> Decision {
        let net = match &self.auth_state {
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
        if self.auth_versions.check_index(index.version).is_err() {
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
            .auth_versions
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
        if let Err(e) = self.to_dispatcher_tx.send(ToDispatcher::Inbound(inbound)) {
            let err_resp = service_protocol::Response {
                success: false,
                data: format!("service unavailable: {}", e).into_bytes(),
            };
            self.swarm.send_response(request_id, err_resp);
        }
    }

    /// Announce providing a key; returns an error string.
    async fn start_providing_record(&self, key_str: &str) -> Result<(), String> {
        self.swarm
            .start_providing(kad::RecordKey::new(&key_str))
            .await
            .map(|_| ())
            .map_err(|e| format!("start_providing failed: {e}"))
    }

    async fn handle_ping(&mut self, event: ping::Event) -> Result<(), Box<dyn Error>> {
        match event {
            ping::Event {
                peer,
                result: Ok(rtt),
                ..
            } => {
                if let Ok(mut map) = self.node_rtts.write() {
                    map.insert(peer, rtt);
                }
                self.node_ping_failures.remove(&peer);
            }
            ping::Event { peer, .. } => {
                if let Ok(mut map) = self.node_rtts.write() {
                    map.remove(&peer);
                }
                // Disconnect once consecutive failures reach max_failures. The
                // threshold is re-read each time (hot-tunable); 0 means never
                // disconnect.
                let max = self.config.ping_max_failures();
                if max > 0 {
                    let count = self.node_ping_failures.entry(peer).or_insert(0);
                    *count += 1;
                    if *count >= max {
                        self.node_ping_failures.remove(&peer);
                        self.swarm.disconnect_peer(peer);
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_identify(&mut self, event: identify::Event) -> Result<(), Box<dyn Error>> {
        match event {
            identify::Event::Received { peer_id, info, .. } => {
                if info.agent_version.starts_with(AGENT_NAME) {
                    self.add_bootstrap_node_if_new(&info, peer_id).await?;
                } else {
                    // Non-OAHD node; try to disconnect.
                    // Cannot disconnect directly; just log.
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
                    LogLevel::Warning,
                    "Identify错误",
                    format!("{}: {}", peer_id, error),
                )
                .emit();
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_relay(&mut self, event: relay::Event) -> Result<(), Box<dyn Error>> {
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
        Ok(())
    }

    async fn handle_relay_client(
        &mut self,
        event: relay::client::Event,
    ) -> Result<(), Box<dyn Error>> {
        match event {
            relay::client::Event::ReservationReqAccepted {
                relay_peer_id,
                renewal,
                ..
            } => {
                self.pending_relays.remove(&relay_peer_id);
                self.active_relays.insert(relay_peer_id);
                self.relay_failures.remove(&relay_peer_id);
                self.relay_backoff.remove(&relay_peer_id);
                LogStruct::new(
                    LogLevel::Preset,
                    if renewal {
                        "中继预约续期"
                    } else {
                        "中继预约成功"
                    },
                    format!(
                        "{}/{}: {}",
                        self.active_relays.len(),
                        RELAY_TARGET,
                        relay_peer_id
                    ),
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
        Ok(())
    }

    fn get_best_peer(&self, providers: &[PeerId]) -> Option<PeerId> {
        let map = self.node_rtts.read().ok()?;
        providers
            .iter()
            .filter_map(|p| map.get(p).map(|rtt| (p, rtt)))
            .min_by_key(|(_, rtt)| *rtt)
            .map(|(p, _)| *p)
    }

    /// Handle a backend readiness change reported by the ServiceDispatcher.
    async fn handle_backend_status(&mut self, status: BackendStatus) {
        if status.ready {
            self.backend_ready.insert(status.service.clone());
            self.withdraw_deadline.remove(&status.service);
            if self.bootstrap_completed {
                self.announce_local_services().await;
            }
        } else {
            self.backend_ready.remove(&status.service);
            // An announced service that drops enters the grace period; if it
            // reconnects within 3s it is not withdrawn.
            if self.announced_services.contains(&status.service) {
                self.withdraw_deadline
                    .entry(status.service)
                    .or_insert_with(|| Instant::now() + WITHDRAW_GRACE);
            }
        }
    }

    /// Process due withdrawals: announced services whose grace period expired and
    /// whose backend is still not ready get stop_providing.
    async fn reconcile_services(&mut self) {
        let due = due_withdrawals(&self.withdraw_deadline, &self.backend_ready, Instant::now());
        for service in due {
            self.withdraw_deadline.remove(&service);
            self.withdraw_service(&service).await;
        }
    }

    /// Withdraw a single service's DHT announcement. Does not touch the global
    /// `/oahd/service/types` record.
    async fn withdraw_service(&mut self, service: &str) {
        let key = format!("/oahd/service/{}", service);
        match self.swarm.stop_providing(kad::RecordKey::new(&key)).await {
            Ok(()) => {
                self.announced_services.remove(service);
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

    /// Announce local services whose backend is ready and which are not yet
    /// announced, and sync the global service-type record.
    ///
    /// Called only after bootstrap completes and the backend handshake succeeds,
    /// to avoid announcing unavailable services early.
    async fn announce_local_services(&mut self) {
        let targets: Vec<String> = self
            .config
            .read()
            .services
            .dispatcher
            .local_services
            .iter()
            .filter(|s| auth::is_valid_service_name(&s.name))
            .filter(|s| self.backend_ready.contains(&s.name))
            .filter(|s| !self.announced_services.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        if targets.is_empty() {
            return;
        }

        let mut newly = Vec::new();
        for name in &targets {
            let key = format!("/oahd/service/{}", name);
            let record_key = libp2p::kad::RecordKey::new(&key);
            if let Err(e) = self.swarm.start_providing(record_key).await {
                LogStruct::new(
                    LogLevel::Warning,
                    "注册服务失败",
                    format!("{}: {}", name, e),
                )
                .emit();
                continue;
            }
            self.announced_services.insert(name.clone());
            newly.push(name.clone());
            LogStruct::new(LogLevel::Preset, format!("已注册服务: {}", name), "").emit();
        }

        if !newly.is_empty() {
            self.update_service_types().await;
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
        for t in &self.announced_services {
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

    /// Consumer-side relay top-up: best-effort reach the target number of usable
    /// relays.
    ///
    /// Triggers: bootstrap completion, relay listener closed, periodic tick. Soft
    /// target.
    async fn reconcile_relays(&mut self) {
        let need_relay = {
            let cfg = self.config.read();
            !(cfg.network.ipv4_enabled && cfg.network.ipv6_enabled)
        };
        if !need_relay {
            return;
        }

        let target = RELAY_TARGET;
        if target == 0 {
            return;
        }

        let now = Instant::now();
        self.expire_pending_relays(now);

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
            if self.active_relays.len() + self.pending_relays.len() >= desired {
                break;
            }
            if self.active_relays.contains(&peer_id) || self.pending_relays.contains_key(&peer_id) {
                continue;
            }
            if self
                .relay_backoff
                .get(&peer_id)
                .is_some_and(|until| *until > now)
            {
                continue;
            }

            let mut relay_listen_addr = addr.clone();
            relay_listen_addr.push(Protocol::P2pCircuit);
            match self.swarm.listen_on(relay_listen_addr).await {
                Ok(_) => {
                    self.pending_relays.insert(peer_id, now);
                }
                Err(e) => {
                    LogStruct::new(
                        LogLevel::Warning,
                        "中继预约请求失败",
                        format!("{}: {}", peer_id, e),
                    )
                    .emit();
                    self.record_relay_failure(peer_id, now);
                }
            }
        }
    }

    /// Handle a closed relay listener. Returns whether a tracked relay was lost.
    async fn handle_listener_closed(&mut self, addresses: Vec<Multiaddr>) -> bool {
        let mut lost = false;
        for addr in addresses {
            if !addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                continue;
            }
            let Some(peer_id) = extract_peer_id_from_multiaddr(&addr) else {
                continue;
            };
            let was_active = self.active_relays.remove(&peer_id);
            let was_pending = self.pending_relays.remove(&peer_id).is_some();
            if !was_active && !was_pending {
                // Not a tracked reservation; ignore.
                continue;
            }
            lost = true;
            if was_active {
                LogStruct::new(LogLevel::Warning, "中继连接断开", peer_id.to_string()).emit();
            }
            self.record_relay_failure(peer_id, Instant::now());
        }
        lost
    }

    /// Reservations unconfirmed for too long are treated as failed, so pending
    /// entries never hold a slot forever.
    fn expire_pending_relays(&mut self, now: Instant) {
        let timeout = Duration::from_secs(
            (self.config.relay_retry_interval() as u64)
                .saturating_mul(3)
                .max(180),
        );
        let expired: Vec<PeerId> = self
            .pending_relays
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= timeout)
            .map(|(peer, _)| *peer)
            .collect();
        for peer_id in expired {
            self.pending_relays.remove(&peer_id);
            self.record_relay_failure(peer_id, now);
        }
    }

    /// Record a relay failure and set exponential backoff; at the threshold,
    /// evict the node from the bootstrap list.
    fn record_relay_failure(&mut self, peer_id: PeerId, now: Instant) {
        let count = {
            let c = self.relay_failures.entry(peer_id).or_insert(0);
            *c += 1;
            *c
        };

        let backoff = Duration::from_secs(relay_backoff_secs(count));
        self.relay_backoff.insert(peer_id, now + backoff);

        let max = self.config.relay_max_failures();
        if max > 0 && count >= max {
            self.evict_bootstrap(peer_id);
        }
    }

    /// Remove a failed node from the bootstrap list and persist (no floor kept).
    fn evict_bootstrap(&mut self, peer_id: PeerId) {
        let nodes = self.config.bootstrap_nodes();
        let filtered: Vec<Multiaddr> = nodes
            .into_iter()
            .filter(|addr| extract_peer_id_from_multiaddr(addr) != Some(peer_id))
            .collect();
        self.config.set_bootstrap_nodes(filtered);
        self.config.save_to_default();

        self.active_relays.remove(&peer_id);
        self.pending_relays.remove(&peer_id);
        self.relay_failures.remove(&peer_id);
        self.relay_backoff.remove(&peer_id);

        LogStruct::new(
            LogLevel::Warning,
            "移除失效中继",
            format!("已从 bootstrap 列表移除: {}", peer_id),
        )
        .emit();
    }

    /// Announce this node as a bootstrap provider.
    async fn announce_bootstrap_provider(&mut self) {
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
    async fn add_bootstrap_node_if_new(
        &self,
        info: &identify::Info,
        peer_id: PeerId,
    ) -> Result<(), Box<dyn Error>> {
        if !is_dual_stack_addrs(&info.listen_addrs) {
            return Ok(());
        }
        let key = kad::RecordKey::new(&BOOTSTRAP_PROVIDER_KEY);
        let is_provider = matches!(
            self.swarm.get_providers(key).await,
            Ok(providers) if providers.contains(&peer_id)
        );
        if !is_provider {
            return Ok(());
        }
        let candidates = bootstrap_addrs_from(&info.listen_addrs, peer_id);
        if candidates.is_empty() {
            return Ok(());
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
        Ok(())
    }

    /// Handle a message arriving from the ServiceDispatcher.
    async fn handle_controller_message(&mut self, msg: ToController) {
        match msg {
            ToController::Control(cmd) => self.handle_command(cmd).await,
            ToController::BackendStatus(status) => self.handle_backend_status(status).await,
        }
    }

    async fn handle_command(&mut self, req: ControlCommand) {
        let ControlCommand { msg, resp_tx } = req;

        let result: Option<Result<Vec<u8>, String>> = match msg {
            Message::ListServices { .. } => {
                let types_key = kad::RecordKey::new(b"/oahd/service/types");
                match self.swarm.get_record(types_key).await {
                    Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                        let types =
                            serde_json::from_slice::<Vec<String>>(&peer_record.record.value)
                                .unwrap_or_default();
                        Some(Ok(to_cbor(&types)))
                    }
                    _ => Some(Ok(to_cbor(&Vec::<String>::new()))),
                }
            }
            Message::DiscoverProviders { service, .. } => {
                let key = format!("/oahd/service/{}", service);
                let record_key = kad::RecordKey::new(&key);
                match self.swarm.get_providers(record_key).await {
                    Ok(providers) => {
                        let providers: Vec<String> =
                            providers.into_iter().map(|p| p.to_string()).collect();
                        Some(Ok(to_cbor(&providers)))
                    }
                    Err(e) => Some(Err(format!("{e:?}"))),
                }
            }
            Message::QueryPublicIp { .. } => {
                let network = &self.config.read().network;
                let ip_info = serde_json::json!({
                    "ipv4": network.ipv4_address.map(|a| a.to_string()),
                    "ipv6": network.ipv6_address.map(|a| a.to_string()),
                });
                Some(Ok(to_cbor(&ip_info)))
            }
            Message::Whoami { .. } => {
                let node = self.config.read().node.clone();
                let result = WhoamiResult {
                    peer_id: self.my_peer_id.to_string(),
                    name: node.name,
                    description: node.description,
                };
                Some(Ok(to_cbor(&result)))
            }
            Message::ReconnectBootstrap { .. } => {
                let nodes = self.config.bootstrap_nodes();
                let mut any_success = false;
                for addr in &nodes {
                    if self.swarm.dial(addr.clone()).await.is_ok() {
                        any_success = true;
                    }
                }
                let result = serde_json::json!({
                    "success": any_success
                });
                Some(Ok(to_cbor(&result)))
            }
            Message::ReannounceServices { .. } => {
                self.announce_local_services().await;
                let result = serde_json::json!({"success": true});
                Some(Ok(to_cbor(&result)))
            }
            Message::ReloadConfig { .. } => {
                // Rebuild the Swarm from the current config.toml. Note: it drops
                // connections and clears the DHT local store.
                let result = match self.swarm.reload().await {
                    Ok(_) => {
                        // The old Swarm is dropped, invalidating relay reservations
                        // and the bootstrap announcement; clear local tracking.
                        self.active_relays.clear();
                        self.pending_relays.clear();
                        self.relay_failures.clear();
                        self.relay_backoff.clear();
                        self.is_bootstrap_provider = false;
                        // The DHT local store is cleared with the rebuild, so service
                        // announcements are void; re-announce on the next
                        // BootstrapCompleted based on backend_ready.
                        self.announced_services.clear();
                        self.withdraw_deadline.clear();
                        self.bootstrap_completed = false;
                        // Auth state is recomputed from config; version records cleared.
                        self.auth_state = resolve_auth_state(&self.config);
                        if let AuthState::Misconfigured(reason) = &self.auth_state {
                            LogStruct::new(LogLevel::Warning, "鉴权网络配置错误", reason.clone())
                                .emit();
                        }
                        self.auth_versions.clear();
                        serde_json::json!({"success": true})
                    }
                    Err(e) => {
                        serde_json::json!({"success": false, "error": e.to_string()})
                    }
                };
                Some(Ok(to_cbor(&result)))
            }
            Message::RefreshCache { .. } => {
                let result = match self.swarm.refresh_record_cache().await {
                    Ok(cleared) => serde_json::json!({"success": true, "cleared": cleared}),
                    Err(e) => serde_json::json!({"success": false, "error": e.to_string()}),
                };
                Some(Ok(to_cbor(&result)))
            }
            Message::RelayStatus { .. } => {
                let need_relay = {
                    let cfg = self.config.read();
                    !(cfg.network.ipv4_enabled && cfg.network.ipv6_enabled)
                };
                let active: Vec<String> =
                    self.active_relays.iter().map(|p| p.to_string()).collect();
                let pending: Vec<String> =
                    self.pending_relays.keys().map(|p| p.to_string()).collect();
                let result = serde_json::json!({
                    "need_relay": need_relay,
                    "target": RELAY_TARGET,
                    "active": active,
                    "pending": pending,
                });
                Some(Ok(to_cbor(&result)))
            }
            Message::PqStatus { .. } => {
                let result = serde_json::json!({
                    "enabled": self.config.pq_enabled(),
                    "transport": self.config.pq_transport_enabled(),
                    "identity": self.config.pq_identity_enabled(),
                    "required": self.config.pq_required(),
                });
                Some(Ok(to_cbor(&result)))
            }
            Message::AuthStatus { .. } => {
                let network = self.config.auth_network();
                let (state, detail) = match &self.auth_state {
                    AuthState::Disabled => ("disabled", None),
                    AuthState::Active(_) => ("active", None),
                    AuthState::Misconfigured(r) => ("misconfigured", Some(r.clone())),
                };
                let local_required: Vec<String> = self
                    .config
                    .auth_required_services()
                    .into_iter()
                    .filter(|n| auth::is_valid_service_name(n))
                    .collect();

                let mut report = serde_json::json!({
                    "network": network,
                    "state": state,
                    "local_required": local_required.clone(),
                });
                if let Some(d) = detail {
                    report["detail"] = serde_json::json!(d);
                }
                if let Some(v) = self.auth_versions.index_version() {
                    report["index_version"] = serde_json::json!(v);
                }
                let mut whitelist_versions = serde_json::Map::new();
                for name in &local_required {
                    if let Some(v) = self.auth_versions.whitelist_version(name) {
                        whitelist_versions.insert(name.clone(), serde_json::json!(v));
                    }
                }
                report["whitelist_versions"] = serde_json::Value::Object(whitelist_versions);

                Some(Ok(to_cbor(&report)))
            }
            Message::AddKey {
                key,
                value,
                providing,
                ..
            } => {
                if key.is_empty() {
                    Some(Err("missing 'key' field".to_string()))
                } else {
                    let ack = || to_cbor(&serde_json::json!({"success": true, "key": key.clone()}));
                    match value {
                        Some(bytes) => {
                            match self
                                .swarm
                                .put_record(kad::RecordKey::new(&key), bytes)
                                .await
                            {
                                Ok(_) if providing => {
                                    match self.start_providing_record(&key).await {
                                        Ok(_) => Some(Ok(ack())),
                                        Err(e) => Some(Err(e)),
                                    }
                                }
                                Ok(_) => Some(Ok(ack())),
                                Err(e) => Some(Err(format!("put_record failed: {e}"))),
                            }
                        }
                        None if providing => match self.start_providing_record(&key).await {
                            Ok(_) => Some(Ok(ack())),
                            Err(e) => Some(Err(e)),
                        },
                        None => Some(Err(
                            "at least one of 'value' or 'providing' is required".to_string()
                        )),
                    }
                }
            }
            Message::QueryKey { key, .. } => {
                if key.is_empty() {
                    Some(Err("missing key".to_string()))
                } else {
                    match self.swarm.get_record(kad::RecordKey::new(&key)).await {
                        Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                            let value = peer_record.record.value.clone();
                            let providers = self
                                .swarm
                                .get_providers(kad::RecordKey::new(&key))
                                .await
                                .unwrap_or_default();
                            Some(Ok(to_cbor(&QueryKeyResult {
                                key,
                                value: Some(value),
                                providers: providers.into_iter().map(|p| p.to_string()).collect(),
                            })))
                        }
                        _ => {
                            let providers = self
                                .swarm
                                .get_providers(kad::RecordKey::new(&key))
                                .await
                                .unwrap_or_default();
                            if providers.is_empty() {
                                Some(Err("key not found".to_string()))
                            } else {
                                Some(Ok(to_cbor(&QueryKeyResult {
                                    key,
                                    value: None,
                                    providers: providers
                                        .into_iter()
                                        .map(|p| p.to_string())
                                        .collect(),
                                })))
                            }
                        }
                    }
                }
            }
            Message::ServiceRequest {
                service, payload, ..
            } => {
                let key = format!("/oahd/service/{}", service);
                let record_key = kad::RecordKey::new(&key);
                match self.swarm.get_providers(record_key).await {
                    Ok(providers) => {
                        if providers.is_empty() {
                            Some(Err("No provider found".to_string()))
                        } else {
                            let best_peer = self
                                .get_best_peer(&providers)
                                .or_else(|| providers.first().copied())
                                .unwrap();
                            let request = service_protocol::Request { service, payload };
                            match self.swarm.send_request(&best_peer, request).await {
                                Ok(resp) => Some(Ok(resp.data)),
                                Err(e) => Some(Err(e.to_string())),
                            }
                        }
                    }
                    Err(e) => Some(Err(format!("{e:?}"))),
                }
            }
            Message::ServiceRequestTo {
                service,
                peer,
                payload,
                ..
            } => match peer.parse::<PeerId>() {
                Ok(peer_id) => {
                    let request = service_protocol::Request { service, payload };
                    match self.swarm.send_request(&peer_id, request).await {
                        Ok(resp) => Some(Ok(resp.data)),
                        Err(e) => Some(Err(e.to_string())),
                    }
                }
                Err(e) => Some(Err(format!("invalid peer id: {e}"))),
            },
            _ => Some(Err("command not supported".to_string())),
        };

        let _ =
            resp_tx.send(result.unwrap_or_else(|| Err("command produced no result".to_string())));
    }
}

/// `whoami` result payload.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct WhoamiResult {
    peer_id: String,
    name: String,
    description: String,
}

/// `query_key` result payload.
#[derive(serde::Serialize)]
struct QueryKeyResult {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    providers: Vec<String>,
}

/// Serialize a result to CBOR.
fn to_cbor<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).expect("CBOR 编码失败");
    buf
}

fn extract_peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    let iter = addr.iter();
    for proto in iter {
        if let Protocol::P2p(id) = proto {
            return Some(id);
        }
    }
    None
}

/// Select service names whose grace period expired and whose backend is still not
/// ready.
fn due_withdrawals(
    deadlines: &HashMap<String, Instant>,
    ready: &HashSet<String>,
    now: Instant,
) -> Vec<String> {
    deadlines
        .iter()
        .filter(|(svc, dl)| **dl <= now && !ready.contains(*svc))
        .map(|(svc, _)| svc.clone())
        .collect()
}

/// Consecutive failures -> backoff seconds.
fn relay_backoff_secs(count: u32) -> u64 {
    let shift = count.saturating_sub(1).min(5);
    (30u64 << shift).min(600)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn whoami_result_round_trip() {
        let result = WhoamiResult {
            peer_id: "12D3KooW".to_string(),
            name: "节点A".to_string(),
            description: "测试节点".to_string(),
        };
        let bytes = to_cbor(&result);
        let decoded: WhoamiResult = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn due_withdrawals_empty_before_deadline() {
        let now = Instant::now();
        let mut deadlines = HashMap::new();
        deadlines.insert("cmd".to_string(), now + Duration::from_secs(3));
        assert!(due_withdrawals(&deadlines, &HashSet::new(), now).is_empty());
    }

    #[test]
    fn due_withdrawals_includes_expired_unready() {
        let now = Instant::now();
        let mut deadlines = HashMap::new();
        deadlines.insert("cmd".to_string(), now - Duration::from_secs(1));
        let due = due_withdrawals(&deadlines, &HashSet::new(), now);
        assert_eq!(due, vec!["cmd".to_string()]);
    }

    #[test]
    fn due_withdrawals_skips_ready_service() {
        let now = Instant::now();
        let mut deadlines = HashMap::new();
        deadlines.insert("cmd".to_string(), now - Duration::from_secs(1));
        let due = due_withdrawals(&deadlines, &set(&["cmd"]), now);
        assert!(due.is_empty());
    }

    #[test]
    fn due_withdrawals_only_expired_entries() {
        let now = Instant::now();
        let mut deadlines = HashMap::new();
        deadlines.insert("cmd".to_string(), now - Duration::from_secs(5));
        deadlines.insert("ocr".to_string(), now + Duration::from_secs(5));
        let due = due_withdrawals(&deadlines, &HashSet::new(), now);
        assert_eq!(due, vec!["cmd".to_string()]);
    }
}
