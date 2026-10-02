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

use std::env;
use std::path::PathBuf;

/// Directory that holds `config.toml`.
///
/// `NEXUSNET_CONFIG_PATH` if set (non-empty); otherwise the current directory.
fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_CONFIG_PATH")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    PathBuf::from("./")
}

/// Directory that holds the node identity key (`keypair.bin`).
///
/// `NEXUSNET_KEYPAIR_PATH` if set (non-empty); otherwise the current directory.
fn keypair_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_KEYPAIR_PATH")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    PathBuf::from("./")
}

/// Directory that holds the log file.
///
/// `NEXUSNET_LOG_PATH` if set (non-empty); otherwise the current directory.
fn log_path() -> PathBuf {
    if let Some(path) = env::var_os("NEXUSNET_LOG_PATH")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    PathBuf::from("./")
}

/// Selects the base directory used by [`fail_path_gener`].
#[derive(Debug, PartialEq)]
pub enum FailType {
    /// Directory holding `config.toml`.
    Config,
    /// Directory holding `keypair.bin`.
    Keypair,
    /// Directory holding the log file.
    Log,
}

/// Join `fail_name` onto the directory selected by `fail_type`.
pub fn fail_path_gener(fail_type: FailType, fail_name: &str) -> PathBuf {
    let base = match fail_type {
        FailType::Config => config_path(),
        FailType::Keypair => keypair_path(),
        FailType::Log => log_path(),
    };
    base.join(fail_name)
}
