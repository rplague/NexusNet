use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot};
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
    pending_requests:
        HashMap<String, Arc<Mutex<HashMap<String, oneshot::Sender<Result<Vec<u8>, String>>>>>>,
    backend_writers: HashMap<String, Arc<Mutex<OwnedWriteHalf>>>,
}

impl ServiceDispatcher {
    pub fn new(
        inbound_rx: mpsc::UnboundedReceiver<InboundServiceRequest>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        config: ConfigHandle,
    ) -> Self {
        let local_services = config.read().services.dispatcher.local_services.clone();
        let mut pending_requests = HashMap::new();
        for svc in &local_services {
            pending_requests.insert(svc.name.clone(), Arc::new(Mutex::new(HashMap::new())));
        }
        Self {
            inbound_rx,
            cmd_tx,
            config,
            local_services,
            pending_requests,
            backend_writers: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        self.init_backend_connections().await;

        while let Some(req) = self.inbound_rx.recv().await {
            let pending_map = self.pending_requests.get(&req.service).cloned(); // 获取该服务的等待表
            let writer = self.backend_writers.get(&req.service).cloned();

            if let (Some(pending_map), Some(writer)) = (pending_map, writer) {
                tokio::spawn(async move {
                    let response =
                        Self::handle_request_with_backend(req.payload, pending_map, writer).await;
                    let _ = req.response_tx.send(response);
                });
            } else {
                // 没有对应的后端连接，直接返回错误
                let _ = req
                    .response_tx
                    .send(Err(format!("No backend for service {}", req.service)));
            }
        }
    }

    async fn init_backend_connections(&mut self) {
        for service in &self.local_services {
            let addr = format!("127.0.0.1:{}", service.port);
            // 重试机制：最多尝试 3 次，每次间隔 1 秒
            let mut retries = 3;
            let stream = loop {
                match TcpStream::connect(&addr).await {
                    Ok(stream) => break stream,
                    Err(e) => {
                        retries -= 1;
                        if retries == 0 {
                            panic!("无法连接到后端服务 {}: {}", service.name, e);
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            };

            // 分割读写半句柄
            let (read_half, write_half) = stream.into_split();

            // 启动读任务
            let service_name = service.name.clone();
            let pending_map = self.pending_requests.get(&service_name).cloned().unwrap();
            let cmd_tx = self.cmd_tx.clone();
            let writer = Arc::new(Mutex::new(write_half));

            tokio::spawn(Self::backend_read_loop(
                read_half,
                service_name,
                pending_map,
                cmd_tx,
                writer.clone(),
            ));

            // 存储写半句柄
            self.backend_writers.insert(service.name.clone(), writer);

            LogStruct::new(
                LogLevel::Preset,
                "后端连接建立",
                format!("{} -> {}", service.name, addr),
            )
            .emit();
        }
    }

    async fn backend_read_loop(
        mut read_half: OwnedReadHalf,
        service_name: String,
        pending_map: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Vec<u8>, String>>>>>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        writer: Arc<Mutex<OwnedWriteHalf>>,
    ) {
        loop {
            let mut len_buf = [0u8; 4];
            if let Err(e) = read_half.read_exact(&mut len_buf).await {
                LogStruct::new(
                    LogLevel::Error,
                    "后端读错误",
                    format!("{} 读取 uuid_len 失败: {}", service_name, e),
                )
                .emit();
                break;
            }
            let uuid_len = u32::from_be_bytes(len_buf);

            if uuid_len == 0 {
                // 主动控制指令：没有 UUID，直接读取 payload_len + payload
                let mut payload_len_buf = [0u8; 4];
                if let Err(e) = read_half.read_exact(&mut payload_len_buf).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取控制指令 payload_len 失败: {}", service_name, e),
                    )
                    .emit();
                    break;
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
                    break;
                }
                if let Ok(cmd_str) = String::from_utf8(payload) {
                    let parts: Vec<&str> = cmd_str.splitn(3, '|').collect();
                    if parts.len() == 3 {
                        let prefix = parts[0].to_string();
                        let content = parts[1].to_string();
                        let payload = parts[2].as_bytes().to_vec();
                        // 构造 Command
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let command = Command {
                            prefix,
                            content,
                            payload,
                            resp_tx,
                        };
                        // 发送给 NodeController
                        if let Err(e) = cmd_tx.send(command) {
                            LogStruct::new(
                                LogLevel::Error,
                                "发送命令失败",
                                format!("{}: {}", service_name, e),
                            )
                            .emit();
                        } else {
                            // 等待执行结果
                            match timeout(Duration::from_secs(30), resp_rx).await {
                                Ok(Ok(Ok(result_data))) => {
                                    let mut response = Vec::new();
                                    response.extend_from_slice(&0u32.to_be_bytes());
                                    response.extend_from_slice(
                                        &(result_data.len() as u32).to_be_bytes(),
                                    );
                                    response.extend_from_slice(&result_data);

                                    let mut writer_guard = writer.lock().await;
                                    if let Err(e) = writer_guard.write_all(&response).await {
                                        LogStruct::new(
                                            LogLevel::Error,
                                            "发送命令响应失败",
                                            format!("{}: {}", service_name, e),
                                        )
                                        .emit();
                                    } else {
                                        let _ = writer_guard.flush().await;
                                    }
                                }
                                Ok(Ok(Err(e))) => {
                                    LogStruct::new(
                                        LogLevel::Warning,
                                        "命令执行失败",
                                        format!("{}: {}", service_name, e),
                                    )
                                    .emit();
                                }
                                Ok(Err(_)) => {
                                    LogStruct::new(
                                        LogLevel::Error,
                                        "命令响应通道关闭",
                                        &service_name,
                                    )
                                    .emit();
                                }
                                Err(_) => {}
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
                    LogStruct::new(LogLevel::Warning, "控制指令非 UTF-8", &service_name).emit();
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
                    break;
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
                        break;
                    }
                };

                // 读取 payload_len
                let mut payload_len_buf = [0u8; 4];
                if let Err(e) = read_half.read_exact(&mut payload_len_buf).await {
                    LogStruct::new(
                        LogLevel::Error,
                        "后端读错误",
                        format!("{} 读取响应 payload_len 失败: {}", service_name, e),
                    )
                    .emit();
                    break;
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
                    break;
                }

                // 从等待表中查找并唤醒等待者
                let sender = {
                    let mut map = pending_map.lock().await;
                    map.remove(&uuid)
                };
                if let Some(tx) = sender {
                    let _ = tx.send(Ok(payload));
                }
            }
        }
        LogStruct::new(
            LogLevel::Warning,
            "后端连接断开",
            format!("{} 读循环退出", service_name),
        )
        .emit();
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
        pending_map: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Vec<u8>, String>>>>>,
        writer: Arc<Mutex<OwnedWriteHalf>>,
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

        match timeout(Duration::from_secs(30), resp_rx).await {
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
