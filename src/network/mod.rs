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

//! Network layer facade.
//!
//! This module unifies build-time and run-time concerns: the former covers
//! identity, address probing, behaviour wiring and Swarm construction; the
//! latter is the Swarm-owning actor event loop. Only the [`Network`] facade is
//! exposed:
//!
//! ```text
//! Network::start(config, keypair)
//!     ├─ builder::build_swarm      build the Swarm: transport, behaviours, listen
//!     └─ actor::spawn              start the actor, returning NetworkStart { handle, events, task }
//! ```
//!
//! - [`NetworkHandle`]: async command facade covering DHT, service calls, transport control and reload.
//! - [`NetworkEvent`]: event stream raised by the actor, handled by `NodeController`.
//! - [`NetworkError`]: unified error type replacing the old `String` and `.expect()`.
//!
//! Module layout:
//! - `identity` — node identity: ED25519 and optional PQ
//! - `addr`     — public IP probing and Multiaddr conversion
//! - `behaviour`— libp2p behaviour aggregation and event enum
//! - `builder`  — `build_swarm`
//! - `actor`    — Swarm actor, commands/events, `NetworkHandle`

mod actor;
mod addr;
mod behaviour;
mod builder;
mod identity;
pub mod pq;

pub use actor::{NetworkEvent, NetworkHandle, NetworkStart};
pub use addr::update_config_with_public_ip;
pub use behaviour::AGENT_NAME;
pub use identity::KeyManager;

use crate::config::ConfigHandle;
use std::fmt;

/// Unified network-layer error type.
#[derive(Debug, Clone)]
pub enum NetworkError {
    /// The actor task has exited; commands cannot be delivered.
    ActorGone,
    /// The protocol is disabled by config.
    Disabled,
    /// Failed to build the Swarm.
    Build(String),
    /// Kademlia query failed.
    Kad(String),
    /// Service request failed.
    Request(String),
    /// Transport operation (listen/dial) failed.
    Transport(String),
    /// Cancelled by a hot reload.
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

/// Network layer entry facade.
pub struct Network;

impl Network {
    /// Build the Swarm and start the actor.
    pub fn start(
        config: ConfigHandle,
        keypair: libp2p::identity::Keypair,
        pq_keys: Option<pq::PqKeys>,
    ) -> Result<NetworkStart, NetworkError> {
        actor::spawn(config, keypair, pq_keys)
    }
}
