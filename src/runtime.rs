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

//! Process-independent node runtime.
//!
//! The desktop binary and the mobile bridges share this module. It owns the
//! network, dispatcher and controller tasks but leaves signal handling,
//! logging mode and lifecycle policy to the host.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use libp2p::PeerId;
use libp2p::multiaddr::Protocol;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::config::ConfigHandle;
use crate::controller::NodeController;
use crate::log::{self, LogEntry, LogLevel, LogStruct};
use crate::network::{KeyManager, Network};
use crate::service_dispatcher::{
    ControlCommand, DispatcherChannels, DispatcherCommand, InboundRequest, ServiceDispatcher,
    ToController,
};
use crate::sidecar_protocol::Message;
use crate::{network, paths};

/// Structured log sink installed before the node starts.
pub type LogSink = Arc<dyn Fn(LogEntry) + Send + Sync + 'static>;

/// Node startup options.
pub struct NodeOptions {
    /// Path to `config.toml`.
    pub config_path: PathBuf,
    /// Path to `keypair.bin` (not a directory).
    pub keypair_path: PathBuf,
    /// Optional structured log sink. When set, normal file/terminal logging is
    /// disabled process-wide.
    pub log_sink: Option<LogSink>,
    /// Probe local public addresses and update the config (desktop behavior).
    pub update_public_ip: bool,
    /// Wait up to 30 seconds for configured address families (desktop behavior).
    pub wait_for_addresses: bool,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            config_path: paths::fail_path_gener(paths::FailType::Config, "config.toml"),
            keypair_path: paths::fail_path_gener(paths::FailType::Keypair, "keypair.bin"),
            log_sink: None,
            update_public_ip: true,
            wait_for_addresses: true,
        }
    }
}

/// Node startup/runtime error.
#[derive(Debug)]
pub struct NodeError(pub String);

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NodeError {}

impl From<String> for NodeError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NodeError {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<crate::config::ConfigError> for NodeError {
    fn from(value: crate::config::ConfigError) -> Self {
        Self(value.message)
    }
}

struct NodeHandleInner {
    control_tx: mpsc::Sender<ToController>,
    shutdown_tx: watch::Sender<bool>,
    peer_id: PeerId,
    config: ConfigHandle,
    controller_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

/// A running node. Cheap to clone; all clones share the same node.
#[derive(Clone)]
pub struct NodeHandle {
    inner: Arc<NodeHandleInner>,
}

impl NodeHandle {
    pub fn peer_id(&self) -> PeerId {
        self.inner.peer_id
    }

    pub fn config(&self) -> ConfigHandle {
        self.inner.config.clone()
    }

    /// Send a sidecar control message to the node controller.
    pub async fn control(&self, msg: Message) -> Result<Vec<u8>, String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        let command = ControlCommand { msg, resp_tx };
        let result = timeout(Duration::from_secs(120), async {
            self.inner
                .control_tx
                .send(ToController::Control(command))
                .await
                .map_err(|_| "节点控制器不可用".to_string())?;
            resp_rx
                .await
                .map_err(|_| "节点控制器未返回结果".to_string())?
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err("控制命令超时".to_string()),
        }
    }

    /// Stop the node and wait for the controller task to exit.
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(true);
        let task = self.inner.controller_task.lock().await.take();
        if let Some(task) = task {
            let _ = timeout(Duration::from_secs(8), task).await;
        }
    }

    /// Wait until the node stops on its own.
    pub async fn wait(&self) {
        let task = self.inner.controller_task.lock().await.take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for NodeHandleInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Load the node configuration, start the swarm, dispatcher and controller.
pub async fn start_node(options: NodeOptions) -> Result<NodeHandle, NodeError> {
    if let Some(sink) = options.log_sink {
        log::install_sink(sink);
    }

    let config_handle = ConfigHandle::try_from_toml_file(&options.config_path, true)?;
    let mut key_manager = KeyManager::load_or_create(&options.keypair_path)
        .map_err(|e| NodeError(format!("加载节点身份失败: {e}")))?;
    if config_handle.pq_enabled()
        && let Err(e) = key_manager.ensure_pq_keys()
    {
        LogStruct::new(LogLevel::Warning, "PQ 密钥初始化失败", e.to_string()).emit();
    }

    if options.wait_for_addresses {
        network::wait_for_configured_addresses(&config_handle).await;
    }
    if options.update_public_ip
        && let Err(e) = network::update_config_with_public_ip(&config_handle)
    {
        LogStruct::new(LogLevel::Warning, "更新公网IP失败", e.to_string()).emit();
    }

    let peer_id = key_manager.peer_id();
    let announce: Vec<String> = config_handle
        .dialable_addrs(peer_id)
        .iter()
        .map(|a| a.to_string())
        .collect();
    let identity = if announce.is_empty() {
        format!("PeerId: {}", peer_id)
    } else {
        format!("PeerId: {}\n    {}", peer_id, announce.join("\n    "))
    };
    LogStruct::new(LogLevel::Important, "节点身份", identity).emit();

    let keypair = key_manager.keypair().clone();
    let pq_keys = key_manager.take_pq_keys();
    let network = Network::start(config_handle.clone(), keypair, pq_keys)
        .await
        .map_err(|e| NodeError(format!("启动网络失败: {e}")))?;

    for addr in config_handle.bootstrap_nodes() {
        if addr
            .iter()
            .any(|proto| matches!(proto, Protocol::P2p(peer) if peer == peer_id))
        {
            LogStruct::new(LogLevel::Warning, "跳过自身 bootstrap", addr.to_string()).emit();
            continue;
        }
        if let Err(e) = network.handle.dial(addr).await {
            LogStruct::new(LogLevel::Warning, "拨号节点失败", e.to_string()).emit();
        }
    }

    let (to_controller_tx, to_controller_rx) = mpsc::channel(128);
    let (to_dispatcher_tx, to_dispatcher_rx) = mpsc::channel::<InboundRequest>(256);
    let (dispatcher_cmd_tx, dispatcher_cmd_rx) = mpsc::channel::<DispatcherCommand>(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let (dispatcher, readiness_rx) = ServiceDispatcher::new(
        to_dispatcher_rx,
        dispatcher_cmd_rx,
        to_controller_tx.clone(),
        config_handle.clone(),
        peer_id,
        shutdown_rx.clone(),
    );
    tokio::spawn(async move {
        dispatcher.run().await;
    });

    // Keep the controller channel open even if the dispatcher exits early.
    let keepalive_tx = to_controller_tx.clone();

    let controller = NodeController::new(
        config_handle.clone(),
        peer_id,
        to_controller_rx,
        DispatcherChannels {
            inbound_tx: to_dispatcher_tx,
            command_tx: dispatcher_cmd_tx,
            readiness_rx,
        },
        network,
        shutdown_rx,
    );
    let controller_task = tokio::spawn(async move {
        controller.run().await;
    });

    Ok(NodeHandle {
        inner: Arc::new(NodeHandleInner {
            control_tx: keepalive_tx,
            shutdown_tx,
            peer_id,
            config: config_handle,
            controller_task: tokio::sync::Mutex::new(Some(controller_task)),
        }),
    })
}
