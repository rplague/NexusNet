use crate::auth::{self, AuthCache};
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::{NetworkEvent, NetworkHandle, NetworkStart};
use crate::service_dispatcher::{Command, InboundServiceRequest};
use crate::service_protocol;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, identify, kad, ping, relay};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// DHT provider 键：声明/发现 bootstrap 节点。
const BOOTSTRAP_PROVIDER_KEY: &[u8] = b"/oahd/bootstrap";
/// 中继预约目标数量（固定为 3，不可配置）。
const RELAY_TARGET: usize = 3;
/// 鉴权缓存两次刷新之间的最小间隔，避免缓存失效时高频触发 DHT 查询
const AUTH_MIN_REFRESH: Duration = Duration::from_secs(5);

/// 节点鉴权运行状态
enum AuthState {
    /// `auth.network == "none"`，不鉴权
    Disabled,
    /// 鉴权启用且权威公钥有效
    Active(auth::AuthNetwork),
    /// 已设网络但权威公钥缺失/非法：失败即拒绝（fail-closed）
    Misconfigured(String),
}

/// 单次入站请求的鉴权结论
enum Decision {
    Allow,
    /// 服务不在鉴权索引中：放行，并将本地该服务的鉴权要求同步为关闭
    AllowUnprotected,
    Deny(String),
}

/// 由配置解析鉴权状态
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
    /// 已确认可用的中继。
    active_relays: HashSet<PeerId>,
    /// 已发起、等待确认的中继预约，值为发起时间（用于超时判定）。
    pending_relays: HashMap<PeerId, Instant>,
    /// 中继连续失败计数。
    relay_failures: HashMap<PeerId, u32>,
    /// 中继退避截止时间。
    relay_backoff: HashMap<PeerId, Instant>,
    /// 缓存的 bootstrap 提供者集合，来自 DHT get_providers。
    bootstrap_providers: HashSet<PeerId>,
    /// 自身是否已成功声明为 bootstrap 提供者。
    is_bootstrap_provider: bool,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
    swarm: NetworkHandle,
    event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    shutdown_rx: watch::Receiver<bool>,
    /// 鉴权运行状态（构造与 reload 时解析）
    auth_state: AuthState,
    /// 与后台刷新任务共享的鉴权缓存
    auth_cache: Arc<RwLock<AuthCache>>,
    /// 唤醒后台刷新任务
    auth_notify: Arc<Notify>,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        cmd_rx: mpsc::UnboundedReceiver<Command>,
        inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
        network: NetworkStart,
        shutdown_rx: watch::Receiver<bool>,
        auth_cache: Arc<RwLock<AuthCache>>,
        auth_notify: Arc<Notify>,
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
            bootstrap_providers: HashSet::new(),
            is_bootstrap_provider: false,
            cmd_rx,
            inbound_req_tx,
            swarm,
            event_rx,
            shutdown_rx,
            auth_state,
            auth_cache,
            auth_notify,
        }
    }

    /// 运行节点编排循环，接收 SwarmActor 转发的事件 + backend 命令
    pub async fn run(mut self) -> Result<(), Box<dyn Error>> {
        // 周期补约：即使 bootstrap 未完成或中继掉线，也会持续尝试补足目标数量。
        let retry = Duration::from_secs(self.config.relay_retry_interval().max(1) as u64);
        let mut relay_tick = tokio::time::interval_at(tokio::time::Instant::now() + retry, retry);
        relay_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(event) = self.event_rx.recv() => {
                    self.handle_controller_event(event).await?;
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = relay_tick.tick() => {
                    self.announce_bootstrap_provider().await;
                    self.refresh_bootstrap_providers().await;
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
                self.announce_local_services().await?;
                self.announce_bootstrap_provider().await;
                self.refresh_bootstrap_providers().await;
                self.reconcile_relays().await;
                self.auth_notify.notify_one();
            }
            NetworkEvent::InboundServiceRequest {
                peer,
                request_id,
                service,
                payload,
                response_tx,
            } => match self.authorize(&peer, &service) {
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
                    self.auth_notify.notify_one();
                }
            },
            NetworkEvent::Relay(event) => self.handle_relay(event).await?,
            NetworkEvent::RelayClient(event) => self.handle_relay_client(event).await?,
            NetworkEvent::ListenerClosed { addresses } => {
                // 中继掉线：立即尝试补足目标数量。
                if self.handle_listener_closed(addresses).await {
                    self.reconcile_relays().await;
                }
            }
        }
        Ok(())
    }

    /// 鉴权判定：只读本地缓存，同步返回，不阻塞事件循环
    fn authorize(&self, peer: &PeerId, service: &str) -> Decision {
        match &self.auth_state {
            AuthState::Disabled => return Decision::Allow,
            AuthState::Misconfigured(reason) => return Decision::Deny(reason.clone()),
            AuthState::Active(_) => {}
        }
        if !self.config.service_requires_auth(service) {
            return Decision::Allow;
        }

        let now = auth::now_unix();
        let ttl = Duration::from_secs(self.config.auth_cache_ttl() as u64);
        let Ok(cache) = self.auth_cache.read() else {
            return Decision::Deny("auth cache poisoned".to_string());
        };
        let Some(index) = cache.fresh_index(now, ttl) else {
            return Decision::Deny("auth index unavailable".to_string());
        };
        if !index.services.contains_key(service) {
            return Decision::AllowUnprotected;
        }
        let Some(whitelist) = cache.fresh_whitelist(service, ttl) else {
            return Decision::Deny("whitelist unavailable".to_string());
        };
        if whitelist.is_member(peer) {
            Decision::Allow
        } else {
            Decision::Deny("peer not authorized".to_string())
        }
    }

    /// 将入站请求转发给 ServiceDispatcher
    fn forward_inbound(
        &self,
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    ) {
        let inbound = InboundServiceRequest {
            service,
            payload,
            response_tx,
        };
        if let Err(e) = self.inbound_req_tx.send(inbound) {
            let err_resp = service_protocol::Response {
                success: false,
                data: format!("service unavailable: {}", e).into_bytes(),
            };
            self.swarm.send_response(request_id, err_resp);
        }
    }

    /// 声明提供某 key，返回错误字符串
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
                // 连续失败达 max_failures 时主动断开。阈值每次读配置，支持热调；0 表示不断连。
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
                if info.agent_version.starts_with("/oahd/") {
                    self.add_bootstrap_node_if_new(&info, peer_id).await?;
                } else {
                    // 非 OAHD 节点，尝试断开
                    // 无法直接断开，仅记录
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

    async fn announce_local_services(&mut self) -> Result<(), Box<dyn Error>> {
        let local_services: Vec<_> = self
            .config
            .read()
            .services
            .dispatcher
            .local_services
            .iter()
            .filter(|s| auth::is_valid_service_name(&s.name))
            .cloned()
            .collect();
        if local_services.is_empty() {
            return Ok(());
        }

        for local_service in &local_services {
            let key = format!("/oahd/service/{}", local_service.name);
            let record_key = libp2p::kad::RecordKey::new(&key);
            if let Err(e) = self.swarm.start_providing(record_key).await {
                LogStruct::new(
                    LogLevel::Warning,
                    "注册服务失败",
                    format!("{}: {}", local_service.name, e),
                )
                .emit();
                continue;
            }
            LogStruct::new(
                LogLevel::Preset,
                format!("已注册服务: {}", local_service.name),
                "",
            )
            .emit();
        }

        // 合并服务类型列表并写回 DHT
        let my_types: Vec<String> = local_services.iter().map(|s| s.name.clone()).collect();
        let types_key = kad::RecordKey::new(b"/oahd/service/types");

        let existing_types = match self.swarm.get_record(types_key.clone()).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                serde_json::from_slice::<Vec<String>>(&peer_record.record.value).unwrap_or_default()
            }
            _ => Vec::new(),
        };

        let mut all_types = existing_types;
        for t in &my_types {
            if !all_types.contains(t) {
                all_types.push(t.clone());
            }
        }

        if let Ok(types_json) = serde_json::to_vec(&all_types)
            && let Err(e) = self.swarm.put_record(types_key, types_json).await
        {
            LogStruct::new(LogLevel::Warning, "更新服务类型列表失败", e.to_string()).emit();
        }

        Ok(())
    }

    /// 消费者侧中继补约：尽力把可用中继补足到目标数量。
    ///
    /// 触发点：bootstrap 完成、中继监听器关闭、周期 tick。软目标，不强求。
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

        // 去重候选：peer_id -> 首个地址
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

    /// 处理中继监听器关闭。返回是否有被跟踪的中继因此失效。
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
                // 非本次跟踪的预约，忽略。
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

    /// 超过一定时间仍未确认的预约视为失败，避免 pending 永久占用名额。
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

    /// 记录一次中继失败并设置指数退避；达到阈值则从 bootstrap 剔除该节点。
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

    /// 从 bootstrap 列表移除失效节点并持久化（不保底）。
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

    /// 声明自身为 bootstrap 提供者（仅双栈 + allow_bootstrap 时）。
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

    /// 刷新缓存的 bootstrap 提供者集合（失败则保留旧缓存）。
    async fn refresh_bootstrap_providers(&mut self) {
        let key = kad::RecordKey::new(&BOOTSTRAP_PROVIDER_KEY);
        match self.swarm.get_providers(key).await {
            Ok(providers) => {
                self.bootstrap_providers = providers.into_iter().collect();
            }
            Err(e) => {
                LogStruct::new(LogLevel::Debug, "刷新 bootstrap 提供者失败", e.to_string()).emit();
            }
        }
    }

    /// Identify 命中后，按「先双栈、再 bootstrap」判定是否加入本地 bootstrap 列表。
    ///
    /// 地址一律取自 Identify 信息，无需 DHT 存地址。
    async fn add_bootstrap_node_if_new(
        &self,
        info: &identify::Info,
        peer_id: PeerId,
    ) -> Result<(), Box<dyn Error>> {
        // ① 先校验双栈
        if !is_dual_stack_addrs(&info.listen_addrs) {
            return Ok(());
        }
        // ② 再确认对方是 bootstrap 提供者
        if !self.bootstrap_providers.contains(&peer_id) {
            return Ok(());
        }
        // ③ 用 Identify 地址写入（v4、v6 各一个）
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
                LogLevel::Debug,
                "配置更新",
                format!("添加 bootstrap 节点: {}", peer_id),
            )
            .emit();
        }
        Ok(())
    }

    async fn handle_command(&mut self, cmd: Command) {
        let Command {
            prefix,
            content,
            payload,
            resp_tx,
        } = cmd;

        let result: Option<Result<Vec<u8>, String>> = match prefix.as_str() {
            "@" => match content.as_str() {
                "list_services" => {
                    let types_key = kad::RecordKey::new(b"/oahd/service/types");
                    match self.swarm.get_record(types_key).await {
                        Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                            let types =
                                serde_json::from_slice::<Vec<String>>(&peer_record.record.value)
                                    .unwrap_or_default();
                            Some(Ok(serde_json::to_vec(&types).unwrap()))
                        }
                        _ => Some(Ok(serde_json::to_vec::<Vec<String>>(&vec![]).unwrap())),
                    }
                }
                "discover_providers" => {
                    let service_type = String::from_utf8(payload).unwrap_or_default();
                    let key = format!("/oahd/service/{}", service_type);
                    let record_key = kad::RecordKey::new(&key);
                    match self.swarm.get_providers(record_key).await {
                        Ok(providers) => Some(Ok(serde_json::to_vec(&providers).unwrap())),
                        Err(e) => Some(Err(format!("{e:?}"))),
                    }
                }
                "query_public_ip" => {
                    let network = &self.config.read().network;
                    let ip_info = serde_json::json!({
                        "ipv4": network.ipv4_address.map(|a| a.to_string()),
                        "ipv6": network.ipv6_address.map(|a| a.to_string()),
                    });
                    Some(Ok(serde_json::to_vec(&ip_info).unwrap()))
                }
                "reconnect_bootstrap" => {
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
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "reannounce_services" => {
                    let result = match self.announce_local_services().await {
                        Ok(_) => serde_json::json!({"success": true}),
                        Err(e) => {
                            serde_json::json!({"success": false, "error": e.to_string()})
                        }
                    };
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "reload_config" => {
                    // 用当前 config.toml 重建 Swarm。注意：会瞬断连接并清空 DHT 本地存储。
                    let result = match self.swarm.reload().await {
                        Ok(_) => {
                            // 旧 Swarm 已丢弃，中继预约与 bootstrap 声明随之失效，清空本地跟踪。
                            self.active_relays.clear();
                            self.pending_relays.clear();
                            self.relay_failures.clear();
                            self.relay_backoff.clear();
                            self.bootstrap_providers.clear();
                            self.is_bootstrap_provider = false;
                            // 鉴权状态随配置重算，缓存清空后由刷新任务重建
                            self.auth_state = resolve_auth_state(&self.config);
                            if let AuthState::Misconfigured(reason) = &self.auth_state {
                                LogStruct::new(
                                    LogLevel::Warning,
                                    "鉴权网络配置错误",
                                    reason.clone(),
                                )
                                .emit();
                            }
                            if let Ok(mut cache) = self.auth_cache.write() {
                                cache.clear();
                            }
                            self.auth_notify.notify_one();
                            serde_json::json!({"success": true})
                        }
                        Err(e) => {
                            serde_json::json!({"success": false, "error": e.to_string()})
                        }
                    };
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "relay_status" => {
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
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "pq_status" => {
                    let result = serde_json::json!({
                        "enabled": self.config.pq_enabled(),
                        "transport": self.config.pq_transport_enabled(),
                        "identity": self.config.pq_identity_enabled(),
                        "required": self.config.pq_required(),
                    });
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "auth_status" => {
                    let network = self.config.auth_network();
                    let (state, detail) = match &self.auth_state {
                        AuthState::Disabled => ("disabled", None),
                        AuthState::Active(_) => ("active", None),
                        AuthState::Misconfigured(r) => ("misconfigured", Some(r.clone())),
                    };
                    let now = auth::now_unix();
                    let ttl = Duration::from_secs(self.config.auth_cache_ttl() as u64);
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

                    if let Ok(cache) = self.auth_cache.read() {
                        if let Some(idx) = cache.index() {
                            report["index"] = serde_json::json!({
                                "version": idx.version,
                                "expires_at": idx.expires_at,
                                "age_secs": idx.fetched_at.elapsed().as_secs(),
                                "services": idx.services.len(),
                                "fresh": idx.is_fresh(now, ttl),
                            });
                        }
                        let mut whitelists = serde_json::Map::new();
                        for name in &local_required {
                            if let Some(w) = cache.whitelist(name) {
                                whitelists.insert(
                                    name.clone(),
                                    serde_json::json!({
                                        "version": w.version,
                                        "members": w.members.len(),
                                        "age_secs": w.fetched_at.elapsed().as_secs(),
                                    }),
                                );
                            }
                        }
                        report["cached_whitelists"] = serde_json::Value::Object(whitelists);

                        let not_in_index: Vec<String> = match cache.index() {
                            Some(idx) => local_required
                                .iter()
                                .filter(|n| !idx.services.contains_key(*n))
                                .cloned()
                                .collect(),
                            None => local_required.clone(),
                        };
                        report["required_but_not_in_index"] = serde_json::json!(not_in_index);
                    }

                    Some(Ok(serde_json::to_vec(&report).unwrap()))
                }
                "add_key" => match serde_json::from_slice::<serde_json::Value>(&payload) {
                    Ok(json) => {
                        let key_str = json["key"].as_str().unwrap_or_default().to_string();
                        if key_str.is_empty() {
                            Some(Err("missing 'key' field".to_string()))
                        } else {
                            let providing = json
                                .get("providing")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let ack = || {
                                serde_json::to_vec(&serde_json::json!({
                                    "success": true,
                                    "key": key_str.clone(),
                                }))
                                .unwrap()
                            };

                            match parse_add_key_value(&json) {
                                Err(e) => Some(Err(e)),
                                Ok(None) if !providing => Some(Err(
                                    "at least one of 'value', 'value_b64' or 'providing' is required"
                                        .to_string(),
                                )),
                                Ok(None) => match self.start_providing_record(&key_str).await {
                                    Ok(_) => Some(Ok(ack())),
                                    Err(e) => Some(Err(e)),
                                },
                                Ok(Some(bytes)) => {
                                    let key = kad::RecordKey::new(&key_str);
                                    match self.swarm.put_record(key, bytes).await {
                                        Ok(_) if providing => {
                                            match self.start_providing_record(&key_str).await {
                                                Ok(_) => Some(Ok(ack())),
                                                Err(e) => Some(Err(e)),
                                            }
                                        }
                                        Ok(_) => Some(Ok(ack())),
                                        Err(e) => Some(Err(format!("put_record failed: {e}"))),
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => Some(Err(format!("invalid JSON: {e}"))),
                },
                "query_key" => {
                    let key_str = String::from_utf8_lossy(&payload).to_string();
                    if key_str.is_empty() {
                        Some(Err("missing key".to_string()))
                    } else {
                        let key = kad::RecordKey::new(&key_str);
                        match self.swarm.get_record(key.clone()).await {
                            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                                let value =
                                    String::from_utf8_lossy(&peer_record.record.value).into_owned();
                                let value_b64 = STANDARD.encode(&peer_record.record.value);
                                let record_key = kad::RecordKey::new(&key_str);
                                match self.swarm.get_providers(record_key).await {
                                    Ok(providers) => {
                                        let mut resp = serde_json::json!({
                                            "key": key_str,
                                            "value": value,
                                            "value_b64": value_b64,
                                        });
                                        if !providers.is_empty() {
                                            resp["providers"] = serde_json::json!(providers);
                                        }
                                        Some(Ok(serde_json::to_vec(&resp).unwrap()))
                                    }
                                    Err(_) => {
                                        let resp = serde_json::json!({
                                            "key": key_str,
                                            "value": value,
                                            "value_b64": value_b64,
                                        });
                                        Some(Ok(serde_json::to_vec(&resp).unwrap()))
                                    }
                                }
                            }
                            _ => {
                                let record_key = kad::RecordKey::new(&key_str);
                                match self.swarm.get_providers(record_key).await {
                                    Ok(providers) => {
                                        let mut resp = serde_json::json!({
                                            "key": key_str,
                                        });
                                        if !providers.is_empty() {
                                            resp["providers"] = serde_json::json!(providers);
                                        }
                                        Some(Ok(serde_json::to_vec(&resp).unwrap()))
                                    }
                                    Err(_) => Some(Err("key not found".to_string())),
                                }
                            }
                        }
                    }
                }
                _ => Some(Err("Unknown command".to_string())),
            },
            "service_request" => {
                let key = format!("/oahd/service/{}", content);
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
                            let request = service_protocol::Request {
                                service: content,
                                payload,
                            };
                            match self.swarm.send_request(&best_peer, request).await {
                                Ok(resp) => Some(Ok(resp.data)),
                                Err(e) => Some(Err(e.to_string())),
                            }
                        }
                    }
                    Err(e) => Some(Err(format!("{e:?}"))),
                }
            }
            "service_request_to" => {
                let parts: Vec<&str> = content.splitn(2, "/in").collect();
                if parts.len() != 2 {
                    Some(Err("expected format: <service>/in<peerid>".to_string()))
                } else {
                    let service = parts[0].to_string();
                    match parts[1].parse::<PeerId>() {
                        Ok(peer_id) => {
                            let request = service_protocol::Request { service, payload };
                            match self.swarm.send_request(&peer_id, request).await {
                                Ok(resp) => Some(Ok(resp.data)),
                                Err(e) => Some(Err(e.to_string())),
                            }
                        }
                        Err(e) => Some(Err(format!("invalid peer id: {e}"))),
                    }
                }
            }
            _ => Some(Err("command not supported".to_string())),
        };

        let _ =
            resp_tx.send(result.unwrap_or_else(|| Err("command produced no result".to_string())));
    }
}

/// 鉴权缓存后台刷新任务：周期刷新，并在被 `Notify` 唤醒时刷新
pub async fn auth_refresher(
    config: ConfigHandle,
    swarm: NetworkHandle,
    cache: Arc<RwLock<AuthCache>>,
    notify: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let interval = Duration::from_secs(config.auth_refresh_interval().max(1) as u64);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = Instant::now()
        .checked_sub(AUTH_MIN_REFRESH)
        .unwrap_or_else(Instant::now);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = notify.notified() => {}
            _ = shutdown.changed() => break,
        }
        if last.elapsed() < AUTH_MIN_REFRESH {
            tokio::time::sleep(AUTH_MIN_REFRESH - last.elapsed()).await;
        }
        refresh_auth_once(&config, &swarm, &cache).await;
        last = Instant::now();
    }
}

/// 取回并校验索引与所需白名单并写入缓存；失败保留旧缓存（自然过期后失败即拒绝）
async fn refresh_auth_once(
    config: &ConfigHandle,
    swarm: &NetworkHandle,
    cache: &Arc<RwLock<AuthCache>>,
) {
    let net = match resolve_auth_state(config) {
        AuthState::Disabled => return,
        AuthState::Active(net) => net,
        AuthState::Misconfigured(_) => return,
    };

    let index_key = kad::RecordKey::new(&net.index_key());
    let doc = match swarm.get_record(index_key).await {
        Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
            let now = auth::now_unix();
            match auth::verify_index(&net, &peer_record.record.value, now) {
                Ok(doc) => doc,
                Err(e) => {
                    LogStruct::new(LogLevel::Warning, "鉴权索引校验失败", e.to_string()).emit();
                    return;
                }
            }
        }
        Ok(_) => {
            LogStruct::new(LogLevel::Warning, "鉴权索引未找到", net.name.clone()).emit();
            return;
        }
        Err(e) => {
            LogStruct::new(LogLevel::Warning, "鉴权索引查询失败", e.to_string()).emit();
            return;
        }
    };

    let services = doc.services.clone();
    if let Ok(mut c) = cache.write()
        && let Err(e) = c.insert_index(doc)
    {
        LogStruct::new(LogLevel::Warning, "鉴权索引写入缓存失败", e.to_string()).emit();
    }

    for entry in services {
        if !config.service_requires_auth(&entry.name) {
            continue;
        }
        let key = match net.service_key(&entry.name) {
            Ok(k) => kad::RecordKey::new(&k),
            Err(_) => continue,
        };
        match swarm.get_record(key).await {
            Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                match auth::verify_whitelist(&net, &entry.name, &peer_record.record.value, &entry) {
                    Ok(doc) => {
                        if let Ok(mut c) = cache.write()
                            && let Err(e) = c.insert_whitelist(&entry.name, doc)
                        {
                            LogStruct::new(
                                LogLevel::Warning,
                                "白名单写入缓存失败",
                                format!("{}: {}", entry.name, e),
                            )
                            .emit();
                        }
                    }
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "白名单校验失败",
                            format!("{}: {}", entry.name, e),
                        )
                        .emit();
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                LogStruct::new(
                    LogLevel::Warning,
                    "白名单查询失败",
                    format!("{}: {}", entry.name, e),
                )
                .emit();
            }
        }
    }
}

/// 解析 `add_key` 的 `value` / `value_b64`，返回待写入字节
///
/// 二者都存在时优先 `value`（UTF-8 文本）；均缺省返回 `Ok(None)`
fn parse_add_key_value(json: &serde_json::Value) -> Result<Option<Vec<u8>>, String> {
    if let Some(v) = json.get("value") {
        return v
            .as_str()
            .map(|s| Some(s.as_bytes().to_vec()))
            .ok_or_else(|| "'value' must be a string".to_string());
    }
    if let Some(v) = json.get("value_b64") {
        let s = v
            .as_str()
            .ok_or_else(|| "'value_b64' must be a string".to_string())?;
        return STANDARD
            .decode(s)
            .map(Some)
            .map_err(|e| format!("invalid base64 in 'value_b64': {e}"));
    }
    Ok(None)
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

/// 连续失败次数 → 退避秒数（指数退避，上限 10 分钟）。
fn relay_backoff_secs(count: u32) -> u64 {
    let shift = count.saturating_sub(1).min(5);
    (30u64 << shift).min(600)
}

/// 地址列表是否同时包含 IPv4 与 IPv6（双栈）。
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

/// 从 Identify 的监听地址中为每个 IP 族各取一个，补上 `/p2p/<peer_id>`。
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
    use super::{
        bootstrap_addrs_from, is_dual_stack_addrs, parse_add_key_value, relay_backoff_secs,
    };
    use libp2p::multiaddr::Protocol;
    use libp2p::{Multiaddr, PeerId};
    use std::str::FromStr;

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        assert_eq!(relay_backoff_secs(1), 30);
        assert_eq!(relay_backoff_secs(2), 60);
        assert_eq!(relay_backoff_secs(3), 120);
        assert_eq!(relay_backoff_secs(4), 240);
        assert_eq!(relay_backoff_secs(5), 480);
        assert_eq!(relay_backoff_secs(6), 600);
        assert_eq!(relay_backoff_secs(100), 600);
    }

    #[test]
    fn dual_stack_detection() {
        let v4 = Multiaddr::from_str("/ip4/1.2.3.4/tcp/5000").unwrap();
        let v6 = Multiaddr::from_str("/ip6/2001:db8::1/tcp/5000").unwrap();
        assert!(!is_dual_stack_addrs(&[v4.clone()]));
        assert!(!is_dual_stack_addrs(&[v6.clone()]));
        assert!(is_dual_stack_addrs(&[v4, v6]));
    }

    #[test]
    fn bootstrap_addrs_picks_one_per_family_with_peer_id() {
        let pid = peer();
        let v4 = Multiaddr::from_str("/ip4/1.2.3.4/tcp/5000").unwrap();
        let v6a = Multiaddr::from_str("/ip6/2001:db8::1/tcp/5000").unwrap();
        let v6b = Multiaddr::from_str("/ip6/2001:db8::2/tcp/5000").unwrap();

        let addrs = bootstrap_addrs_from(&[v4, v6a, v6b], pid);
        assert_eq!(addrs.len(), 2);
        assert!(addrs.iter().all(|a| {
            a.iter()
                .any(|p| matches!(p, Protocol::P2p(id) if id == pid))
        }));
        assert_eq!(
            addrs
                .iter()
                .filter(|a| a.iter().any(|p| matches!(p, Protocol::Ip4(_))))
                .count(),
            1
        );
        assert_eq!(
            addrs
                .iter()
                .filter(|a| a.iter().any(|p| matches!(p, Protocol::Ip6(_))))
                .count(),
            1
        );
    }

    #[test]
    fn parse_add_key_value_prefers_text() {
        let v = serde_json::json!({"value": "hi", "value_b64": "aGk="});
        assert_eq!(parse_add_key_value(&v).unwrap(), Some(b"hi".to_vec()));
    }

    #[test]
    fn parse_add_key_value_decodes_b64() {
        let v = serde_json::json!({"value_b64": "aGk="});
        assert_eq!(parse_add_key_value(&v).unwrap(), Some(b"hi".to_vec()));
    }

    #[test]
    fn parse_add_key_value_none() {
        let v = serde_json::json!({"key": "k"});
        assert_eq!(parse_add_key_value(&v).unwrap(), None);
    }

    #[test]
    fn parse_add_key_value_rejects_bad_base64() {
        let v = serde_json::json!({"value_b64": "!!!"});
        assert!(parse_add_key_value(&v).is_err());
    }

    #[test]
    fn parse_add_key_value_rejects_non_string() {
        let v = serde_json::json!({"value": 123});
        assert!(parse_add_key_value(&v).is_err());
    }
}
