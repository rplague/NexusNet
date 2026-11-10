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

use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use libp2p::PeerId;
use libp2p::futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Duration, timeout};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::auth;
use crate::config::{ConfigHandle, LocalServiceEntry};
use crate::log::{LogLevel, LogStruct};
use crate::service_protocol;
use crate::sidecar_protocol::{Message, PROTOCOL_VERSION, SidecarCodec, SidecarError};

/// Initial reconnect backoff, in seconds.
const INITIAL_BACKOFF: u64 = 1;
/// Maximum reconnect backoff, in seconds.
const MAX_BACKOFF: u64 = 60;
/// Maximum queued requests per backend.
const BACKEND_QUEUE_CAPACITY: usize = 64;
/// Maximum queued backend events for the dispatcher.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// A backend-issued control command and its reply channel.
pub struct ControlCommand {
    pub msg: Message,
    pub resp_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Backend connection readiness change.
pub struct BackendStatus {
    pub service: String,
    pub ready: bool,
    /// Backend task instance; stale instances are ignored after a reload.
    instance: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BackendReadiness {
    pub ready: HashSet<String>,
}

/// All upstream messages from the dispatcher to the controller.
pub enum ToController {
    /// A backend-issued control command (with a reply channel).
    Control(ControlCommand),
}

/// Dispatcher endpoints handed to the controller after startup.
pub struct DispatcherChannels {
    pub inbound_tx: mpsc::Sender<InboundRequest>,
    pub command_tx: mpsc::Sender<DispatcherCommand>,
    pub readiness_rx: watch::Receiver<Arc<BackendReadiness>>,
}

/// An inbound P2P service request to forward to a local backend.
pub struct InboundRequest {
    pub service: String,
    pub payload: Vec<u8>,
    pub response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
}

/// Control commands for the dispatcher task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatcherCommand {
    /// Reconcile backend tasks with the current configuration.
    Reload,
}

/// Commands from the router to a backend task.
enum BackendCmd {
    /// Forward a service request; the result is delivered on its reply channel.
    Request(InboundRequest),
    /// Write an already-built frame (e.g. a control reply) to the backend.
    WriteReply(Message),
    /// Stop the backend task and close its connection.
    Shutdown,
}

/// Events from a backend task to the router.
enum BackendEvent {
    /// A backend connection readiness change.
    Status(BackendStatus),
    /// A control message received from a backend, to relay to the controller.
    Control {
        service: String,
        msg: Message,
        instance: u64,
    },
}

fn command_timeout(msg: &Message, base: Duration, kad: Duration) -> Duration {
    let required = match msg {
        Message::ServiceRequest { .. } | Message::ServiceRequestTo { .. } => {
            kad.saturating_add(service_protocol::REQUEST_TIMEOUT.saturating_mul(2))
        }
        Message::QueryKey { .. } => kad.saturating_add(kad),
        Message::ListServices { .. }
        | Message::DiscoverProviders { .. }
        | Message::ReannounceServices { .. } => kad,
        Message::AddKey { .. } => kad.saturating_add(kad),
        _ => Duration::ZERO,
    };
    required.max(base).saturating_add(Duration::from_secs(5))
}

/// Routes between `NodeController` and one persistent task per local backend.
#[derive(Debug, PartialEq, Eq)]
enum LocalRoute {
    Local(String),
    Reject(String),
    Remote,
}

fn route_local(ready: &HashSet<String>, my_peer_id: PeerId, msg: &Message) -> LocalRoute {
    match msg {
        Message::ServiceRequest { service, .. } if ready.contains(service) => {
            LocalRoute::Local(service.clone())
        }
        Message::ServiceRequestTo { service, peer, .. }
            if peer.parse::<PeerId>().ok() == Some(my_peer_id) =>
        {
            if ready.contains(service) {
                LocalRoute::Local(service.clone())
            } else {
                LocalRoute::Reject(format!("local service {service} is not ready"))
            }
        }
        _ => LocalRoute::Remote,
    }
}

pub struct ServiceDispatcher {
    inbound_rx: mpsc::Receiver<InboundRequest>,
    event_rx: mpsc::Receiver<BackendEvent>,
    /// Keeps the backend event channel open even when there are no backends.
    event_tx: mpsc::Sender<BackendEvent>,
    command_rx: mpsc::Receiver<DispatcherCommand>,
    to_controller_tx: mpsc::Sender<ToController>,
    /// Service name -> command sender for its backend task.
    backends: HashMap<String, mpsc::Sender<BackendCmd>>,
    /// Service name -> configured backend port, used to detect reloads.
    backend_ports: HashMap<String, u16>,
    /// Service name -> active backend task instance.
    backend_instances: HashMap<String, u64>,
    next_backend_instance: u64,
    my_peer_id: PeerId,
    backend_ready: HashSet<String>,
    readiness_tx: watch::Sender<Arc<BackendReadiness>>,
    config: ConfigHandle,
    shutdown_rx: watch::Receiver<bool>,
    query_timeout: Duration,
    kad_timeout: Duration,
}

/// Valid services that should have a backend connection under the current
/// configuration. `publish` does not affect this list: unpublished services
/// stay reachable locally.
fn desired_local_services(config: &ConfigHandle) -> Vec<LocalServiceEntry> {
    let config = config.read();
    if !config.services.dispatcher.enabled {
        return Vec::new();
    }

    let mut services = Vec::with_capacity(config.services.dispatcher.local_services.len());
    let mut rejected = Vec::new();
    let mut seen = HashSet::new();
    for svc in &config.services.dispatcher.local_services {
        if !auth::is_valid_service_name(&svc.name) {
            rejected.push(svc.name.clone());
            continue;
        }
        if !seen.insert(svc.name.clone()) {
            continue;
        }
        services.push(svc.clone());
    }
    drop(config);

    if !rejected.is_empty() {
        LogStruct::new(
            LogLevel::Warning,
            "服务名非法，已跳过",
            format!(
                "{}（保留名 'service' 或含非法字符，不会建立后端连接）",
                rejected.join(", ")
            ),
        )
        .emit();
    }
    services
}

fn start_backend(
    name: String,
    port: u16,
    instance: u64,
    event_tx: mpsc::Sender<BackendEvent>,
    shutdown_rx: watch::Receiver<bool>,
) -> mpsc::Sender<BackendCmd> {
    let (cmd_tx, cmd_rx) = mpsc::channel(BACKEND_QUEUE_CAPACITY);
    tokio::spawn(backend_task(
        name,
        format!("127.0.0.1:{port}"),
        instance,
        cmd_rx,
        event_tx,
        shutdown_rx,
    ));
    cmd_tx
}

impl ServiceDispatcher {
    pub fn new(
        inbound_rx: mpsc::Receiver<InboundRequest>,
        command_rx: mpsc::Receiver<DispatcherCommand>,
        to_controller_tx: mpsc::Sender<ToController>,
        config: ConfigHandle,
        my_peer_id: PeerId,
        shutdown_rx: watch::Receiver<bool>,
    ) -> (Self, watch::Receiver<Arc<BackendReadiness>>) {
        let local_services = desired_local_services(&config);
        let query_timeout = Duration::from_secs(config.dispatcher_query_timeout().into());
        let kad_timeout = Duration::from_secs(config.kademlia_query_timeout().into());
        let (readiness_tx, readiness_rx) = watch::channel(Arc::new(BackendReadiness::default()));
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let mut backends = HashMap::new();
        let mut backend_ports = HashMap::new();
        let mut backend_instances = HashMap::new();
        let mut next_backend_instance = 1u64;
        for svc in &local_services {
            let instance = next_backend_instance;
            next_backend_instance = next_backend_instance.wrapping_add(1);
            let cmd_tx = start_backend(
                svc.name.clone(),
                svc.port,
                instance,
                event_tx.clone(),
                shutdown_rx.clone(),
            );
            backends.insert(svc.name.clone(), cmd_tx);
            backend_ports.insert(svc.name.clone(), svc.port);
            backend_instances.insert(svc.name.clone(), instance);
        }
        if !config.dispatcher_enabled() {
            LogStruct::new(
                LogLevel::Warning,
                "ServiceDispatcher 已禁用",
                "services.dispatcher.enabled = false",
            )
            .emit();
        }

        (
            Self {
                inbound_rx,
                event_rx,
                event_tx,
                command_rx,
                to_controller_tx,
                backends,
                backend_ports,
                backend_instances,
                next_backend_instance,
                my_peer_id,
                backend_ready: HashSet::new(),
                readiness_tx,
                config,
                shutdown_rx,
                query_timeout,
                kad_timeout,
            },
            readiness_rx,
        )
    }

    /// Reconcile backend tasks with the current configuration.
    ///
    /// Existing connections are kept, removed services are asked to stop, and
    /// newly configured services are connected without restarting the node.
    fn reload(&mut self) {
        let desired = desired_local_services(&self.config);
        let desired_ports: HashMap<String, u16> = desired
            .iter()
            .map(|service| (service.name.clone(), service.port))
            .collect();

        let removed: Vec<String> = self
            .backends
            .keys()
            .filter(|name| {
                desired_ports
                    .get(*name)
                    .is_none_or(|port| self.backend_ports.get(*name) != Some(port))
            })
            .cloned()
            .collect();
        for name in &removed {
            if let Some(cmd_tx) = self.backends.get(name) {
                let _ = cmd_tx.try_send(BackendCmd::Shutdown);
            }
        }
        self.backends.retain(|name, _| !removed.contains(name));
        self.backend_ports.retain(|name, _| !removed.contains(name));
        self.backend_instances
            .retain(|name, _| !removed.contains(name));

        let mut readiness_changed = false;
        for name in &removed {
            readiness_changed |= self.backend_ready.remove(name);
        }
        if readiness_changed {
            let _ = self.readiness_tx.send(Arc::new(BackendReadiness {
                ready: self.backend_ready.clone(),
            }));
        }

        for svc in desired {
            if self.backends.contains_key(&svc.name) {
                continue;
            }
            let instance = self.next_backend_instance;
            self.next_backend_instance = self.next_backend_instance.wrapping_add(1);
            let cmd_tx = start_backend(
                svc.name.clone(),
                svc.port,
                instance,
                self.event_tx.clone(),
                self.shutdown_rx.clone(),
            );
            self.backends.insert(svc.name.clone(), cmd_tx);
            self.backend_ports.insert(svc.name.clone(), svc.port);
            self.backend_instances.insert(svc.name, instance);
        }

        self.query_timeout = Duration::from_secs(self.config.dispatcher_query_timeout().into());
        self.kad_timeout = Duration::from_secs(self.config.kademlia_query_timeout().into());
    }

    pub async fn run(mut self) {
        loop {
            tokio::select! {
                maybe = self.inbound_rx.recv() => {
                    let Some(req) = maybe else {
                        return;
                    };
                    let Some(cmd_tx) = self.backends.get(&req.service) else {
                        let _ = req
                            .response_tx
                            .send(Err(format!("No backend for service {}", req.service)));
                        continue;
                    };
                    if let Err(e) = cmd_tx.try_send(BackendCmd::Request(req)) {
                        let (cmd, reason) = match e {
                            mpsc::error::TrySendError::Full(cmd) => (cmd, "backend overloaded"),
                            mpsc::error::TrySendError::Closed(cmd) => {
                                (cmd, "No backend for service")
                            }
                        };
                        if let BackendCmd::Request(req) = cmd {
                            let _ = req
                                .response_tx
                                .send(Err(format!("{reason} {}", req.service)));
                        }
                    }
                }
                maybe = self.event_rx.recv() => {
                    let Some(event) = maybe else {
                        return;
                    };
                    match event {
                        BackendEvent::Status(status) => {
                            if self.backend_instances.get(&status.service)
                                != Some(&status.instance)
                            {
                                continue;
                            }
                            if status.ready {
                                self.backend_ready.insert(status.service.clone());
                            } else {
                                self.backend_ready.remove(&status.service);
                            }
                            let _ = self.readiness_tx.send(Arc::new(BackendReadiness {
                                ready: self.backend_ready.clone(),
                            }));
                        }
                        BackendEvent::Control {
                            service,
                            msg,
                            instance,
                        } => {
                            if self.backend_instances.get(&service) != Some(&instance) {
                                continue;
                            }
                            self.forward_control(service, msg).await
                        }
                    }
                }
                Some(command) = self.command_rx.recv() => match command {
                    DispatcherCommand::Reload => self.reload(),
                },
                _ = self.shutdown_rx.changed() => {
                    return;
                }
            }
        }
    }

    /// Relay a backend control message to the controller, then route the reply
    /// back to the originating backend task for writing.
    async fn forward_control(&self, service: String, msg: Message) {
        let Some(id) = msg.id() else {
            LogStruct::new(
                LogLevel::Warning,
                "后端协议错误",
                format!("{service}: 非控制消息 {msg:?}"),
            )
            .emit();
            return;
        };
        let Some(backend_tx) = self.backends.get(&service).cloned() else {
            return;
        };

        match self.local_route(&msg) {
            LocalRoute::Local(target) => {
                let payload = match &msg {
                    Message::ServiceRequest { payload, .. }
                    | Message::ServiceRequestTo { payload, .. } => payload.clone(),
                    _ => Vec::new(),
                };
                self.forward_local(service, target, id, payload);
                return;
            }
            LocalRoute::Reject(reason) => {
                let _ = backend_tx.try_send(BackendCmd::WriteReply(Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new("backend_not_ready", reason)),
                }));
                return;
            }
            LocalRoute::Remote => {}
        }

        let budget = command_timeout(&msg, self.query_timeout, self.kad_timeout);
        let (resp_tx, resp_rx) = oneshot::channel();
        if self
            .to_controller_tx
            .send(ToController::Control(ControlCommand { msg, resp_tx }))
            .await
            .is_err()
        {
            let _ = backend_tx.try_send(BackendCmd::WriteReply(Message::Reply {
                id,
                ok: false,
                result: None,
                error: Some(SidecarError::new("node_gone", "node unavailable")),
            }));
            return;
        }

        tokio::spawn(async move {
            let reply = match timeout(budget, resp_rx).await {
                Ok(Ok(Ok(result))) => Message::Reply {
                    id,
                    ok: true,
                    result: Some(result),
                    error: None,
                },
                Ok(Ok(Err(e))) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new("command_failed", e)),
                },
                Ok(Err(_)) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new("node_gone", "command channel closed")),
                },
                Err(_) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new("timeout", "command execution timeout")),
                },
            };
            let _ = backend_tx.send(BackendCmd::WriteReply(reply)).await;
        });
    }

    fn local_route(&self, msg: &Message) -> LocalRoute {
        route_local(&self.backend_ready, self.my_peer_id, msg)
    }

    fn forward_local(&self, origin: String, target: String, id: Uuid, payload: Vec<u8>) {
        let Some(origin_tx) = self.backends.get(&origin).cloned() else {
            return;
        };
        let Some(target_tx) = self.backends.get(&target).cloned() else {
            let _ = origin_tx.try_send(BackendCmd::WriteReply(Message::Reply {
                id,
                ok: false,
                result: None,
                error: Some(SidecarError::new(
                    "no_backend",
                    format!("no backend for service {target}"),
                )),
            }));
            return;
        };

        tokio::spawn(async move {
            let (response_tx, response_rx) = oneshot::channel();
            let request = InboundRequest {
                service: target,
                payload,
                response_tx,
            };
            if let Err(error) = target_tx.try_send(BackendCmd::Request(request)) {
                let message = match error {
                    mpsc::error::TrySendError::Full(_) => "backend queue full".to_string(),
                    mpsc::error::TrySendError::Closed(_) => "backend unavailable".to_string(),
                };
                let _ = origin_tx
                    .send(BackendCmd::WriteReply(Message::Reply {
                        id,
                        ok: false,
                        result: None,
                        error: Some(SidecarError::new("backend_busy", message)),
                    }))
                    .await;
                return;
            }

            let reply = match timeout(service_protocol::REQUEST_TIMEOUT, response_rx).await {
                Ok(Ok(Ok(response))) => {
                    if response.success {
                        Message::Reply {
                            id,
                            ok: true,
                            result: Some(response.data),
                            error: None,
                        }
                    } else {
                        let message = String::from_utf8_lossy(&response.data).into_owned();
                        Message::Reply {
                            id,
                            ok: false,
                            result: None,
                            error: Some(SidecarError::new(
                                "service_failed",
                                if message.is_empty() {
                                    "local service failed".to_string()
                                } else {
                                    message
                                },
                            )),
                        }
                    }
                }
                Ok(Ok(Err(error))) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new("backend_error", error)),
                },
                Ok(Err(_)) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new(
                        "backend_error",
                        "backend response channel closed",
                    )),
                },
                Err(_) => Message::Reply {
                    id,
                    ok: false,
                    result: None,
                    error: Some(SidecarError::new(
                        "timeout",
                        "local service request timeout",
                    )),
                },
            };
            let _ = origin_tx.send(BackendCmd::WriteReply(reply)).await;
        });
    }
}

/// One task per local backend: owns the TCP connection and its reply table.
async fn backend_task(
    service: String,
    addr: String,
    instance: u64,
    mut cmd_rx: mpsc::Receiver<BackendCmd>,
    event_tx: mpsc::Sender<BackendEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let stream = match TcpStream::connect(&addr).await {
            Ok(stream) => stream,
            Err(e) => {
                LogStruct::new(
                    LogLevel::Warning,
                    "后端连接失败",
                    format!("{service}: {e}, {backoff} 秒后重试"),
                )
                .emit();
                if wait_backoff(&mut shutdown_rx, &mut cmd_rx, &service, backoff).await {
                    return;
                }
                backoff = min(backoff * 2, MAX_BACKOFF);
                continue;
            }
        };

        let mut framed = Framed::new(stream, SidecarCodec);
        if let Err(e) = handshake(&mut framed).await {
            LogStruct::new(
                LogLevel::Warning,
                "后端协议握手失败",
                format!("{service}: {e}"),
            )
            .emit();
            if wait_backoff(&mut shutdown_rx, &mut cmd_rx, &service, backoff).await {
                return;
            }
            backoff = min(backoff * 2, MAX_BACKOFF);
            continue;
        }

        backoff = INITIAL_BACKOFF;
        LogStruct::new(
            LogLevel::Preset,
            "后端连接建立",
            format!("{service} -> {addr}"),
        )
        .emit();
        let _ = event_tx
            .send(BackendEvent::Status(BackendStatus {
                service: service.clone(),
                ready: true,
                instance,
            }))
            .await;

        // Pending service requests, keyed by request id.
        let mut pending: HashMap<
            Uuid,
            oneshot::Sender<Result<service_protocol::Response, String>>,
        > = HashMap::new();
        let mut shutdown = false;

        loop {
            tokio::select! {
                frame = framed.next() => match frame {
                    Some(Ok(Message::Reply { id, ok, result, error })) => match pending.remove(&id) {
                        Some(tx) => {
                            let outcome = if ok {
                                Ok(service_protocol::Response {
                                    success: true,
                                    data: result.unwrap_or_default(),
                                })
                            } else {
                                Err(error
                                    .map(|e| e.message)
                                    .unwrap_or_else(|| "backend error".to_string()))
                            };
                            let _ = tx.send(outcome);
                        }
                        None => LogStruct::new(
                            LogLevel::Warning,
                            "未知响应",
                            format!("{service}: 未匹配的 id {id}"),
                        )
                        .emit(),
                    },
                    // Control messages are relayed to the controller without
                    // blocking this read loop.
                    Some(Ok(control)) => {
                        let _ = event_tx
                            .send(BackendEvent::Control {
                                service: service.clone(),
                                msg: control,
                                instance,
                            })
                            .await;
                    }
                    Some(Err(e)) => {
                        LogStruct::new(LogLevel::Error, "后端读错误", format!("{service}: {e}")).emit();
                        break;
                    }
                    None => break,
                },
                maybe = cmd_rx.recv() => match maybe {
                    None => {
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err("dispatcher gone".to_string()));
                        }
                        return;
                    }
                    Some(BackendCmd::Request(req)) => {
                        let id = Uuid::new_v4();
                        let frame = Message::Request {
                            id,
                            service: service.clone(),
                            payload: req.payload,
                        };
                        if let Err(e) = framed.send(frame).await {
                            let _ = req
                                .response_tx
                                .send(Err(format!("write to backend failed: {e}")));
                            break;
                        }
                        pending.insert(id, req.response_tx);
                    }
                    Some(BackendCmd::WriteReply(msg)) => {
                        if framed.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(BackendCmd::Shutdown) => {
                        shutdown = true;
                        break;
                    }
                },
                _ = shutdown_rx.changed() => {
                    shutdown = true;
                    break;
                }
            }
        }

        // Fail everything still in flight.
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("backend disconnected".to_string()));
        }
        if shutdown {
            return;
        }

        let _ = event_tx
            .send(BackendEvent::Status(BackendStatus {
                service: service.clone(),
                ready: false,
                instance,
            }))
            .await;
        LogStruct::new(
            LogLevel::Warning,
            "后端连接断开",
            format!("{service} 将在 {backoff} 秒后重连"),
        )
        .emit();
        if wait_backoff(&mut shutdown_rx, &mut cmd_rx, &service, backoff).await {
            return;
        }
        backoff = min(backoff * 2, MAX_BACKOFF);
    }
}

/// Exchange hello with the backend and check the protocol version.
async fn handshake(framed: &mut Framed<TcpStream, SidecarCodec>) -> Result<(), String> {
    framed
        .send(Message::Hello {
            version: PROTOCOL_VERSION,
        })
        .await
        .map_err(|e| e.to_string())?;
    match framed.next().await {
        Some(Ok(Message::Hello { version })) if version == PROTOCOL_VERSION => Ok(()),
        Some(Ok(Message::Hello { version })) => Err(format!(
            "协议版本不兼容: 对端 {version}, 本端 {PROTOCOL_VERSION}"
        )),
        Some(Ok(_)) => Err("期望 hello 首帧".to_string()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err("连接在握手前关闭".to_string()),
    }
}

/// Wait out the reconnect delay, honoring shutdown and rejecting inbound
/// requests while not connected. Returns `true` if the task should exit.
async fn wait_backoff(
    shutdown_rx: &mut watch::Receiver<bool>,
    cmd_rx: &mut mpsc::Receiver<BackendCmd>,
    service: &str,
    backoff: u64,
) -> bool {
    let sleep = tokio::time::sleep(Duration::from_secs(backoff));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            _ = shutdown_rx.changed() => return true,
            maybe = cmd_rx.recv() => match maybe {
                None => return true,
                Some(BackendCmd::Request(req)) => {
                    let _ = req
                        .response_tx
                        .send(Err(format!("backend not connected: {service}")));
                }
                Some(BackendCmd::WriteReply(_)) => {}
                Some(BackendCmd::Shutdown) => return true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeConfig;
    use uuid::Uuid;

    const KAD: Duration = Duration::from_secs(60);
    const BASE: Duration = Duration::from_secs(60);
    const MARGIN: Duration = Duration::from_secs(5);

    fn id() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn service_request_budget_covers_dht_and_request() {
        let msg = Message::ServiceRequest {
            id: id(),
            service: "cmd".into(),
            payload: vec![],
        };
        let expected = KAD
            .saturating_add(service_protocol::REQUEST_TIMEOUT.saturating_mul(2))
            .saturating_add(MARGIN);
        assert_eq!(command_timeout(&msg, BASE, KAD), expected);
    }

    #[test]
    fn service_request_to_budget_covers_lookup_and_relay_retry() {
        let msg = Message::ServiceRequestTo {
            id: id(),
            service: "cmd".into(),
            peer: PeerId::random().to_string(),
            payload: vec![],
        };
        let expected = KAD
            .saturating_add(service_protocol::REQUEST_TIMEOUT.saturating_mul(2))
            .saturating_add(MARGIN);
        assert_eq!(command_timeout(&msg, BASE, KAD), expected);
    }

    #[test]
    fn query_key_budget_covers_two_dht_queries() {
        let msg = Message::QueryKey {
            id: id(),
            key: "/k".into(),
        };
        assert_eq!(
            command_timeout(&msg, BASE, KAD),
            KAD.saturating_add(KAD).saturating_add(MARGIN)
        );
    }

    #[test]
    fn dht_query_budget_tracks_kademlia_timeout() {
        let msg = Message::DiscoverProviders {
            id: id(),
            service: "cmd".into(),
        };
        assert_eq!(
            command_timeout(&msg, Duration::from_secs(5), KAD),
            KAD.saturating_add(MARGIN)
        );
    }

    #[test]
    fn local_command_keeps_the_base_timeout() {
        let msg = Message::Whoami { id: id() };
        assert_eq!(
            command_timeout(&msg, BASE, KAD),
            BASE.saturating_add(MARGIN)
        );
    }

    #[test]
    fn publish_flag_does_not_gate_backend_connections() {
        let mut config = NodeConfig::default();
        config.services.dispatcher.local_services = vec![
            LocalServiceEntry {
                name: "private".to_string(),
                port: 7001,
                require_auth: false,
                publish: false,
            },
            LocalServiceEntry {
                name: "public".to_string(),
                port: 7002,
                require_auth: false,
                publish: true,
            },
        ];
        let names: Vec<String> = desired_local_services(&ConfigHandle::new(config))
            .into_iter()
            .map(|service| service.name)
            .collect();
        assert_eq!(names, vec!["private".to_string(), "public".to_string()]);
    }

    #[test]
    fn disabled_dispatcher_has_no_backends() {
        let mut config = NodeConfig::default();
        config.services.dispatcher.enabled = false;
        config.services.dispatcher.local_services = vec![LocalServiceEntry {
            name: "cmd".to_string(),
            port: 7003,
            require_auth: false,
            publish: true,
        }];
        assert!(desired_local_services(&ConfigHandle::new(config)).is_empty());
    }

    #[tokio::test]
    async fn reload_adds_and_removes_backends() {
        let config = ConfigHandle::new(NodeConfig::default());
        let (_inbound_tx, inbound_rx) = mpsc::channel(1);
        let (_command_tx, command_rx) = mpsc::channel(1);
        let (controller_tx, _controller_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (mut dispatcher, _readiness) = ServiceDispatcher::new(
            inbound_rx,
            command_rx,
            controller_tx,
            config.clone(),
            PeerId::random(),
            shutdown_rx,
        );
        assert!(dispatcher.backends.is_empty());

        config.write().services.dispatcher.local_services = vec![LocalServiceEntry {
            name: "cmd".to_string(),
            port: 7004,
            require_auth: false,
            publish: false,
        }];
        dispatcher.reload();
        assert!(dispatcher.backends.contains_key("cmd"));
        assert_eq!(dispatcher.backend_ports.get("cmd"), Some(&7004));

        config.write().services.dispatcher.local_services[0].port = 7005;
        dispatcher.reload();
        assert_eq!(dispatcher.backend_ports.get("cmd"), Some(&7005));

        config.write().services.dispatcher.local_services.clear();
        dispatcher.reload();
        assert!(dispatcher.backends.is_empty());
        assert!(dispatcher.backend_ports.is_empty());

        let _ = shutdown_tx.send(true);
    }

    #[test]
    fn routes_local_ready_requests() {
        let my_peer_id = PeerId::random();
        let ready = HashSet::from(["cmd".to_string()]);
        let id = Uuid::new_v4();
        assert_eq!(
            route_local(
                &ready,
                my_peer_id,
                &Message::ServiceRequest {
                    id,
                    service: "cmd".into(),
                    payload: vec![],
                }
            ),
            LocalRoute::Local("cmd".to_string())
        );
        assert_eq!(
            route_local(
                &ready,
                my_peer_id,
                &Message::ServiceRequest {
                    id,
                    service: "remote".into(),
                    payload: vec![],
                }
            ),
            LocalRoute::Remote
        );
        assert_eq!(
            route_local(
                &ready,
                my_peer_id,
                &Message::ServiceRequestTo {
                    id,
                    service: "cmd".into(),
                    peer: my_peer_id.to_string(),
                    payload: vec![],
                }
            ),
            LocalRoute::Local("cmd".to_string())
        );
        assert!(matches!(
            route_local(
                &HashSet::new(),
                my_peer_id,
                &Message::ServiceRequestTo {
                    id,
                    service: "cmd".into(),
                    peer: my_peer_id.to_string(),
                    payload: vec![],
                }
            ),
            LocalRoute::Reject(_)
        ));
        assert_eq!(
            route_local(
                &ready,
                my_peer_id,
                &Message::ServiceRequestTo {
                    id,
                    service: "cmd".into(),
                    peer: PeerId::random().to_string(),
                    payload: vec![],
                }
            ),
            LocalRoute::Remote
        );
    }
}
