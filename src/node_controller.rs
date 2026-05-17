use crate::config::ConfigHandle;
use crate::net::{NetHandle, NetBehaviourEvent};
use crate::log::{LogStruct, LogLevel};
use crate::service_protocol;
use crate::service_dispatcher::{self, Command, InboundServiceRequest};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, futures, identify, kad, ping, request_response};
use std::collections::HashMap;
use std::error::Error;
use tokio::sync::{mpsc, oneshot};
use std::time::Duration;
use std::sync::{Arc, RwLock};

pub struct NodeController {
    config: ConfigHandle,
    net: NetHandle,
    my_peer_id: PeerId,
    bootstrap_triggered: bool,
    pending_records: HashMap<kad::QueryId, oneshot::Sender<Result<kad::GetRecordOk, kad::GetRecordError>>>,
    pending_providers: HashMap<kad::QueryId, oneshot::Sender<Result<Vec<PeerId>, kad::GetProvidersError>>>,
    pending_service_responses: HashMap<request_response::OutboundRequestId, oneshot::Sender<Result<service_protocol::Response, String>>>,
    node_rtts: Arc<RwLock<HashMap<PeerId, Duration>>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    inbound_req_tx: mpsc::UnboundedSender<InboundServiceRequest>,
}

impl NodeController {
    pub fn new(
        config: ConfigHandle,
        net: NetHandle,
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
            net,
            my_peer_id,
            bootstrap_triggered: false,
            pending_records: HashMap::new(),
            pending_service_responses: HashMap::new(),
            pending_providers: HashMap::new(),
            node_rtts,
            cmd_rx,
            inbound_req_tx,
        }
    }

    pub async fn run(mut self) -> Result<(), Box<dyn Error>> {
        let mut event_rx = self.net.run().await;
        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    match event {
                        NetBehaviourEvent::Ping(ping_event) => { self.handle_ping(ping_event).await?; }
                        NetBehaviourEvent::Identify(identify_event) => { self.handle_identify(identify_event).await?; }
                        NetBehaviourEvent::Kademlia(kad_event) => { self.handle_kademlia(kad_event).await?; }
                        NetBehaviourEvent::ServiceReq(req_event) => { self.handle_service_req(req_event).await; }
                    }
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd).await;
                }
            }
        }
    }

    async fn handle_ping(&mut self, event: ping::Event) -> Result<(), Box<dyn Error>> {
        match event {
            ping::Event {peer, result: Ok(rtt), .. } => {
                if let Ok(mut map) = self.node_rtts.write() {
                    map.insert(peer, rtt);
                }
            }
            ping::Event {peer,  .. } => {
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
                    for addr in &info.listen_addrs {
                        self.net.with_swarm_mut(|swarm| {
                            swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
                        });
                    }
                    let full_addr = info.listen_addrs
                        .iter()
                        .find(|addr| addr.iter().any(|proto| matches!(proto, Protocol::P2p(_))))
                        .cloned()
                        .unwrap_or_else(|| {
                            let mut addr = info.listen_addrs.first().cloned().unwrap_or_else(Multiaddr::empty);
                            addr.push(Protocol::P2p(peer_id));
                            addr
                        });
                    self.add_bootstrap_node(full_addr, peer_id).await?;
                    LogStruct::new(LogLevel::Preset, "节点发现", format!("同版本节点: {}", peer_id)).emit();
                } else {
                    self.net.with_swarm_mut(|swarm| {
                        swarm.disconnect_peer_id(peer_id);
                    });
                }
            }
            identify::Event::Error { peer_id, error, .. } => {
                LogStruct::new(LogLevel::Warning, "Identify错误", format!("{}: {}", peer_id, error)).emit();
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_kademlia(&mut self, event: kad::Event) -> Result<(), Box<dyn Error>> {
        match event {
            kad::Event::OutboundQueryProgressed { result, id,.. } => {
                match result {
                    kad::QueryResult::Bootstrap(result) => {
                        if result.is_ok() {
                            if !self.bootstrap_triggered {
                                self.bootstrap_triggered = true;
                                self.announce_local_services().await?;
                            }
                        } else if let Err(e) = result {
                            LogStruct::new(LogLevel::Warning, "Kademlia", format!("Bootstrap 失败: {:?}", e)).emit();
                        }
                    }
                    kad::QueryResult::GetRecord(result) => {
                        if let Some(sender) = self.pending_records.remove(&id) {
                            let _ = sender.send(result);
                        }
                    }
                    kad::QueryResult::GetProviders(result) => {
                        if let Some(sender) = self.pending_providers.remove(&id) {
                            let send_result = match result {
                                Ok(providers_ok) => {
                                    match providers_ok {
                                        kad::GetProvidersOk::FoundProviders { providers, .. } => Ok(providers.into_iter().collect()),
                                        kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. } => Ok(Vec::new()),
                                    }
                                }
                                Err(e) => Err(e),
                            };
                            let _ = sender.send(send_result);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
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

    pub async fn send_request_to_peer(
        &mut self,
        peer: PeerId,
        service: String,
        payload: Vec<u8>,
    ) -> Result<service_protocol::Response, String> {
        let request = service_protocol::Request { service, payload };

        let (tx, rx) = oneshot::channel();

        let request_id = self.net.with_swarm_mut(|swarm| {
            swarm
                .behaviour_mut()
                .service_req
                .send_request(&peer, request)
        }).ok_or("Swarm 未初始化")?;

        self.pending_service_responses.insert(request_id, tx);

        rx.await.map_err(|_| "请求通道已关闭".to_string())?
    }

    pub async fn call_service(
        &mut self,
        service: String,
        payload: Vec<u8>,
    ) -> Result<service_protocol::Response, String> {
        let providers = self.discover_providers(&service).await
            .map_err(|e| format!("Service discovery failed: {}", e))?;
        if providers.is_empty() {
            return Err("No provider found for this service".to_string());
        }

        let best_peer = self.get_best_peer(&providers)
            .or_else(|| providers.first().copied())
            .ok_or("No available provider")?;

        self.send_request_to_peer(best_peer, service, payload).await
    } 

    async fn handle_service_req(&mut self, event: request_response::Event<service_protocol::Request, service_protocol::Response>) -> Result<(), Box<dyn Error>> {
        match event {
            request_response::Event::Message { peer, message, .. } => {
                match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let (tx, rx) = oneshot::channel();
                        let inbound = InboundServiceRequest {
                            service: request.service,
                            payload: request.payload,
                            response_tx: tx,
                        };
                        // 转发给 ServiceDispatcher
                        if let Err(e) = self.inbound_req_tx.send(inbound) {
                            // 如果 ServiceDispatcher 已关闭，返回错误
                            let err_resp = service_protocol::Response {
                                success: false,
                                data: format!("service unavailable: {}", e).into_bytes(),
                            };
                            let _ = self.net.with_swarm_mut(|swarm| {
                                swarm.behaviour_mut().service_req.send_response(channel, err_resp);
                            });
                            return Ok(());
                        }
                        // 等待 ServiceDispatcher 的处理结果
                        let result = match rx.await {
                            Ok(res) => res,
                            Err(_) => service_protocol::Response {
                                success: false,
                                data: b"service closed".to_vec(),
                            },
                        };
                        // 将结果发送回请求方
                        self.net.with_swarm_mut(|swarm| {
                            let _ = swarm.behaviour_mut().service_req.send_response(channel, result);
                        });
                    }
                    request_response::Message::Response { request_id, response, .. } => {
                        if let Some(sender) = self.pending_service_responses.remove(&request_id) {
                            let _ = sender.send(Ok(response));
                        }
                    }
                }
            }
            request_response::Event::OutboundFailure { request_id, error, .. } => {
                if let Some(sender) = self.pending_service_responses.remove(&request_id) {
                    let _ = sender.send(Err(error.to_string()));
                }
            }
            request_response::Event::InboundFailure { request_id, error, .. } => {
                LogStruct::new(LogLevel::Warning, "服务请求失败", format!("{} 入站失败: {:?}", request_id, error)).emit();
            }
            _ => {}
        }
        Ok(())
    }

    async fn get_global_service_types(&mut self) -> Result<Vec<String>, Box<dyn Error>> {
        let types_key = kad::RecordKey::new(b"/oahd/service/types");
        let (tx, rx) = oneshot::channel();
        let query_id = self.net.with_swarm_mut(|swarm| {
            swarm.behaviour_mut().kademlia.get_record(types_key)
        }).ok_or("Swarm 未初始化")?;
        self.pending_records.insert(query_id, tx);

        match rx.await {
            Ok(Ok(record_ok)) => {
                match record_ok {
                    kad::GetRecordOk::FoundRecord(peer_record) => {
                        let types: Vec<String> = serde_json::from_slice(&peer_record.record.value)?;
                        Ok(types)
                    }
                    kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. } => Ok(Vec::new()),
                }
            }
            Ok(Err(e)) => Err(Box::new(e)),
            Err(_) => Err("通道关闭".into()),
        }
    }

    async fn announce_local_services(&mut self) -> Result<(), Box<dyn Error>> {
        let local_services = self.config.read().services.dispatcher.local_services.clone();
        if local_services.is_empty() {
            return Ok(());
        }

        for local_service in &local_services {
            let key = format!("/oahd/service/{}", local_service.name);
            let record_key = libp2p::kad::RecordKey::new(&key);
            self.net.with_swarm_mut(|swarm| {
                swarm.behaviour_mut().kademlia.start_providing(record_key)?;
                Ok::<_, Box<dyn Error>>(())
            });
            LogStruct::new(LogLevel::Preset, format!("已注册服务: {}", local_service.name), "").emit();
        }

        let my_types: Vec<String> = local_services.iter().map(|s| s.name.clone()).collect();
        let types_key = kad::RecordKey::new(b"/oahd/service/types");
        let (tx, rx) = oneshot::channel();
        let query_id = self.net.with_swarm_mut(|swarm| {
            swarm.behaviour_mut().kademlia.get_record(types_key.clone())
        }).expect("msg");
        self.pending_records.insert(query_id, tx);

        let existing_types = match rx.await {
            Ok(Ok(record_ok)) => {
                match record_ok {
                    kad::GetRecordOk::FoundRecord(peer_record) => {
                        serde_json::from_slice::<Vec<String>>(&peer_record.record.value).unwrap_or_default()
                    }
                    kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. } => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        let mut all_types = existing_types;
        for t in my_types {
            if !all_types.contains(&t) {
                all_types.push(t);
            }
        }
        let types_json = serde_json::to_vec(&all_types)?;
        let record = kad::Record {
            key: types_key,
            value: types_json,
            publisher: None,
            expires: None,
        };
        self.net.with_swarm_mut(|swarm| {
            swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::Majority)?;
            Ok::<_, Box<dyn Error>>(())
        }).ok_or("Swarm 未初始化")??;
        Ok(())
    }

    pub async fn discover_providers(&mut self, service_type: &str) -> Result<Vec<PeerId>, Box<dyn Error>> {
        let key = format!("/oahd/service/{}", service_type);
        let record_key = kad::RecordKey::new(&key);

        let (tx, rx) = oneshot::channel();

        let query_id = self.net.with_swarm_mut(|swarm| {
            swarm.behaviour_mut().kademlia.get_providers(record_key)
        }).ok_or("Swarm 未初始化")?;

        self.pending_providers.insert(query_id, tx);

        let result = rx.await??;
        Ok(result)
    }

    async fn add_bootstrap_node(&self, full_addr: Multiaddr, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
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
            LogStruct::new(LogLevel::Debug, "配置更新", format!("添加新 bootstrap 节点: {}", peer_id)).emit();
        }
        Ok(())
    }

    async fn handle_command(&mut self, cmd: Command) {
        let result = match cmd.prefix.as_str() {
            "@" => match cmd.content.as_str() {
                "discover_providers" => {
                    let service_type = String::from_utf8(cmd.payload).unwrap_or_default();
                    self.discover_providers(&service_type).await
                        .map(|peers| serde_json::to_vec(&peers).unwrap())
                        .map_err(|e| e.to_string())
                }
                "list_services" => {
                    self.get_global_service_types().await
                        .map(|types| serde_json::to_vec(&types).unwrap())
                        .map_err(|e| e.to_string())
                }
                _ => Err("Unknown command".to_string()),
            },
            "service_request" => {
                let service = cmd.content;
                let payload = cmd.payload;
                self.call_service(service, payload).await
                    .map(|resp| resp.data)
                    .map_err(|e| e.to_string())
            },
            _ => Err("command not supported".to_string()),
        };
        let _ = cmd.resp_tx.send(result);
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