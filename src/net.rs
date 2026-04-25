use std::net::IpAddr;
use std::fs;
use crate::config::NodeConfig;
use crate::service_discovery::ServiceDiscovery;
use libp2p::multiaddr::Protocol;
use libp2p::{
    kad,
    identity,
    ping,
    swarm::NetworkBehaviour,
    identify,
    StreamProtocol,
    Multiaddr,
    PeerId,
};

// 导入Kademlia相关类型
use libp2p::kad::store::MemoryStore;

use crate::addr_watcher;

// 定义组合行为
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NetBehaviourEvent")]
pub struct NetBehaviour {
    pub ping: ping::Behaviour,
    pub identify: identify::Behaviour,
    pub kademlia: kad::Behaviour<MemoryStore>,
    pub addr_watcher: addr_watcher::Behaviour,
}

// 定义行为事件枚举
#[derive(Debug)]
pub enum NetBehaviourEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kademlia(kad::Event),
    AddrWatcher(addr_watcher::AddrWatcherEvent),
}

// 实现 From trait 用于事件转换
impl From<ping::Event> for NetBehaviourEvent {
    fn from(event: ping::Event) -> Self {
        NetBehaviourEvent::Ping(event)
    }
}

impl From<identify::Event> for NetBehaviourEvent {
    fn from(event: identify::Event) -> Self {
        NetBehaviourEvent::Identify(event)
    }
}

impl From<kad::Event> for NetBehaviourEvent {
    fn from(event: kad::Event) -> Self {
        NetBehaviourEvent::Kademlia(event)
    }
}

impl From<addr_watcher::AddrWatcherEvent> for NetBehaviourEvent {
    fn from(event: addr_watcher::AddrWatcherEvent) -> Self {
        NetBehaviourEvent::AddrWatcher(event)
    }
}

pub fn get_network_addresses() -> Result<(String, String), Box<dyn std::error::Error>> {
    let mut ipv4_addresses = String::new();
    let mut ipv6_addresses = String::new();

    // 获取所有网络接口的IP地址
    for iface in get_if_addrs::get_if_addrs()? {
        let addr = iface.addr;
        
        match addr.ip() {
            IpAddr::V4(ipv4) => {
                // 过滤掉本地回环和私有地址
                if !ipv4.is_loopback() && !ipv4.is_private() && !ipv4.is_link_local() {
                    ipv4_addresses = ipv4.to_string();
                }
            }
            IpAddr::V6(ipv6) => {
                // 过滤掉本地回环、本地链路和唯一本地地址
                if !(ipv6.is_loopback() 
                  || ipv6.is_unspecified()
                  || ipv6.is_multicast()
                  || ipv6.octets()[0] == 0xfe || (ipv6.octets()[1] & 0xc0) == 0x80 // 本地链路地址 fe80::/10
                  || (ipv6.octets()[0] == 0xfc || ipv6.octets()[0] == 0xfd)) // 唯一本地地址 fc00::/7
                {
                    ipv6_addresses = ipv6.to_string();
                }
            }
        }
    }

    Ok((ipv4_addresses, ipv6_addresses))
}

pub fn appropriate_address_filter(
    listen_addrs: &[Multiaddr],
    config: &NodeConfig,
) -> Option<Multiaddr> {
    // 定义优先级：如果两者都启用，IPv4优先
    let preferred_order = if config.network.ipv4_enabled && config.network.ipv6_enabled {
        vec![Protocol::Ip4([0, 0, 0, 0].into()), Protocol::Ip6([0, 0, 0, 0, 0, 0, 0, 0].into())]
    } else if config.network.ipv4_enabled {
        vec![Protocol::Ip4([0, 0, 0, 0].into())]
    } else if config.network.ipv6_enabled {
        vec![Protocol::Ip6([0, 0, 0, 0, 0, 0, 0, 0].into())]
    } else {
        return None; // 两者都禁用
    };

    // 按优先级查找地址
    for protocol_type in preferred_order {
        for addr in listen_addrs {
            if addr.iter().any(|proto| {
                matches!(proto, Protocol::Ip4(_)) && matches!(protocol_type, Protocol::Ip4(_)) ||
                matches!(proto, Protocol::Ip6(_)) && matches!(protocol_type, Protocol::Ip6(_))
            }) {
                return Some(addr.clone());
            }
        }
    }
    
    None
}

pub fn get_key() -> Result<identity::Keypair, Box<dyn std::error::Error>> {
    let keypair = if std::path::Path::new("keypair.bin").exists() {
        let bytes = fs::read("keypair.bin")?;
        identity::Keypair::from_protobuf_encoding(&bytes)?
    } else {
        let keypair = identity::Keypair::generate_ed25519();
        let encoded = keypair.to_protobuf_encoding()?;
        fs::write("keypair.bin", &encoded)?;
        keypair
    };
    
    Ok(keypair)
}

pub fn create_behaviour(
    keypair: &identity::Keypair,
    protocol_name: &str,
    config: &NodeConfig,
) -> Result<NetBehaviour, Box<dyn std::error::Error>> {
    // Ping 配置
    let ping_config = ping::Config::new()
        .with_interval(std::time::Duration::from_secs(15))
        .with_timeout(std::time::Duration::from_secs(10));

    // Identify 配置
    let identify_config = identify::Config::new(
        format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")),
        keypair.public(),
    ).with_agent_version(format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")));
    
    // Kademlia 配置
    let store = MemoryStore::new(keypair.public().to_peer_id());
    let mut kademlia_config = kad::Config::new(libp2p::StreamProtocol::new("/ipfs/kad/1.0.0"));
    kademlia_config.set_record_ttl(Some(std::time::Duration::from_secs(3600)));
    kademlia_config.set_periodic_bootstrap_interval(Some(std::time::Duration::from_secs(20)));
    let mut kademlia = kad::Behaviour::with_config(keypair.public().to_peer_id(), store, kademlia_config);
    kademlia.set_mode(Some(kad::Mode::Server));
    // 创建组合行为
    let behaviour = NetBehaviour {
        ping: ping::Behaviour::new(ping_config),
        identify: identify::Behaviour::new(identify_config),
        kademlia,
        addr_watcher: addr_watcher::Behaviour::new(&config.services.address_watcher),
    };
    
    Ok(behaviour)
}

// 节点连接列表构建

// 节点连接状态
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Connected,        // 已连接
    Disconnected,     // 未连接
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    // 基础信息
    pub peer_id: PeerId,
    // 化名
    pub name_string: Option<String>,
    // 本机实际观测到的地址
    pub observed_addresses: Option<Multiaddr>,
    // 连接的对方地址
    pub addresses: Option<Multiaddr>,
    // 公钥
    pub public_key: Option<identity::PublicKey>,
    // 延迟
    pub rtt: Option<std::time::Duration>,
    // 连接状态
    pub connection_status: ConnectionStatus,
    // 节点支持的协议
    pub supported_protocols: Option<Vec<StreamProtocol>>,
    // 版本
    pub agent_version: Option<String>,
    // 节点评分或信誉值
    pub score: Option<i32>,
    // 其他自定义标签
    pub tags: Option<Vec<String>>,
}