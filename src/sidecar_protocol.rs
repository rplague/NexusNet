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

//! Node <-> sidecar/backend protocol v2 (CBOR contract).
//!
//! Wire format: `u32_be(len) || cbor(message)`, `len <= MAX_FRAME`.

use std::fmt;

use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder};
use uuid::Uuid;

use bytes::{Buf, BufMut, BytesMut};

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum frame size (including the CBOR payload).
pub const MAX_FRAME: usize = 16 << 20;

/// Structured error.
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

/// Serde default for optional boolean fields that default to `true`.
fn default_true() -> bool {
    true
}

/// Protocol messages are discriminated by a text field `t`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// Handshake (first frame in both directions).
    Hello {
        version: u32,
    },
    /// Node -> backend: forward an inbound service request.
    Request {
        id: Uuid,
        service: String,
        payload: Vec<u8>,
    },
    /// Correlated reply (service response / control result, both directions).
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
    Whoami {
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
    /// Clear the local DHT record cache; later queries go back to the network.
    RefreshCache {
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
    /// Stop providing and/or remove the locally stored record for `key`.
    ///
    /// `providing` (default true) stops announcing this node as a provider;
    /// `record` (default false) removes the locally stored record (only if this
    /// node is its publisher). Both are local operations; remote copies expire
    /// on their own TTLs.
    RemoveKey {
        id: Uuid,
        key: String,
        #[serde(default = "default_true")]
        providing: bool,
        #[serde(default)]
        record: bool,
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
    /// Get the correlation id; `hello` has none.
    pub fn id(&self) -> Option<Uuid> {
        match self {
            Message::Hello { .. } => None,
            Message::Request { id, .. }
            | Message::Reply { id, .. }
            | Message::ListServices { id }
            | Message::DiscoverProviders { id, .. }
            | Message::QueryPublicIp { id }
            | Message::Whoami { id }
            | Message::ReconnectBootstrap { id }
            | Message::ReannounceServices { id }
            | Message::ReloadConfig { id }
            | Message::RefreshCache { id }
            | Message::RelayStatus { id }
            | Message::PqStatus { id }
            | Message::AuthStatus { id }
            | Message::QueryKey { id, .. }
            | Message::AddKey { id, .. }
            | Message::RemoveKey { id, .. }
            | Message::ServiceRequest { id, .. }
            | Message::ServiceRequestTo { id, .. } => Some(*id),
        }
    }
}

/// Frame encode/decode error.
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

/// Encode a message into a full frame (length prefix + CBOR).
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

/// Decode a frame body (without the length prefix).
pub fn decode_frame(body: &[u8]) -> Result<Message, FrameError> {
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    ciborium::de::from_reader(body).map_err(|e| FrameError::Decode(e.to_string()))
}

/// A `tokio_util` codec for the sidecar wire format.
///
/// Wraps the same `u32_be(len) || cbor(message)` framing as [`encode_frame`] /
/// [`decode_frame`], so a single `Framed` can be used in a `select!` (its
/// `Stream::next` is cancel-safe, unlike a raw `read_exact`).
pub struct SidecarCodec;

impl Decoder for SidecarCodec {
    type Item = Message;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, FrameError> {
        let Some(len) = src
            .get(..4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
        else {
            return Ok(None);
        };
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        if src.len() < 4 + len {
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let body = src.split_to(len);
        decode_frame(&body).map(Some)
    }
}

impl Encoder<Message> for SidecarCodec {
    type Error = FrameError;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), FrameError> {
        let frame = encode_frame(&msg)?;
        dst.reserve(frame.len());
        dst.put_slice(&frame);
        Ok(())
    }
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
    fn codec_handles_partial_then_full_frame() {
        let msg = Message::Whoami { id: Uuid::new_v4() };
        let frame = encode_frame(&msg).unwrap();
        let (head, tail) = frame.split_at(3);

        let mut buf = BytesMut::new();
        buf.put_slice(head);
        assert!(SidecarCodec.decode(&mut buf).unwrap().is_none());

        buf.put_slice(tail);
        assert_eq!(SidecarCodec.decode(&mut buf).unwrap(), Some(msg));
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
        round_trip(Message::Whoami { id });
        round_trip(Message::RefreshCache { id });
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
        round_trip(Message::RemoveKey {
            id,
            key: "/oahd/service/cmd".into(),
            providing: true,
            record: false,
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
        // 0xff is the CBOR "break"; standalone it is invalid.
        assert!(matches!(decode_frame(&[0xff]), Err(FrameError::Decode(_))));
    }
}
