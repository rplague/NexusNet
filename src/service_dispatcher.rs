use tokio::sync::{mpsc, oneshot};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{ConfigHandle, LocalServiceEntry};
use crate::log::{LogStruct, LogLevel};
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
    backend_writers: HashMap<String, tokio::io::WriteHalf<TcpStream>>,
}

impl ServiceDispatcher {

    pub fn new(
        inbound_rx: mpsc::UnboundedReceiver<InboundServiceRequest>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        config: ConfigHandle,
    ) -> Self {
        let local_services = config.read().services.dispatcher.local_services.clone();
        Self {
            inbound_rx,
            cmd_tx,
            config,
            local_services,
        }
    }


    pub async fn run(mut self) {

        self.init_backend_connections().await;

        while let Some(req) = self.inbound_rx.recv().await {
            // 启动新任务处理每个请求，避免阻塞后续请求
            let cmd_tx = self.cmd_tx.clone();
            tokio::spawn(async move {
                let response = Self::handle_request(req.service, req.payload).await;
                let _ = req.response_tx.send(response);
                // 如果需要主动调用远程服务，可以构造 Command 并通过 cmd_tx 发送
                // 例如：
                // let (resp_tx, resp_rx) = oneshot::channel();
                // let cmd = Command {
                //     prefix: "service_request".to_string(),
                //     content: "some_service".to_string(),
                //     payload: some_payload,
                //     resp_tx,
                // };
                // if cmd_tx.send(cmd).is_ok() {
                //     let remote_resp = resp_rx.await.unwrap();
                //     // 处理远程响应...
                // }
            });
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
            
            // 启动读任务（持续读取后端响应）
            let service_name = service.name.clone();
            tokio::spawn(Self::backend_read_loop(read_half, service_name.clone()));
            
            // 存储写半句柄以备后续发送请求
            self.backend_writers.insert(service.name.clone(), write_half);
            
            LogStruct::new(LogLevel::Preset, "后端连接建立", &format!("{} -> {}", service.name, addr)).emit();
        }
    }
    
    async fn backend_read_loop(mut read_half: tokio::io::ReadHalf<TcpStream>, service_name: String) {
        let mut buf = [0u8; 4096];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) => {
                    // 连接被后端正常关闭
                    LogStruct::new(LogLevel::Warning, "后端连接关闭", &format!("{} 连接已关闭", service_name)).emit();
                    break;
                }
                Ok(n) => {
                    // TODO: 这里收到的是后端主动发送的响应或推送数据
                    // 需要解析并与等待中的请求关联（后续实现）
                    LogStruct::new(LogLevel::Debug, "后端响应", &format!("{} 收到 {} 字节", service_name, n)).emit();
                }
                Err(e) => {
                    LogStruct::new(LogLevel::Error, "后端读错误", &format!("{}: {}", service_name, e)).emit();
                    break;
                }
            }
        }
        // 若连接断开，将来可以实现自动重连
    }

    async fn handle_request(service: String, payload: Vec<u8>) -> Result<service_protocol::Response, String> {
        // 当前简单返回 pong，未来可匹配 service 名称调用本地处理器
        Ok(service_protocol::Response {
            success: true,
            data: b"pong".to_vec(),
        })
    }
}
