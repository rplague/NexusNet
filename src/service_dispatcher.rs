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
use std::collections::HashMap;

use libp2p::futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Duration, timeout};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::auth;
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::service_protocol;
use crate::sidecar_protocol::{Message, PROTOCOL_VERSION, SidecarCodec, SidecarError};

/// Initial reconnect backoff, in seconds.
const INITIAL_BACKOFF: u64 = 1;
/// Maximum reconnect backoff, in seconds.
const MAX_BACKOFF: u64 = 60;

/// A backend-issued control command and its reply channel.
pub struct ControlCommand {
    pub msg: Message,
    pub resp_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Backend connection readiness change, reported by `ServiceDispatcher` to
/// `NodeController`.
pub struct BackendStatus {
    pub service: String,
    pub ready: bool,
}

/// All upstream messages from the dispatcher to the controller.
pub enum ToController {
    /// A backend-issued control command (with a reply channel).
    Control(ControlCommand),
    /// A backend connection readiness change.
    BackendStatus(BackendStatus),
}

/// An inbound P2P service request to forward to a local backend.
pub struct InboundRequest {
    pub service: String,
    pub payload: Vec<u8>,
    pub response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
}

/// Commands from the router to a backend task.
enum BackendCmd {
    /// Forward a service request; the result is delivered on its reply channel.
    Request(InboundRequest),
    /// Write an already-built frame (e.g. a control reply) to the backend.
    WriteReply(Message),
}

/// Events from a backend task to the router.
enum BackendEvent {
    /// A backend connection readiness change.
    Status(BackendStatus),
    /// A control message received from a backend, to relay to the controller.
    Control { service: String, msg: Message },
}

/// Routes between `NodeController` and one persistent task per local backend.
pub struct ServiceDispatcher {
    inbound_rx: mpsc::UnboundedReceiver<InboundRequest>,
    event_rx: mpsc::UnboundedReceiver<BackendEvent>,
    to_controller_tx: mpsc::UnboundedSender<ToController>,
    /// Service name -> command sender for its backend task.
    backends: HashMap<String, mpsc::UnboundedSender<BackendCmd>>,
    shutdown_rx: watch::Receiver<bool>,
    query_timeout: Duration,
}

impl ServiceDispatcher {
    pub fn new(
        inbound_rx: mpsc::UnboundedReceiver<InboundRequest>,
        to_controller_tx: mpsc::UnboundedSender<ToController>,
        config: ConfigHandle,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let configured = config.read().services.dispatcher.local_services.clone();
        // Services with invalid names (reserved "service" or illegal characters)
        // get no backend connection.
        let mut local_services = Vec::with_capacity(configured.len());
        let mut rejected = Vec::new();
        for svc in configured {
            if auth::is_valid_service_name(&svc.name) {
                local_services.push(svc);
            } else {
                rejected.push(svc.name);
            }
        }
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

        let query_timeout = Duration::from_secs(config.dispatcher_query_timeout().into());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut backends = HashMap::new();
        if config.dispatcher_enabled() {
            for svc in &local_services {
                let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                backends.insert(svc.name.clone(), cmd_tx);
                tokio::spawn(backend_task(
                    svc.name.clone(),
                    format!("127.0.0.1:{}", svc.port),
                    cmd_rx,
                    event_tx.clone(),
                    shutdown_rx.clone(),
                ));
            }
        } else {
            LogStruct::new(
                LogLevel::Warning,
                "ServiceDispatcher 已禁用",
                "services.dispatcher.enabled = false",
            )
            .emit();
        }

        Self {
            inbound_rx,
            event_rx,
            to_controller_tx,
            backends,
            shutdown_rx,
            query_timeout,
        }
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
                    let _ = cmd_tx.send(BackendCmd::Request(req));
                }
                maybe = self.event_rx.recv() => {
                    let Some(event) = maybe else {
                        return;
                    };
                    match event {
                        BackendEvent::Status(status) => {
                            let _ = self.to_controller_tx.send(ToController::BackendStatus(status));
                        }
                        BackendEvent::Control { service, msg } => self.forward_control(service, msg),
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    return;
                }
            }
        }
    }

    /// Relay a backend control message to the controller, then route the reply
    /// back to the originating backend task for writing.
    fn forward_control(&self, service: String, msg: Message) {
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

        let (resp_tx, resp_rx) = oneshot::channel();
        if self
            .to_controller_tx
            .send(ToController::Control(ControlCommand { msg, resp_tx }))
            .is_err()
        {
            let _ = backend_tx.send(BackendCmd::WriteReply(Message::Reply {
                id,
                ok: false,
                result: None,
                error: Some(SidecarError::new("node_gone", "node unavailable")),
            }));
            return;
        }

        let query_timeout = self.query_timeout;
        tokio::spawn(async move {
            let reply = match timeout(query_timeout, resp_rx).await {
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
            let _ = backend_tx.send(BackendCmd::WriteReply(reply));
        });
    }
}

/// One task per local backend: owns the TCP connection and its reply table.
async fn backend_task(
    service: String,
    addr: String,
    mut cmd_rx: mpsc::UnboundedReceiver<BackendCmd>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
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
        let _ = event_tx.send(BackendEvent::Status(BackendStatus {
            service: service.clone(),
            ready: true,
        }));

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
                        let _ = event_tx.send(BackendEvent::Control {
                            service: service.clone(),
                            msg: control,
                        });
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

        let _ = event_tx.send(BackendEvent::Status(BackendStatus {
            service: service.clone(),
            ready: false,
        }));
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
    cmd_rx: &mut mpsc::UnboundedReceiver<BackendCmd>,
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
            },
        }
    }
}
