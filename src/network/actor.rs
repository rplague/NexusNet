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

//! Swarm Actor：独占 `Swarm` 的单一事件循环。
//!
//! # 为什么需要 Actor
//!
//! libp2p 是事件驱动模型：发起查询立刻返回一个 `QueryId`，真正的结果要等未来
//! 某次轮询 `Swarm` 才出现。若调用方既轮询 Swarm 又等待查询结果，就会互相阻塞，
//! 形成重入死锁。因此把轮询 Swarm 与等待结果拆成两个执行体：
//!
//! - **`SwarmActor`**：永不停止地轮询 `Swarm`，串行处理事件与命令。
//! - **调用方**：通过 [`NetworkHandle`] 发命令并 `.await` 一个 oneshot 回执。
//!
//! # 三张挂起表
//!
//! | 表 | 键 | 值 |
//! |---|---|---|
//! | `pending_kad` | Kademlia `QueryId` | DHT 查询的 oneshot |
//! | `pending_outbound` | `OutboundRequestId` | 服务调用的 oneshot |
//! | `pending_inbound` | 自造 request_id | `(ConnectionId, ResponseChannel)` |
//!
//! 此外，本模块还承担「Identify → Kademlia 地址桥接」这一网络层自身职责：
//! 把对端宣告的监听地址 `add_address` 进 DHT，触发自动 bootstrap。否则路由表
//! 永远为空，服务无法被发现。

use crate::config::ConfigHandle;
use crate::network::NetworkError;
use crate::network::behaviour::{NetBehaviour, NetBehaviourEvent};
use crate::network::builder::build_swarm;
use crate::service_protocol;

use libp2p::futures::StreamExt;
use libp2p::swarm::{ConnectionId, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, identity, kad, ping, relay, request_response};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// 上层 NodeController 通过 [`NetworkHandle`] 发给 Actor 的命令。
pub enum SwarmCommand {
    KadGetRecord {
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
    /// 用当前配置重建整个 Swarm，用于热重载。
    Reload {
        resp: oneshot::Sender<Result<(), NetworkError>>,
    },
    Shutdown,
}

/// Actor 转发给 NodeController 的事件。
#[allow(clippy::large_enum_variant)]
pub enum NetworkEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    /// 本轮 Swarm 生命周期内首次 bootstrap 成功。
    BootstrapCompleted,
    InboundServiceRequest {
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    },
    Relay(relay::Event),
    RelayClient(relay::client::Event),
    /// 某个监听器关闭。`addresses` 含该监听器曾监听的全部地址；
    /// 中继预约失败或掉线会以带 `p2p-circuit` 的地址出现在这里。
    ListenerClosed {
        addresses: Vec<Multiaddr>,
    },
}

/// 对外异步门面。所有方法通过命令通道与 Actor 通信，内部使用 oneshot 回执。
#[derive(Clone)]
pub struct NetworkHandle {
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
}

impl NetworkHandle {
    fn new(cmd_tx: mpsc::UnboundedSender<SwarmCommand>) -> Self {
        Self { cmd_tx }
    }

    /// 通用「发命令 + 等回执」辅助：消除各方法的 oneshot 样板。
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

    pub async fn get_providers(&self, key: kad::RecordKey) -> Result<Vec<PeerId>, NetworkError> {
        match self
            .request(|resp| SwarmCommand::KadGetProviders { key, resp })
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

    /// 主动断开某 peer，无需等待回执。
    pub fn disconnect_peer(&self, peer: PeerId) {
        let _ = self.cmd_tx.send(SwarmCommand::DisconnectPeer { peer });
    }

    /// 用当前 `ConfigHandle` 重建 Swarm。会瞬断所有连接并清空 DHT 本地存储。
    pub async fn reload(&self) -> Result<(), NetworkError> {
        match self.request(|resp| SwarmCommand::Reload { resp }).await {
            Ok(inner) => inner,
            Err(e) => Err(e),
        }
    }

    /// 通知 Actor 退出循环。
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(SwarmCommand::Shutdown);
    }
}

/// Kademlia 查询挂起表条目。
enum KadPending {
    GetRecord(oneshot::Sender<Result<kad::GetRecordOk, NetworkError>>),
    GetProviders(oneshot::Sender<Result<Vec<PeerId>, NetworkError>>),
    PutRecord(oneshot::Sender<Result<(), NetworkError>>),
    StartProviding(oneshot::Sender<Result<(), NetworkError>>),
}

impl KadPending {
    /// 查询被取消时回填错误，例如 reload。
    fn fail(self, err: NetworkError) {
        match self {
            KadPending::GetRecord(tx) => {
                let _ = tx.send(Err(err));
            }
            KadPending::GetProviders(tx) => {
                let _ = tx.send(Err(err));
            }
            KadPending::PutRecord(tx) => {
                let _ = tx.send(Err(err));
            }
            KadPending::StartProviding(tx) => {
                let _ = tx.send(Err(err));
            }
        }
    }
}

/// 启动结果：门面 + 事件流 + 任务句柄。
pub struct NetworkStart {
    pub handle: NetworkHandle,
    pub events: mpsc::UnboundedReceiver<NetworkEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

pub(crate) struct SwarmActor {
    swarm: Swarm<NetBehaviour>,
    config: ConfigHandle,
    keypair: identity::Keypair,
    cmd_rx: mpsc::UnboundedReceiver<SwarmCommand>,
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    bootstrap_triggered: bool,
    pending_kad: HashMap<kad::QueryId, KadPending>,
    pending_outbound: HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<Result<service_protocol::Response, NetworkError>>,
    >,
    pending_inbound: HashMap<
        String,
        (
            ConnectionId,
            request_response::ResponseChannel<service_protocol::Response>,
        ),
    >,
}

/// 构建 Swarm 并启动 Actor 任务。
pub fn spawn(
    config: ConfigHandle,
    keypair: identity::Keypair,
) -> Result<NetworkStart, NetworkError> {
    let swarm = build_swarm(&config, &keypair)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let handle = NetworkHandle::new(cmd_tx.clone());

    let mut actor = SwarmActor {
        swarm,
        config,
        keypair,
        cmd_rx,
        cmd_tx,
        event_tx,
        bootstrap_triggered: false,
        pending_kad: HashMap::new(),
        pending_outbound: HashMap::new(),
        pending_inbound: HashMap::new(),
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

    /// 取可变 Kademlia 引用；若被 Toggle 关闭则返回 `Disabled`。
    fn kad(&mut self) -> Result<&mut kad::Behaviour<kad::store::MemoryStore>, NetworkError> {
        self.swarm
            .behaviour_mut()
            .kademlia
            .as_mut()
            .ok_or(NetworkError::Disabled)
    }

    async fn handle_event(&mut self, event: SwarmEvent<NetBehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(NetBehaviourEvent::Kademlia(kad_event)) => {
                self.handle_kad_event(kad_event);
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReq(svc_event)) => {
                self.handle_service_req(svc_event).await;
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
            SwarmEvent::ConnectionClosed { connection_id, .. } => {
                // 连接关闭后其挂起的入站请求无法再回复，清理避免泄漏。
                self.pending_inbound
                    .retain(|_, (cid, _)| *cid != connection_id);
            }
            SwarmEvent::ListenerClosed { addresses, .. } => {
                let _ = self
                    .event_tx
                    .send(NetworkEvent::ListenerClosed { addresses });
            }
            _ => {}
        }
    }

    /// Identify → Kademlia 地址桥接。
    ///
    /// 把 OAHD 对端宣告的监听地址写入 DHT 路由表；`add_address` 会在新节点插入时
    /// 触发自动 bootstrap，节流约 500ms，从而最终发出 `BootstrapCompleted`。
    fn on_identify_peer(&mut self, peer_id: PeerId, info: &identify::Info) {
        if !info.agent_version.starts_with("/oahd/") {
            return;
        }
        let Ok(kad) = self.kad() else {
            return;
        };
        for addr in &info.listen_addrs {
            kad.add_address(&peer_id, addr.clone());
        }
    }

    /// 处理 Kademlia 查询进度事件，按 QueryId 唤醒对应挂起表条目。
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
                if let Some(KadPending::GetRecord(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(result.map_err(|e| NetworkError::Kad(format!("{e:?}"))));
                }
            }
            kad::QueryResult::GetProviders(result) => {
                if let Some(KadPending::GetProviders(sender)) = self.pending_kad.remove(&id) {
                    let send_result = match result {
                        Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) => {
                            Ok(providers.into_iter().collect())
                        }
                        Ok(kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. }) => {
                            Ok(Vec::new())
                        }
                        Err(e) => Err(NetworkError::Kad(format!("{e:?}"))),
                    };
                    let _ = sender.send(send_result);
                }
            }
            kad::QueryResult::PutRecord(result) => {
                if let Some(KadPending::PutRecord(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(
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
                connection_id,
                message,
                ..
            } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let request_id = Uuid::new_v4().to_string();
                    let (resp_tx, resp_rx) = oneshot::channel();

                    self.pending_inbound
                        .insert(request_id.clone(), (connection_id, channel));

                    let _ = self.event_tx.send(NetworkEvent::InboundServiceRequest {
                        request_id: request_id.clone(),
                        service: request.service,
                        payload: request.payload,
                        response_tx: resp_tx,
                    });

                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        // 无论成功/失败/通道关闭，都必须回填响应，避免对端永久挂起。
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
                    if let Some(sender) = self.pending_outbound.remove(&request_id) {
                        let _ = sender.send(Ok(response));
                    }
                }
            },
            request_response::Event::OutboundFailure {
                request_id, error, ..
            } => {
                if let Some(sender) = self.pending_outbound.remove(&request_id) {
                    let _ = sender.send(Err(NetworkError::Request(error.to_string())));
                }
            }
            _ => {}
        }
    }

    /// 处理单条命令；返回 `false` 表示要求 Actor 退出。
    async fn handle_command(&mut self, cmd: SwarmCommand) -> bool {
        match cmd {
            SwarmCommand::KadGetRecord { key, resp } => {
                let query_id = match self.kad() {
                    Ok(kad) => kad.get_record(key),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                self.pending_kad
                    .insert(query_id, KadPending::GetRecord(resp));
            }
            SwarmCommand::KadGetProviders { key, resp } => {
                let query_id = match self.kad() {
                    Ok(kad) => kad.get_providers(key),
                    Err(e) => {
                        let _ = resp.send(Err(e));
                        return true;
                    }
                };
                self.pending_kad
                    .insert(query_id, KadPending::GetProviders(resp));
            }
            SwarmCommand::KadPutRecord { key, value, resp } => {
                let record = kad::Record::new(key, value);
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
                            .insert(query_id, KadPending::PutRecord(resp));
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
            SwarmCommand::ServiceSendRequest {
                peer,
                request,
                resp,
            } => {
                let request_id = self
                    .swarm
                    .behaviour_mut()
                    .service_req
                    .send_request(&peer, request);
                self.pending_outbound.insert(request_id, resp);
            }
            SwarmCommand::ServiceSendResponse {
                request_id,
                response,
            } => {
                if let Some((_, channel)) = self.pending_inbound.remove(&request_id) {
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .service_req
                        .send_response(channel, response);
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

    /// 用当前配置重建 Swarm：清空挂起表、重建、重新拨号 bootstrap。
    async fn reload(&mut self) -> Result<(), NetworkError> {
        // 1. 回填所有挂起请求，避免调用方永久等待。
        for (_, pending) in self.pending_kad.drain() {
            pending.fail(NetworkError::Reloaded);
        }
        for (_, tx) in self.pending_outbound.drain() {
            let _ = tx.send(Err(NetworkError::Reloaded));
        }
        self.pending_inbound.clear();

        // 2. 重建 Swarm。旧 Swarm 在此 drop，连接与监听随之关闭。
        let new_swarm = build_swarm(&self.config, &self.keypair)?;
        self.swarm = new_swarm;
        self.bootstrap_triggered = false;

        // 3. 重新拨号静态 bootstrap 节点。
        for addr in self.config.bootstrap_nodes() {
            let _ = self.swarm.dial(addr);
        }

        Ok(())
    }
}
