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

//! 统一路径解析。
//!
//! - systemd 部署：由 `NEXUSNET_HOME` / `NEXUSNET_LOG_FILE` 等环境变量锚定标准目录。
//! - 本地 `cargo run`：未设置任何环境变量时回退当前目录（`./config.toml`、`./keypair.bin`、`./log`），
//!   以兼容既有行为。

use std::env;
use std::path::PathBuf;

/// 当前进程是否显式指定了应用数据根目录（`NEXUSNET_HOME`）。
fn has_data_home() -> bool {
    env::var_os("NEXUSNET_HOME").is_some_and(|v| !v.is_empty())
}

/// 应用数据根目录。
///
/// 优先级：`NEXUSNET_HOME` 环境变量 → 默认 `/var/lib/nexusnet`。
fn data_home() -> PathBuf {
    match env::var_os("NEXUSNET_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/var/lib/nexusnet"),
    }
}

/// 配置文件路径。
///
/// 优先级：`NEXUSNET_CONFIG` → `$NEXUSNET_HOME/config.toml` → `./config.toml`。
pub fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_CONFIG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if has_data_home() {
        return data_home().join("config.toml");
    }
    PathBuf::from("./config.toml")
}

/// 节点身份密钥路径。
///
/// 优先级：`NEXUSNET_KEYPAIR` → `$NEXUSNET_HOME/keypair.bin` → `./keypair.bin`。
pub fn keypair_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_KEYPAIR") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if has_data_home() {
        return data_home().join("keypair.bin");
    }
    PathBuf::from("./keypair.bin")
}

/// 日志文件路径。
///
/// 优先级：`NEXUSNET_LOG_FILE` → `$NEXUSNET_HOME/log` → `./log`。
pub fn log_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_LOG_FILE") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if has_data_home() {
        return data_home().join("log");
    }
    PathBuf::from("./log")
}
