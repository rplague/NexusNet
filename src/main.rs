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

use nexusnet::log::{LogLevel, LogStruct};
use nexusnet::paths::{self, FailType};
use nexusnet::runtime::{NodeHandle, NodeOptions};
use nexusnet::setup;
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if let Some(code) = setup::run_from_args() {
        std::process::exit(code);
    }

    let handle = nexusnet::start_node(NodeOptions {
        config_path: paths::fail_path_gener(FailType::Config, "config.toml"),
        keypair_path: paths::fail_path_gener(FailType::Keypair, "keypair.bin"),
        log_sink: None,
        update_public_ip: true,
        wait_for_addresses: true,
    })
    .await?;

    wait_for_exit(&handle).await;
    handle.shutdown().await;
    Ok(())
}

#[cfg(unix)]
use nexusnet::sidecar_protocol::Message;

#[cfg(unix)]
async fn wait_for_exit(handle: &NodeHandle) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).expect("无法安装 SIGINT 处理器");
    let mut sigterm = signal(SignalKind::terminate()).expect("无法安装 SIGTERM 处理器");
    let mut sighup = signal(SignalKind::hangup()).expect("无法安装 SIGHUP 处理器");

    loop {
        tokio::select! {
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            _ = sighup.recv() => {
                LogStruct::new(LogLevel::Warning, "收到 SIGHUP", "重新加载配置并重建网络...").emit();
                if let Err(e) = handle.control(Message::ReloadConfig { id: uuid::Uuid::new_v4() }).await {
                    LogStruct::new(LogLevel::Error, "配置重载失败", e).emit();
                }
            }
        }
    }

    LogStruct::new(LogLevel::Warning, "收到退出信号", "正在关闭...").emit();
}

#[cfg(windows)]
async fn wait_for_exit(_handle: &NodeHandle) {
    let _ = tokio::signal::ctrl_c().await;
    LogStruct::new(LogLevel::Warning, "收到退出信号", "正在关闭...").emit();
}
