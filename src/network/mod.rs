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

//! 网络层门面。
//!
//! 本模块统一了构建期与运行期：前者包含身份、地址探测、行为装配与 Swarm 构建，
//! 后者是独占 Swarm 的 Actor 事件循环。对外只暴露一个 [`Network`] 门面：
//!
//! ```text
//! Network::start(config, keypair)
//!     ├─ builder::build_swarm      构建 Swarm：传输层、行为、监听
//!     └─ actor::spawn              启动 Actor，返回 NetworkStart { handle, events, task }
//! ```
//!
//! - [`NetworkHandle`]：异步命令门面，覆盖 DHT、服务调用、传输控制与 reload。
//! - [`NetworkEvent`]：Actor 上抛的事件流，交给 `NodeController` 处理。
//! - [`NetworkError`]：统一错误类型，替代原先的 `String` 与 `.expect()`。
//!
//! 模块划分：
//! - `identity` — 节点身份：ED25519 与可选 PQ
//! - `addr`     — 公网 IP 探测与 Multiaddr 转换
//! - `behaviour`— libp2p 行为聚合与事件枚举
//! - `builder`  — `build_swarm`
//! - `actor`    — Swarm Actor、命令/事件、`NetworkHandle`

mod actor;
mod addr;
mod behaviour;
mod builder;
mod identity;
pub mod pq;

pub use actor::{NetworkEvent, NetworkHandle, NetworkStart};
pub use addr::{dialable_addrs, update_config_with_public_ip};
pub use identity::KeyManager;

use crate::config::ConfigHandle;
use std::fmt;

/// 网络层统一错误类型。
#[derive(Debug, Clone)]
pub enum NetworkError {
    /// Actor 任务已退出，命令无法送达。
    ActorGone,
    /// 对应协议在配置中被禁用。
    Disabled,
    /// 构建 Swarm 失败。
    Build(String),
    /// Kademlia 查询失败。
    Kad(String),
    /// 服务请求失败。
    Request(String),
    /// 监听或拨号等传输层操作失败。
    Transport(String),
    /// 因热重载被取消。
    Reloaded,
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkError::ActorGone => write!(f, "network actor is gone"),
            NetworkError::Disabled => write!(f, "protocol disabled by config"),
            NetworkError::Build(e) => write!(f, "swarm build failed: {e}"),
            NetworkError::Kad(e) => write!(f, "kademlia error: {e}"),
            NetworkError::Request(e) => write!(f, "request failed: {e}"),
            NetworkError::Transport(e) => write!(f, "transport error: {e}"),
            NetworkError::Reloaded => write!(f, "cancelled by network reload"),
        }
    }
}

impl std::error::Error for NetworkError {}

/// 网络层入口门面。
pub struct Network;

impl Network {
    /// 构建 Swarm 并启动 Actor。
    pub fn start(
        config: ConfigHandle,
        keypair: libp2p::identity::Keypair,
        pq_keys: Option<pq::PqKeys>,
    ) -> Result<NetworkStart, NetworkError> {
        actor::spawn(config, keypair, pq_keys)
    }
}
