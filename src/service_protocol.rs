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

use libp2p::StreamProtocol;
use libp2p::request_response::{self, Config, cbor};
use serde::{Deserialize, Serialize};
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
    let cfg = Config::default().with_request_timeout(Duration::from_secs(30));

    // 将 &str 转换为 StreamProtocol
    let protocol = StreamProtocol::new(SERVICE_REQ_PROTOCOL);

    cbor::Behaviour::new(
        vec![(protocol, request_response::ProtocolSupport::Full)],
        cfg,
    )
}
