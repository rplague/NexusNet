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

use crate::paths;
use crate::{LogLevel, LogStruct};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use std::collections::HashMap;
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
default_u32_fn!(default_relay_retry_interval, 60);
default_u32_fn!(default_relay_max_failures, 3);
default_u32_fn!(default_types_sync_seconds, 300);

fn default_auth_network() -> String {
    "none".to_string()
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
}

impl NodeConfig {
    fn validate(&self) -> Result<(), String> {
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
        if self.services.dispatcher.enabled {
            if self.services.dispatcher.query_timeout_secs == 0 {
                return Err("services.dispatcher.query_timeout_secs 必须大于 0".to_string());
            }
            if self.services.dispatcher.types_sync_secs == 0 {
                return Err("services.dispatcher.types_sync_secs 必须大于 0".to_string());
            }
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

    /// Always returns a valid `NodeConfig`.
    fn from_toml_file(path: impl AsRef<Path>, create_if_missing: bool) -> NodeConfig {
        let path = path.as_ref();

        // If creation is not requested and the file is missing, return defaults.
        if !create_if_missing && !path.exists() {
            LogStruct::new(
                LogLevel::Warning,
                "配置文件不存在",
                "配置文件不存在且未要求创建，使用默认配置",
            )
            .emit();
            return NodeConfig::default();
        }

        // Try to read and parse the file.
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<NodeConfig>(&content) {
                Ok(config) => {
                    if let Err(e) = config.validate() {
                        LogStruct::new(LogLevel::Critical, "配置校验失败", e).emit();
                        std::process::exit(1);
                    }
                    return config;
                }
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

        // Reached here when the file is missing or was renamed after a parse failure.
        // Create the default config and write it to disk.
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
    /// Whether this node may act as a bootstrap node when dual-stack (publishes
    /// a DHT provider record).
    #[serde(default = "bool_true")]
    pub allow_bootstrap: bool,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            name: default_name(),
            description: default_description(),
            allow_bootstrap: bool_true(),
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
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            ipv4_enabled: bool_false(),
            ipv4_address: None,
            ipv6_enabled: bool_false(),
            ipv6_address: None,
            port: default_port(),
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
    pub relay: RelayService,
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
    #[serde(default = "Vec::new")]
    pub local_services: Vec<LocalServiceEntry>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        DispatcherConfig {
            enabled: bool_true(),
            query_timeout_secs: default_query_timeout_seconds(),
            types_sync_secs: default_types_sync_seconds(),
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
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AuthNetworkConfig {
    /// Authority ed25519 public key (base64), parsed by the `auth` module.
    #[serde(default)]
    pub authority: String,
}

impl Default for AuthNetworkConfig {
    fn default() -> Self {
        AuthNetworkConfig {
            authority: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<NodeConfig>>,
}

/// Config file path, resolved once via `paths::fail_path_gener`.
fn config_filepath() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| paths::fail_path_gener(paths::FailType::Config, "config.toml"))
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
        Self::from_toml_file(config_filepath(), true)
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

    /// Save to the default config path (`NEXUSNET_CONFIG_PATH`).
    pub fn save_to_default(&self) {
        self.save_to_file(config_filepath());
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

    /// Interval, in seconds, between global service-type index reconciliations.
    pub fn dispatcher_types_sync_interval(&self) -> u32 {
        self.read().services.dispatcher.types_sync_secs.max(1)
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

    pub fn kademlia_replication_factor(&self) -> u32 {
        self.read().services.kademlia.replication_factor
    }

    pub fn kademlia_query_timeout(&self) -> u32 {
        self.read().services.kademlia.query_timeout_seconds
    }

    pub fn bootstrap_nodes(&self) -> Vec<Multiaddr> {
        self.read().services.kademlia.bootstrap_nodes.clone()
    }

    /// External addresses (`<ip>/tcp/<port>/p2p/<peer_id>`) derived from the
    /// configured IPv4/IPv6 addresses.
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

    // ========== Convenience setters ==========
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

    /// Set whether a local service requires authentication; returns true if it
    /// was found and actually changed.
    pub fn set_service_require_auth(&self, name: &str, require_auth: bool) -> bool {
        let mut cfg = self.write();
        match cfg
            .services
            .dispatcher
            .local_services
            .iter_mut()
            .find(|s| s.name == name)
        {
            Some(svc) => {
                let changed = svc.require_auth != require_auth;
                svc.require_auth = require_auth;
                changed
            }
            None => false,
        }
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
            format!("/ip4/1.2.3.4/tcp/5000/p2p/{pid}")
        );
        assert_eq!(
            addrs[1].to_string(),
            format!("/ip6/2001:db8::1/tcp/5000/p2p/{pid}")
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
}
