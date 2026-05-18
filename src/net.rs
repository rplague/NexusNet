use crate::{LogLevel, LogStruct, config::ConfigHandle};
use crate::service_protocol;
use libp2p::request_response::{self, cbor};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, futures::StreamExt, identify, identity, kad, noise, ping, swarm::{NetworkBehaviour, SwarmEvent}, tcp, yamux
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    net::IpAddr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc;
pub struct KeyManager {
    keypair: identity::Keypair,
    path: PathBuf,
}

impl KeyManager {
    /// 从指定路径加载密钥，若不存在则生成并原子保存
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();

        // 尝试读取并解析现有密钥文件
        match fs::read(path) {
            Ok(bytes) => {
                match identity::Keypair::from_protobuf_encoding(&bytes) {
                    Ok(keypair) => {
                        LogStruct::new(LogLevel::Important, "密钥加载成功", path.display().to_string()).emit();
                        return Ok(KeyManager {
                            keypair,
                            path: path.to_path_buf(),
                        });
                    }
                    Err(e) => {
                        LogStruct::new(LogLevel::Error, "密钥文件解析失败", e.to_string()).emit();
                        return Err(e.into());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                LogStruct::new(LogLevel::Warning, "密钥文件不存在，将生成新密钥", path.display().to_string()).emit();
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
            LogStruct::new(LogLevel::Critical, "写入临时密钥文件失败", format!("路径: {}, 错误: {}", temp_path.display(), e)).emit();
            return Err(e.into());
        }
        if let Err(e) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            LogStruct::new(LogLevel::Critical, "重命名密钥文件失败", format!("从 {} 到 {}, 错误: {}", temp_path.display(), path.display(), e)).emit();
            return Err(e.into());
        }

        LogStruct::new(LogLevel::Important, "新密钥生成并保存成功", path.display().to_string()).emit();

        Ok(KeyManager {
            keypair,
            path: path.to_path_buf(),
        })
    }
    pub fn keypair(&self) -> &identity::Keypair { &self.keypair }
    /// 获取 peer id
    pub fn peer_id(&self) -> PeerId { self.keypair.public().to_peer_id() }
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

pub fn update_config_with_public_ip(config: &ConfigHandle) -> Result<(), Box<dyn std::error::Error>> {
    let public_ips = get_public_ips();
    let ipv4_addrs: Vec<IpAddr> = public_ips.iter().filter(|ip| ip.is_ipv4()).copied().collect();
    let ipv6_addrs: Vec<IpAddr> = public_ips.iter().filter(|ip| ip.is_ipv6()).copied().collect();

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
    LogStruct::new(LogLevel::Preset, "网络配置已自动更新", format!("IPv4: {:?}, IPv6: {:?}, 公告地址: {:?}", ipv4_addrs, ipv6_addrs, announce_addrs)).emit();
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
    fn from(event: ping::Event) -> Self { NetBehaviourEvent::Ping(event) }
}
impl From<identify::Event> for NetBehaviourEvent {
    fn from(event: identify::Event) -> Self { NetBehaviourEvent::Identify(event) }
}
impl From<kad::Event> for NetBehaviourEvent {
    fn from(event: kad::Event) -> Self { NetBehaviourEvent::Kademlia(event) }
}
impl From<request_response::Event<service_protocol::Request, service_protocol::Response>> for NetBehaviourEvent {
    fn from(event: request_response::Event<service_protocol::Request, service_protocol::Response>) -> Self {
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
        kad_config.set_record_ttl(Some(Duration::from_secs(config.kademlia_record_ttl().into())));
        kad_config.set_query_timeout(Duration::from_secs(config.kademlia_query_timeout().into()));
        let mut kademlia = kad::Behaviour::with_config(keypair.public().to_peer_id(), store, kad_config);
        kademlia.set_mode(Some(kad::Mode::Server));

        let service_req = service_protocol::new_service_req_behaviour();
        Self { ping, identify, kademlia, service_req }
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
                addrs.push(to_multiaddr(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), cfg.network.port as u16));
            }
        }
        if cfg.network.ipv6_enabled {
            if let Some(ip) = cfg.network.ipv6_address {
                addrs.push(to_multiaddr(ip, cfg.network.port as u16));
            } else {
                addrs.push(to_multiaddr(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), cfg.network.port as u16));
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

#[derive(Clone)]
pub struct NetHandle {
    config: ConfigHandle,
    swarm: Arc<RwLock<Option<Swarm<NetBehaviour>>>>,
}

impl NetHandle {
    pub fn new(config: ConfigHandle) -> Self {
        Self {
            config,
            swarm: Arc::new(RwLock::new(None)),
        }
    }

    /// 启动网络（必须调用一次以初始化 Swarm）
    pub fn start(&self, keymanager: &KeyManager) -> Result<(), Box<dyn std::error::Error>> {
        let swarm = build_swarm(&self.config, keymanager)?;
        *self.swarm.write().unwrap() = Some(swarm);
        Ok(())
    }

    /// 访问 Swarm（只读）
    pub fn with_swarm<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Swarm<NetBehaviour>) -> R,
    {
        let guard = self.swarm.read().unwrap();
        guard.as_ref().map(f)
    }

    /// 修改 Swarm（写）
    pub fn with_swarm_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut Swarm<NetBehaviour>) -> R,
    {
        let mut guard = self.swarm.write().unwrap();
        guard.as_mut().map(f)
    }

    /// 拨号到指定地址
    pub fn dial(&self, addr: Multiaddr) -> Result<(), String> {
        self.with_swarm_mut(|swarm| {
            swarm.dial(addr).map_err(|e| e.to_string())
        }).unwrap_or(Err("Swarm 未初始化".into()))
    }

    /// 运行事件循环，返回一个接收网络行为事件的通道
    pub async fn run(&mut self) -> mpsc::UnboundedReceiver<NetBehaviourEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let swarm_opt = {
            let mut w = self.swarm.write().unwrap();
            w.take()
        };
        let swarm = swarm_opt.expect("swarm not started");
        tokio::spawn(async move {
            let mut swarm = swarm;
            loop {
                let event = swarm.select_next_some().await;
                if let SwarmEvent::Behaviour(bev) = event {
                    let _ = tx.send(bev);
                }
            }
        });

        rx
    }
}