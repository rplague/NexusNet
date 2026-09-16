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

//! 节点身份的加载与保存：ED25519 主密钥，可选 PQ 辅助密钥。

use crate::network::pq::PqKeys;
use crate::{LogLevel, LogStruct};
use libp2p::{PeerId, identity};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub struct KeyManager {
    keypair: identity::Keypair,
    #[allow(dead_code)]
    path: PathBuf,
    pq_path: PathBuf,
    pq_keys: Option<PqKeys>,
}

impl KeyManager {
    /// 从指定路径加载密钥，若不存在则生成并原子保存。
    ///
    /// PQ 密钥（`keypair.pq.bin`）若存在则加载；不存在则留待 `ensure_pq_keys`。
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();
        let pq_path = path.with_extension("pq.bin");

        let keypair = match fs::read(path) {
            Ok(bytes) => match identity::Keypair::from_protobuf_encoding(&bytes) {
                Ok(keypair) => {
                    LogStruct::new(
                        LogLevel::Important,
                        "密钥加载成功",
                        path.display().to_string(),
                    )
                    .emit();
                    keypair
                }
                Err(e) => {
                    LogStruct::new(LogLevel::Error, "密钥文件解析失败", e.to_string()).emit();
                    return Err(e.into());
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                LogStruct::new(
                    LogLevel::Warning,
                    "密钥文件不存在，将生成新密钥",
                    path.display().to_string(),
                )
                .emit();
                let keypair = identity::Keypair::generate_ed25519();
                let encoded = keypair.to_protobuf_encoding()?;
                atomic_write(path, &encoded, "密钥文件")?;
                LogStruct::new(
                    LogLevel::Important,
                    "新密钥生成并保存成功",
                    path.display().to_string(),
                )
                .emit();
                keypair
            }
            Err(e) => {
                LogStruct::new(LogLevel::Error, "无法读取密钥文件", e.to_string()).emit();
                return Err(e.into());
            }
        };

        let pq_keys = load_pq_keys(&pq_path);

        Ok(KeyManager {
            keypair,
            path: path.to_path_buf(),
            pq_keys,
            pq_path,
        })
    }

    pub fn keypair(&self) -> &identity::Keypair {
        &self.keypair
    }

    /// 获取 peer id
    pub fn peer_id(&self) -> PeerId {
        self.keypair.public().to_peer_id()
    }

    /// 是否已加载 PQ 密钥
    #[allow(dead_code)]
    pub fn has_pq_keys(&self) -> bool {
        self.pq_keys.is_some()
    }

    /// 取出 PQ 密钥所有权（供网络层注入 Actor）。
    pub fn take_pq_keys(&mut self) -> Option<PqKeys> {
        self.pq_keys.take()
    }

    /// 确保 PQ 密钥存在：缺失则生成并原子保存。
    pub fn ensure_pq_keys(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.pq_keys.is_some() {
            return Ok(());
        }
        let keys = PqKeys::generate();
        atomic_write(&self.pq_path, &keys.to_bytes(), "PQ 密钥文件")?;
        LogStruct::new(
            LogLevel::Important,
            "PQ 密钥生成并保存成功",
            self.pq_path.display().to_string(),
        )
        .emit();
        self.pq_keys = Some(keys);
        Ok(())
    }
}

fn load_pq_keys(pq_path: &Path) -> Option<PqKeys> {
    let bytes = match fs::read(pq_path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    match PqKeys::from_bytes(&bytes) {
        Ok(keys) => {
            LogStruct::new(
                LogLevel::Important,
                "PQ 密钥加载成功",
                pq_path.display().to_string(),
            )
            .emit();
            Some(keys)
        }
        Err(e) => {
            LogStruct::new(LogLevel::Warning, "PQ 密钥解析失败，将忽略", e.to_string()).emit();
            None
        }
    }
}

/// 原子写入：先写临时文件，再重命名。
fn atomic_write(path: &Path, data: &[u8], what: &str) -> Result<(), Box<dyn std::error::Error>> {
    let temp_path = path.with_extension("tmp");
    if let Err(e) = fs::write(&temp_path, data) {
        LogStruct::new(
            LogLevel::Critical,
            format!("写入临时{}失败", what),
            format!("路径: {}, 错误: {}", temp_path.display(), e),
        )
        .emit();
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        LogStruct::new(
            LogLevel::Critical,
            format!("重命名{}失败", what),
            format!(
                "从 {} 到 {}, 错误: {}",
                temp_path.display(),
                path.display(),
                e
            ),
        )
        .emit();
        return Err(e.into());
    }
    Ok(())
}
