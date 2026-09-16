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
    pq_secret_key: Option<Vec<u8>>,
    pq_public_key: Option<Vec<u8>>,
}

impl KeyManager {
    /// 从指定路径加载密钥，若不存在则生成并原子保存
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();
        let pq_path = path.with_extension("pq.bin");

        // 尝试读取并解析现有 ED25519 密钥文件
        match fs::read(path) {
            Ok(bytes) => match identity::Keypair::from_protobuf_encoding(&bytes) {
                Ok(keypair) => {
                    LogStruct::new(
                        LogLevel::Important,
                        "密钥加载成功",
                        path.display().to_string(),
                    )
                    .emit();
                    let (pq_secret, pq_public) = KeyManager::load_pq_keys(&pq_path);
                    return Ok(KeyManager {
                        keypair,
                        path: path.to_path_buf(),
                        pq_secret_key: pq_secret,
                        pq_public_key: pq_public,
                        pq_path,
                    });
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
            }
            Err(e) => {
                LogStruct::new(LogLevel::Error, "无法读取密钥文件", e.to_string()).emit();
                return Err(e.into());
            }
        }

        // 生成新密钥对
        let keypair = identity::Keypair::generate_ed25519();
        let encoded = keypair.to_protobuf_encoding()?;

        // 原子写入：先写临时文件，再重命名
        let temp_path = path.with_extension("tmp");
        if let Err(e) = fs::write(&temp_path, &encoded) {
            LogStruct::new(
                LogLevel::Critical,
                "写入临时密钥文件失败",
                format!("路径: {}, 错误: {}", temp_path.display(), e),
            )
            .emit();
            return Err(e.into());
        }
        if let Err(e) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            LogStruct::new(
                LogLevel::Critical,
                "重命名密钥文件失败",
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

        LogStruct::new(
            LogLevel::Important,
            "新密钥生成并保存成功",
            path.display().to_string(),
        )
        .emit();

        Ok(KeyManager {
            keypair,
            path: path.to_path_buf(),
            pq_secret_key: None,
            pq_public_key: None,
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

    /// 是否有 PQ 密钥
    #[allow(dead_code)]
    pub fn has_pq_keys(&self) -> bool {
        self.pq_secret_key.is_some() && self.pq_public_key.is_some()
    }

    /// 获取 PQ 公钥
    #[allow(dead_code)]
    pub fn pq_public_key(&self) -> Option<&[u8]> {
        self.pq_public_key.as_deref()
    }

    /// 获取 PQ 私钥
    #[allow(dead_code)]
    pub fn pq_secret_key(&self) -> Option<&[u8]> {
        self.pq_secret_key.as_deref()
    }

    /// 保存 PQ 密钥到 sidecar 文件
    #[allow(dead_code)]
    pub fn save_pq_keys(
        &mut self,
        secret: Vec<u8>,
        public: Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = Vec::with_capacity(8 + secret.len() + public.len());
        buf.extend_from_slice(&(secret.len() as u32).to_be_bytes());
        buf.extend_from_slice(&secret);
        buf.extend_from_slice(&(public.len() as u32).to_be_bytes());
        buf.extend_from_slice(&public);

        let temp_path = self.pq_path.with_extension("tmp");
        fs::write(&temp_path, &buf)?;
        fs::rename(&temp_path, &self.pq_path)?;

        self.pq_secret_key = Some(secret);
        self.pq_public_key = Some(public);
        Ok(())
    }

    /// 从 sidecar 文件加载 PQ 密钥
    fn load_pq_keys(pq_path: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let bytes = match fs::read(pq_path) {
            Ok(b) => b,
            Err(_) => return (None, None),
        };
        if bytes.len() < 8 {
            return (None, None);
        }
        let secret_len = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        if 4 + secret_len + 4 > bytes.len() {
            return (None, None);
        }
        let secret = bytes[4..4 + secret_len].to_vec();
        let public_len =
            u32::from_be_bytes(bytes[4 + secret_len..8 + secret_len].try_into().unwrap()) as usize;
        if 8 + secret_len + public_len > bytes.len() {
            return (None, None);
        }
        let public = bytes[8 + secret_len..8 + secret_len + public_len].to_vec();
        (Some(secret), Some(public))
    }
}
