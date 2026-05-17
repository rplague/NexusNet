mod boot;
mod config;
mod net;
mod log;
mod node_controller;
mod service_protocol;
mod service_dispatcher;

use std::error::Error;
use log::{LogLevel, LogStruct};
use net::{KeyManager, NetHandle};
use node_controller::NodeController;

fn main() -> Result<(), Box<dyn Error>> {
    let config_handle = boot::init();
    if let Err(e) = net::update_config_with_public_ip(&config_handle) {
        LogStruct::new(LogLevel::Warning, "更新公网IP失败", e.to_string()).emit();
    }
    let key_manager = KeyManager::load_or_create("keypair.bin")?;
    let peer_id = key_manager.peer_id();
    LogStruct::new(LogLevel::Important, "节点身份", format!("PeerId: {}", peer_id)).emit();

    let net_handle = NetHandle::new(config_handle.clone());
    net_handle.start(&key_manager)?;

    // 连接已有的 bootstrap 节点
    for addr in config_handle.bootstrap_nodes() {
        if let Err(e) = net_handle.dial(addr) {
            LogStruct::new(LogLevel::Warning, "拨号节点失败", e.to_string()).emit();
        }
    }

        let controller = NodeController::new(config_handle, net_handle, peer_id);
    tokio::runtime::Runtime::new()?.block_on(async {
        if let Err(e) = controller.run().await {
            LogStruct::new(LogLevel::Critical, "节点运行错误", e.to_string()).emit();
        }
        Ok::<_, Box<dyn Error>>(())
    })?;

    Ok(())
}