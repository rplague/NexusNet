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

mod auth;
mod config;
mod controller;
mod log;
mod network;
mod paths;
mod service_dispatcher;
mod service_protocol;
mod sidecar_protocol;

use crate::config::ConfigHandle;
use controller::NodeController;
use log::{LogLevel, LogStruct};
use network::{KeyManager, Network};
use std::error::Error;
use tokio::sync::mpsc;
#[cfg(unix)]
use tokio::sync::oneshot;
use tokio::sync::watch;

#[cfg(unix)]
use crate::service_dispatcher::ControlCommand;
use crate::service_dispatcher::{ServiceDispatcher, ToController};
#[cfg(unix)]
use crate::sidecar_protocol::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_handle = ConfigHandle::load_or_create_default();
    let mut key_manager = KeyManager::load_or_create(paths::fail_path_gener(
        paths::FailType::Keypair,
        "keypair.bin",
    ))?;
    if config_handle.pq_enabled()
        && let Err(e) = key_manager.ensure_pq_keys()
    {
        LogStruct::new(LogLevel::Warning, "PQ 密钥初始化失败", e.to_string()).emit();
    }
    if let Err(e) = network::update_config_with_public_ip(&config_handle) {
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

    // Build the network and start the Swarm actor.
    let keypair = key_manager.keypair().clone();
    let pq_keys = key_manager.take_pq_keys();
    let network = Network::start(config_handle.clone(), keypair, pq_keys)?;

    // Dial the configured bootstrap nodes.
    for addr in config_handle.bootstrap_nodes() {
        if let Err(e) = network.handle.dial(addr).await {
            LogStruct::new(LogLevel::Warning, "拨号节点失败", e.to_string()).emit();
        }
    }

    //   to_controller_tx -> ServiceDispatcher reports control commands and
    //                       backend status to NodeController
    //   to_dispatcher_tx -> NodeController forwards inbound service requests to
    //                       ServiceDispatcher
    let (to_controller_tx, to_controller_rx) = mpsc::channel(128);
    let (to_dispatcher_tx, to_dispatcher_rx) = mpsc::channel(256);

    // Signal handling: SIGINT/SIGTERM/Ctrl-C trigger shutdown; SIGHUP requests a
    // hot reload (unix only).
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_tx = to_controller_tx.clone();
    tokio::spawn(async move {
        wait_for_signal(signal_tx).await;
        LogStruct::new(LogLevel::Warning, "收到退出信号", "正在关闭...").emit();
        let _ = shutdown_tx.send(true);
    });

    let dispatcher = ServiceDispatcher::new(
        to_dispatcher_rx,
        to_controller_tx.clone(),
        config_handle.clone(),
        shutdown_rx.clone(),
    );
    tokio::spawn(async move {
        dispatcher.run().await;
    });

    // Keep-alive: even if the dispatcher exits early, NodeController's
    // controller channel stays open.
    let _to_controller_keepalive = to_controller_tx;

    let controller = NodeController::new(
        config_handle,
        peer_id,
        to_controller_rx,
        to_dispatcher_tx,
        network,
        shutdown_rx,
    );
    controller.run().await;
    Ok(())
}

/// Request a hot reload on SIGHUP (fire-and-forget).
#[cfg(unix)]
fn request_reload(reload_tx: &mpsc::Sender<ToController>) {
    let (resp_tx, _resp_rx) = oneshot::channel();
    let _ = reload_tx.try_send(ToController::Control(ControlCommand {
        msg: Message::ReloadConfig {
            id: uuid::Uuid::new_v4(),
        },
        resp_tx,
    }));
}

/// Wait for an exit signal, handling SIGHUP reloads while waiting.
#[cfg(unix)]
async fn wait_for_signal(reload_tx: mpsc::Sender<ToController>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).expect("无法安装 SIGINT 处理器");
    let mut sigterm = signal(SignalKind::terminate()).expect("无法安装 SIGTERM 处理器");
    let mut sighup = signal(SignalKind::hangup()).expect("无法安装 SIGHUP 处理器");

    loop {
        tokio::select! {
            _ = sigint.recv() => return,
            _ = sigterm.recv() => return,
            _ = sighup.recv() => {
                LogStruct::new(LogLevel::Warning, "收到 SIGHUP", "重新加载配置并重建网络...").emit();
                request_reload(&reload_tx);
            }
        }
    }
}

#[cfg(windows)]
async fn wait_for_signal(_reload_tx: mpsc::Sender<ToController>) {
    // Windows service (NSSM) triggers graceful shutdown via Ctrl+C; no SIGHUP
    // hot reload.
    let _ = tokio::signal::ctrl_c().await;
}
