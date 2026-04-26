// 公共 API 方法：供外部调用，当前未使用
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::{LogStruct, LogLevel};
use crate::service_dispatcher::ServiceDispatcher;
use libp2p::PeerId;
use libp2p::swarm::Swarm;
use crate::net::NetBehaviour;

/// 请求体：向远程节点发送的服务调用请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequest {
    /// 目标服务名，如 "ocr"
    pub service: String,
    /// 业务载荷（序列化后的字节，如 JSON）
    pub payload: Vec<u8>,
    /// 请求 ID，用于客户端匹配请求和响应
    pub request_id: u64,
}

/// 响应体：远程节点处理后的返回结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceResponse {
    /// 处理状态: "ok" 或 "error"
    pub status: String,
    /// 返回数据
    pub data: Option<Vec<u8>>,
    /// 对应的请求 ID
    pub request_id: u64,
}

impl ServiceRequest {
    pub fn new(service: &str, payload: Vec<u8>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        ServiceRequest {
            service: service.to_string(),
            payload,
            request_id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// 向远程节点发送服务调用请求
///
/// 返回 `request_id`，调用方可通过该 ID 匹配后续的 `OutboundResponse` 事件。
pub fn send_service_request(
    swarm: &mut Swarm<NetBehaviour>,
    peer: &PeerId,
    service: &str,
    payload: Vec<u8>,
) -> Result<u64, String> {
    let request = ServiceRequest::new(service, payload);
    let request_id = request.request_id;

    let log = LogStruct {
        level: LogLevel::Debug,
        topic: "P2P请求".to_string(),
        content: format!("向 {} 发送请求: service={}, request_id={}", peer, service, request_id),
    };
    log.logout();

    let _outbound_id = swarm.behaviour_mut().request_response.send_request(peer, request);
    Ok(request_id)
}

/// 处理接收到的远程请求：解析 ServiceRequest → dispatcher.forward → 返回 ServiceResponse
pub fn handle_incoming_request(
    request: ServiceRequest,
    dispatcher: &ServiceDispatcher,
) -> ServiceResponse {
    let log = LogStruct {
        level: LogLevel::Debug,
        topic: "请求处理".to_string(),
        content: format!("收到远程请求: service={}, request_id={}", request.service, request.request_id),
    };
    log.logout();

    match dispatcher.forward(&request.service, &request.payload) {
        Ok(data) => ServiceResponse {
            status: "ok".to_string(),
            data: Some(data),
            request_id: request.request_id,
        },
        Err(e) => ServiceResponse {
            status: "error".to_string(),
            data: Some(e.into_bytes()),
            request_id: request.request_id,
        },
    }
}
