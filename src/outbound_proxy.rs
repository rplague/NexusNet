use crate::{LogStruct, LogLevel, config::OutboundProxyConfig};
use libp2p::PeerId;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// 本地进程通过控制端口发送给 event loop 的指令
#[derive(Debug)]
pub enum ProxyCommand {
    /// 查询特定服务的提供者
    DiscoverServices {
        service: String,
        response_tx: tokio::sync::oneshot::Sender<ProxyResult>,
    },
    /// 向指定 Peer 发送 P2P 请求
    SendRequest {
        peer: PeerId,
        service: String,
        payload: Vec<u8>,
        response_tx: tokio::sync::oneshot::Sender<ProxyResult>,
    },
}

/// 代理操作的结果
#[derive(Debug, serde::Serialize)]
pub struct ProxyResult {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 启动出站代理 TCP 监听器
///
/// 在 `bind_addr` 上监听 JSON 行协议，每个连接接受以下命令：
///
/// 1. 服务发现查询
/// ```json
/// {"type": "discover", "service": "cold-storage"}
/// ```
/// 响应：
/// ```json
/// {"type": "discover_result", "success": true, "service": "cold-storage", "providers": [...]}
/// ```
///
/// 2. 主动发送请求
/// ```json
/// {"type": "request", "peer": "12D3KooW...", "service": "ocr", "payload": "...base64..."}
/// ```
/// 响应：
/// ```json
/// {"type": "response", "success": true, "data": "...", "request_id": 1}
/// ```
pub async fn start(
    config: &OutboundProxyConfig,
    cmd_tx: mpsc::Sender<ProxyCommand>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !config.enabled {
        let log = LogStruct {
            level: LogLevel::Debug,
            topic: "出站代理".to_string(),
            content: "出站代理未启用".to_string(),
        };
        log.logout();
        return Ok(());
    }

    let bind_addr: SocketAddr = format!("{}:{}", config.bind, config.port).parse()?;
    let listener = TcpListener::bind(bind_addr).await?;

    let log = LogStruct {
        level: LogLevel::Preset,
        topic: "出站代理".to_string(),
        content: format!("本地控制端口已启动: {}:{}", config.bind, config.port),
    };
    log.logout();

    loop {
        let (stream, addr) = listener.accept().await?;
        let cmd_tx = cmd_tx.clone();

        tokio::spawn(async move {
            let log = LogStruct {
                level: LogLevel::Debug,
                topic: "出站代理".to_string(),
                content: format!("新连接: {}", addr),
            };
            log.logout();

            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // 连接关闭
                    Ok(_) => {}
                    Err(e) => {
                        let log = LogStruct {
                            level: LogLevel::Warning,
                            topic: "出站代理".to_string(),
                            content: format!("读取失败 ({}): {}", addr, e),
                        };
                        log.logout();
                        break;
                    }
                }

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let response = match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(cmd) => {
                        let cmd_type = cmd.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match cmd_type {
                            "discover" => {
                                let service = cmd.get("service")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if service.is_empty() {
                                    ProxyResult {
                                        success: false,
                                        message: "缺少 service 字段".to_string(),
                                        data: None,
                                    }
                                } else {
                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                    let cmd = ProxyCommand::DiscoverServices {
                                        service,
                                        response_tx: tx,
                                    };
                                    if cmd_tx.send(cmd).await.is_err() {
                                        ProxyResult {
                                            success: false,
                                            message: "出站代理未连接到 P2P 节点".to_string(),
                                            data: None,
                                        }
                                    } else {
                                        rx.await.unwrap_or_else(|_| ProxyResult {
                                            success: false,
                                            message: "P2P 节点未响应".to_string(),
                                            data: None,
                                        })
                                    }
                                }
                            }
                            "request" => {
                                let peer_str = cmd.get("peer")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let service = cmd.get("service")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let payload_b64 = cmd.get("payload")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                if peer_str.is_empty() || service.is_empty() {
                                    ProxyResult {
                                        success: false,
                                        message: "缺少 peer 或 service 字段".to_string(),
                                        data: None,
                                    }
                                } else {
                                    let peer = match peer_str.parse::<PeerId>() {
                                        Ok(p) => p,
                                        Err(_) => {
                                            let resp = ProxyResult {
                                                success: false,
                                                message: format!("无效的 PeerId: {}", peer_str),
                                                data: None,
                                            };
                                            let _ = writer.write_all(
                                                serde_json::to_string(&resp).unwrap().as_bytes()
                                            ).await;
                                            let _ = writer.write_all(b"\n").await;
                                            continue;
                                        }
                                    };
                                    let payload = match base64_decode(payload_b64) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            let resp = ProxyResult {
                                                success: false,
                                                message: format!("payload base64 解码失败: {}", e),
                                                data: None,
                                            };
                                            let _ = writer.write_all(
                                                serde_json::to_string(&resp).unwrap().as_bytes()
                                            ).await;
                                            let _ = writer.write_all(b"\n").await;
                                            continue;
                                        }
                                    };

                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                    let cmd = ProxyCommand::SendRequest {
                                        peer,
                                        service,
                                        payload,
                                        response_tx: tx,
                                    };
                                    if cmd_tx.send(cmd).await.is_err() {
                                        ProxyResult {
                                            success: false,
                                            message: "出站代理未连接到 P2P 节点".to_string(),
                                            data: None,
                                        }
                                    } else {
                                        rx.await.unwrap_or_else(|_| ProxyResult {
                                            success: false,
                                            message: "P2P 节点未响应".to_string(),
                                            data: None,
                                        })
                                    }
                                }
                            }
                            "ping" => {
                                ProxyResult {
                                    success: true,
                                    message: "pong".to_string(),
                                    data: None,
                                }
                            }
                            _ => {
                                ProxyResult {
                                    success: false,
                                    message: format!("未知命令类型: {}", cmd_type),
                                    data: None,
                                }
                            }
                        }
                    }
                    Err(e) => {
                        ProxyResult {
                            success: false,
                            message: format!("JSON 解析错误: {}", e),
                            data: None,
                        }
                    }
                };

                let response_json = serde_json::to_string(&response).unwrap();
                if let Err(e) = writer.write_all(response_json.as_bytes()).await {
                    let log = LogStruct {
                        level: LogLevel::Warning,
                        topic: "出站代理".to_string(),
                        content: format!("写入响应失败 ({}): {}", addr, e),
                    };
                    log.logout();
                    break;
                }
                if let Err(e) = writer.write_all(b"\n").await {
                    let log = LogStruct {
                        level: LogLevel::Warning,
                        topic: "出站代理".to_string(),
                        content: format!("写入换行失败 ({}): {}", addr, e),
                    };
                    log.logout();
                    break;
                }
            }

            let log = LogStruct {
                level: LogLevel::Debug,
                topic: "出站代理".to_string(),
                content: format!("连接关闭: {}", addr),
            };
            log.logout();
        });
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    engine.decode(input)
}
