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

/// Protocol ID.
pub const SERVICE_REQ_PROTOCOL: &str = "/oahd/service_req/1.0.0";

/// Timeout for one P2P service request/response exchange.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Request structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub service: String,
    pub payload: Vec<u8>,
}

/// Response structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub success: bool,
    pub data: Vec<u8>,
}

/// Create a request-response behaviour with explicit message size limits.
pub fn new_service_req_behaviour(
    max_request_bytes: u64,
    max_response_bytes: u64,
) -> cbor::Behaviour<Request, Response> {
    let cfg = Config::default().with_request_timeout(REQUEST_TIMEOUT);

    // Convert &str to StreamProtocol.
    let protocol = StreamProtocol::new(SERVICE_REQ_PROTOCOL);

    let codec = cbor::codec::Codec::<Request, Response>::default()
        .set_request_size_maximum(max_request_bytes)
        .set_response_size_maximum(max_response_bytes);

    cbor::Behaviour::with_codec(
        codec,
        vec![(protocol, request_response::ProtocolSupport::Full)],
        cfg,
    )
}
