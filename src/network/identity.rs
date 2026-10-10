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

//! Node identity load/save: ED25519 primary key, optional PQ auxiliary keys.

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
    /// Load the keypair from `path`, generating and atomically saving it if absent.
    ///
    /// The PQ keys (`keypair.pq.bin`) are loaded if present; otherwise they are
    /// left to [`ensure_pq_keys`].
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

    /// Get the node's `PeerId`.
    pub fn peer_id(&self) -> PeerId {
        self.keypair.public().to_peer_id()
    }

    /// Whether PQ keys are loaded.
    #[allow(dead_code)]
    pub fn has_pq_keys(&self) -> bool {
        self.pq_keys.is_some()
    }

    /// Take ownership of the PQ keys (to inject into the network actor).
    pub fn take_pq_keys(&mut self) -> Option<PqKeys> {
        self.pq_keys.take()
    }

    /// Ensure PQ keys exist, generating and atomically saving them if missing.
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

/// Atomic write: write to a temp file, then rename.
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
