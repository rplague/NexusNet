use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::net::NetHandle;
use crate::service_dispatcher::{Command, InboundServiceRequest};
use crate::service_protocol;
use crate::swarm_actor::{ControllerEvent, SwarmHandle};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, identify, kad, ping, relay};
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct NodeController {
    config: ConfigHandle,
    my_peer_id: PeerId,
    node_rtts: Arc<RwLock<HashMap<PeerId, Duration>>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
    swarm: SwarmHandle,
    event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        cmd_rx: mpsc::UnboundedReceiver<Command>,
        inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
        net_handle: NetHandle,
    ) -> Self {
        let node_rtts = Arc::new(RwLock::new(HashMap::new()));
        if let Ok(mut map) = node_rtts.write() {
            map.insert(my_peer_id, Duration::ZERO);
        }

        let (swarm, event_rx, _actor_handle) = net_handle.spawn_actor();

        Self {
            config,
            my_peer_id,
            node_rtts,
            cmd_rx,
            inbound_req_tx,
            swarm,
            event_rx,
        }
    }

    /// 运行节点编排循环，接收 SwarmActor 转发的事件 + backend 命令
    pub async fn run(mut self) -> Result<(), Box<dyn Error>> {
        loop {
            tokio::select! {
                Some(event) = self.event_rx.recv() => {
                    self.handle_controller_event(event).await?;
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd).await;
                }
            }
        }
    }

    async fn handle_controller_event(
        &mut self,
        event: ControllerEvent,
    ) -> Result<(), Box<dyn Error>> {
        match event {
            ControllerEvent::Ping(event) => self.handle_ping(event).await?,
            ControllerEvent::Identify(event) => self.handle_identify(event).await?,
            ControllerEvent::BootstrapCompleted => {
                self.announce_local_services().await?;
                self.request_relay_reservation_if_needed().await?;
            }
            ControllerEvent::InboundServiceRequest {
                request_id,
                service,
                payload,
                response_tx,
            } => {
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
            ControllerEvent::Relay(event) => self.handle_relay(event).await?,
            ControllerEvent::RelayClient(event) => self.handle_relay_client(event).await?,
        }
        Ok(())
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
            }
            ping::Event { peer, .. } => {
                if let Ok(mut map) = self.node_rtts.write() {
                    map.remove(&peer);
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
            relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } => {
                LogStruct::new(
                    LogLevel::Preset,
                    "中继预约成功",
                    format!("中继节点: {}", relay_peer_id),
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
        let local_services = self
            .config
            .read()
            .services
            .dispatcher
            .local_services
            .clone();
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

        if let Ok(types_json) = serde_json::to_vec(&all_types) {
            if let Err(e) = self.swarm.put_record(types_key, types_json).await {
                LogStruct::new(LogLevel::Warning, "更新服务类型列表失败", e).emit();
            }
        }

        Ok(())
    }

    async fn request_relay_reservation_if_needed(&self) -> Result<(), Box<dyn Error>> {
        let need_relay = {
            let cfg = self.config.read();
            !(cfg.network.ipv4_enabled && cfg.network.ipv6_enabled)
        };
        if !need_relay {
            return Ok(());
        }

        let candidates = self.config.bootstrap_nodes();
        for addr in &candidates {
            if let Some(peer_id) = extract_peer_id_from_multiaddr(addr) {
                if peer_id == self.my_peer_id {
                    continue;
                }
                let mut relay_listen_addr = addr.clone();
                relay_listen_addr.push(Protocol::P2pCircuit);
                match self.swarm.listen_on(relay_listen_addr).await {
                    Ok(_) => {
                        LogStruct::new(
                            LogLevel::Preset,
                            "请求中继预约",
                            format!("中继节点: {}", peer_id),
                        )
                        .emit();
                        return Ok(());
                    }
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "中继预约失败",
                            format!("{}: {}", peer_id, e),
                        )
                        .emit();
                    }
                }
            }
        }
        LogStruct::new(
            LogLevel::Warning,
            "无可用中继",
            "未找到可用的中继节点，部分跨 IP 族通信不可用",
        )
        .emit();
        Ok(())
    }

    async fn add_bootstrap_node_if_new(
        &self,
        info: &identify::Info,
        peer_id: PeerId,
    ) -> Result<(), Box<dyn Error>> {
        let full_addr = info
            .listen_addrs
            .iter()
            .find(|addr| addr.iter().any(|proto| matches!(proto, Protocol::P2p(_))))
            .cloned()
            .unwrap_or_else(|| {
                let mut addr = info
                    .listen_addrs
                    .first()
                    .cloned()
                    .unwrap_or_else(Multiaddr::empty);
                addr.push(Protocol::P2p(peer_id));
                addr
            });

        let current_nodes = self.config.bootstrap_nodes();
        let known_peer_ids: std::collections::HashSet<PeerId> = current_nodes
            .iter()
            .filter_map(extract_peer_id_from_multiaddr)
            .collect();

        if !known_peer_ids.contains(&peer_id) {
            let mut new_nodes = current_nodes;
            new_nodes.push(full_addr);
            self.config.set_bootstrap_nodes(new_nodes);
            self.config.save_to_default();
            LogStruct::new(
                LogLevel::Debug,
                "配置更新",
                format!("添加新 bootstrap 节点: {}", peer_id),
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
                        if self.swarm.listen_on(addr.clone()).await.is_ok() {
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
                "add_key" => match serde_json::from_slice::<serde_json::Value>(&payload) {
                    Ok(json) => {
                        let key_str = json["key"].as_str().unwrap_or_default().to_string();
                        if key_str.is_empty() {
                            Some(Err("missing 'key' field".to_string()))
                        } else {
                            let key = kad::RecordKey::new(&key_str);
                            let has_value = json.get("value").and_then(|v| v.as_str());
                            let providing = json
                                .get("providing")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            if has_value.is_none() && !providing {
                                Some(Err("at least one of 'value' or 'providing' is required"
                                    .to_string()))
                            } else if let Some(value_str) = has_value {
                                match self
                                    .swarm
                                    .put_record(key, value_str.as_bytes().to_vec())
                                    .await
                                {
                                    Ok(_) => {
                                        if providing {
                                            match self
                                                .swarm
                                                .start_providing(kad::RecordKey::new(&key_str))
                                                .await
                                            {
                                                Ok(_) => {
                                                    let json = serde_json::json!({
                                                        "success": true,
                                                        "key": key_str,
                                                    });
                                                    Some(Ok(serde_json::to_vec(&json).unwrap()))
                                                }
                                                Err(e) => Some(Err(format!(
                                                    "start_providing failed: {e}"
                                                ))),
                                            }
                                        } else {
                                            let json = serde_json::json!({
                                                "success": true,
                                                "key": key_str,
                                            });
                                            Some(Ok(serde_json::to_vec(&json).unwrap()))
                                        }
                                    }
                                    Err(e) => Some(Err(format!("put_record failed: {e}"))),
                                }
                            } else {
                                match self
                                    .swarm
                                    .start_providing(kad::RecordKey::new(&key_str))
                                    .await
                                {
                                    Ok(_) => {
                                        let json = serde_json::json!({
                                            "success": true,
                                            "key": key_str,
                                        });
                                        Some(Ok(serde_json::to_vec(&json).unwrap()))
                                    }
                                    Err(e) => Some(Err(format!("start_providing failed: {e}"))),
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
                                let record_key = kad::RecordKey::new(&key_str);
                                match self.swarm.get_providers(record_key).await {
                                    Ok(providers) => {
                                        let mut resp = serde_json::json!({
                                            "key": key_str,
                                            "value": value,
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
                                Err(e) => Some(Err(e)),
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
                                Err(e) => Some(Err(e)),
                            }
                        }
                        Err(e) => Some(Err(format!("invalid peer id: {e}"))),
                    }
                }
            }
            _ => Some(Err("command not supported".to_string())),
        };

        let _ = resp_tx.send(result.unwrap());
    }
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
