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

//! 节点 ↔ 边车/后端协议 v2（CBOR 契约）
//!
//! 线上格式：`u32_be(len) || cbor(message)`，`len <= MAX_FRAME`

use std::fmt;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

/// 协议版本
pub const PROTOCOL_VERSION: u32 = 2;
/// 单帧上限（含 CBOR 载荷）
pub const MAX_FRAME: usize = 16 << 20;

/// 结构化错误
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarError {
    pub code: String,
    pub message: String,
}

impl SidecarError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 协议消息判别字段为文本 `t`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// 握手（双向首帧）
    Hello {
        version: u32,
    },
    /// 节点 → 后端：转发入站服务请求
    Request {
        id: Uuid,
        service: String,
        payload: Vec<u8>,
    },
    /// 关联回复（服务响应 / 控制结果，双向）
    Reply {
        id: Uuid,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Vec<u8>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<SidecarError>,
    },
    ListServices {
        id: Uuid,
    },
    DiscoverProviders {
        id: Uuid,
        service: String,
    },
    QueryPublicIp {
        id: Uuid,
    },
    ReconnectBootstrap {
        id: Uuid,
    },
    ReannounceServices {
        id: Uuid,
    },
    ReloadConfig {
        id: Uuid,
    },
    RelayStatus {
        id: Uuid,
    },
    PqStatus {
        id: Uuid,
    },
    AuthStatus {
        id: Uuid,
    },
    QueryKey {
        id: Uuid,
        key: String,
    },
    AddKey {
        id: Uuid,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Vec<u8>>,
        #[serde(default)]
        providing: bool,
    },
    ServiceRequest {
        id: Uuid,
        service: String,
        payload: Vec<u8>,
    },
    ServiceRequestTo {
        id: Uuid,
        service: String,
        peer: String,
        payload: Vec<u8>,
    },
}

impl Message {
    /// 取关联 id；`hello` 无 id
    pub fn id(&self) -> Option<Uuid> {
        match self {
            Message::Hello { .. } => None,
            Message::Request { id, .. }
            | Message::Reply { id, .. }
            | Message::ListServices { id }
            | Message::DiscoverProviders { id, .. }
            | Message::QueryPublicIp { id }
            | Message::ReconnectBootstrap { id }
            | Message::ReannounceServices { id }
            | Message::ReloadConfig { id }
            | Message::RelayStatus { id }
            | Message::PqStatus { id }
            | Message::AuthStatus { id }
            | Message::QueryKey { id, .. }
            | Message::AddKey { id, .. }
            | Message::ServiceRequest { id, .. }
            | Message::ServiceRequestTo { id, .. } => Some(*id),
        }
    }
}

/// 帧编解码错误
#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    TooLarge(usize),
    Encode(String),
    Decode(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "io error: {e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large: {n} bytes"),
            FrameError::Encode(e) => write!(f, "cbor encode error: {e}"),
            FrameError::Decode(e) => write!(f, "cbor decode error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// 将消息编码为完整帧（长度前缀 + CBOR）
pub fn encode_frame(msg: &Message) -> Result<Vec<u8>, FrameError> {
    let mut body = Vec::new();
    ciborium::ser::into_writer(msg, &mut body).map_err(|e| FrameError::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// 解析帧体（不含长度前缀）
pub fn decode_frame(body: &[u8]) -> Result<Message, FrameError> {
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    ciborium::de::from_reader(body).map_err(|e| FrameError::Decode(e.to_string()))
}

/// 读取一个完整帧
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message, FrameError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    decode_frame(&body)
}

/// 写入一个完整帧
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), FrameError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) {
        let frame = encode_frame(&msg).unwrap();
        let body = &frame[4..];
        assert_eq!(frame.len(), 4 + body.len());
        assert_eq!(
            u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize,
            body.len()
        );
        assert_eq!(decode_frame(body).unwrap(), msg);
    }

    #[test]
    fn request_reply_round_trip() {
        let id = Uuid::new_v4();
        round_trip(Message::Request {
            id,
            service: "cmd".into(),
            payload: vec![0, 1, 2, 255],
        });
        round_trip(Message::Reply {
            id,
            ok: false,
            result: None,
            error: Some(SidecarError::new("timeout", "too slow")),
        });
    }

    #[test]
    fn control_ops_round_trip() {
        let id = Uuid::new_v4();
        round_trip(Message::ListServices { id });
        round_trip(Message::DiscoverProviders {
            id,
            service: "cmd".into(),
        });
        round_trip(Message::AddKey {
            id,
            key: "/k".into(),
            value: Some(vec![9, 8, 7]),
            providing: true,
        });
        round_trip(Message::ServiceRequestTo {
            id,
            service: "cmd".into(),
            peer: "12D3KooW".into(),
            payload: vec![],
        });
    }

    #[test]
    fn oversized_frame_rejected() {
        assert!(matches!(
            decode_frame(&vec![0u8; MAX_FRAME + 1]),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn malformed_body_rejected() {
        // 0xff 是 CBOR 的 break，单独出现非法
        assert!(matches!(decode_frame(&[0xff]), Err(FrameError::Decode(_))));
    }
}
