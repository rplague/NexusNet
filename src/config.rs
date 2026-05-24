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

use crate::{LogLevel, LogStruct};
use libp2p::Multiaddr;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
// 定义配置结构体

fn bool_true() -> bool {
    true
}

fn bool_false() -> bool {
    false
}

const fn hours(h: u32) -> u32 {
    h * 3600
}

macro_rules! default_u32_fn {
    ($name:ident, $value:expr) => {
        fn $name() -> u32 {
            $value
        }
    };
}

default_u32_fn!(default_port, 5000);
default_u32_fn!(default_interval, 15);
default_u32_fn!(default_timeout, 10);
default_u32_fn!(default_max_failures, 2);
default_u32_fn!(default_record_ttl_seconds, hours(1));
default_u32_fn!(default_replication_factor, 20);
default_u32_fn!(default_query_timeout_seconds, 60);

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct NodeConfig {
    #[serde(default)]
    pub node: NodeInfo,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub services: ServicesConfig,
    #[serde(default)]
    pub crypto: CryptoConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            node: NodeInfo::default(),
            network: NetworkConfig::default(),
            services: ServicesConfig::default(),
            crypto: CryptoConfig::default(),
        }
    }
}

impl NodeConfig {
    /// 返回值总是有效的 NodeConfig。
    fn from_toml_file(path: impl AsRef<Path>, create_if_missing: bool) -> NodeConfig {
        let path = path.as_ref();

        // 如果不需要创建且文件不存在，直接返回默认配置
        if !create_if_missing && !path.exists() {
            LogStruct::new(
                LogLevel::Warning,
                "配置文件不存在",
                "配置文件不存在且未要求创建，使用默认配置",
            )
            .emit();
            return NodeConfig::default();
        }

        // 尝试读取并解析文件
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => return config,
                Err(e) => {
                    LogStruct::new(
                        LogLevel::Error,
                        "配置文件解析失败，将会创建新的配置文件",
                        e.to_string(),
                    )
                    .emit();
                    if let Err(rename_err) = rename_bad_config(path) {
                        LogStruct::new(
                            LogLevel::Critical,
                            "重命名损坏的配置文件失败",
                            rename_err.to_string(),
                        )
                        .emit();
                        std::process::exit(1);
                    }
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                LogStruct::new(LogLevel::Critical, "无法读取配置文件", e.to_string()).emit();
                std::process::exit(1);
            }
        }

        // 走到这里：文件不存在 或 解析失败后重命名完成
        // 创建默认配置并写入文件
        let default_config = NodeConfig::default();
        let toml_string = match toml::to_string_pretty(&default_config) {
            Ok(s) => s,
            Err(e) => {
                LogStruct::new(LogLevel::Critical, "序列化默认配置失败", e.to_string()).emit();
                std::process::exit(1);
            }
        };
        if let Err(e) = fs::write(path, toml_string) {
            LogStruct::new(LogLevel::Critical, "写入默认配置文件失败", e.to_string()).emit();
            std::process::exit(1);
        }
        default_config
    }
}

fn rename_bad_config(path: &Path) -> io::Result<()> {
    let mut backup_path = path.with_extension("bak");
    let mut counter = 1;
    while backup_path.exists() {
        backup_path = path.with_extension(format!("bak.{}", counter));
        counter += 1;
    }
    fs::rename(path, &backup_path)?;
    Ok(())
}

fn default_name() -> String {
    "未设置的p2p节点".to_string()
}

fn default_description() -> String {
    "无详细描述".to_string()
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct NodeInfo {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_description")]
    pub description: String,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            name: default_name(),
            description: default_description(),
        }
    }
}

fn default_announce_addresses() -> Vec<Multiaddr> {
    vec![]
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct NetworkConfig {
    #[serde(default = "bool_false")]
    pub ipv4_enabled: bool,
    #[serde(default)]
    pub ipv4_address: Option<IpAddr>,
    #[serde(default = "bool_false")]
    pub ipv6_enabled: bool,
    #[serde(default)]
    pub ipv6_address: Option<IpAddr>,
    #[serde(default = "default_port")]
    pub port: u32,
    #[serde(default = "default_announce_addresses")]
    pub announce_addresses: Vec<Multiaddr>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            ipv4_enabled: bool_false(),
            ipv4_address: None,
            ipv6_enabled: bool_false(),
            ipv6_address: None,
            port: default_port(),
            announce_addresses: default_announce_addresses(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct ServicesConfig {
    #[serde(default)]
    pub ping: PingService,
    #[serde(default)]
    pub kademlia: KademliaService,
    #[serde(default)]
    pub dispatcher: DispatcherConfig,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PingService {
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default = "default_interval")]
    pub interval_secs: u32,
    #[serde(default = "default_timeout")]
    pub with_timeout: u32,
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
}

impl Default for PingService {
    fn default() -> Self {
        PingService {
            enabled: bool_true(),
            interval_secs: default_interval(),
            with_timeout: default_timeout(),
            max_failures: default_max_failures(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct KademliaService {
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default = "default_record_ttl_seconds")]
    pub record_ttl_seconds: u32,
    #[serde(default = "default_replication_factor")]
    pub replication_factor: u32,
    #[serde(default = "default_query_timeout_seconds")]
    pub query_timeout_seconds: u32,
    #[serde(default = "default_announce_addresses")]
    pub bootstrap_nodes: Vec<Multiaddr>,
}

impl Default for KademliaService {
    fn default() -> Self {
        KademliaService {
            enabled: bool_true(),
            record_ttl_seconds: default_record_ttl_seconds(),
            replication_factor: default_replication_factor(),
            query_timeout_seconds: default_query_timeout_seconds(),
            bootstrap_nodes: default_announce_addresses(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LocalServiceEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DispatcherConfig {
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default = "default_query_timeout_seconds")]
    pub query_timeout_secs: u32,
    #[serde(default = "default_record_ttl_seconds")]
    pub record_ttl_secs: u32,
    #[serde(default = "Vec::new")]
    pub local_services: Vec<LocalServiceEntry>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        DispatcherConfig {
            enabled: bool_true(),
            query_timeout_secs: default_query_timeout_seconds(),
            record_ttl_secs: default_record_ttl_seconds(),
            local_services: vec![],
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CryptoConfig {
    #[serde(default = "bool_false")]
    pub pq_transport_enabled: bool,
    #[serde(default = "bool_false")]
    pub pq_identity_enabled: bool,
    #[serde(default = "bool_false")]
    pub pq_required: bool,
}

impl Default for CryptoConfig {
    fn default() -> Self {
        CryptoConfig {
            pq_transport_enabled: false,
            pq_identity_enabled: false,
            pq_required: false,
        }
    }
}

#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<NodeConfig>>,
}

impl ConfigHandle {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
        }
    }

    pub fn from_toml_file(path: impl AsRef<Path>, create_if_missing: bool) -> Self {
        let config = NodeConfig::from_toml_file(path, create_if_missing);
        Self::new(config)
    }

    pub fn load_or_create_default() -> Self {
        Self::from_toml_file("./config.toml", true)
    }

    /// 将当前配置保存到 TOML 文件
    /// 如果文件已存在，会覆盖写入
    /// 如果写入失败（如权限不足、磁盘满），会记录 Critical 日志并退出程序
    fn save_to_file(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        let snapshot = self.snapshot();
        let toml_string = match toml::to_string_pretty(&snapshot) {
            Ok(s) => s,
            Err(e) => {
                LogStruct::new(LogLevel::Critical, "序列化配置失败", e.to_string()).emit();
                std::process::exit(1);
            }
        };

        let temp_path = path.with_extension("tmp");

        if let Err(e) = fs::write(&temp_path, toml_string) {
            LogStruct::new(
                LogLevel::Critical,
                "写入临时配置文件失败",
                format!("路径: {}, 错误: {}", temp_path.display(), e),
            )
            .emit();
            std::process::exit(1);
        }

        if let Err(e) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            LogStruct::new(
                LogLevel::Critical,
                "重命名配置文件失败",
                format!(
                    "从 {} 到 {}, 错误: {}",
                    temp_path.display(),
                    path.display(),
                    e
                ),
            )
            .emit();
            std::process::exit(1);
        }
    }

    /// 便捷方法：保存到默认路径 `./config.toml`
    pub fn save_to_default(&self) {
        self.save_to_file("./config.toml");
    }

    /// 获取只读锁
    pub fn read(&self) -> RwLockReadGuard<'_, NodeConfig> {
        self.inner.read().expect("RwLock 被污染")
    }

    /// 获取写锁
    pub fn write(&self) -> RwLockWriteGuard<'_, NodeConfig> {
        self.inner.write().expect("RwLock 被污染")
    }

    // ========== 便捷只读方法 ==========
    pub fn ping_enabled(&self) -> bool {
        self.read().services.ping.enabled
    }

    pub fn ping_interval(&self) -> u32 {
        self.read().services.ping.interval_secs
    }

    pub fn ping_timeout(&self) -> u32 {
        self.read().services.ping.with_timeout
    }

    pub fn kademlia_enabled(&self) -> bool {
        self.read().services.kademlia.enabled
    }

    pub fn kademlia_record_ttl(&self) -> u32 {
        self.read().services.kademlia.record_ttl_seconds
    }

    pub fn kademlia_replication_factor(&self) -> u32 {
        self.read().services.kademlia.replication_factor
    }

    pub fn kademlia_query_timeout(&self) -> u32 {
        self.read().services.kademlia.query_timeout_seconds
    }

    pub fn bootstrap_nodes(&self) -> Vec<Multiaddr> {
        self.read().services.kademlia.bootstrap_nodes.clone()
    }

    pub fn listen_port(&self) -> u32 {
        self.read().network.port
    }

    pub fn ipv4_enabled(&self) -> bool {
        self.read().network.ipv4_enabled
    }

    // ========== 便捷修改方法 ==========
    pub fn set_ping_enabled(&self, enabled: bool) {
        self.write().services.ping.enabled = enabled;
    }

    pub fn set_ping_interval(&self, secs: u32) {
        self.write().services.ping.interval_secs = secs;
    }

    pub fn set_kademlia_enabled(&self, enabled: bool) {
        self.write().services.kademlia.enabled = enabled;
    }

    pub fn set_kademlia_record_ttl(&self, ttl_secs: u32) {
        self.write().services.kademlia.record_ttl_seconds = ttl_secs;
    }

    pub fn set_kademlia_replication_factor(&self, factor: u32) {
        self.write().services.kademlia.replication_factor = factor;
    }

    pub fn set_kademlia_query_timeout(&self, timeout_secs: u32) {
        self.write().services.kademlia.query_timeout_seconds = timeout_secs;
    }

    pub fn set_bootstrap_nodes(&self, nodes: Vec<Multiaddr>) {
        self.write().services.kademlia.bootstrap_nodes = nodes;
    }

    pub fn set_listen_port(&self, port: u32) {
        self.write().network.port = port;
    }

    pub fn set_ipv4(&self, enabled: bool, address: Option<IpAddr>) {
        let mut cfg = self.write();
        cfg.network.ipv4_enabled = enabled;
        cfg.network.ipv4_address = address;
    }

    /// 替换整个 NodeConfig
    pub fn replace_config(&self, new_config: NodeConfig) {
        *self.write() = new_config;
    }

    /// 获取配置的快照
    pub fn snapshot(&self) -> NodeConfig {
        self.read().clone()
    }
}
