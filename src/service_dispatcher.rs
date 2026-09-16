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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::config::{ConfigHandle, LocalServiceEntry};
use crate::log::{LogLevel, LogStruct};
use crate::service_protocol;

pub struct Command {
    pub prefix: String,
    pub content: String,
    pub payload: Vec<u8>,
    pub resp_tx: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// 单个后端的「UUID → 响应等待者」挂起表。
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Vec<u8>, String>>>>>;
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
    cmd_tx: mpsc::UnboundedSender<Command>,
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
        cmd_tx: mpsc::UnboundedSender<Command>,
        config: ConfigHandle,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let local_services = config.read().services.dispatcher.local_services.clone();
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
                    let pending_map = self.pending_requests.get(&req.service).cloned(); // 获取该服务的等待表
                    let writer = {
                        let map = self.backend_writers.lock().await;
                        map.get(&req.service).cloned()
                    };

                    if let (Some(pending_map), Some(writer)) = (pending_map, writer) {
                        let query_timeout = self.query_timeout;
                        tokio::spawn(async move {
                            let response = Self::handle_request_with_backend(
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
        cmd_tx: mpsc::UnboundedSender<Command>,
        backend_writers: BackendWriters,
        addr: String,
        query_timeout: Duration,
    ) {
        let mut backoff = 1u64;

        loop {
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    backoff = 1;
                    let (read_half, write_half) = stream.into_split();
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

                    let _ = Self::read_loop_inner(
                        read_half,
                        &service_name,
                        &pending_map,
                        &cmd_tx,
                        &writer,
                        query_timeout,
                    )
                    .await;

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

    async fn read_loop_inner(
        mut read_half: OwnedReadHalf,
        service_name: &str,
        pending_map: &PendingMap,
        cmd_tx: &mpsc::UnboundedSender<Command>,
        writer: &BackendWriter,
        query_timeout: Duration,
    ) -> Result<(), ()> {
        loop {
            let mut len_buf = [0u8; 4];
            if let Err(e) = read_half.read_exact(&mut len_buf).await {
                LogStruct::new(
                    LogLevel::Error,
                    "后端读错误",
                    format!("{} 读取 uuid_len 失败: {}", service_name, e),
                )
                .emit();
                return Err(());
            }
            let uuid_len = u32::from_be_bytes(len_buf);

            if uuid_len == 0 {
                let mut payload_len_buf = [0u8; 4];
                if let Err(e) = read_half.read_exact(&mut payload_len_buf).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取控制指令 payload_len 失败: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }
                let payload_len = u32::from_be_bytes(payload_len_buf) as usize;
                let mut payload = vec![0u8; payload_len];
                if let Err(e) = read_half.read_exact(&mut payload).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取控制指令 payload 失败: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }
                if let Ok(cmd_str) = String::from_utf8(payload) {
                    let parts: Vec<&str> = cmd_str.splitn(3, '|').collect();
                    if parts.len() == 3 {
                        let prefix = parts[0].to_string();
                        let content = parts[1].to_string();
                        let payload = parts[2].as_bytes().to_vec();
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let command = Command {
                            prefix,
                            content,
                            payload,
                            resp_tx,
                        };
                        if let Err(e) = cmd_tx.send(command) {
                            LogStruct::new(
                                LogLevel::Error,
                                "发送命令失败",
                                format!("{}: {}", service_name, e),
                            )
                            .emit();
                            Self::write_response(
                                writer,
                                format!("failed to send command: {}", e).as_bytes(),
                            )
                            .await;
                        } else {
                            match timeout(query_timeout, resp_rx).await {
                                Ok(Ok(Ok(result_data))) => {
                                    Self::write_response(writer, &result_data).await;
                                }
                                Ok(Ok(Err(e))) => {
                                    LogStruct::new(
                                        LogLevel::Warning,
                                        "命令执行失败",
                                        format!("{}: {}", service_name, e),
                                    )
                                    .emit();
                                    Self::write_response(writer, e.as_bytes()).await;
                                }
                                Ok(Err(_)) => {
                                    LogStruct::new(
                                        LogLevel::Error,
                                        "命令响应通道关闭",
                                        service_name,
                                    )
                                    .emit();
                                    Self::write_response(
                                        writer,
                                        b"command response channel closed",
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    Self::write_response(writer, b"command execution timeout")
                                        .await;
                                }
                            }
                        }
                    } else {
                        LogStruct::new(
                            LogLevel::Warning,
                            "控制指令格式错误",
                            format!("{} 期望 'prefix|content', 实际: {}", service_name, cmd_str),
                        )
                        .emit();
                    }
                } else {
                    LogStruct::new(LogLevel::Warning, "控制指令非 UTF-8", service_name).emit();
                }
            } else {
                let uuid_len_usize = uuid_len as usize;
                let mut uuid_bytes = vec![0u8; uuid_len_usize];
                if let Err(e) = read_half.read_exact(&mut uuid_bytes).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取 UUID 失败: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }
                let uuid = match String::from_utf8(uuid_bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Error,
                            "后端协议错误",
                            format!("{} UUID 非 UTF-8: {}", service_name, e),
                        )
                        .emit();
                        return Err(());
                    }
                };

                let mut payload_len_buf = [0u8; 4];
                if let Err(e) = read_half.read_exact(&mut payload_len_buf).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取响应 payload_len 失败: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }
                let payload_len = u32::from_be_bytes(payload_len_buf) as usize;
                let mut payload = vec![0u8; payload_len];
                if let Err(e) = read_half.read_exact(&mut payload).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取响应 payload 失败: {}", service_name, e),
                    )
                    .emit();
                    return Err(());
                }

                let sender = {
                    let mut map = pending_map.lock().await;
                    map.remove(&uuid)
                };
                if let Some(tx) = sender {
                    let _ = tx.send(Ok(payload));
                }
            }
        }
    }

    async fn write_response(writer: &BackendWriter, data: &[u8]) {
        let mut response = Vec::new();
        response.extend_from_slice(&0u32.to_be_bytes());
        response.extend_from_slice(&(data.len() as u32).to_be_bytes());
        response.extend_from_slice(data);

        let mut writer_guard = writer.lock().await;
        if let Err(e) = writer_guard.write_all(&response).await {
            LogStruct::new(
                LogLevel::Error,
                "发送命令响应失败",
                format!("(write_response) {}", e),
            )
            .emit();
        } else {
            let _ = writer_guard.flush().await;
        }
    }

    fn encode_request(uuid: &str, payload: &[u8]) -> Vec<u8> {
        let uuid_bytes = uuid.as_bytes();
        let mut out = Vec::with_capacity(4 + uuid_bytes.len() + 4 + payload.len());
        out.extend_from_slice(&(uuid_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(uuid_bytes);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
    async fn handle_request_with_backend(
        payload: Vec<u8>,
        pending_map: PendingMap,
        writer: BackendWriter,
        query_timeout: Duration,
    ) -> Result<service_protocol::Response, String> {
        let uuid = Uuid::new_v4().to_string();
        let (resp_tx, resp_rx) = oneshot::channel();

        {
            let mut map = pending_map.lock().await;
            map.insert(uuid.clone(), resp_tx);
        }

        let data = Self::encode_request(&uuid, &payload);
        {
            let mut writer = writer.lock().await; // 获取 MutexGuard
            if let Err(e) = writer.write_all(&data).await {
                let mut map = pending_map.lock().await;
                map.remove(&uuid);
                return Err(format!("Write to backend failed: {}", e));
            }
            let _ = writer.flush().await;
        } // MutexGuard 在此释放

        match timeout(query_timeout, resp_rx).await {
            Ok(Ok(Ok(resp_data))) => Ok(service_protocol::Response {
                success: true,
                data: resp_data,
            }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("Backend response channel closed".to_string()),
            Err(_) => {
                // 超时，清理 pending 条目
                let mut map = pending_map.lock().await;
                map.remove(&uuid);
                Err("Backend request timeout".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceDispatcher;

    #[test]
    fn encode_request_frames_uuid_and_payload_big_endian() {
        let frame = ServiceDispatcher::encode_request("abc", b"xy");
        // uuid_len (4B BE) = 3
        assert_eq!(&frame[0..4], &3u32.to_be_bytes());
        // uuid = "abc"
        assert_eq!(&frame[4..7], b"abc");
        // payload_len (4B BE) = 2
        assert_eq!(&frame[7..11], &2u32.to_be_bytes());
        // payload = "xy"
        assert_eq!(&frame[11..13], b"xy");
        assert_eq!(frame.len(), 13);
    }

    #[test]
    fn encode_request_empty_payload() {
        let frame = ServiceDispatcher::encode_request("id", b"");
        assert_eq!(&frame[0..4], &2u32.to_be_bytes());
        assert_eq!(&frame[4..6], b"id");
        assert_eq!(&frame[6..10], &0u32.to_be_bytes());
        assert_eq!(frame.len(), 10);
    }
}
