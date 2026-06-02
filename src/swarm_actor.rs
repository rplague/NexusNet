use crate::log::{LogLevel, LogStruct};
use crate::net::{NetBehaviour, NetBehaviourEvent};
use crate::service_protocol;
use libp2p::futures::StreamExt;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm, identify, kad, ping, relay, request_response};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Orchestrator 通过此通道向 SwarmActor 发送命令
pub enum SwarmCommand {
    KadGetRecord {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<kad::GetRecordOk, kad::GetRecordError>>,
    },
    KadGetProviders {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<Vec<PeerId>, kad::GetProvidersError>>,
    },
    KadPutRecord {
        key: kad::RecordKey,
        value: Vec<u8>,
        resp: oneshot::Sender<Result<(), String>>,
    },
    KadStartProviding {
        key: kad::RecordKey,
        resp: oneshot::Sender<Result<(), String>>,
    },
    ServiceSendRequest {
        peer: PeerId,
        request: service_protocol::Request,
        resp: oneshot::Sender<Result<service_protocol::Response, String>>,
    },
    ServiceSendResponse {
        request_id: String,
        response: service_protocol::Response,
    },
    ListenOn {
        addr: Multiaddr,
        resp: oneshot::Sender<Result<(), String>>,
    },
    Shutdown,
}

/// SwarmActor 通过此通道向 Orchestrator 转发非 Kad 事件
pub enum ControllerEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    BootstrapCompleted,
    InboundServiceRequest {
        request_id: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
    },
    Relay(relay::Event),
    RelayClient(relay::client::Event),
}

/// Orchestrator 持有的 handle，所有方法通过 cmd_tx 发命令给 SwarmActor
#[derive(Clone)]
pub struct SwarmHandle {
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
}

impl SwarmHandle {
    pub fn new(cmd_tx: mpsc::UnboundedSender<SwarmCommand>) -> Self {
        Self { cmd_tx }
    }

    pub async fn get_record(
        &self,
        key: kad::RecordKey,
    ) -> Result<kad::GetRecordOk, kad::GetRecordError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::KadGetRecord { key, resp: tx })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }

    pub async fn get_providers(
        &self,
        key: kad::RecordKey,
    ) -> Result<Vec<PeerId>, kad::GetProvidersError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::KadGetProviders { key, resp: tx })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }

    pub async fn put_record(
        &self,
        key: kad::RecordKey,
        value: Vec<u8>,
    ) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::KadPutRecord {
                key,
                value,
                resp: tx,
            })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }

    pub async fn start_providing(&self, key: kad::RecordKey) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::KadStartProviding { key, resp: tx })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }

    pub async fn send_request(
        &self,
        peer: &PeerId,
        request: service_protocol::Request,
    ) -> Result<service_protocol::Response, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::ServiceSendRequest {
                peer: *peer,
                request,
                resp: tx,
            })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }

    pub fn send_response(
        &self,
        request_id: String,
        response: service_protocol::Response,
    ) {
        let _ = self
            .cmd_tx
            .send(SwarmCommand::ServiceSendResponse { request_id, response });
    }

    pub async fn listen_on(&self, addr: Multiaddr) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SwarmCommand::ListenOn { addr, resp: tx })
            .expect("SwarmActor died");
        rx.await.expect("SwarmActor died")
    }
}

/// Kademlia 查询挂起表（Actor 内部使用）
enum KadPending {
    GetRecord(oneshot::Sender<Result<kad::GetRecordOk, kad::GetRecordError>>),
    GetProviders(oneshot::Sender<Result<Vec<PeerId>, kad::GetProvidersError>>),
    PutRecord(oneshot::Sender<Result<(), String>>),
    StartProviding(oneshot::Sender<Result<(), String>>),
}

pub(crate) struct SwarmActor {
    swarm: Swarm<NetBehaviour>,
    cmd_rx: mpsc::UnboundedReceiver<SwarmCommand>,
    cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    bootstrap_triggered: bool,
    pending_kad: HashMap<kad::QueryId, KadPending>,
    pending_outbound:
        HashMap<request_response::OutboundRequestId, oneshot::Sender<Result<service_protocol::Response, String>>>,
    pending_inbound:
        HashMap<String, request_response::ResponseChannel<service_protocol::Response>>,
}

impl SwarmActor {
    pub fn spawn(
        swarm: Swarm<NetBehaviour>,
    ) -> (
        SwarmHandle,
        mpsc::UnboundedReceiver<ControllerEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let handle = SwarmHandle::new(cmd_tx.clone());

        let mut actor = SwarmActor {
            swarm,
            cmd_rx,
            cmd_tx,
            event_tx,
            bootstrap_triggered: false,
            pending_kad: HashMap::new(),
            pending_outbound: HashMap::new(),
            pending_inbound: HashMap::new(),
        };

        let join_handle = tokio::spawn(async move {
            actor.run().await;
        });

        (handle, event_rx, join_handle)
    }

    async fn run(&mut self) {
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => {
                    if let Err(()) = self.handle_event(event).await {
                        break;
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => break,
                    }
                }
            }
        }
    }

    async fn handle_event(
        &mut self,
        event: SwarmEvent<NetBehaviourEvent>,
    ) -> Result<(), ()> {
        match event {
            SwarmEvent::Behaviour(NetBehaviourEvent::Kademlia(kad_event)) => {
                self.handle_kad_event(kad_event).await;
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::ServiceReq(svc_event)) => {
                self.handle_service_req(svc_event).await;
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Ping(event)) => {
                let _ = self.event_tx.send(ControllerEvent::Ping(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Identify(event)) => {
                let _ = self.event_tx.send(ControllerEvent::Identify(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::Relay(event)) => {
                let _ = self.event_tx.send(ControllerEvent::Relay(event));
            }
            SwarmEvent::Behaviour(NetBehaviourEvent::RelayClient(event)) => {
                let _ = self.event_tx.send(ControllerEvent::RelayClient(event));
            }
            _ => {
                LogStruct::new(LogLevel::Debug, "未处理事件", format!("{:?}", event)).emit();
            }
        }
        Ok(())
    }

    async fn handle_kad_event(&mut self, event: kad::Event) {
        let kad::Event::OutboundQueryProgressed { result, id, .. } = event else {
            return;
        };

        match result {
            kad::QueryResult::Bootstrap(result) => {
                if result.is_ok() && !self.bootstrap_triggered {
                    self.bootstrap_triggered = true;
                    let _ = self.event_tx.send(ControllerEvent::BootstrapCompleted);
                }
            }
            kad::QueryResult::GetRecord(result) => {
                if let Some(KadPending::GetRecord(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(result);
                }
            }
            kad::QueryResult::GetProviders(result) => {
                if let Some(KadPending::GetProviders(sender)) = self.pending_kad.remove(&id) {
                    let send_result = match result {
                        Ok(providers_ok) => match providers_ok {
                            kad::GetProvidersOk::FoundProviders { providers, .. } => {
                                Ok(providers.into_iter().collect())
                            }
                            kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. } => {
                                Ok(Vec::new())
                            }
                        },
                        Err(e) => Err(e),
                    };
                    let _ = sender.send(send_result);
                }
            }
            kad::QueryResult::PutRecord(result) => {
                if let Some(KadPending::PutRecord(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(result.map(|_| ()).map_err(|e| format!("{e:?}")));
                }
            }
            kad::QueryResult::StartProviding(result) => {
                if let Some(KadPending::StartProviding(sender)) = self.pending_kad.remove(&id) {
                    let _ = sender.send(result.map(|_| ()).map_err(|e| format!("{e:?}")));
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
            request_response::Event::Message { message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let request_id = Uuid::new_v4().to_string();
                    let (resp_tx, resp_rx) = oneshot::channel();

                    self.pending_inbound.insert(request_id.clone(), channel);

                    let _ = self.event_tx.send(ControllerEvent::InboundServiceRequest {
                        request_id: request_id.clone(),
                        service: request.service,
                        payload: request.payload,
                        response_tx: resp_tx,
                    });

                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        match resp_rx.await {
                            Ok(Ok(response)) => {
                                let _ = cmd_tx.send(SwarmCommand::ServiceSendResponse {
                                    request_id,
                                    response,
                                });
                            }
                            Ok(Err(e)) => {
                                let err_resp = service_protocol::Response {
                                    success: false,
                                    data: e.into_bytes(),
                                };
                                let _ = cmd_tx.send(SwarmCommand::ServiceSendResponse {
                                    request_id,
                                    response: err_resp,
                                });
                            }
                            Err(_) => {}
                        }
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
                    let _ = sender.send(Err(error.to_string()));
                }
            }
            _ => {}
        }
    }

    async fn handle_command(&mut self, cmd: SwarmCommand) {
        match cmd {
            SwarmCommand::KadGetRecord { key, resp } => {
                let query_id = self.swarm.behaviour_mut().kademlia.get_record(key);
                self.pending_kad
                    .insert(query_id, KadPending::GetRecord(resp));
            }
            SwarmCommand::KadGetProviders { key, resp } => {
                let query_id = self.swarm.behaviour_mut().kademlia.get_providers(key);
                self.pending_kad
                    .insert(query_id, KadPending::GetProviders(resp));
            }
            SwarmCommand::KadPutRecord { key, value, resp } => {
                let record = kad::Record::new(key.to_vec(), value);
                match self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .put_record(record, kad::Quorum::One)
                {
                    Ok(query_id) => {
                        self.pending_kad
                            .insert(query_id, KadPending::PutRecord(resp));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(format!("{e:?}")));
                    }
                }
            }
            SwarmCommand::KadStartProviding { key, resp } => {
                match self.swarm.behaviour_mut().kademlia.start_providing(key) {
                    Ok(query_id) => {
                        self.pending_kad
                            .insert(query_id, KadPending::StartProviding(resp));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(format!("{e:?}")));
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
                if let Some(channel) = self.pending_inbound.remove(&request_id) {
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
                    .map_err(|e| e.to_string());
                let _ = resp.send(result);
            }
            SwarmCommand::Shutdown => {}
        }
    }
}
