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

use crate::limits::{DHT_MAX_RECORD_BYTES, OLD_COMPAT_RECORD_BYTES};
use crate::paths;
use crate::{LogLevel, LogStruct};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
// Node configuration schema.

fn bool_true() -> bool {
    true
}

fn bool_false() -> bool {
    false
}

fn is_false(value: &bool) -> bool {
    !*value
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
default_u32_fn!(default_provider_record_ttl_seconds, hours(1));
default_u32_fn!(default_provider_publication_interval_seconds, 30 * 60);
default_u32_fn!(default_replication_factor, 20);
default_u32_fn!(default_query_timeout_seconds, 60);
default_u32_fn!(default_relay_retry_interval, 60);
default_u32_fn!(default_relay_max_failures, 3);
default_u32_fn!(default_types_sync_seconds, 300);
default_u32_fn!(default_type_ttl_seconds, 1800);
default_u32_fn!(default_max_inflight_dht, 4);

/// Default P2P service request/response size limit (16 MiB), aligned with
/// `sidecar_protocol::MAX_FRAME`.
fn default_max_message_bytes() -> u64 {
    16 << 20
}

const MAX_MESSAGE_BYTES_LIMIT: u64 = 64 << 20;

fn default_auth_network() -> String {
    "none".to_string()
}

/// Compatibility knobs for mixed-version networks.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CompatConfig {
    /// Maximum size of a DHT record written by this node.
    ///
    /// The default fits under the old Kademlia 16 KiB packet limit so older
    /// nodes can still store records. Raise it after every node supports the
    /// larger limit.
    #[serde(default = "default_compat_max_record_bytes")]
    pub max_record_bytes: u64,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            max_record_bytes: default_compat_max_record_bytes(),
        }
    }
}

/// Conservative default: leaves room for protobuf framing and closer peers.
fn default_compat_max_record_bytes() -> u64 {
    OLD_COMPAT_RECORD_BYTES as u64
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct NodeConfig {
    #[serde(default)]
    pub node: NodeInfo,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub services: ServicesConfig,
    #[serde(default)]
    pub crypto: CryptoConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub compat: CompatConfig,
}

/// Kind of configuration loading failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    Io,
    Parse,
    Invalid,
}

/// Configuration loading failure that does not terminate the process.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub message: String,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

impl NodeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.network.port > u16::MAX as u32 {
            return Err(format!(
                "network.port 超出范围: {} (最大 {})",
                self.network.port,
                u16::MAX
            ));
        }
        if self.services.ping.enabled {
            if self.services.ping.interval_secs == 0 {
                return Err("services.ping.interval_secs 必须大于 0".to_string());
            }
            if self.services.ping.with_timeout == 0 {
                return Err("services.ping.with_timeout 必须大于 0".to_string());
            }
        }
        if self.services.kademlia.enabled {
            if self.services.kademlia.record_ttl_seconds == 0 {
                return Err("services.kademlia.record_ttl_seconds 必须大于 0".to_string());
            }
            if self.services.kademlia.query_timeout_seconds == 0 {
                return Err("services.kademlia.query_timeout_seconds 必须大于 0".to_string());
            }
        }
        if self.services.kademlia.provider_record_ttl_seconds == 0 {
            return Err("services.kademlia.provider_record_ttl_seconds 必须大于 0".to_string());
        }
        if self.services.kademlia.provider_publication_interval_seconds == 0 {
            return Err(
                "services.kademlia.provider_publication_interval_seconds 必须大于 0".to_string(),
            );
        }
        let provider_ttl = self.services.kademlia.provider_record_ttl_seconds;
        let provider_republish =
            2u32.saturating_mul(self.services.kademlia.provider_publication_interval_seconds);
        if provider_ttl < provider_republish {
            return Err(format!(
                "services.kademlia.provider_record_ttl_seconds 必须不小于 2 * provider_publication_interval_seconds ({provider_ttl} < {provider_republish})"
            ));
        }
        if self.services.dispatcher.enabled {
            if self.services.dispatcher.query_timeout_secs == 0 {
                return Err("services.dispatcher.query_timeout_secs 必须大于 0".to_string());
            }
            if self.services.dispatcher.types_sync_secs == 0 {
                return Err("services.dispatcher.types_sync_secs 必须大于 0".to_string());
            }
        }
        if !(1..=16).contains(&self.services.registry.max_inflight_dht) {
            return Err("services.registry.max_inflight_dht 必须在 1..=16".to_string());
        }
        if self.services.registry.type_ttl_secs == 0 {
            return Err("services.registry.type_ttl_secs 必须大于 0".to_string());
        }
        if self.services.registry.type_ttl_secs
            <= 2u32.saturating_mul(self.services.dispatcher.types_sync_secs)
        {
            return Err(format!(
                "services.registry.type_ttl_secs 必须大于 2 * services.dispatcher.types_sync_secs ({} <= {})",
                self.services.registry.type_ttl_secs,
                2u32.saturating_mul(self.services.dispatcher.types_sync_secs)
            ));
        }
        if self.compat.max_record_bytes == 0
            || self.compat.max_record_bytes > DHT_MAX_RECORD_BYTES as u64
        {
            return Err(format!(
                "compat.max_record_bytes 必须在 1..={DHT_MAX_RECORD_BYTES}"
            ));
        }
        if !(1..=16).contains(&self.services.dispatcher.max_inflight_dht) {
            return Err("services.dispatcher.max_inflight_dht 必须在 1..=16".to_string());
        }
        if self.services.dispatcher.max_request_bytes == 0
            || self.services.dispatcher.max_request_bytes > MAX_MESSAGE_BYTES_LIMIT
        {
            return Err(format!(
                "services.dispatcher.max_request_bytes 必须在 1..={MAX_MESSAGE_BYTES_LIMIT}"
            ));
        }
        if self.services.dispatcher.max_response_bytes == 0
            || self.services.dispatcher.max_response_bytes > MAX_MESSAGE_BYTES_LIMIT
        {
            return Err(format!(
                "services.dispatcher.max_response_bytes 必须在 1..={MAX_MESSAGE_BYTES_LIMIT}"
            ));
        }
        if self.services.relay.retry_interval_secs == 0 {
            return Err("services.relay.retry_interval_secs 必须大于 0".to_string());
        }
        let mut names: Vec<&str> = Vec::new();
        for service in &self.services.dispatcher.local_services {
            if names.contains(&service.name.as_str()) {
                return Err(format!("local_services 名称重复: {}", service.name));
            }
            if service.port == 0 {
                return Err(format!("local_services.{} 端口不能为 0", service.name));
            }
            names.push(&service.name);
        }
        if self.crypto.pq_required && !self.crypto.pq_transport_enabled {
            return Err(
                "crypto.pq_required = true 需要 crypto.pq_transport_enabled = true".to_string(),
            );
        }
        if self.crypto.pq_identity_enabled && !self.crypto.pq_transport_enabled {
            return Err(
                "crypto.pq_identity_enabled = true 需要 crypto.pq_transport_enabled = true"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Load a configuration file without terminating the process on failure.
    ///
    /// Missing files are created with defaults when `create_if_missing` is true;
    /// otherwise defaults are returned. Parse, validation and I/O errors are
    /// reported to the caller.
    pub fn try_from_toml_file(
        path: impl AsRef<Path>,
        create_if_missing: bool,
    ) -> Result<NodeConfig, ConfigError> {
        let path = path.as_ref();

        if !create_if_missing && !path.exists() {
            LogStruct::new(
                LogLevel::Warning,
                "配置文件不存在",
                "配置文件不存在且未要求创建，使用默认配置",
            )
            .emit();
            return Ok(NodeConfig::default());
        }

        match fs::read_to_string(path) {
            Ok(content) => {
                let config = toml::from_str::<NodeConfig>(&content)
                    .map_err(|e| ConfigError::new(ConfigErrorKind::Parse, e.to_string()))?;
                config
                    .validate()
                    .map_err(|e| ConfigError::new(ConfigErrorKind::Invalid, e))?;
                if let Some(completed) = complete_missing_fields(&content, &config) {
                    try_write_atomic(path, &completed).map_err(|e| {
                        ConfigError::new(
                            ConfigErrorKind::Io,
                            format!("写入配置文件失败 {}: {e}", path.display()),
                        )
                    })?;
                    LogStruct::new(
                        LogLevel::Preset,
                        "配置已自动补全",
                        path.display().to_string(),
                    )
                    .emit();
                }
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let default_config = NodeConfig::default();
                let toml_string = toml::to_string_pretty(&default_config).map_err(|e| {
                    ConfigError::new(ConfigErrorKind::Io, format!("序列化默认配置失败: {e}"))
                })?;
                try_write_atomic(path, &toml_string).map_err(|e| {
                    ConfigError::new(
                        ConfigErrorKind::Io,
                        format!("写入默认配置文件失败 {}: {e}", path.display()),
                    )
                })?;
                Ok(default_config)
            }
            Err(e) => Err(ConfigError::new(
                ConfigErrorKind::Io,
                format!("无法读取配置文件 {}: {e}", path.display()),
            )),
        }
    }

    /// Legacy loader used by the desktop binary: always returns a valid config,
    /// repairing or exiting on unrecoverable errors.
    fn from_toml_file(path: impl AsRef<Path>, create_if_missing: bool) -> NodeConfig {
        let path = path.as_ref();
        match Self::try_from_toml_file(path, create_if_missing) {
            Ok(config) => config,
            Err(error) if error.kind == ConfigErrorKind::Parse => {
                LogStruct::new(
                    LogLevel::Error,
                    "配置文件解析失败，将会创建新的配置文件",
                    &error.message,
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
                let default_config = NodeConfig::default();
                let toml_string = match toml::to_string_pretty(&default_config) {
                    Ok(s) => s,
                    Err(e) => {
                        LogStruct::new(LogLevel::Critical, "序列化默认配置失败", e.to_string())
                            .emit();
                        std::process::exit(1);
                    }
                };
                if let Err(e) = fs::write(path, toml_string) {
                    LogStruct::new(LogLevel::Critical, "写入默认配置文件失败", e.to_string())
                        .emit();
                    std::process::exit(1);
                }
                default_config
            }
            Err(error) => {
                LogStruct::new(LogLevel::Critical, "配置加载失败", error.message).emit();
                std::process::exit(1);
            }
        }
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

/// Fill in keys that exist in `config` but are missing from the TOML document.
///
/// Existing values, unknown keys, comments and array-of-table entries are left
/// untouched. Returns `None` when the document is already complete.
fn complete_missing_fields(original: &str, config: &NodeConfig) -> Option<String> {
    let mut doc = original.parse::<toml_edit::DocumentMut>().ok()?;
    let complete = toml::to_string_pretty(config).ok()?;
    let defaults = complete.parse::<toml_edit::DocumentMut>().ok()?;
    merge_missing(doc.as_table_mut(), defaults.as_table());
    let updated = doc.to_string();
    (updated != original).then_some(updated)
}

fn merge_missing(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (key, src_item) in src.iter() {
        match dst.get_mut(key) {
            None => {
                dst.insert(key, src_item.clone());
            }
            Some(dst_item) => {
                if let (Some(dst_table), Some(src_table)) =
                    (dst_item.as_table_mut(), src_item.as_table())
                {
                    merge_missing(dst_table, src_table);
                }
            }
        }
    }
}

fn try_write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, content)?;
    if let Err(e) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    Ok(())
}

fn write_atomic_or_exit(path: &Path, content: &str) {
    if let Err(e) = try_write_atomic(path, content) {
        LogStruct::new(
            LogLevel::Critical,
            "写入配置文件失败",
            format!("路径: {}, 错误: {}", path.display(), e),
        )
        .emit();
        std::process::exit(1);
    }
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
    /// Whether this node may act as a bootstrap node when dual-stack (publishes
    /// a DHT provider record).
    #[serde(default = "bool_true")]
    pub allow_bootstrap: bool,
    /// Whether to automatically add bootstrap providers discovered via Identify.
    #[serde(default = "bool_false")]
    pub accept_bootstrap_providers: bool,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            name: default_name(),
            description: default_description(),
            allow_bootstrap: bool_true(),
            accept_bootstrap_providers: bool_false(),
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
    /// Always require relay reservations even when a public address is detected.
    ///
    /// Mobile networks often expose a global IPv6 address while still blocking
    /// inbound connections (CGNAT/firewall), so IP-based reachability is not
    /// trustworthy. Desktop defaults to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_relay: bool,
    /// Whether a public address was detected on a local interface at startup.
    ///
    /// Runtime only: never read from or written to `config.toml`.
    #[serde(skip)]
    pub has_public_address: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            ipv4_enabled: bool_false(),
            ipv4_address: None,
            ipv6_enabled: bool_false(),
            ipv6_address: None,
            port: default_port(),
            force_relay: bool_false(),
            has_public_address: bool_false(),
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
    #[serde(default)]
    pub registry: RegistryService,
    #[serde(default)]
    pub relay: RelayService,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RegistryService {
    #[serde(default = "default_max_inflight_dht")]
    pub max_inflight_dht: u32,
    /// How long a service type remains in the DHT index without a provider
    /// renewing it. Must exceed two dispatcher sync intervals.
    #[serde(default = "default_type_ttl_seconds")]
    pub type_ttl_secs: u32,
}

impl Default for RegistryService {
    fn default() -> Self {
        RegistryService {
            max_inflight_dht: default_max_inflight_dht(),
            type_ttl_secs: default_type_ttl_seconds(),
        }
    }
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
    /// How long a provider record stays discoverable without being renewed.
    #[serde(default = "default_provider_record_ttl_seconds")]
    pub provider_record_ttl_seconds: u32,
    /// How often an active provider republishes its provider record.
    #[serde(default = "default_provider_publication_interval_seconds")]
    pub provider_publication_interval_seconds: u32,
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
            provider_record_ttl_seconds: default_provider_record_ttl_seconds(),
            provider_publication_interval_seconds: default_provider_publication_interval_seconds(),
            replication_factor: default_replication_factor(),
            query_timeout_seconds: default_query_timeout_seconds(),
            bootstrap_nodes: default_announce_addresses(),
        }
    }
}

/// Relay reservation (consumer side) configuration.
///
/// A single-stack node reserves a `p2p-circuit` on bootstrap nodes
/// (conceptually dual-stack relays) to gain reachability across IP families.
/// The target count is fixed at 3 and best-effort only.
///
/// Whether reservations are needed is decided by the node's own stack: a
/// non-dual-stack node is a consumer and needs no switch.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RelayService {
    /// Interval between top-up checks, in seconds.
    #[serde(default = "default_relay_retry_interval")]
    pub retry_interval_secs: u32,
    /// Consecutive reservation failures before evicting the node from the
    /// bootstrap list.
    #[serde(default = "default_relay_max_failures")]
    pub max_failures: u32,
}

impl Default for RelayService {
    fn default() -> Self {
        RelayService {
            retry_interval_secs: default_relay_retry_interval(),
            max_failures: default_relay_max_failures(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LocalServiceEntry {
    pub name: String,
    pub port: u16,
    /// Whether this service requires authentication.
    #[serde(default = "bool_false")]
    pub require_auth: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DispatcherConfig {
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default = "default_query_timeout_seconds")]
    pub query_timeout_secs: u32,
    /// How often to reconcile the global service-type index with the DHT.
    #[serde(default = "default_types_sync_seconds")]
    pub types_sync_secs: u32,
    /// Maximum number of DHT control commands executing concurrently.
    #[serde(default = "default_max_inflight_dht")]
    pub max_inflight_dht: u32,
    /// Maximum encoded size of one P2P service request.
    #[serde(default = "default_max_message_bytes")]
    pub max_request_bytes: u64,
    /// Maximum encoded size of one P2P service response.
    #[serde(default = "default_max_message_bytes")]
    pub max_response_bytes: u64,
    #[serde(default = "Vec::new")]
    pub local_services: Vec<LocalServiceEntry>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        DispatcherConfig {
            enabled: bool_true(),
            query_timeout_secs: default_query_timeout_seconds(),
            types_sync_secs: default_types_sync_seconds(),
            max_inflight_dht: default_max_inflight_dht(),
            max_request_bytes: default_max_message_bytes(),
            max_response_bytes: default_max_message_bytes(),
            local_services: vec![],
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct CryptoConfig {
    #[serde(default = "bool_false")]
    pub pq_transport_enabled: bool,
    #[serde(default = "bool_false")]
    pub pq_identity_enabled: bool,
    #[serde(default = "bool_false")]
    pub pq_required: bool,
}

/// Node-level authentication configuration.
///
/// `network = "none"` disables authentication; any other value must have a
/// matching authority public key under `networks`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AuthConfig {
    /// Authentication network this node belongs to; "none" disables
    /// authentication.
    #[serde(default = "default_auth_network")]
    pub network: String,
    /// Network name -> trust anchor.
    #[serde(default)]
    pub networks: HashMap<String, AuthNetworkConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            network: default_auth_network(),
            networks: HashMap::new(),
        }
    }
}

/// Trust anchor of a single authentication network.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct AuthNetworkConfig {
    /// Authority ed25519 public key (base64), parsed by the `auth` module.
    #[serde(default)]
    pub authority: String,
}

#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<NodeConfig>>,
    path: Option<Arc<PathBuf>>,
}

/// Config file path, resolved once via `paths::fail_path_gener`.
fn config_filepath() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| paths::fail_path_gener(paths::FailType::Config, "config.toml"))
}

#[allow(dead_code)]
impl ConfigHandle {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
            path: None,
        }
    }

    pub fn from_toml_file(path: impl AsRef<Path>, create_if_missing: bool) -> Self {
        let path = path.as_ref().to_path_buf();
        let config = NodeConfig::from_toml_file(&path, create_if_missing);
        Self {
            inner: Arc::new(RwLock::new(config)),
            path: Some(Arc::new(path)),
        }
    }

    /// Load a configuration file without terminating the process.
    pub fn try_from_toml_file(
        path: impl AsRef<Path>,
        create_if_missing: bool,
    ) -> Result<Self, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let config = NodeConfig::try_from_toml_file(&path, create_if_missing)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(config)),
            path: Some(Arc::new(path)),
        })
    }

    pub fn load_or_create_default() -> Self {
        Self::from_toml_file(config_filepath(), true)
    }

    /// Path this handle was loaded from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref().map(PathBuf::as_path)
    }

    /// Save the current config to a TOML file.
    ///
    /// Overwrites an existing file. On write failure (e.g. permissions, disk
    /// full) it logs Critical and exits the process.
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

        write_atomic_or_exit(path, &toml_string);
    }

    /// Save to the bound path, falling back to `NEXUSNET_CONFIG_PATH`.
    pub fn save_to_default(&self) {
        match &self.path {
            Some(path) => self.save_to_file(path.as_path()),
            None => self.save_to_file(config_filepath()),
        }
    }

    /// Save to an explicit path without exiting on failure.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref();
        let snapshot = self.snapshot();
        let toml_string =
            toml::to_string_pretty(&snapshot).map_err(|e| format!("序列化配置失败: {e}"))?;
        try_write_atomic(path, &toml_string)
            .map_err(|e| format!("写入配置失败 {}: {e}", path.display()))
    }

    /// Acquire a read lock.
    pub fn read(&self) -> RwLockReadGuard<'_, NodeConfig> {
        self.inner.read().expect("RwLock 被污染")
    }

    /// Acquire a write lock.
    pub fn write(&self) -> RwLockWriteGuard<'_, NodeConfig> {
        self.inner.write().expect("RwLock 被污染")
    }

    // ========== Convenience getters ==========
    pub fn allow_bootstrap(&self) -> bool {
        self.read().node.allow_bootstrap
    }

    pub fn accept_bootstrap_providers(&self) -> bool {
        self.read().node.accept_bootstrap_providers
    }

    pub fn ping_enabled(&self) -> bool {
        self.read().services.ping.enabled
    }

    pub fn ping_interval(&self) -> u32 {
        self.read().services.ping.interval_secs
    }

    pub fn ping_timeout(&self) -> u32 {
        self.read().services.ping.with_timeout
    }

    pub fn ping_max_failures(&self) -> u32 {
        self.read().services.ping.max_failures
    }

    pub fn dispatcher_enabled(&self) -> bool {
        self.read().services.dispatcher.enabled
    }

    pub fn dispatcher_query_timeout(&self) -> u32 {
        self.read().services.dispatcher.query_timeout_secs
    }

    pub fn dispatcher_max_inflight_dht(&self) -> usize {
        self.read().services.dispatcher.max_inflight_dht as usize
    }

    pub fn dispatcher_max_request_bytes(&self) -> u64 {
        self.read().services.dispatcher.max_request_bytes
    }

    pub fn dispatcher_max_response_bytes(&self) -> u64 {
        self.read().services.dispatcher.max_response_bytes
    }

    /// Interval, in seconds, between global service-type index reconciliations.
    pub fn dispatcher_types_sync_interval(&self) -> u32 {
        self.read().services.dispatcher.types_sync_secs.max(1)
    }

    pub fn registry_max_inflight_dht(&self) -> usize {
        self.read().services.registry.max_inflight_dht as usize
    }

    /// Lifetime, in seconds, of one service-type lease.
    pub fn registry_type_ttl_secs(&self) -> u64 {
        self.read().services.registry.type_ttl_secs as u64
    }

    /// Maximum DHT record size this node writes.
    pub fn compat_max_record_bytes(&self) -> u64 {
        self.read().compat.max_record_bytes
    }
    pub fn relay_retry_interval(&self) -> u32 {
        self.read().services.relay.retry_interval_secs
    }

    pub fn relay_max_failures(&self) -> u32 {
        self.read().services.relay.max_failures
    }

    pub fn pq_transport_enabled(&self) -> bool {
        self.read().crypto.pq_transport_enabled
    }

    pub fn pq_identity_enabled(&self) -> bool {
        self.read().crypto.pq_identity_enabled
    }

    pub fn pq_required(&self) -> bool {
        self.read().crypto.pq_required
    }

    /// Whether PQ is enabled (transport or identity).
    pub fn pq_enabled(&self) -> bool {
        let c = self.read();
        c.crypto.pq_transport_enabled || c.crypto.pq_identity_enabled
    }

    // ========== Authentication ==========
    pub fn auth_network(&self) -> String {
        self.read().auth.network.clone()
    }

    /// Raw base64 authority public key for `network`; parsing is handled by
    /// the `auth` module.
    pub fn auth_authority(&self, network: &str) -> Option<String> {
        self.read()
            .auth
            .networks
            .get(network)
            .map(|n| n.authority.clone())
    }

    /// Local services that require authentication (used to target whitelist
    /// refreshes).
    pub fn auth_required_services(&self) -> Vec<String> {
        self.read()
            .services
            .dispatcher
            .local_services
            .iter()
            .filter(|s| s.require_auth)
            .map(|s| s.name.clone())
            .collect()
    }

    pub fn service_requires_auth(&self, name: &str) -> bool {
        self.read()
            .services
            .dispatcher
            .local_services
            .iter()
            .any(|s| s.name == name && s.require_auth)
    }

    pub fn kademlia_enabled(&self) -> bool {
        self.read().services.kademlia.enabled
    }

    pub fn kademlia_record_ttl(&self) -> u32 {
        self.read().services.kademlia.record_ttl_seconds
    }

    pub fn kademlia_provider_record_ttl(&self) -> u32 {
        self.read().services.kademlia.provider_record_ttl_seconds
    }

    pub fn kademlia_provider_publication_interval(&self) -> u32 {
        self.read()
            .services
            .kademlia
            .provider_publication_interval_seconds
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

    /// External WebSocket addresses (`<ip>/tcp/<port>/ws/p2p/<peer_id>`)
    /// derived from the configured IPv4/IPv6 addresses.
    pub fn dialable_addrs(&self, peer_id: PeerId) -> Vec<Multiaddr> {
        let cfg = self.read();
        let port = cfg.network.port as u16;
        [cfg.network.ipv4_address, cfg.network.ipv6_address]
            .into_iter()
            .flatten()
            .map(|ip| {
                let mut a = Multiaddr::empty();
                match ip {
                    IpAddr::V4(v4) => a.push(Protocol::Ip4(v4)),
                    IpAddr::V6(v6) => a.push(Protocol::Ip6(v6)),
                }
                a.push(Protocol::Tcp(port));
                a.push(Protocol::Ws("/".into()));
                a.push(Protocol::P2p(peer_id));
                a
            })
            .collect()
    }

    pub fn listen_port(&self) -> u32 {
        self.read().network.port
    }

    pub fn ipv4_enabled(&self) -> bool {
        self.read().network.ipv4_enabled
    }

    /// Whether a public address was detected on a local interface at startup.
    pub fn has_public_address(&self) -> bool {
        self.read().network.has_public_address
    }

    /// A relay is needed unless this node is dual-stack and has a public address.
    pub fn needs_relay(&self) -> bool {
        let cfg = self.read();
        cfg.network.force_relay
            || !(cfg.network.ipv4_enabled && cfg.network.ipv6_enabled)
            || !cfg.network.has_public_address
    }

    // ========== Convenience setters ==========
    pub fn set_has_public_address(&self, present: bool) {
        self.write().network.has_public_address = present;
    }

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

    /// Replace the entire `NodeConfig`.
    pub fn replace_config(&self, new_config: NodeConfig) {
        *self.write() = new_config;
    }

    /// Get a snapshot of the config.
    pub fn snapshot(&self) -> NodeConfig {
        self.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    #[test]
    fn dialable_addrs_appends_peer_id() {
        let mut cfg = NodeConfig::default();
        cfg.network.port = 5000;
        cfg.network.ipv4_address = Some(IpAddr::V4(std::net::Ipv4Addr::new(1, 2, 3, 4)));
        cfg.network.ipv6_address = Some("2001:db8::1".parse().unwrap());
        let handle = ConfigHandle::new(cfg);
        let pid = peer();

        let addrs = handle.dialable_addrs(pid);
        assert_eq!(addrs.len(), 2);
        assert_eq!(
            addrs[0].to_string(),
            format!("/ip4/1.2.3.4/tcp/5000/ws/p2p/{pid}")
        );
        assert_eq!(
            addrs[1].to_string(),
            format!("/ip6/2001:db8::1/tcp/5000/ws/p2p/{pid}")
        );
    }

    #[test]
    fn dialable_addrs_only_ipv6() {
        let mut cfg = NodeConfig::default();
        cfg.network.ipv6_address = Some("2001:db8::1".parse().unwrap());
        let handle = ConfigHandle::new(cfg);
        let pid = peer();

        let addrs = handle.dialable_addrs(pid);
        assert_eq!(addrs.len(), 1);
        assert!(addrs[0].to_string().ends_with(&format!("/p2p/{pid}")));
    }

    #[test]
    fn public_address_flag_defaults_to_false_and_is_not_serialized() {
        let config = NodeConfig::default();
        assert!(!config.network.has_public_address);

        let handle = ConfigHandle::new(config);
        handle.set_has_public_address(true);
        assert!(handle.has_public_address());

        let toml = toml::to_string_pretty(&handle.snapshot()).unwrap();
        assert!(!toml.contains("has_public_address"));
    }

    #[test]
    fn needs_relay_requires_dual_stack_and_public_address() {
        let mut dual = NodeConfig::default();
        dual.network.ipv4_enabled = true;
        dual.network.ipv6_enabled = true;

        let handle = ConfigHandle::new(dual.clone());
        handle.set_has_public_address(false);
        assert!(handle.needs_relay());

        handle.set_has_public_address(true);
        assert!(!handle.needs_relay());

        dual.network.ipv6_enabled = false;
        let single = ConfigHandle::new(dual);
        single.set_has_public_address(true);
        assert!(single.needs_relay());

        single.set_has_public_address(false);
        assert!(single.needs_relay());
    }

    #[test]
    fn parse_auth_section() {
        let toml = r#"
            [auth]
            network = "myorg"
            [auth.networks.myorg]
            authority = "AAAA"
        "#;
        let node: NodeConfig = toml::from_str(toml).unwrap();
        let handle = ConfigHandle::new(node);
        assert_eq!(handle.auth_network(), "myorg");
        assert_eq!(handle.auth_authority("myorg"), Some("AAAA".to_string()));
        assert_eq!(handle.auth_authority("other"), None);
    }

    #[test]
    fn auth_config_round_trip() {
        let toml = r#"
            [auth]
            network = "myorg"
            [auth.networks.myorg]
            authority = "BBBB"
        "#;
        let node: NodeConfig = toml::from_str(toml).unwrap();
        let s = toml::to_string_pretty(&node).unwrap();
        let back: NodeConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.auth.network, "myorg");
        assert_eq!(back.auth.networks.get("myorg").unwrap().authority, "BBBB");
    }

    #[test]
    fn legacy_config_without_auth_is_default() {
        let toml = r#"
            [node]
            name = "old"
        "#;
        let node: NodeConfig = toml::from_str(toml).unwrap();
        let handle = ConfigHandle::new(node);
        assert_eq!(handle.auth_network(), "none");
        assert!(handle.auth_required_services().is_empty());
    }

    #[test]
    fn default_config_passes_validation() {
        assert_eq!(NodeConfig::default().validate(), Ok(()));
    }

    #[test]
    fn rejects_port_above_u16() {
        let mut config = NodeConfig::default();
        config.network.port = u16::MAX as u32 + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_pq_required_without_transport() {
        let mut config = NodeConfig::default();
        config.crypto.pq_required = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_pq_identity_without_transport() {
        let mut config = NodeConfig::default();
        config.crypto.pq_identity_enabled = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_kademlia_timeout() {
        let mut config = NodeConfig::default();
        config.services.kademlia.query_timeout_seconds = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn provider_record_ttl_defaults() {
        let config = NodeConfig::default();
        assert_eq!(config.services.kademlia.provider_record_ttl_seconds, 3600);
        assert_eq!(
            config
                .services
                .kademlia
                .provider_publication_interval_seconds,
            1800
        );
        assert!(config.validate().is_ok());

        let handle = ConfigHandle::new(config);
        assert_eq!(handle.kademlia_provider_record_ttl(), 3600);
        assert_eq!(handle.kademlia_provider_publication_interval(), 1800);
    }

    #[test]
    fn rejects_zero_provider_record_ttl() {
        let mut config = NodeConfig::default();
        config.services.kademlia.provider_record_ttl_seconds = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_provider_publication_interval() {
        let mut config = NodeConfig::default();
        config
            .services
            .kademlia
            .provider_publication_interval_seconds = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_provider_ttl_not_above_twice_publication() {
        let mut config = NodeConfig::default();
        config
            .services
            .kademlia
            .provider_publication_interval_seconds = 1800;
        config.services.kademlia.provider_record_ttl_seconds = 1800;
        assert!(config.validate().is_err());

        config.services.kademlia.provider_record_ttl_seconds = 3599;
        assert!(config.validate().is_err());

        config.services.kademlia.provider_record_ttl_seconds = 3600;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn provider_record_ttl_config_round_trip() {
        let toml = r#"
            [services.kademlia]
            provider_record_ttl_seconds = 7200
            provider_publication_interval_seconds = 1800
        "#;
        let node: NodeConfig = toml::from_str(toml).unwrap();
        assert_eq!(node.services.kademlia.provider_record_ttl_seconds, 7200);
        assert_eq!(
            node.services.kademlia.provider_publication_interval_seconds,
            1800
        );

        let serialized = toml::to_string_pretty(&node).unwrap();
        let back: NodeConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back.services.kademlia.provider_record_ttl_seconds, 7200);
        assert_eq!(
            back.services.kademlia.provider_publication_interval_seconds,
            1800
        );
    }

    #[test]
    fn provider_lifecycle_defaults_when_absent_from_config() {
        let node: NodeConfig = toml::from_str("[node]\nname = \"old\"\n").unwrap();
        assert_eq!(
            node.services.kademlia.provider_record_ttl_seconds,
            default_provider_record_ttl_seconds()
        );
        assert_eq!(
            node.services.kademlia.provider_publication_interval_seconds,
            default_provider_publication_interval_seconds()
        );
    }

    #[test]
    fn rejects_duplicate_or_zero_port_local_services() {
        let mut config = NodeConfig::default();
        config.services.dispatcher.local_services = vec![
            LocalServiceEntry {
                name: "cmd".to_string(),
                port: 5014,
                require_auth: false,
            },
            LocalServiceEntry {
                name: "cmd".to_string(),
                port: 5015,
                require_auth: false,
            },
        ];
        assert!(config.validate().is_err());

        config.services.dispatcher.local_services[1].name = "ocr".to_string();
        config.services.dispatcher.local_services[1].port = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn registry_max_inflight_dht_defaults_and_validates() {
        let config = NodeConfig::default();
        assert_eq!(config.services.registry.max_inflight_dht, 4);
        assert_eq!(config.services.registry.type_ttl_secs, 1800);
        assert!(config.validate().is_ok());
        assert_eq!(
            ConfigHandle::new(config.clone()).registry_max_inflight_dht(),
            4
        );
        assert_eq!(
            ConfigHandle::new(config.clone()).dispatcher_max_inflight_dht(),
            4
        );

        let mut config = NodeConfig::default();
        config.services.registry.max_inflight_dht = 0;
        assert!(config.validate().is_err());

        config.services.registry.max_inflight_dht = 17;
        assert!(config.validate().is_err());

        config.services.registry.max_inflight_dht = 16;
        assert!(config.validate().is_ok());

        let mut config = NodeConfig::default();
        config.services.dispatcher.max_inflight_dht = 0;
        assert!(config.validate().is_err());
        config.services.dispatcher.max_inflight_dht = 17;
        assert!(config.validate().is_err());
        config.services.dispatcher.max_inflight_dht = 16;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn registry_type_ttl_must_exceed_two_sync_intervals() {
        let mut config = NodeConfig::default();
        config.services.dispatcher.types_sync_secs = 900;
        config.services.registry.type_ttl_secs = 1800;
        assert!(config.validate().is_err());

        config.services.registry.type_ttl_secs = 1801;
        assert!(config.validate().is_ok());

        config.services.registry.type_ttl_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn dispatcher_message_limits_default_parse_and_validate() {
        let config = NodeConfig::default();
        assert_eq!(config.services.dispatcher.max_request_bytes, 16 << 20);
        assert_eq!(config.services.dispatcher.max_response_bytes, 16 << 20);
        assert!(config.validate().is_ok());

        let handle = ConfigHandle::new(config);
        assert_eq!(handle.dispatcher_max_request_bytes(), 16 << 20);
        assert_eq!(handle.dispatcher_max_response_bytes(), 16 << 20);

        let parsed: NodeConfig = toml::from_str(
            r#"
            [services.dispatcher]
            max_request_bytes = 2097152
            max_response_bytes = 1048576
            "#,
        )
        .unwrap();
        assert_eq!(parsed.services.dispatcher.max_request_bytes, 2 << 20);
        assert_eq!(parsed.services.dispatcher.max_response_bytes, 1 << 20);

        let mut invalid = NodeConfig::default();
        invalid.services.dispatcher.max_request_bytes = 0;
        assert!(invalid.validate().is_err());

        invalid.services.dispatcher.max_request_bytes = (64 << 20) + 1;
        assert!(invalid.validate().is_err());

        invalid.services.dispatcher.max_request_bytes = 64 << 20;
        assert!(invalid.validate().is_ok());

        invalid.services.dispatcher.max_response_bytes = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn config_completion_adds_missing_fields_and_keeps_comments() {
        let original = r#"# 保留注释
[node]
name = "自定义"
custom_key = "keep"

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
"#;
        let config: NodeConfig = toml::from_str(original).unwrap();
        let completed = complete_missing_fields(original, &config).expect("should complete");

        assert!(completed.contains("# 保留注释"));
        assert!(completed.contains("custom_key = \"keep\""));
        assert!(completed.contains("query_timeout_secs"));
        assert!(completed.contains("max_request_bytes"));
        assert!(completed.contains("bootstrap_nodes"));
        assert!(completed.contains("name = \"cmd\""));

        let reparsed: NodeConfig = toml::from_str(&completed).unwrap();
        assert_eq!(reparsed.node.name, "自定义");
        assert_eq!(reparsed.services.dispatcher.local_services.len(), 1);
    }

    #[test]
    fn config_completion_is_noop_when_complete() {
        let config = NodeConfig::default();
        let original = toml::to_string_pretty(&config).unwrap();
        assert!(complete_missing_fields(&original, &config).is_none());
    }

    #[test]
    fn from_toml_file_persists_completed_config() {
        let path =
            std::env::temp_dir().join(format!("nexusnet-config-{}.toml", uuid::Uuid::new_v4()));
        fs::write(&path, "[node]\nname = \"x\"\n").unwrap();

        let config = NodeConfig::from_toml_file(&path, true);
        assert_eq!(config.node.name, "x");

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("name = \"x\""));
        assert!(saved.contains("max_request_bytes"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bootstrap_provider_auto_discovery_is_opt_in() {
        assert!(!NodeConfig::default().node.accept_bootstrap_providers);
        let config: NodeConfig =
            toml::from_str("[node]\naccept_bootstrap_providers = true\n").unwrap();
        assert!(config.node.accept_bootstrap_providers);
    }
}
