use crate::config::DispatcherConfig;
use crate::{LogLevel, LogStruct};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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

        stream
            .write_all(request_body)
            .map_err(|e| format!("写入请求到 {} 失败: {}", backend, e))?;
        stream
            .flush()
            .map_err(|e| format!("刷新写入到 {} 失败: {}", backend, e))?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|e| format!("读取 {} 响应失败: {}", backend, e))?;

        log_info(
            "服务调度器".to_string(),
            format!("收到 {} 响应 ({} 字节)", service_name, response.len()),
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

    /// 获取所有已注册的服务名
    pub fn list_services(&self) -> Vec<&str> {
        self.routes.keys().map(|s| s.as_str()).collect()
    }

    /// 调度器是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

/// 处理一条管理连接：读取 JSON 命令，执行或转发
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

    // 读取一行 JSON
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("读取管理命令失败: {}", e))?;

    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    // 解析 JSON
    let cmd: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("JSON 解析失败: {}", e))?;

    let action = cmd["action"]
        .as_str()
        .ok_or_else(|| "缺少 action 字段".to_string())?;

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

            // 写入响应
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

            let resp_line = serde_json::to_string(&resp_json)
                .map_err(|e| format!("序列化响应失败: {}", e))?;

            // stream 已被 BufReader 借用，用内部 stream 写入
            // 由于 BufReader 持有 &mut stream，我们需要重新获取写入端
            // 实际上 BufReader 被 drop 后 stream 恢复可写
            drop(reader);
            writeln!(
                std::io::BufWriter::new(&mut stream),
                "{}",
                resp_line
            )
            .map_err(|e| format!("写入响应失败: {}", e))?;
        }

        "list_services" => {
            // 返回本地注册的服务列表
            let services: Vec<String> = config
                .local_services
                .iter()
                .map(|s| s.name.clone())
                .collect();
            let resp = serde_json::json!({
                "status": "ok",
                "services": services
            });
            drop(reader);
            writeln!(
                std::io::BufWriter::new(&mut stream),
                "{}",
                serde_json::to_string(&resp).unwrap()
            )
            .map_err(|e| format!("写入响应失败: {}", e))?;
        }

        "ping" => {
            drop(reader);
            writeln!(
                std::io::BufWriter::new(&mut stream),
                "{{\"status\":\"ok\",\"message\":\"pong\"}}"
            )
            .map_err(|e| format!("写入响应失败: {}", e))?;
        }

        other => {
            let resp = serde_json::json!({
                "status": "error",
                "message": format!("未知 action: {}", other)
            });
            drop(reader);
            writeln!(
                std::io::BufWriter::new(&mut stream),
                "{}",
                serde_json::to_string(&resp).unwrap()
            )
            .map_err(|e| format!("写入响应失败: {}", e))?;
        }
    }

    Ok(())
}

fn log_info(topic: String, content: String) {
    let log = LogStruct {
        level: LogLevel::Debug,
        topic,
        content,
    };
    log.logout();
}
