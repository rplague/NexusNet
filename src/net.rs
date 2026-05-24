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

use crate::service_protocol;
use crate::{LogLevel, LogStruct, config::ConfigHandle};
use libp2p::request_response::{self, cbor};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, identify, identity, kad, noise, ping,
    swarm::NetworkBehaviour, tcp, yamux,
};
use std::{
    fs, io,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
pub struct KeyManager {
    keypair: identity::Keypair,
    path: PathBuf,
    pq_secret_key: Option<Vec<u8>>,
    pq_public_key: Option<Vec<u8>>,
    pq_path: PathBuf,
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
    pub fn has_pq_keys(&self) -> bool {
        self.pq_secret_key.is_some() && self.pq_public_key.is_some()
    }

    /// 获取 PQ 公钥
    pub fn pq_public_key(&self) -> Option<&[u8]> {
        self.pq_public_key.as_deref()
    }

    /// 获取 PQ 私钥
    pub fn pq_secret_key(&self) -> Option<&[u8]> {
        self.pq_secret_key.as_deref()
    }

    /// 保存 PQ 密钥到 sidecar 文件
    pub fn save_pq_keys(&mut self, secret: Vec<u8>, public: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
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
        let public_len = u32::from_be_bytes(bytes[4 + secret_len..8 + secret_len].try_into().unwrap()) as usize;
        if 8 + secret_len + public_len > bytes.len() {
            return (None, None);
        }
        let public = bytes[8 + secret_len..8 + secret_len + public_len].to_vec();
        (Some(secret), Some(public))
    }
}

/// 获取本机所有公网 IP
pub fn get_public_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    if let Ok(ifaces) = get_if_addrs::get_if_addrs() {
        for iface in ifaces {
            let ip = iface.addr.ip();
            match ip {
                IpAddr::V4(v4) => {
                    if !v4.is_loopback() && !v4.is_private() && !v4.is_link_local() {
                        ips.push(IpAddr::V4(v4));
                    }
                }
                IpAddr::V6(v6) => {
                    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                        continue;
                    }
                    let octets = v6.octets();
                    if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
                        continue;
                    }
                    if octets[0] == 0xfc || octets[0] == 0xfd {
                        continue;
                    }
                    ips.push(IpAddr::V6(v6));
                }
            }
        }
    }
    ips
}

/// 将 IP + 端口转换为 Multiaddr
pub fn to_multiaddr(ip: IpAddr, port: u16) -> Multiaddr {
    let mut addr = Multiaddr::empty();
    match ip {
        IpAddr::V4(v4) => {
            addr.push(libp2p::multiaddr::Protocol::Ip4(v4));
        }
        IpAddr::V6(v6) => {
            addr.push(libp2p::multiaddr::Protocol::Ip6(v6));
        }
    }
    addr.push(libp2p::multiaddr::Protocol::Tcp(port));
    addr
}

pub fn update_config_with_public_ip(
    config: &ConfigHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let public_ips = get_public_ips();
    let ipv4_addrs: Vec<IpAddr> = public_ips
        .iter()
        .filter(|ip| ip.is_ipv4())
        .copied()
        .collect();
    let ipv6_addrs: Vec<IpAddr> = public_ips
        .iter()
        .filter(|ip| ip.is_ipv6())
        .copied()
        .collect();

    if ipv4_addrs.is_empty() && ipv6_addrs.is_empty() {
        LogStruct::new(LogLevel::Warning, "未发现公网IP", "无法自动更新网络配置").emit();
        return Ok(());
    }

    let port = config.listen_port();
    let mut announce_addrs = Vec::new();
    for ip in &ipv4_addrs {
        announce_addrs.push(to_multiaddr(*ip, port as u16));
    }
    for ip in &ipv6_addrs {
        announce_addrs.push(to_multiaddr(*ip, port as u16));
    }

    {
        let cfg = config.read();
        let current_ipv4 = cfg.network.ipv4_address;
        let current_ipv6 = cfg.network.ipv6_address;
        let current_announce = &cfg.network.announce_addresses;

        if current_ipv4 == ipv4_addrs.first().copied()
            && current_ipv6 == ipv6_addrs.first().copied()
            && current_announce == &announce_addrs
        {
            return Ok(());
        }
    }

    {
        let mut cfg = config.write();
        if !ipv4_addrs.is_empty() {
            cfg.network.ipv4_enabled = true;
            cfg.network.ipv4_address = ipv4_addrs.first().copied();
        } else {
            cfg.network.ipv4_enabled = false;
            cfg.network.ipv4_address = None;
        }
        if !ipv6_addrs.is_empty() {
            cfg.network.ipv6_enabled = true;
            cfg.network.ipv6_address = ipv6_addrs.first().copied();
        } else {
            cfg.network.ipv6_enabled = false;
            cfg.network.ipv6_address = None;
        }
        cfg.network.announce_addresses = announce_addrs.clone();
    }

    // 原子保存到文件
    config.save_to_default();
    LogStruct::new(
        LogLevel::Preset,
        "网络配置已自动更新",
        format!(
            "IPv4: {:?}, IPv6: {:?}, 公告地址: {:?}",
            ipv4_addrs, ipv6_addrs, announce_addrs
        ),
    )
    .emit();
    Ok(())
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NetBehaviourEvent")]
pub struct NetBehaviour {
    pub ping: ping::Behaviour,
    pub identify: identify::Behaviour,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    pub service_req: cbor::Behaviour<service_protocol::Request, service_protocol::Response>,
}

#[derive(Debug)]
pub enum NetBehaviourEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kademlia(kad::Event),
    ServiceReq(request_response::Event<service_protocol::Request, service_protocol::Response>),
}

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
impl From<request_response::Event<service_protocol::Request, service_protocol::Response>>
    for NetBehaviourEvent
{
    fn from(
        event: request_response::Event<service_protocol::Request, service_protocol::Response>,
    ) -> Self {
        NetBehaviourEvent::ServiceReq(event)
    }
}

impl NetBehaviour {
    pub fn new(config: &ConfigHandle, keypair: &identity::Keypair) -> Self {
        // Ping 配置
        let ping_config = ping::Config::new()
            .with_interval(Duration::from_secs(config.ping_interval().into()))
            .with_timeout(Duration::from_secs(config.ping_timeout().into()));
        let ping = ping::Behaviour::new(ping_config);

        // Identify 配置
        let protocol_name = "/oahd";
        let identify_config = identify::Config::new(
            format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")),
            keypair.public(),
        )
        .with_agent_version(format!("{}/{}", protocol_name, env!("CARGO_PKG_VERSION")))
        .with_push_listen_addr_updates(true);
        let identify = identify::Behaviour::new(identify_config);

        // Kademlia 配置
        let store = kad::store::MemoryStore::new(keypair.public().to_peer_id());
        let mut kad_config = kad::Config::new(StreamProtocol::new("/ipfs/kad/1.0.0"));
        kad_config.set_record_ttl(Some(Duration::from_secs(
            config.kademlia_record_ttl().into(),
        )));
        kad_config.set_query_timeout(Duration::from_secs(config.kademlia_query_timeout().into()));
        let mut kademlia =
            kad::Behaviour::with_config(keypair.public().to_peer_id(), store, kad_config);
        kademlia.set_mode(Some(kad::Mode::Server));

        let service_req = service_protocol::new_service_req_behaviour();
        Self {
            ping,
            identify,
            kademlia,
            service_req,
        }
    }
}

// let mut swarm = SwarmBuilder::with_existing_identity(keypair)
//         .with_tokio()
//         .with_tcp(
//             tcp::Config::default(),
//             noise::Config::new,
//             yamux::Config::default,
//         )?
//         .with_behaviour(|_| behaviour)?
//         .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(300)))
//         .build();

pub fn build_swarm(
    config: &ConfigHandle,
    keymanager: &KeyManager,
) -> Result<Swarm<NetBehaviour>, Box<dyn std::error::Error>> {
    let keypair = keymanager.keypair();
    let behaviour = NetBehaviour::new(config, keypair);

    // 获取监听地址（从配置中读取）
    let listen_addrs: Vec<Multiaddr> = {
        let cfg = config.read();
        let mut addrs = Vec::new();
        if cfg.network.ipv4_enabled {
            if let Some(ip) = cfg.network.ipv4_address {
                addrs.push(to_multiaddr(ip, cfg.network.port as u16));
            } else {
                // 监听所有接口
                addrs.push(to_multiaddr(
                    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    cfg.network.port as u16,
                ));
            }
        }
        if cfg.network.ipv6_enabled {
            if let Some(ip) = cfg.network.ipv6_address {
                addrs.push(to_multiaddr(ip, cfg.network.port as u16));
            } else {
                addrs.push(to_multiaddr(
                    IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                    cfg.network.port as u16,
                ));
            }
        }
        addrs
    };

    let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    for addr in listen_addrs {
        swarm.listen_on(addr)?;
    }

    Ok(swarm)
}

pub struct NetHandle {
    config: ConfigHandle,
    swarm: Option<Swarm<NetBehaviour>>,
}

impl NetHandle {
    pub fn new(config: ConfigHandle) -> Self {
        Self {
            config,
            swarm: None,
        }
    }

    /// 启动网络（必须调用一次以初始化 Swarm）
    pub fn start(&mut self, keymanager: &KeyManager) -> Result<(), Box<dyn std::error::Error>> {
        let swarm = build_swarm(&self.config, keymanager)?;
        self.swarm = Some(swarm);
        Ok(())
    }

    /// 移交 Swarm 所有权，启动事件循环由调用方驱动
    pub fn run(mut self) -> Swarm<NetBehaviour> {
        self.swarm.take().expect("swarm not started")
    }

    /// 启动前用于拨号 bootstrap 节点
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), String> {
        self.swarm
            .as_mut()
            .ok_or("Swarm not started".into())
            .and_then(|swarm| swarm.dial(addr).map_err(|e| e.to_string()))
    }
}
