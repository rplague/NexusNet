use crate::config::DispatcherConfig;
use crate::{LogLevel, LogStruct};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use tokio::sync::mpsc;

/// 本地服务调度器
///
/// 架构：微内核 + 边车模式
/// 通信层（NexusNet P2P）对外接收请求，Dispatcher 根据 service=xxx 字段
/// 将请求转发到本地回环地址上的对应业务进程（如 OCR、冷存储）。
///
/// 业务进程是独立的进程，监听 localhost:port，语言无关。
///
/// 扩展：管理通道
/// Dispatcher 在 management_port 上开启 TCP 监听，接收本地进程（如 CLI）
/// 发来的 JSON 命令。其中 call_remote 类型的命令会通过 ManagerCommand 通道
/// 交给节点主循环处理（走 P2P request-response 协议）。
pub struct ServiceDispatcher {
    config: DispatcherConfig,
    /// 服务名 → 后端地址的表
    routes: HashMap<String, String>,
}

/// 本地服务通过 dispatcher 管理口发出的命令
pub enum ManagerCommand {
    /// 调用远程节点的服务
    CallRemote {
        /// 目标节点 PeerId (Base58 编码)
        peer: String,
        /// 目标服务名
        service: String,
        /// 请求载荷
        payload: Vec<u8>,
        /// 发送响应给管理口的通道
        response_tx: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
    },
}

impl ServiceDispatcher {
    pub fn new(config: &DispatcherConfig) -> Self {
        let mut routes = HashMap::new();
        for entry in &config.local_services {
            let addr = format!("{}:{}", entry.host, entry.port);
            routes.insert(entry.name.clone(), addr);
        }

        if config.enabled {
            let count = routes.len();
            log_info(
                "服务调度器".to_string(),
                format!("已加载 {} 个本地服务路由", count),
            );
            for (name, addr) in &routes {
                log_info("服务调度器".to_string(), format!("  {} -> {}", name, addr));
            }
        }

        ServiceDispatcher {
            config: config.clone(),
            routes,
        }
    }

    /// 启动管理端口监听
    ///
    /// 在 `management_port` 上接受 TCP 连接，JSON 协议：
    /// ```json
    /// {"action":"call_remote","peer":"PeerId","service":"ocr","payload":"base64..."}
    /// {"action":"list_services"}
    /// {"action":"peers"}
    /// ```
    /// 返回 JSON 响应。
    /// `cmd_tx` 用于将 CallRemote 请求转发到主循环。
    pub fn start_management(
        config: DispatcherConfig,
        cmd_tx: mpsc::Sender<ManagerCommand>,
    ) {
        let port = config.management_port;
        let bind_addr = format!("127.0.0.1:{}", port);

        log_info(
            "服务调度器".to_string(),
            format!("管理端口监听于 {}", bind_addr),
        );

        std::thread::spawn(move || {
            let listener = match TcpListener::bind(&bind_addr) {
                Ok(l) => l,
                Err(e) => {
                    log_info(
                        "服务调度器".to_string(),
                        format!("管理端口绑定失败: {}", e),
                    );
                    return;
                }
            };

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(e) =
                            handle_management_connection(stream, &config, &cmd_tx)
                        {
                            log_info(
                                "服务调度器".to_string(),
                                format!("管理连接处理错误: {}", e),
                            );
                        }
                    }
                    Err(e) => {
                        log_info(
                            "服务调度器".to_string(),
                            format!("管理连接接受失败: {}", e),
                        );
                    }
                }
            }
        });
    }

    /// 根据服务名查询后端地址
    pub fn get_backend(&self, service_name: &str) -> Option<&str> {
        self.routes.get(service_name).map(|s| s.as_str())
    }

    /// 转发请求到后端服务并返回响应
    ///
    /// 协议约定：
    ///   发送：4 字节大端长度前缀 + 请求载荷（{request_length}{request_body}）
    ///   接收：4 字节大端长度前缀 + 响应载荷（{response_length}{response_body}）
    ///   读取超时：10 秒
    ///
    /// `request_body` 是已序列化的请求载荷（如 JSON）。
    /// 返回后端服务的响应字节。
    pub fn forward(&self, service_name: &str, request_body: &[u8]) -> Result<Vec<u8>, String> {
        let backend = self
            .get_backend(service_name)
            .ok_or_else(|| format!("未知服务: {}", service_name))?;

        log_info(
            "服务调度器".to_string(),
            format!("转发请求到 {} ({})", service_name, backend),
        );

        let mut stream =
            TcpStream::connect(backend).map_err(|e| format!("连接后端 {} 失败: {}", backend, e))?;

        // 设置读超时，防止后端不关连接时永久挂起
        let timeout = Duration::from_secs(10);
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("设置读超时失败: {}", e))?;

        // 发送：4 字节长度前缀 + 请求体
        let req_len = (request_body.len() as u32).to_be_bytes();
        let mut send_buf = Vec::with_capacity(4 + request_body.len());
        send_buf.extend_from_slice(&req_len);
        send_buf.extend_from_slice(request_body);

        stream
            .write_all(&send_buf)
            .map_err(|e| format!("写入请求到 {} 失败: {}", backend, e))?;
        stream
            .flush()
            .map_err(|e| format!("刷新写入到 {} 失败: {}", backend, e))?;

        // 读取：先读 4 字节长度头，再读指定长度的响应体
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .map_err(|e| format!("读取 {} 响应长度前缀失败: {}", backend, e))?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;

        if resp_len == 0 {
            log_info(
                "服务调度器".to_string(),
                format!("收到 {} 空响应", service_name),
            );
            return Ok(Vec::new());
        }

        let mut response = vec![0u8; resp_len];
        stream
            .read_exact(&mut response)
            .map_err(|e| format!("读取 {} 响应体 ({} 字节) 失败: {}", backend, resp_len, e))?;

        log_info(
            "服务调度器".to_string(),
            format!("收到 {} 响应 ({} 字节)", service_name, resp_len),
        );

        Ok(response)
    }

    /// 健康检查：尝试连接所有本地后端，返回不可达的服务列表
    pub fn health_check(&self) -> Vec<String> {
        let mut unhealthy = Vec::new();
        for (name, addr) in &self.routes {
            match TcpStream::connect_timeout(
                &addr.parse().unwrap(),
                std::time::Duration::from_secs(3),
            ) {
                Ok(_stream) => {}
                Err(_) => {
                    unhealthy.push(format!("{} ({})", name, addr));
                }
            }
        }
        unhealthy
    }

    /// 获取所有已注册的服务名（供管理口和服务发现使用）
    pub fn list_services(&self) -> Vec<&str> {
        self.routes.keys().map(|s| s.as_str()).collect()
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

    /// 获取配置中的服务列表（名称集合）
    pub fn get_service_names(&self) -> Vec<String> {
        self.config
            .local_services
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }
}

/// 处理一条管理连接：读取 JSON 命令，执行或转发
///
/// 协议版本 2 (Content-Length 前缀)：
///   客户端先发送 4 字节大端 u32 = JSON 请求体的字节长度（不含长度头自身），
///   再发送精确长度的 JSON 内容。
///   服务端先发送 4 字节大端 u32 = JSON 响应体的字节长度，
///   再发送精确长度的 JSON 内容。
///
/// 向后兼容：如果前 4 字节不是有效的长度（> 0x100000 = 1MB），
/// 则回退到 read_line 模式（单行 JSON 协议）。
fn handle_management_connection(
    mut stream: TcpStream,
    config: &DispatcherConfig,
    cmd_tx: &mpsc::Sender<ManagerCommand>,
) -> Result<(), String> {
    let peer_addr = stream.peer_addr().ok();
    log_info(
        "管理通道".to_string(),
        format!("新的管理连接: {:?}", peer_addr),
    );

    // 读取 4 字节 Content-Length 前缀
    let mut len_buf = [0u8; 4];
    let json_body = match stream.peek(&mut len_buf) {
        Ok(n) if n >= 4 => {
            let content_len = u32::from_be_bytes(len_buf) as usize;
            if content_len > 0 && content_len <= 1_048_576 {
                // 新协议：Content-Length 前缀
                // 消费掉长度头（已经 peek 过了，需要用 exact_read 覆盖掉这 4 字节）
                let mut full_buf = vec![0u8; 4 + content_len];
                stream
                    .read_exact(&mut full_buf)
                    .map_err(|e| format!("读取管理命令失败: {}", e))?;
                let body = &full_buf[4..];
                String::from_utf8(body.to_vec())
                    .map_err(|e| format!("UTF-8 解码失败: {}", e))?
            } else if content_len == 0 {
                return Ok(());
            } else {
                // 超过 1MB 或无效，回退到 read_line
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .map_err(|e| format!("读取管理命令失败: {}", e))?;
                line.trim().to_string()
            }
        }
        _ => {
            // 回退到 read_line
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("读取管理命令失败: {}", e))?;
            line.trim().to_string()
        }
    };

    let line = json_body.trim();
    if line.is_empty() {
        return Ok(());
    }

    // 解析 JSON
    let cmd: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("JSON 解析失败: {}", e))?;

    let action = cmd["action"]
        .as_str()
        .ok_or_else(|| "缺少 action 字段".to_string())?;

    // 先保存 `reader` 的所有权（在回退路径中创建），这里不需要它了
    // 管理口新协议直接使用 `stream` 写入 Content-Length 前缀

    match action {
        "call_remote" => {
            let peer = cmd["peer"]
                .as_str()
                .ok_or_else(|| "缺少 peer 字段".to_string())?
                .to_string();
            let service = cmd["service"]
                .as_str()
                .ok_or_else(|| "缺少 service 字段".to_string())?
                .to_string();
            let payload_b64 = cmd["payload"]
                .as_str()
                .ok_or_else(|| "缺少 payload 字段".to_string())?;

            let payload = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                payload_b64,
            )
            .map_err(|e| format!("payload base64 解码失败: {}", e))?;

            let (tx, rx) = tokio::sync::oneshot::channel();

            let cmd_tx = cmd_tx.clone();
            let response = tokio::runtime::Runtime::new()
                .map_err(|e| format!("创建 runtime 失败: {}", e))?
                .block_on(async move {
                    cmd_tx
                        .send(ManagerCommand::CallRemote {
                            peer,
                            service,
                            payload,
                            response_tx: tx,
                        })
                        .await
                        .map_err(|e| format!("发送命令失败: {}", e))?;

                    rx.await
                        .map_err(|e| format!("接收响应失败: {}", e))?
                });

            let resp_json = match &response {
                Ok(data) => serde_json::json!({
                    "status": "ok",
                    "data": base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        data
                    )
                }),
                Err(e) => serde_json::json!({
                    "status": "error",
                    "message": e
                }),
            };

            write_response(&mut stream, &resp_json)?;
        }

        "list_services" => {
            // 返回本地注册的服务列表（完整格式，与服务发现兼容）
            let services: Vec<serde_json::Value> = config
                .local_services
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "service_type": s.name,
                        "provider": "local",
                        "addrs": [format!("{}:{}", s.host, s.port)],
                        "ttl": 0
                    })
                })
                .collect();
            write_raw_json(&mut stream, &serde_json::to_value(&services).unwrap())?;
        }

        "ping" => {
            let resp = serde_json::json!({"status": "ok", "message": "pong"});
            write_response(&mut stream, &resp)?;
        }

        other => {
            let resp = serde_json::json!({
                "status": "error",
                "message": format!("未知 action: {}", other)
            });
            write_response(&mut stream, &resp)?;
        }
    }

    Ok(())
}

/// 向管理连接写入 Content-Length 前缀 + JSON 响应
fn write_response(stream: &mut TcpStream, value: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_string(value)
        .map_err(|e| format!("序列化响应失败: {}", e))?;
    let len = body.len() as u32;
    let len_bytes = len.to_be_bytes();
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len_bytes);
    buf.extend_from_slice(body.as_bytes());
    stream
        .write_all(&buf)
        .map_err(|e| format!("写入响应失败: {}", e))
}

/// 写入原始 JSON（不是标准结构体，直接传已构造的 Value）
fn write_raw_json(stream: &mut TcpStream, value: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_string(value)
        .map_err(|e| format!("序列化响应失败: {}", e))?;
    let len = body.len() as u32;
    let len_bytes = len.to_be_bytes();
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len_bytes);
    buf.extend_from_slice(body.as_bytes());
    stream
        .write_all(&buf)
        .map_err(|e| format!("写入响应失败: {}", e))
}

fn log_info(topic: String, content: String) {
    let log = LogStruct {
        level: LogLevel::Debug,
        topic,
        content,
    };
    log.logout();
}
