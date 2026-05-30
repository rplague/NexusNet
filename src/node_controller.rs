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

use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::net::{NetBehaviour, NetBehaviourEvent, NetHandle};
use crate::service_dispatcher::{Command, InboundServiceRequest};
use crate::service_protocol;
use libp2p::futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm, identify, kad, ping, relay, request_response};
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Kademlia 异步查询的统一回调类型
enum KadCallback {
    GetRecord(oneshot::Sender<Result<kad::GetRecordOk, kad::GetRecordError>>),
    GetProviders(oneshot::Sender<Result<Vec<PeerId>, kad::GetProvidersError>>),
    ServiceTypeMerge(Vec<String>),
    /// @list_services 命令 → get_record 完成 → 直接通过 cmd.resp_tx 返回
    ListServices(oneshot::Sender<Result<Vec<u8>, String>>),
    /// @discover_providers 命令 → get_providers 完成 → 直接通过 cmd.resp_tx 返回
    DiscoverProviders(oneshot::Sender<Result<Vec<u8>, String>>),
    /// P2P 服务调用 Phase 1: discover → Phase 2: send_request → Phase 3: response
    ServiceCall {
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    /// @add_key Phase 1: put_record 完成 → 若 need_provide 则继续 start_providing
    AddKeyPhase1 {
        key: kad::RecordKey,
        need_provide: bool,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    /// @add_key Phase 2: start_providing 完成 → 返回结果
    AddKeyPhase2 {
        key: kad::RecordKey,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    /// @query_key Phase 1: get_record 完成 → 继续 get_providers
    QueryKeyPhase1 {
        key: kad::RecordKey,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
    /// @query_key Phase 2: get_providers 完成 → 合并 JSON 返回
    QueryKeyPhase2 {
        key: kad::RecordKey,
        value: Option<String>,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
}

/// P2P 服务请求响应的统一回调类型
enum SvcCallback {
    Async(oneshot::Sender<Result<service_protocol::Response, String>>),
    CommandResponse(oneshot::Sender<Result<Vec<u8>, String>>),
}

pub struct NodeController {
    config: ConfigHandle,
    my_peer_id: PeerId,
    bootstrap_triggered: bool,
    /// 统一的 Kademlia 异步查询等待表
    pending_kad: HashMap<kad::QueryId, KadCallback>,
    pending_service_responses: HashMap<request_response::OutboundRequestId, SvcCallback>,
    node_rtts: Arc<RwLock<HashMap<PeerId, Duration>>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        my_peer_id: PeerId,
        cmd_rx: mpsc::UnboundedReceiver<Command>,
        inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
    ) -> Self {
        let node_rtts = Arc::new(RwLock::new(HashMap::new()));
        if let Ok(mut map) = node_rtts.write() {
            map.insert(my_peer_id, Duration::ZERO);
        }

        Self {
            config,
            my_peer_id,
            bootstrap_triggered: false,
            pending_kad: HashMap::new(),
            pending_service_responses: HashMap::new(),
            node_rtts,
            cmd_rx,
            inbound_req_tx,
        }
    }

    /// 运行节点，需要 NetHandle 来驱动 Swarm 事件循环
    /// Swarm 不再 spawn 到独立 task，而是在主 select! 循环中直接驱动，
    /// 这样 handler 方法可以直接访问 &mut Swarm 进行 DHT 操作。
    pub async fn run(mut self, net_handle: NetHandle) -> Result<(), Box<dyn Error>> {
        let mut swarm = net_handle.run();
        loop {
            tokio::select! {
                event = swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(NetBehaviourEvent::Ping(ping_event)) => {
                            self.handle_ping(ping_event).await?;
                        }
                        SwarmEvent::Behaviour(NetBehaviourEvent::Identify(identify_event)) => {
                            self.handle_identify(identify_event, &mut swarm).await?;
                        }
                        SwarmEvent::Behaviour(NetBehaviourEvent::Kademlia(kad_event)) => {
                            self.handle_kademlia(kad_event, &mut swarm).await?;
                        }
                        SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReq(req_event)) => {
                            self.handle_service_req(req_event, &mut swarm).await?;
                        }
                        SwarmEvent::Behaviour(NetBehaviourEvent::Relay(event)) => {
                            self.handle_relay(event).await?;
                        }
                        SwarmEvent::Behaviour(NetBehaviourEvent::RelayClient(event)) => {
                            self.handle_relay_client(event).await?;
                        }
                        _ => {
                            LogStruct::new(LogLevel::Debug, "未处理事件",
                                format!("{:?}", event)).emit();
                        }
                    }
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd, &mut swarm).await;
                }
            }
        }
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

    async fn handle_identify(
        &mut self,
        event: identify::Event,
        swarm: &mut Swarm<NetBehaviour>,
    ) -> Result<(), Box<dyn Error>> {
        match event {
            identify::Event::Received { peer_id, info, .. } => {
                if info.agent_version.starts_with("/oahd/") {
                    for addr in &info.listen_addrs {
                        swarm
                            .behaviour_mut()
                            .kademlia
                            .add_address(&peer_id, addr.clone());
                    }
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
                    self.add_bootstrap_node(full_addr, peer_id).await?;
                } else {
                    swarm.disconnect_peer_id(peer_id);
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

    async fn handle_kademlia(
        &mut self,
        event: kad::Event,
        swarm: &mut Swarm<NetBehaviour>,
    ) -> Result<(), Box<dyn Error>> {
        if let kad::Event::OutboundQueryProgressed { result, id, .. } = event {
            match result {
                kad::QueryResult::Bootstrap(result) => {
                    if result.is_ok() {
                        if !self.bootstrap_triggered {
                            self.bootstrap_triggered = true;
                            self.announce_local_services(swarm).await?;
                            self.request_relay_reservation_if_needed(swarm).await?;
                        }
                    } else if let Err(e) = result {
                        LogStruct::new(
                            LogLevel::Warning,
                            "Kademlia",
                            format!("Bootstrap 失败: {:?}", e),
                        )
                        .emit();
                    }
                }
                kad::QueryResult::GetRecord(result) => {
                    if let Some(cb) = self.pending_kad.remove(&id) {
                        match cb {
                            KadCallback::GetRecord(sender) => {
                                let _ = sender.send(result);
                            }
                            KadCallback::ServiceTypeMerge(my_types) => {
                                let existing_types = match &result {
                                    Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                                        serde_json::from_slice::<Vec<String>>(
                                            &peer_record.record.value,
                                        )
                                        .unwrap_or_default()
                                    }
                                    _ => Vec::new(),
                                };
                                let mut all_types = existing_types;
                                for t in my_types {
                                    if !all_types.contains(&t) {
                                        all_types.push(t);
                                    }
                                }
                                if let Ok(types_json) = serde_json::to_vec(&all_types) {
                                    let record = kad::Record {
                                        key: kad::RecordKey::new(b"/oahd/service/types"),
                                        value: types_json,
                                        publisher: None,
                                        expires: None,
                                    };
                                    let _ = swarm
                                        .behaviour_mut()
                                        .kademlia
                                        .put_record(record, kad::Quorum::Majority);
                                }
                            }
                            KadCallback::ListServices(sender) => {
                                let types = match &result {
                                    Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                                        serde_json::from_slice::<Vec<String>>(
                                            &peer_record.record.value,
                                        )
                                        .unwrap_or_default()
                                    }
                                    _ => Vec::new(),
                                };
                                let _ = sender.send(Ok(serde_json::to_vec(&types).unwrap()));
                            }
                            KadCallback::QueryKeyPhase1 {
                                key,
                                response_tx,
                            } => {
                                let value = match &result {
                                    Ok(kad::GetRecordOk::FoundRecord(peer_record)) => {
                                        Some(String::from_utf8_lossy(
                                            &peer_record.record.value,
                                        )
                                        .into_owned())
                                    }
                                    _ => None,
                                };
                                let record_key = kad::RecordKey::new(&key.to_vec());
                                let query_id =
                                    swarm.behaviour_mut().kademlia.get_providers(record_key);
                                self.pending_kad.insert(
                                    query_id,
                                    KadCallback::QueryKeyPhase2 {
                                        key,
                                        value,
                                        response_tx,
                                    },
                                );
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                }
                kad::QueryResult::GetProviders(result) => {
                    if let Some(cb) = self.pending_kad.remove(&id) {
                        match cb {
                            KadCallback::GetProviders(sender) => {
                                let send_result = match result {
                                    Ok(providers_ok) => match providers_ok {
                                        kad::GetProvidersOk::FoundProviders {
                                            providers, ..
                                        } => Ok(providers.into_iter().collect()),
                                        kad::GetProvidersOk::FinishedWithNoAdditionalRecord {
                                            ..
                                        } => Ok(Vec::new()),
                                    },
                                    Err(e) => Err(e),
                                };
                                let _ = sender.send(send_result);
                            }
                            KadCallback::DiscoverProviders(sender) => {
                                let providers: Vec<PeerId> = match &result {
                                    Ok(providers_ok) => match providers_ok {
                                        kad::GetProvidersOk::FoundProviders {
                                            providers, ..
                                        } => providers.iter().copied().collect(),
                                        _ => Vec::new(),
                                    },
                                    _ => Vec::new(),
                                };
                                let _ = sender.send(Ok(serde_json::to_vec(&providers).unwrap()));
                            }
                            KadCallback::QueryKeyPhase2 {
                                key,
                                value,
                                response_tx,
                            } => {
                                let providers: Vec<PeerId> = match &result {
                                    Ok(kad::GetProvidersOk::FoundProviders {
                                        providers, ..
                                    }) => providers.iter().copied().collect(),
                                    _ => Vec::new(),
                                };
                                let mut resp = serde_json::json!({
                                    "key": String::from_utf8_lossy(&key.to_vec()).into_owned(),
                                });
                                if let Some(v) = &value {
                                    resp["value"] = serde_json::json!(v);
                                }
                                if !providers.is_empty() {
                                    resp["providers"] = serde_json::json!(providers);
                                }
                                let _ = response_tx
                                    .send(Ok(serde_json::to_vec(&resp).unwrap()));
                            }
                            KadCallback::ServiceCall {
                                service,
                                payload,
                                response_tx,
                            } => {
                                let providers: Vec<PeerId> = match &result {
                                    Ok(providers_ok) => match providers_ok {
                                        kad::GetProvidersOk::FoundProviders {
                                            providers, ..
                                        } => providers.iter().copied().collect(),
                                        _ => Vec::new(),
                                    },
                                    _ => Vec::new(),
                                };

                                if providers.is_empty() {
                                    let _ = response_tx.send(Err("No provider found".to_string()));
                                } else {
                                    let best_peer = self
                                        .get_best_peer(&providers)
                                        .or_else(|| providers.first().copied())
                                        .unwrap();

                                    let request = service_protocol::Request { service, payload };
                                    let request_id = swarm
                                        .behaviour_mut()
                                        .service_req
                                        .send_request(&best_peer, request);

                                    self.pending_service_responses.insert(
                                        request_id,
                                        SvcCallback::CommandResponse(response_tx),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                kad::QueryResult::PutRecord(result) => {
                    if let Some(cb) = self.pending_kad.remove(&id) {
                        if let KadCallback::AddKeyPhase1 {
                            key,
                            need_provide,
                            response_tx,
                        } = cb
                        {
                            if let Err(e) = &result {
                                let _ = response_tx
                                    .send(Err(format!("put_record failed: {e:?}")));
                                return Ok(());
                            }
                            if need_provide {
                                match swarm.behaviour_mut().kademlia.start_providing(key.clone()) {
                                    Ok(query_id) => {
                                        self.pending_kad.insert(
                                            query_id,
                                            KadCallback::AddKeyPhase2 { key, response_tx },
                                        );
                                    }
                                    Err(e) => {
                                        let _ = response_tx
                                            .send(Err(format!("start_providing failed: {e}")));
                                    }
                                }
                            } else {
                                let json = serde_json::json!({
                                    "success": true,
                                    "key": String::from_utf8_lossy(&key.to_vec()).into_owned(),
                                });
                                let _ =
                                    response_tx.send(Ok(serde_json::to_vec(&json).unwrap()));
                            }
                        }
                    }
                }
                kad::QueryResult::StartProviding(result) => {
                    if let Some(cb) = self.pending_kad.remove(&id) {
                        if let KadCallback::AddKeyPhase2 { key, response_tx } = cb {
                            match result {
                                Ok(_) => {
                                    let json = serde_json::json!({
                                        "success": true,
                                        "key": String::from_utf8_lossy(&key.to_vec()).into_owned(),
                                    });
                                    let _ =
                                        response_tx.send(Ok(serde_json::to_vec(&json).unwrap()));
                                }
                                Err(e) => {
                                    let _ = response_tx
                                        .send(Err(format!("start_providing failed: {e:?}")));
                                }
                            }
                        }
                    }
                }
                _ => {}
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

    async fn handle_service_req(
        &mut self,
        event: request_response::Event<service_protocol::Request, service_protocol::Response>,
        swarm: &mut Swarm<NetBehaviour>,
    ) -> Result<(), Box<dyn Error>> {
        match event {
            request_response::Event::Message { message, .. } => {
                match message {
                    request_response::Message::Request {
                        request, channel, ..
                    } => {
                        let (tx, rx) = oneshot::channel();
                        let inbound = InboundServiceRequest {
                            service: request.service,
                            payload: request.payload,
                            response_tx: tx,
                        };
                        // 转发给 ServiceDispatcher
                        if let Err(e) = self.inbound_req_tx.send(inbound) {
                            let err_resp = service_protocol::Response {
                                success: false,
                                data: format!("service unavailable: {}", e).into_bytes(),
                            };
                            let _ = swarm
                                .behaviour_mut()
                                .service_req
                                .send_response(channel, err_resp);
                            return Ok(());
                        }
                        // 等待 ServiceDispatcher 的处理结果
                        let result = match rx.await {
                            Ok(Ok(resp)) => resp, // 正常响应
                            Ok(Err(e)) => service_protocol::Response {
                                success: false,
                                data: e.into_bytes(),
                            },
                            Err(_) => service_protocol::Response {
                                success: false,
                                data: b"service closed".to_vec(),
                            },
                        };
                        // 将结果发送回请求方
                        let _ = swarm
                            .behaviour_mut()
                            .service_req
                            .send_response(channel, result);
                    }
                    request_response::Message::Response {
                        request_id,
                        response,
                        ..
                    } => {
                        if let Some(cb) = self.pending_service_responses.remove(&request_id) {
                            match cb {
                                SvcCallback::Async(sender) => {
                                    let _ = sender.send(Ok(response));
                                }
                                SvcCallback::CommandResponse(sender) => {
                                    let _ = sender.send(Ok(response.data));
                                }
                            }
                        }
                    }
                }
            }
            request_response::Event::OutboundFailure {
                request_id, error, ..
            } => {
                if let Some(cb) = self.pending_service_responses.remove(&request_id) {
                    match cb {
                        SvcCallback::Async(sender) => {
                            let _ = sender.send(Err(error.to_string()));
                        }
                        SvcCallback::CommandResponse(sender) => {
                            let _ = sender.send(Err(error.to_string()));
                        }
                    }
                }
            }
            request_response::Event::InboundFailure {
                request_id, error, ..
            } => {
                LogStruct::new(
                    LogLevel::Warning,
                    "服务请求失败",
                    format!("{} 入站失败: {:?}", request_id, error),
                )
                .emit();
            }
            _ => {}
        }
        Ok(())
    }

    async fn announce_local_services(
        &mut self,
        swarm: &mut Swarm<NetBehaviour>,
    ) -> Result<(), Box<dyn Error>> {
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
            swarm.behaviour_mut().kademlia.start_providing(record_key)?;
            LogStruct::new(
                LogLevel::Preset,
                format!("已注册服务: {}", local_service.name),
                "",
            )
            .emit();
        }

        let my_types: Vec<String> = local_services.iter().map(|s| s.name.clone()).collect();
        let types_key = kad::RecordKey::new(b"/oahd/service/types");
        // Phase 1: 发起 get_record 查询；Phase 2 在 handle_kademlia 中异步完成
        //   不能在此处 rx.await — Swarm 与 event loop 在同一 task 中，会导致死锁
        let query_id = swarm.behaviour_mut().kademlia.get_record(types_key);
        self.pending_kad
            .insert(query_id, KadCallback::ServiceTypeMerge(my_types));
        Ok(())
    }

    /// 单栈节点在 bootstrap 后主动向一个已知 relay 节点建立 reservation
    async fn request_relay_reservation_if_needed(
        &mut self,
        swarm: &mut Swarm<NetBehaviour>,
    ) -> Result<(), Box<dyn Error>> {
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
                // 构造 relay listen 地址：/ip4/.../tcp/.../p2p/<relay>/p2p-circuit
                // relay transport 解析到 /p2p-circuit → 自动触发 RESERVE 握手
                let mut relay_listen_addr = addr.clone();
                relay_listen_addr.push(Protocol::P2pCircuit);
                match swarm.listen_on(relay_listen_addr) {
                    Ok(_id) => {
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

    async fn add_bootstrap_node(
        &self,
        full_addr: Multiaddr,
        peer_id: PeerId,
    ) -> Result<(), Box<dyn Error>> {
        let current_nodes = self.config.bootstrap_nodes();
        let mut known_peer_ids = std::collections::HashSet::new();
        for addr in &current_nodes {
            if let Some(parsed_peer) = extract_peer_id_from_multiaddr(addr) {
                known_peer_ids.insert(parsed_peer);
            }
        }
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

    async fn handle_command(&mut self, cmd: Command, swarm: &mut Swarm<NetBehaviour>) {
        let Command {
            prefix,
            content,
            payload,
            resp_tx,
        } = cmd;

        let result: Option<Result<Vec<u8>, String>> = match prefix.as_str() {
            "@" => match content.as_str() {
                // ── 异步命令：不阻塞 event loop，由 KadCallback 处理器完成 ──
                "list_services" => {
                    let types_key = kad::RecordKey::new(b"/oahd/service/types");
                    let query_id = swarm.behaviour_mut().kademlia.get_record(types_key);
                    self.pending_kad
                        .insert(query_id, KadCallback::ListServices(resp_tx));
                    return;
                }
                "discover_providers" => {
                    let service_type = String::from_utf8(payload).unwrap_or_default();
                    let key = format!("/oahd/service/{}", service_type);
                    let record_key = kad::RecordKey::new(&key);
                    let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
                    self.pending_kad
                        .insert(query_id, KadCallback::DiscoverProviders(resp_tx));
                    return;
                }
                // ── 同步命令 ──
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
                        if swarm.dial(addr.clone()).is_ok() {
                            any_success = true;
                        }
                    }
                    let result = serde_json::json!({
                        "success": any_success
                    });
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                "reannounce_services" => {
                    let result = match self.announce_local_services(swarm).await {
                        Ok(_) => serde_json::json!({"success": true}),
                        Err(e) => serde_json::json!({"success": false, "error": e.to_string()}),
                    };
                    Some(Ok(serde_json::to_vec(&result).unwrap()))
                }
                // ── DHT 操作：add_key / query_key ──
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
                                Some(Err(
                                    "at least one of 'value' or 'providing' is required"
                                        .to_string(),
                                ))
                            } else if let Some(value_str) = has_value {
                                let record = kad::Record::new(
                                    key_str.into_bytes(),
                                    value_str.as_bytes().to_vec(),
                                );
                                match swarm.behaviour_mut().kademlia.put_record(
                                    record,
                                    kad::Quorum::One,
                                ) {
                                    Ok(query_id) => {
                                        self.pending_kad.insert(
                                            query_id,
                                            KadCallback::AddKeyPhase1 {
                                                key,
                                                need_provide: providing,
                                                response_tx: resp_tx,
                                            },
                                        );
                                        return;
                                    }
                                    Err(e) => {
                                        Some(Err(format!("put_record failed: {e}")))
                                    }
                                }
                            } else {
                                // 仅 providing
                                match swarm.behaviour_mut().kademlia.start_providing(key.clone()) {
                                    Ok(query_id) => {
                                        self.pending_kad.insert(
                                            query_id,
                                            KadCallback::AddKeyPhase2 {
                                                key,
                                                response_tx: resp_tx,
                                            },
                                        );
                                        return;
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
                        let query_id = swarm.behaviour_mut().kademlia.get_record(key.clone());
                        self.pending_kad.insert(
                            query_id,
                            KadCallback::QueryKeyPhase1 {
                                key,
                                response_tx: resp_tx,
                            },
                        );
                        return;
                    }
                }
                _ => Some(Err("Unknown command".to_string())),
            },
            "service_request" => {
                let service = content;
                let payload = payload;
                let key = format!("/oahd/service/{}", service);
                let record_key = kad::RecordKey::new(&key);
                let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
                self.pending_kad.insert(
                    query_id,
                    KadCallback::ServiceCall {
                        service,
                        payload,
                        response_tx: resp_tx,
                    },
                );
                return;
            }
            "service_request_to" => {
                let parts: Vec<&str> = content.splitn(2, "/in").collect();
                if parts.len() != 2 {
                    Some(Err(
                        "expected format: <service>/in<peerid>".to_string(),
                    ))
                } else {
                    let service = parts[0].to_string();
                    match parts[1].parse::<PeerId>() {
                        Ok(peer_id) => {
                            let request = service_protocol::Request { service, payload };
                            let request_id = swarm
                                .behaviour_mut()
                                .service_req
                                .send_request(&peer_id, request);
                            self.pending_service_responses.insert(
                                request_id,
                                SvcCallback::CommandResponse(resp_tx),
                            );
                            return;
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
