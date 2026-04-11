use crate::{
    LogStruct,
    LogLevel,
    get_network_addresses,
    get_key
};
use std::fs;
use std::path::Path;
use std::io::ErrorKind;
use serde_json::from_str;

use libp2p::{Multiaddr, PeerId};

// 定义配置结构体
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NodeConfig {
    pub node: NodeInfo,
    pub network: NetworkConfig,
    pub services: ServicesConfig,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct NetworkConfig {
    pub ipv4_enabled: bool,
    pub ipv4_address: String,
    pub ipv6_enabled: bool,
    pub ipv6_address: String,
    pub port: u16,
    pub announce_addresses: Vec<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ServicesConfig {
    pub ping: PingService,
    pub kademlia: KademliaService,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct PingService {
    pub enabled: bool,
    pub interval_secs: u32,
    pub with_timeout: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct KademliaService {
    pub enabled: bool,
    pub record_ttl_seconds: u64,
    pub replication_factor: usize,
    pub query_timeout_seconds: u64,
    pub bootstrap_nodes: Vec<String>,
}

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
                    // let log = LogStruct {
                    //     level: LogLevel::Debug,
                    //     topic: "添加引导节点".to_string(),
                    //     content: format!("添加新节点 {}", new_peer_id),
                    // };
                    // log.logout();
                }
            }
        } else {
            return;
        }
        
        // 保存到配置文件
        let json_string = serde_json::to_string_pretty(self).unwrap();
        fs::write("./config.json", json_string).unwrap();
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

pub fn create_new_config_file() -> Result<NodeConfig, Box<dyn std::error::Error>> {
    let config_path = Path::new("./config.json");
    
    let (ipv4_address, ipv6_address) = get_network_addresses()?;
    let keypair = get_key()?;
    let config = NodeConfig {
        node: NodeInfo {
            name: "未设置的p2p节点".to_string(),
            description: "无详细描述".to_string(),
        },
        network: NetworkConfig {
            ipv4_enabled: !ipv4_address.is_empty(),
            ipv4_address: if !ipv4_address.is_empty() { 
                ipv4_address.clone() 
            } else { 
                "不可用".to_string() 
            },
            ipv6_enabled: !ipv6_address.is_empty(),
            ipv6_address: if !ipv6_address.is_empty() { 
                ipv6_address .clone() 
            } else { 
                "不可用".to_string() 
            },
            port: 5000,
            announce_addresses: if !ipv4_address.is_empty() {
                if !ipv6_address.is_empty() {
                    vec![format!("/ip4/{}/tcp/5000/p2p/{}", ipv4_address, PeerId::from(keypair.public())), 
                         format!("/ip6/{}/tcp/5000/p2p/{}", ipv6_address, PeerId::from(keypair.public()))]
                }else{
                    vec![format!("/ip4/{}/tcp/5000/p2p/{}", ipv4_address, PeerId::from(keypair.public()))]
                }
            } else if !ipv6_address.is_empty() {
                    vec![format!("/ip6/{}/tcp/5000/p2p/{}", ipv6_address, PeerId::from(keypair.public()))]
            } else {
                vec![]
            },
        },
        services: ServicesConfig {
            ping: PingService {
                enabled: true,
                interval_secs: 15,
                with_timeout: 10,
            },
            kademlia: KademliaService {
                enabled: true,
                record_ttl_seconds: 3600, // 1小时
                replication_factor: 20,
                query_timeout_seconds: 60,
                bootstrap_nodes: vec![],
            },
        },
    };
    
    let json_string = serde_json::to_string_pretty(&config)?;
    fs::write(config_path, json_string)?;
    
    Ok(config)
}
pub fn read_config_file() -> Result<NodeConfig, Box<dyn std::error::Error>> {
    let path = Path::new("./config.json");
    
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
                match from_str::<NodeConfig>(&content) {
                    Ok(mut config) => {
                        // 成功解析后，更新网络地址信息
                        init_update_config(&mut config);
                        
                        let json_string = serde_json::to_string_pretty(&config)?;
                        fs::write("./config.json", json_string)?;
                        
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

