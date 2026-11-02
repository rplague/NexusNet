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

//! NexusNet reusable node library.
//!
//! Desktop binary startup lives in `main.rs`; mobile hosts (Android JNI,
//! HarmonyOS NAPI) use [`runtime`] directly with explicit paths and a log sink.

pub mod auth;
pub mod config;
pub mod controller;
pub mod cose;
pub mod dht_cache;
pub mod limits;
pub mod log;
pub mod network;
pub mod paths;
pub mod runtime;
pub mod service_dispatcher;
pub mod service_protocol;
pub mod setup;
pub mod sidecar_protocol;

pub use config::{ConfigError, ConfigErrorKind, ConfigHandle, NodeConfig};
pub use log::{LogEntry, LogLevel, LogStruct, install_sink};
pub use runtime::{NodeError, NodeHandle, NodeOptions, start_node};
