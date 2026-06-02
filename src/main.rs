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
mod net;
mod node_controller;
mod service_dispatcher;
mod service_protocol;
mod swarm_actor;

use log::{LogLevel, LogStruct};
use net::{KeyManager, NetHandle};
use node_controller::NodeController;
use std::error::Error;
use tokio::sync::mpsc;

use crate::service_dispatcher::ServiceDispatcher;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_handle = boot::init();
    if let Err(e) = net::update_config_with_public_ip(&config_handle) {
        LogStruct::new(LogLevel::Warning, "更新公网IP失败", e.to_string()).emit();
    }
    let key_manager = KeyManager::load_or_create("keypair.bin")?;
    let peer_id = key_manager.peer_id();
    LogStruct::new(
        LogLevel::Important,
        "节点身份",
        format!("PeerId: {}", peer_id),
    )
    .emit();

    let mut net_handle = NetHandle::new(config_handle.clone());
    net_handle.start(&key_manager)?;

    // 连接已有的 bootstrap 节点
    for addr in config_handle.bootstrap_nodes() {
        if let Err(e) = net_handle.dial(addr) {
            LogStruct::new(LogLevel::Warning, "拨号节点失败", e.to_string()).emit();
        }
    }

    //    cmd_tx -> ServiceDispatcher 发送命令给 NodeController
    //    cmd_rx -> NodeController 接收命令
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    //    inbound_req_tx -> NodeController 发送入站请求给 ServiceDispatcher
    //    inbound_req_rx -> ServiceDispatcher 接收入站请求
    let (inbound_req_tx, inbound_req_rx) = mpsc::unbounded_channel();

    let dispatcher = ServiceDispatcher::new(inbound_req_rx, cmd_tx, config_handle.clone());
    tokio::spawn(async move {
        dispatcher.run().await;
    });

    let controller = NodeController::new(config_handle, peer_id, cmd_rx, inbound_req_tx, net_handle);
    if let Err(e) = controller.run().await {
        LogStruct::new(LogLevel::Critical, "节点运行错误", e.to_string()).emit();
    }
    Ok(())
}
