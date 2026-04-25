use crate::{
    LogStruct,
    LogLevel,
    get_network_addresses,
    get_key
};
use std::fs;
use std::path::Path;
use std::io::ErrorKind;

use libp2p::{Multiaddr, PeerId};

// 定义配置结构体
//
// 所有服务级别的配置结构体都实现了 Default + #[serde(default)]，
// 这样 config.toml 中只需要写用户想覆写的字段，其余自动使用默认值。
// 只有 NodeConfig 本身是必填的根节点。

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NodeConfig {
    pub node: NodeInfo,
    pub network: NetworkConfig,
    #[serde(default)]
    pub services: ServicesConfig,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NetworkConfig {
    #[serde(default = "default_true")]
    pub ipv4_enabled: bool,
    #[serde(default = "default_ip_unavailable")]
    pub ipv4_address: String,
    #[serde(default = "default_false")]
    pub ipv6_enabled: bool,
    #[serde(default = "default_ip_unavailable")]
    pub ipv6_address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub announce_addresses: Vec<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ServicesConfig {
    #[serde(default)]
    pub ping: PingService,
    #[serde(default)]
    pub kademlia: KademliaService,
    #[serde(default)]
    pub service_discovery: ServiceDiscoveryConfig,
    #[serde(default)]
    pub dispatcher: DispatcherConfig,
    #[serde(default)]
    pub address_watcher: AddressWatcherConfig,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        ServicesConfig {
            ping: PingService::default(),
            kademlia: KademliaService::default(),
            service_discovery: ServiceDiscoveryConfig::default(),
            dispatcher: DispatcherConfig::default(),
            address_watcher: AddressWatcherConfig::default(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AddressWatcherConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 地址检测间隔（秒）
    #[serde(default = "default_addr_watch_interval")]
    pub check_interval_secs: u64,
}

impl Default for AddressWatcherConfig {
    fn default() -> Self {
        AddressWatcherConfig {
            enabled: true,
            check_interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceDiscoveryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 本节点提供的服务列表
    #[serde(default)]
    pub services: Vec<String>,
    /// 查询超时（秒）
    #[serde(default = "default_sd_query_timeout")]
    pub query_timeout_secs: u64,
    /// 宣告记录的TTL（秒）
    #[serde(default = "default_sd_ttl")]
    pub record_ttl_secs: u64,
}

impl Default for ServiceDiscoveryConfig {
    fn default() -> Self {
        ServiceDiscoveryConfig {
            enabled: true,
            services: vec![],
            query_timeout_secs: 30,
            record_ttl_secs: 1800,
        }
    }
}

/// 本地服务调度器配置
/// 每个服务是一个独立进程，监听本地回环地址的某个端口。
/// 通信层收到外部请求后，根据 service=xxx 字段将请求转发到对应端口。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DispatcherConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// 本地服务的路由表: 服务名 → 后端地址
    #[serde(default)]
    pub local_services: Vec<LocalServiceEntry>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        DispatcherConfig {
            enabled: false,
            local_services: vec![],
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LocalServiceEntry {
    /// 服务标识名，如 "ocr"、"cold-storage"
    pub name: String,
    /// 后端进程监听地址
    pub host: String,
    /// 后端进程监听端口
    pub port: u16,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct PingService {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ping_interval")]
    pub interval_secs: u32,
    #[serde(default = "default_ping_timeout")]
    pub with_timeout: u32,
}

impl Default for PingService {
    fn default() -> Self {
        PingService {
            enabled: true,
            interval_secs: 15,
            with_timeout: 10,
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct KademliaService {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_kad_ttl")]
    pub record_ttl_seconds: u64,
    #[serde(default = "default_kad_replication")]
    pub replication_factor: usize,
    #[serde(default = "default_kad_query_timeout")]
    pub query_timeout_seconds: u64,
    #[serde(default)]
    pub bootstrap_nodes: Vec<String>,
}

impl Default for KademliaService {
    fn default() -> Self {
        KademliaService {
            enabled: true,
            record_ttl_seconds: 3600,
            replication_factor: 20,
            query_timeout_seconds: 60,
            bootstrap_nodes: vec![],
        }
    }
}

// ─── 默认值辅助函数 ─────────────────────────────────────────

fn default_true() -> bool { true }
fn default_false() -> bool { false }
fn default_port() -> u16 { 5000 }
fn default_ip_unavailable() -> String { "不可用".to_string() }
fn default_ping_interval() -> u32 { 15 }
fn default_ping_timeout() -> u32 { 10 }
fn default_kad_ttl() -> u64 { 3600 }
fn default_kad_replication() -> usize { 20 }
fn default_kad_query_timeout() -> u64 { 60 }
fn default_sd_query_timeout() -> u64 { 30 }
fn default_sd_ttl() -> u64 { 1800 }
fn default_addr_watch_interval() -> u64 { 60 }

// ─── 节点管理 ───────────────────────────────────────────────

impl NodeConfig {
    pub fn insert_bootstrap_nodes(&mut self, info: String) {
        // 从info中提取peer_id
        let new_peer_id = extract_peer_id_from_multiaddr(&info);
        
        if let Some(new_peer_id) = new_peer_id {
            // 查找是否已存在相同peer_id的节点
            let existing_index = self.services.kademlia.bootstrap_nodes.iter()
                .position(|node| {
                    if let Some(existing_peer_id) = extract_peer_id_from_multiaddr(node) {
                        existing_peer_id == new_peer_id
                    } else {
                        false
                    }
                });
            
            match existing_index {
                Some(index) => {
                    // 替换现有节点的地址
                    self.services.kademlia.bootstrap_nodes[index] = info;
                }
                None => {
                    // 添加新节点
                    self.services.kademlia.bootstrap_nodes.push(info);
                }
            }
        } else {
            return;
        }
        
        // 保存到配置文件
        let toml_string = toml::to_string_pretty(self).unwrap();
        fs::write("./config.toml", toml_string).unwrap();
    }
}

// 辅助函数：从Multiaddr中提取PeerId
fn extract_peer_id_from_multiaddr(addr: &str) -> Option<String> {
    // 尝试解析为Multiaddr
    if let Ok(multiaddr) = addr.parse::<Multiaddr>() {
        // 遍历协议组件，查找p2p部分
        for protocol in multiaddr.iter() {
            if let libp2p::multiaddr::Protocol::P2p(peer_id) = protocol {
                return Some(peer_id.to_string());
            }
        }
    }
    None
}

// ─── 配置文件创建与读取 ────────────────────────────────────

pub fn create_new_config_file() -> Result<NodeConfig, Box<dyn std::error::Error>> {
    let config_path = Path::new("./config.toml");
    
    let (ipv4_address, ipv6_address) = get_network_addresses()?;
    let keypair = get_key()?;
    let peer_id = PeerId::from(keypair.public());
    let has_v4 = !ipv4_address.is_empty();
    let has_v6 = !ipv6_address.is_empty();

    let mut announce = Vec::new();
    if has_v4 {
        announce.push(format!("/ip4/{}/tcp/5000/p2p/{}", ipv4_address, peer_id));
    }
    if has_v6 {
        announce.push(format!("/ip6/{}/tcp/5000/p2p/{}", ipv6_address, peer_id));
    }

    let config = NodeConfig {
        node: NodeInfo {
            name: "未设置的p2p节点".to_string(),
            description: "无详细描述".to_string(),
        },
        network: NetworkConfig {
            ipv4_enabled: has_v4,
            ipv4_address: if has_v4 { ipv4_address.clone() } else { "不可用".to_string() },
            ipv6_enabled: has_v6,
            ipv6_address: if has_v6 { ipv6_address } else { "不可用".to_string() },
            port: 5000,
            announce_addresses: announce,
        },
        services: ServicesConfig::default(),
    };
    
    let toml_string = toml::to_string_pretty(&config)?;
    fs::write(config_path, toml_string)?;
    
    Ok(config)
}

pub fn read_config_file() -> Result<NodeConfig, Box<dyn std::error::Error>> {
    let path = Path::new("./config.toml");
    
    match fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                let log = LogStruct {
                    level: LogLevel::Warning,
                    topic: "设置文件异常".to_string(),
                    content: "配置文件为空，自动创建新的设置文件".to_string(),
                };
                log.logout();
                
                match create_new_config_file() {
                    Ok(config) => Ok(config),
                    Err(e) => {
                        let log = LogStruct {
                            level: LogLevel::Critical,
                            topic: "创建配置文件失败".to_string(),
                            content: e.to_string(),
                        };
                        log.logout();
                        Err(e)
                    }
                }
            } else {
                // 解析配置文件
                match toml::from_str::<NodeConfig>(&content) {
                    Ok(mut config) => {
                        // 成功解析后，更新网络地址信息
                        init_update_config(&mut config);
                        
                        let toml_string = toml::to_string_pretty(&config)?;
                        fs::write("./config.toml", toml_string)?;
                        
                        Ok(config)
                    }
                    Err(e) => {
                        let log = LogStruct {
                            level: LogLevel::Warning,
                            topic: "配置文件格式错误".to_string(),
                            content: format!("解析失败: {}，尝试创建新配置", e),
                        };
                        log.logout();
                        
                        // 格式错误，尝试创建新配置
                        create_new_config_file()
                    }
                }
            }
        }
        
        Err(error) => match error.kind() {
            ErrorKind::NotFound => {
                let log = LogStruct {
                    level: LogLevel::Warning,
                    topic: "设置文件不存在".to_string(),
                    content: "自动创建新的设置文件".to_string(),
                };
                log.logout();
                
                match create_new_config_file() {
                    Ok(config) => Ok(config),
                    Err(e) => {
                        let log = LogStruct {
                            level: LogLevel::Critical,
                            topic: "创建配置文件失败".to_string(),
                            content: e.to_string(),
                        };
                        log.logout();
                        Err(e)
                    }
                }
            }
            ErrorKind::PermissionDenied => {
                let log = LogStruct {
                    level: LogLevel::Critical,
                    topic: "权限不足".to_string(),
                    content: "无法读取设置文件！".to_string(),
                };
                log.logout();
                Err(Box::new(std::io::Error::new(
                    ErrorKind::PermissionDenied,
                    "权限不足，无法读取配置文件"
                )))?
            }
            _ => {
                let log = LogStruct {
                    level: LogLevel::Critical,
                    topic: "读取配置文件失败".to_string(),
                    content: error.to_string(),
                };
                log.logout();
                Err(Box::new(error))?
            }
        },
    }
}

fn init_update_config(config: &mut NodeConfig) {
    let keypair = get_key().unwrap();
    let (ipv4_address, ipv6_address) = get_network_addresses().unwrap();
    
    let peer_id = PeerId::from(keypair.public());
    let has_ipv4 = !ipv4_address.is_empty();
    let has_ipv6 = !ipv6_address.is_empty();

    config.network.ipv4_enabled = has_ipv4;
    config.network.ipv4_address = if has_ipv4 { 
        ipv4_address.clone() 
    } else { 
        "不可用".to_string() 
    };
    
    config.network.ipv6_enabled = has_ipv6;
    config.network.ipv6_address = if has_ipv6 { 
        ipv6_address.clone() 
    } else { 
        "不可用".to_string() 
    };
    
    let mut announce_addresses = Vec::new();
    
    if has_ipv4 {
        announce_addresses.push(format!("/ip4/{}/tcp/{}/p2p/{}", 
            ipv4_address, config.network.port, peer_id));
    }
    if has_ipv6 {
        announce_addresses.push(format!("/ip6/{}/tcp/{}/p2p/{}", 
            ipv6_address, config.network.port, peer_id));
    }
    
    config.network.announce_addresses = announce_addresses;
}
