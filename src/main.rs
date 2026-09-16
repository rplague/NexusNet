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

mod boot;
mod config;
mod log;
mod network;
mod node_controller;
mod paths;
mod service_dispatcher;
mod service_protocol;

use log::{LogLevel, LogStruct};
use network::{KeyManager, Network};
use node_controller::NodeController;
use std::error::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::service_dispatcher::{Command, ServiceDispatcher};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_handle = boot::init();
    if let Err(e) = network::update_config_with_public_ip(&config_handle) {
        LogStruct::new(LogLevel::Warning, "更新公网IP失败", e.to_string()).emit();
    }
    let mut key_manager = KeyManager::load_or_create(paths::keypair_path())?;
    if config_handle.pq_enabled()
        && let Err(e) = key_manager.ensure_pq_keys()
    {
        LogStruct::new(LogLevel::Warning, "PQ 密钥初始化失败", e.to_string()).emit();
    }
    let peer_id = key_manager.peer_id();
    let dial: Vec<String> = network::dialable_addrs(&config_handle, peer_id)
        .iter()
        .map(|a| a.to_string())
        .collect();
    let identity = if dial.is_empty() {
        format!("PeerId: {}", peer_id)
    } else {
        format!("PeerId: {}\n    {}", peer_id, dial.join("\n    "))
    };
    LogStruct::new(LogLevel::Important, "节点身份", identity).emit();

    // 构建网络并启动 Swarm Actor
    let keypair = key_manager.keypair().clone();
    let pq_keys = key_manager.take_pq_keys();
    let network = Network::start(config_handle.clone(), keypair, pq_keys)?;

    // 拨号已有的 bootstrap 节点
    for addr in config_handle.bootstrap_nodes() {
        if let Err(e) = network.handle.dial(addr).await {
            LogStruct::new(LogLevel::Warning, "拨号节点失败", e.to_string()).emit();
        }
    }

    //    cmd_tx -> ServiceDispatcher 发送命令给 NodeController
    //    cmd_rx -> NodeController 接收命令
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    //    inbound_req_tx -> NodeController 发送入站请求给 ServiceDispatcher
    //    inbound_req_rx -> ServiceDispatcher 接收入站请求
    let (inbound_req_tx, inbound_req_rx) = mpsc::unbounded_channel();

    // 信号处理：SIGTERM / Ctrl-C 优雅退出；SIGHUP 触发热重载，配合 systemd ExecReload
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let reload_tx = cmd_tx.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法安装 SIGTERM 处理器");
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("无法安装 SIGHUP 处理器");
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = sigterm.recv() => break,
                _ = sighup.recv() => {
                    LogStruct::new(LogLevel::Warning, "收到 SIGHUP", "重新加载配置并重建网络...").emit();
                    let (resp_tx, _resp_rx) = oneshot::channel();
                    let _ = reload_tx.send(Command {
                        prefix: "@".to_string(),
                        content: "reload_config".to_string(),
                        payload: Vec::new(),
                        resp_tx,
                    });
                }
            }
        }
        LogStruct::new(LogLevel::Warning, "收到退出信号", "正在优雅关闭...").emit();
        let _ = shutdown_tx.send(true);
    });

    let dispatcher = ServiceDispatcher::new(
        inbound_req_rx,
        cmd_tx,
        config_handle.clone(),
        shutdown_rx.clone(),
    );
    tokio::spawn(async move {
        dispatcher.run().await;
    });

    let controller = NodeController::new(
        config_handle,
        peer_id,
        cmd_rx,
        inbound_req_tx,
        network,
        shutdown_rx,
    );
    if let Err(e) = controller.run().await {
        LogStruct::new(LogLevel::Critical, "节点运行错误", e.to_string()).emit();
    }
    Ok(())
}
