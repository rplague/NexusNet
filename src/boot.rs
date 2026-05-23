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

use libp2p::Multiaddr;
use std::env;

use crate::{LogLevel, LogStruct, config::ConfigHandle};

pub fn init() -> ConfigHandle {
    let config_handle = ConfigHandle::load_or_create_default();

    let mut port = config_handle.listen_port() as u16;
    let mut bootstrap_nodes = config_handle.bootstrap_nodes();

    let args: Vec<String> = env::args().collect();
    parse_cli_args(&args, &mut port, &mut bootstrap_nodes);

    // 如果发生了变化，写回配置并保存
    let mut changed = false;
    if port as u32 != config_handle.listen_port() {
        config_handle.set_listen_port(port as u32);
        changed = true;
    }
    if bootstrap_nodes != config_handle.bootstrap_nodes() {
        config_handle.set_bootstrap_nodes(bootstrap_nodes.clone());
        changed = true;
    }
    if changed {
        config_handle.save_to_default();
    }

    config_handle
}

fn parse_cli_args(args: &[String], port: &mut u16, bootstrap_nodes: &mut Vec<Multiaddr>) {
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" if i + 1 < args.len() => {
                *port = match args[i + 1].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        LogStruct::new(LogLevel::Warning, "-p 参数输入错误", "使用默认值 5000")
                            .emit();
                        5000
                    }
                };
                i += 1;
            }
            "-c" if i + 1 < args.len() => {
                match args[i + 1].parse::<Multiaddr>() {
                    Ok(addr) => bootstrap_nodes.push(addr),
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "无效的 Multiaddr",
                            format!("忽略: {}\n错误: {}", args[i + 1], e),
                        )
                        .emit();
                    }
                }
                i += 1;
            }
            "--connect-overwrite" if i + 1 < args.len() => {
                bootstrap_nodes.clear();
                match args[i + 1].parse::<Multiaddr>() {
                    Ok(addr) => bootstrap_nodes.push(addr),
                    Err(e) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "无效的 Multiaddr",
                            format!("忽略: {}\n错误: {}", args[i + 1], e),
                        )
                        .emit();
                    }
                }
                i += 1;
            }
            unknown => {
                LogStruct::new(
                    LogLevel::Critical,
                    "错误的参数输入",
                    format!("{} 参数是无法识别的", unknown),
                )
                .emit();
                std::process::exit(1);
            }
        }
        i += 1;
    }
}
