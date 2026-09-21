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
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::auth;
use crate::config::{ConfigHandle, LocalServiceEntry};
use crate::log::{LogLevel, LogStruct};
use crate::service_protocol;
use crate::sidecar_protocol::{self, Message, PROTOCOL_VERSION, SidecarError};

/// 后端发起的控制指令及其响应通道。
pub struct ControlRequest {
    pub msg: Message,
    pub resp_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// 单个后端的「id → 响应等待者」挂起表。
type PendingMap = Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<Vec<u8>, String>>>>>;
/// 单个后端的写半连接。
type BackendWriter = Arc<Mutex<OwnedWriteHalf>>;
/// 服务名 → 写半连接。
type BackendWriters = Arc<Mutex<HashMap<String, BackendWriter>>>;

pub struct InboundServiceRequest {
    pub service: String,
    pub payload: Vec<u8>,
    pub response_tx: oneshot::Sender<Result<service_protocol::Response, String>>,
}

pub struct ServiceDispatcher {
    inbound_rx: mpsc::UnboundedReceiver<InboundServiceRequest>,
    cmd_tx: mpsc::UnboundedSender<ControlRequest>,
    config: ConfigHandle,
    local_services: Vec<LocalServiceEntry>,
    shutdown_rx: watch::Receiver<bool>,
    pending_requests: HashMap<String, PendingMap>,
    backend_writers: BackendWriters,
    query_timeout: Duration,
}

impl ServiceDispatcher {
    pub fn new(
        inbound_rx: mpsc::UnboundedReceiver<InboundServiceRequest>,
        cmd_tx: mpsc::UnboundedSender<ControlRequest>,
        config: ConfigHandle,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let configured = config.read().services.dispatcher.local_services.clone();
        // 名称非法（保留名 "service" 或含非法字符）的服务不建立后端连接
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
        let mut pending_requests = HashMap::new();
        for svc in &local_services {
            pending_requests.insert(svc.name.clone(), Arc::new(Mutex::new(HashMap::new())));
        }
        Self {
            inbound_rx,
            cmd_tx,
            config,
            local_services,
            shutdown_rx,
            pending_requests,
            backend_writers: Arc::new(Mutex::new(HashMap::new())),
            query_timeout,
        }
    }

    pub async fn run(mut self) {
        if self.config.dispatcher_enabled() {
            self.init_backend_connections().await;
        } else {
            LogStruct::new(
                LogLevel::Warning,
                "ServiceDispatcher 已禁用",
                "services.dispatcher.enabled = false",
            )
            .emit();
        }

        loop {
            tokio::select! {
                maybe_req = self.inbound_rx.recv() => {
                    let Some(req) = maybe_req else {
                        // 所有发送者已关闭，结束
                        return;
                    };
                    let pending_map = self.pending_requests.get(&req.service).cloned();
                    let writer = {
                        let map = self.backend_writers.lock().await;
                        map.get(&req.service).cloned()
                    };

                    if let (Some(pending_map), Some(writer)) = (pending_map, writer) {
                        let query_timeout = self.query_timeout;
                        tokio::spawn(async move {
                            let response = Self::handle_request_with_backend(
                                req.service,
                                req.payload,
                                pending_map,
                                writer,
                                query_timeout,
                            )
                            .await;
                            let _ = req.response_tx.send(response);
                        });
                    } else {
                        // 没有对应的后端连接，直接返回错误
                        let _ = req
                            .response_tx
                            .send(Err(format!("No backend for service {}", req.service)));
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    return;
                }
            }
        }
    }

    async fn init_backend_connections(&mut self) {
        let backend_writers = self.backend_writers.clone();
        let query_timeout = self.query_timeout;
        for service in &self.local_services {
            let addr = format!("127.0.0.1:{}", service.port);
            let service_name = service.name.clone();
            let pending_map = self.pending_requests.get(&service_name).cloned().unwrap();
            let cmd_tx = self.cmd_tx.clone();

            tokio::spawn(Self::backend_read_loop(
                service_name,
                pending_map,
                cmd_tx,
                backend_writers.clone(),
                addr,
                query_timeout,
            ));
        }
    }

    async fn backend_read_loop(
        service_name: String,
        pending_map: PendingMap,
        cmd_tx: mpsc::UnboundedSender<ControlRequest>,
        backend_writers: BackendWriters,
        addr: String,
        query_timeout: Duration,
    ) {
        let mut backoff = 1u64;

        loop {
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    backoff = 1;
                    let (mut read_half, write_half) = stream.into_split();
                    let writer = Arc::new(Mutex::new(write_half));

                    backend_writers
                        .lock()
                        .await
                        .insert(service_name.clone(), writer.clone());

                    LogStruct::new(
                        LogLevel::Preset,
                        "后端连接建立",
                        format!("{} -> {}", service_name, addr),
                    )
                    .emit();

                    match Self::handshake(&mut read_half, &writer).await {
                        Ok(()) => {
                            let _ = Self::read_loop_inner(
                                read_half,
                                &service_name,
                                &pending_map,
                                &cmd_tx,
                                &writer,
                                query_timeout,
                            )
                            .await;
                        }
                        Err(e) => {
                            LogStruct::new(
                                LogLevel::Warning,
                                "后端协议握手失败",
                                format!("{}: {}", service_name, e),
                            )
                            .emit();
                        }
                    }

                    backend_writers.lock().await.remove(&service_name);

                    LogStruct::new(
                        LogLevel::Warning,
                        "后端连接断开",
                        format!("{} 将在 {} 秒后重连", service_name, backoff),
                    )
                    .emit();
                }
                Err(e) => {
                    LogStruct::new(
                        LogLevel::Warning,
                        "后端连接失败",
                        format!("{}: {}, {} 秒后重试", service_name, e, backoff),
                    )
                    .emit();
                }
            }

            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = min(backoff * 2, 60);
        }
    }

    /// 与后端交换 hello，校验协议版本。
    async fn handshake(
        read_half: &mut OwnedReadHalf,
        writer: &BackendWriter,
    ) -> Result<(), String> {
        let hello = Message::Hello {
            version: PROTOCOL_VERSION,
        };
        {
            let mut w = writer.lock().await;
            sidecar_protocol::write_frame(&mut *w, &hello)
                .await
                .map_err(|e| e.to_string())?;
        }
        match sidecar_protocol::read_frame(read_half).await {
            Ok(Message::Hello { version }) if version == PROTOCOL_VERSION => Ok(()),
            Ok(Message::Hello { version }) => Err(format!(
                "协议版本不兼容: 对端 {version}, 本端 {PROTOCOL_VERSION}"
            )),
            Ok(_) => Err("期望 hello 首帧".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn read_loop_inner(
        mut read_half: OwnedReadHalf,
        service_name: &str,
        pending_map: &PendingMap,
        cmd_tx: &mpsc::UnboundedSender<ControlRequest>,
        writer: &BackendWriter,
        query_timeout: Duration,
    ) -> Result<(), ()> {
        loop {
            let msg = match sidecar_protocol::read_frame(&mut read_half).await {
                Ok(msg) => msg,
                Err(e) => {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{}: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }
            };

            match msg {
                Message::Reply {
                    id,
                    ok,
                    result,
                    error,
                } => {
                    let sender = pending_map.lock().await.remove(&id);
                    match sender {
                        Some(tx) => {
                            let outcome = if ok {
                                Ok(result.unwrap_or_default())
                            } else {
                                Err(error
                                    .map(|e| e.message)
                                    .unwrap_or_else(|| "backend error".to_string()))
                            };
                            let _ = tx.send(outcome);
                        }
                        None => {
                            LogStruct::new(
                                LogLevel::Warning,
                                "未知响应",
                                format!("{}: 未匹配的 id {}", service_name, id),
                            )
                            .emit();
                        }
                    }
                }
                control => {
                    let Some(id) = control.id() else {
                        LogStruct::new(
                            LogLevel::Warning,
                            "后端协议错误",
                            format!("{}: 非控制消息 {:?}", service_name, control),
                        )
                        .emit();
                        continue;
                    };

                    let (resp_tx, resp_rx) = oneshot::channel();
                    if cmd_tx
                        .send(ControlRequest {
                            msg: control,
                            resp_tx,
                        })
                        .is_err()
                    {
                        let reply = Message::Reply {
                            id,
                            ok: false,
                            result: None,
                            error: Some(SidecarError::new("node_gone", "node unavailable")),
                        };
                        let _ = Self::write_msg(writer, &reply).await;
                        continue;
                    }

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
                    let _ = Self::write_msg(writer, &reply).await;
                }
            }
        }
    }

    async fn write_msg(writer: &BackendWriter, msg: &Message) -> Result<(), String> {
        let mut w = writer.lock().await;
        sidecar_protocol::write_frame(&mut *w, msg)
            .await
            .map_err(|e| e.to_string())
    }

    async fn handle_request_with_backend(
        service: String,
        payload: Vec<u8>,
        pending_map: PendingMap,
        writer: BackendWriter,
        query_timeout: Duration,
    ) -> Result<service_protocol::Response, String> {
        let id = Uuid::new_v4();
        let (resp_tx, resp_rx) = oneshot::channel();
        pending_map.lock().await.insert(id, resp_tx);

        let request = Message::Request {
            id,
            service,
            payload,
        };
        {
            let mut w = writer.lock().await;
            if let Err(e) = sidecar_protocol::write_frame(&mut *w, &request).await {
                pending_map.lock().await.remove(&id);
                return Err(format!("write to backend failed: {}", e));
            }
        }

        match timeout(query_timeout, resp_rx).await {
            Ok(Ok(Ok(data))) => Ok(service_protocol::Response {
                success: true,
                data,
            }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("backend response channel closed".to_string()),
            Err(_) => {
                // 超时，清理 pending 条目
                pending_map.lock().await.remove(&id);
                Err("backend request timeout".to_string())
            }
        }
    }
}
