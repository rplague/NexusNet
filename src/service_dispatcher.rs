#![allow(dead_code)]

use crate::config::DispatcherConfig;
use crate::{LogLevel, LogStruct};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

// ─── 命令通道 ──────────────────────────────────────────────

/// 后端通过 @call_remote 请求节点发送 P2P 远程调用
#[derive(Debug)]
pub enum ManagerCommand {
    CallRemote {
        peer: String,
        service: String,
        payload: Vec<u8>,
        response_tx: oneshot::Sender<Result<Vec<u8>, String>>,
    },
}

// ─── JSON 请求格式 ─────────────────────────────────────────

/// 后端主动请求的 JSON：{"service": "xxx", "payload": "base64..."}
#[derive(Debug, Deserialize)]
struct BackendServiceReq {
    service: String,
    #[serde(with = "base64_bytes")]
    payload: Vec<u8>,
}

/// @call_remote 的参数 JSON：{"peer": "...", "service": "...", "data": "base64..."}
#[derive(Debug, Deserialize)]
struct CallRemoteReq {
    peer: String,
    service: String,
    #[serde(with = "base64_bytes")]
    data: Vec<u8>,
}

/// base64 编解码模块
mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s)
            .map_err(serde::de::Error::custom)
    }
}

// ─── 帧协议 ──────────────────────────────────────────────
//
// [4-byte total_len][4-byte req_id][payload]
//   total_len       = req_id(4) + payload 的总字节数
//   req_id (偶数)   = 节点→后端（转发请求）
//   req_id (奇数)   = 后端→节点（后端主动请求）
//   读端通过奇偶区分：偶数 = 对转发请求的响应，奇数 = 后端发来的请求

// ─── 后端持久连接 ──────────────────────────────────────────

struct BackendConn {
    writer: Mutex<TcpStream>,
    /// 挂起的转发请求：req_id → 响应接收通道
    pending: Mutex<HashMap<u32, std::sync::mpsc::Sender<Result<Vec<u8>, String>>>>,
    /// 转发请求 ID（节点→后端，偶数）
    next_fwd_id: AtomicU32,
    name: String,
}

impl BackendConn {
    fn connect(name: &str, addr: &str) -> Result<(Arc<Self>, TcpStream), String> {
        let stream = TcpStream::connect(addr)
            .map_err(|e| format!("连接后端 {} ({}): {}", name, addr, e))?;
        let reader = stream
            .try_clone()
            .map_err(|e| format!("克隆连接 {}: {}", name, e))?;

        let conn = Arc::new(BackendConn {
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            next_fwd_id: AtomicU32::new(0),
            name: name.to_string(),
        });

        Ok((conn, reader))
    }

    /// 发送转发请求（偶数 req_id），阻塞等待响应
    fn forward(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let req_id = self.next_fwd_id.fetch_add(2, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();

        {
            let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
            pending.insert(req_id, tx);
        }

        let total_len = 4u32 + payload.len() as u32;
        let mut buf = Vec::with_capacity(8 + payload.len());
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(&req_id.to_be_bytes());
        buf.extend_from_slice(payload);

        {
            let mut writer = self.writer.lock().map_err(|e| e.to_string())?;
            writer
                .write_all(&buf)
                .map_err(|e| format!("写入后端 {} 失败: {}", self.name, e))?;
            writer.flush().map_err(|e| format!("刷新失败: {}", e))?;
        }

        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| format!("等待后端 {} 响应超时", self.name))?
    }
}

// ─── 读线程 ────────────────────────────────────────────────

fn reader_loop(
    mut reader: TcpStream,
    conn: Arc<BackendConn>,
    name: &str,
    cmd_tx: mpsc::Sender<ManagerCommand>,
    dispatcher: Arc<ServiceDispatcher>,
) {
    let name = name.to_string();
    loop {
        // 读帧头：4 字节 total_len
        let mut len_buf = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut len_buf) {
            log_info(
                "服务调度器".to_string(),
                format!("后端 {} 连接断开: {}", name, e),
            );
            break;
        }
        let total_len = u32::from_be_bytes(len_buf) as usize;
        if total_len < 4 {
            log_info(
                "服务调度器".to_string(),
                format!("后端 {} 帧格式错误 (total_len={})", name, total_len),
            );
            break;
        }

        // 读 req_id + payload
        let mut body = vec![0u8; total_len];
        if let Err(e) = reader.read_exact(&mut body) {
            log_info(
                "服务调度器".to_string(),
                format!("后端 {} 读帧体失败: {}", name, e),
            );
            break;
        }

        let req_id = u32::from_be_bytes(body[..4].try_into().unwrap());
        let payload = &body[4..];

        // 偶数 req_id = 转发请求的响应，奇数 = 后端请求
        if req_id % 2 == 0 {
            // 响应：交付给挂起的转发请求
            let sender = {
                let mut pending = match conn.pending.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                pending.remove(&req_id)
            };
            if let Some(sender) = sender {
                let _ = sender.send(Ok(payload.to_vec()));
            } else {
                log_info(
                    "服务调度器".to_string(),
                    format!("后端 {} 收到未预期的响应 req_id={}", name, req_id),
                );
            }
        } else {
            // 请求：后端主动发起，需要处理并回复
            let result = handle_backend_request(payload, &dispatcher, &cmd_tx);

            // 写响应帧回后端
            let resp_payload = match &result {
                Ok(data) => data.clone(),
                Err(e) => e.as_bytes().to_vec(),
            };
            let resp_len = 4u32 + resp_payload.len() as u32;
            let mut resp_buf = Vec::with_capacity(8 + resp_payload.len());
            resp_buf.extend_from_slice(&resp_len.to_be_bytes());
            resp_buf.extend_from_slice(&req_id.to_be_bytes());
            resp_buf.extend_from_slice(&resp_payload);

            if let Ok(mut writer) = conn.writer.lock() {
                let _ = writer.write_all(&resp_buf);
                let _ = writer.flush();
            }
        }
    }
}

/// 处理后端发起的请求（在读线程中调用）
fn handle_backend_request(
    payload: &[u8],
    dispatcher: &ServiceDispatcher,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
) -> Result<Vec<u8>, String> {
    let req: BackendServiceReq =
        serde_json::from_slice(payload).map_err(|e| format!("解析后端请求: {}", e))?;

    if let Some(cmd) = req.service.strip_prefix('@') {
        handle_internal_command(cmd, &req.payload, dispatcher, cmd_tx)
    } else {
        // 后端可以请求其他服务（后端间互调）
        dispatcher.forward(&req.service, &req.payload)
    }
}

// ─── 内置命令处理 ──────────────────────────────────────────

fn handle_internal_command(
    cmd: &str,
    payload: &[u8],
    dispatcher: &ServiceDispatcher,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
) -> Result<Vec<u8>, String> {
    match cmd {
        "ping" => Ok(b"pong".to_vec()),

        "list_services" => {
            let names = dispatcher.get_service_names();
            serde_json::to_vec(&names).map_err(|e| e.to_string())
        }

        "call_remote" => {
            let req: CallRemoteReq = serde_json::from_slice(payload)
                .map_err(|e| format!("解析 call_remote 参数: {}", e))?;

            let (tx, rx) = oneshot::channel();

            // 用临时 tokio runtime 发送 async 消息到主循环
            let rt =
                tokio::runtime::Runtime::new().map_err(|e| format!("创建 runtime: {}", e))?;
            rt.block_on(async {
                cmd_tx
                    .send(ManagerCommand::CallRemote {
                        peer: req.peer,
                        service: req.service,
                        payload: req.data,
                        response_tx: tx,
                    })
                    .await
                    .map_err(|e| format!("发送到主循环: {}", e))
            })?;

            // 阻塞等待远程调用结果
            rx.blocking_recv()
                .map_err(|_| "远程调用未收到响应".to_string())?
        }

        other => Err(format!("未知内部命令: @{}", other)),
    }
}

// ─── ServiceDispatcher ─────────────────────────────────────

pub struct ServiceDispatcher {
    config: DispatcherConfig,
    backends: Mutex<HashMap<String, Arc<BackendConn>>>,
}

impl ServiceDispatcher {
    pub fn new(config: &DispatcherConfig) -> Self {
        if config.enabled {
            let count = config.local_services.len();
            log_info(
                "服务调度器".to_string(),
                format!("已加载 {} 个本地服务路由", count),
            );
            for entry in &config.local_services {
                log_info(
                    "服务调度器".to_string(),
                    format!("  {} -> {}:{}", entry.name, entry.host, entry.port),
                );
            }
        }

        ServiceDispatcher {
            config: config.clone(),
            backends: Mutex::new(HashMap::new()),
        }
    }

    /// 连接所有后端并启动读线程
    ///
    /// 需要 self 已被 Arc 包装（读线程需要 Arc<ServiceDispatcher>）。
    /// 返回连接失败的服务名列表。
    pub fn connect_all(
        self: &Arc<Self>,
        cmd_tx: mpsc::Sender<ManagerCommand>,
    ) -> Vec<(String, String)> {
        let mut failures = Vec::new();

        for entry in &self.config.local_services {
            let addr = format!("{}:{}", entry.host, entry.port);
            match BackendConn::connect(&entry.name, &addr) {
                Ok((conn, reader)) => {
                    let conn_for_reader = conn.clone();
                    let dispatcher = self.clone();
                    let cmd_tx = cmd_tx.clone();
                    let name = entry.name.clone();
                    thread::spawn(move || {
                        reader_loop(reader, conn_for_reader, &name, cmd_tx, dispatcher);
                    });
                    self.backends.lock().unwrap().insert(entry.name.clone(), conn);
                    log_info(
                        "服务调度器".to_string(),
                        format!("后端已连接: {} (读线程启动)", entry.name),
                    );
                }
                Err(e) => {
                    failures.push((entry.name.clone(), e));
                }
            }
        }

        failures
    }

    /// 转发请求到后端（阻塞，通过持久连接）
    pub fn forward(&self, service: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        let backends = self.backends.lock().map_err(|e| e.to_string())?;
        let conn = backends
            .get(service)
            .ok_or_else(|| format!("未知服务: {}", service))?;
        conn.forward(payload)
    }

    /// 获取配置中定义的服务名列表
    pub fn get_service_names(&self) -> Vec<String> {
        self.config
            .local_services
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// 获取包含地址信息的原始服务列表
    pub fn list_services_raw(&self) -> Vec<(String, String, u16)> {
        self.config
            .local_services
            .iter()
            .map(|s| (s.name.clone(), s.host.clone(), s.port))
            .collect()
    }

    /// 调度器是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// 健康检查：返回已连接但不在 backends 中的服务
    /// （connect_all 失败的不会在 backends 里）
    pub fn health_check(&self) -> Vec<String> {
        let backends = self.backends.lock().unwrap();
        let mut unhealthy = Vec::new();
        for entry in &self.config.local_services {
            if !backends.contains_key(&entry.name) {
                unhealthy.push(format!("{} ({}:{})", entry.name, entry.host, entry.port));
            }
        }
        unhealthy
    }
}

fn log_info(topic: String, content: String) {
    let log = LogStruct {
        level: LogLevel::Debug,
        topic,
        content,
    };
    log.logout();
}
