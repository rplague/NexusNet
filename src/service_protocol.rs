use libp2p::request_response::{self, cbor, Config};
use libp2p::StreamProtocol;
use serde::{Serialize, Deserialize};
use std::time::Duration;

/// 协议 ID
pub const SERVICE_REQ_PROTOCOL: &str = "/oahd/service_req/1.0.0";

/// 请求结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub service: String,
    pub payload: Vec<u8>,
}

/// 响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub success: bool,
    pub data: Vec<u8>,
}

/// 创建默认配置的请求响应行为
pub fn new_service_req_behaviour() -> cbor::Behaviour<Request, Response> {
    let cfg = Config::default()
        .with_request_timeout(Duration::from_secs(30));
    
    // 将 &str 转换为 StreamProtocol
    let protocol = StreamProtocol::new(SERVICE_REQ_PROTOCOL);
    
    cbor::Behaviour::new(
        vec![(protocol, request_response::ProtocolSupport::Full)],
        cfg,
    )
}