use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket, SocketAddr};
use std::fs;
use std::time::Duration;

use libp2p::{
    PeerId,
    Multiaddr,
    identity,
    ping,
    swarm::NetworkBehaviour,
    identify,
    StreamProtocol,
};
// 定义组合行为
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NetBehaviourEvent")]
pub struct NetBehaviour {
    pub ping: ping::Behaviour,
    pub identify: identify::Behaviour,
}

// 定义行为事件枚举
#[derive(Debug)]
pub enum NetBehaviourEvent {
    Ping(ping::Event),
    Identify(identify::Event),
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

    // 如果没有找到公网IP，尝试通过外部服务获取
    if ipv4_addresses.is_empty() && let Ok(public_ip) = get_public_ipv4() {
        ipv4_addresses = public_ip;
    }
    
    if ipv6_addresses.is_empty() && let Ok(public_ip) = get_public_ipv6() {
        ipv6_addresses = public_ip;
    }

    Ok((ipv4_addresses, ipv6_addresses))
}

// 通过外部服务获取公网IPv4
fn get_public_ipv4() -> Result<String, Box<dyn std::error::Error>> {
    let services = [
        "https://api.ipify.org",
        "https://checkip.amazonaws.com",
        "https://icanhazip.com",
    ];

    for service in services {
        match reqwest::blocking::get(service) {
            Ok(response) => {
                if let Ok(ip) = response.text() {
                    let ip = ip.trim().to_string();
                    // 验证是否为有效的IP地址
                    if ip.parse::<Ipv4Addr>().is_ok() || ip.parse::<Ipv6Addr>().is_ok() {
                        return Ok(ip);
                    }
                }
            }
            Err(_) => continue,
        }
    }

    Err("无法获取公网IP".into())
}

fn get_public_ipv6() -> Result<String, Box<dyn std::error::Error>> {
    let services = [
        "https://api6.ipify.org",          // ipify 的 IPv6 专用端点
        "https://icanhazip.com",           // 支持 IPv6
        "https://checkip.amazonaws.com",   // 支持 IPv6
        "https://v6.ident.me",             // IPv6 专用
        "https://ipv6.seeip.org",          // IPv6 专用
    ];

    for service in services {
        match reqwest::blocking::get(service) {
            Ok(response) => {
                if let Ok(ip) = response.text() {
                    let ip = ip.trim().to_string();
                    
                    // 优先验证 IPv6，如果是 IPv4 则跳过
                    if ip.parse::<Ipv6Addr>().is_ok() {
                        return Ok(ip);
                    }
                    // 如果是 IPv4，继续尝试其他服务获取 IPv6
                    else if ip.parse::<Ipv4Addr>().is_ok() {
                        continue; // 跳过 IPv4 地址，继续寻找 IPv6
                    }
                }
            }
            Err(_) => continue,
        }
    }

    Err("无法获取公网IPv6地址".into())
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
) -> Result<NetBehaviour, Box<dyn std::error::Error>> {
    
    // 创建 Identify 配置
    let identify_config = identify::Config::new(
        format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")),
        keypair.public(),
    )
    .with_agent_version(format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")));
    
    // 创建组合行为
    let behaviour = NetBehaviour {
        ping: ping::Behaviour::new(ping::Config::new().with_interval(std::time::Duration::from_secs(20))), 
        // ping: ping::Behaviour::new(ping::Config::new()), 
        identify: identify::Behaviour::new(identify_config),
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

