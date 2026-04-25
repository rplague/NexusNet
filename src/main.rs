mod log;
use log::LogLevel;
use log::LogStruct;

mod config;
use config::read_config_file;

mod service_discovery;
use service_discovery::ServiceDiscovery;

mod net;

mod addr_watcher;

mod service_dispatcher;
use service_dispatcher::ServiceDispatcher;
use net::{
    get_network_addresses,
    appropriate_address_filter,
    get_key,
    create_behaviour,
    NetBehaviourEvent,
    PeerInfo,
    ConnectionStatus,
};
use libp2p::{
    tcp,
    kad,
    noise,
    yamux,
    swarm::SwarmEvent,
    SwarmBuilder,
    PeerId,
    Multiaddr,
    futures::StreamExt,
};
use service_discovery::SD_KEY_PREFIX;
use libp2p::kad::QueryResult;

use std::env;
use std::error::Error;
use std::time::Duration;

use crate::config::NodeConfig;

fn main() -> Result<(), Box<dyn Error>> {
    // 读取设置
    let mut config = read_config_file().unwrap();

    // 从配置中获取初始连接列表
    let mut connect_list: Vec<String> = if config.services.kademlia.enabled {
        config.services.kademlia.bootstrap_nodes.clone()
    } else {
        Vec::new()
    };

    // 获取端口配置
    let mut port = config.network.port;

    // 简单的命令行参数解析，覆盖设置内容
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                if i + 1 < args.len() {
                    port = args[i + 1].parse().unwrap_or(5000);
                    i += 1;
                }
            }
            "--connect" | "-c" => {
                if i + 1 < args.len() {
                    connect_list.clear();
                    connect_list.push(args[i + 1].clone());
                    i += 1;
                }
            }
            "--query" | "-q" => {
                if i + 1 < args.len() {
                    // 保存查询指令，在节点启动后执行
                    let svc_type = args[i + 1].clone();
                    // 存到环境变量中的特殊标记，稍后在 run_node 中处理
                    let log = LogStruct {
                        level: LogLevel::Preset,
                        topic: "服务发现查询".to_string(),
                        content: format!("节点启动后将查询服务: {}", svc_type),
                    };
                    log.logout();
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    // 各个参数检查和报告
    if connect_list.is_empty() {
        let log = LogStruct {
            level: LogLevel::Warning,
            topic: "没有设置连接目标".to_string(),
            content: "节点将会被动监听……".to_string(),
        };
        log.logout();
    } else {
        let log = LogStruct {
            level: LogLevel::Preset,
            topic: "连接目标".to_string(),
            content: format!("将尝试连接 {} 个节点", connect_list.len()),
        };
        log.logout();
    }
    let connect_list = connect_list;
    // 提取查询目标（如果有）
    let query_service = std::env::var("NEXUS_QUERY_SERVICE").ok();

    tokio::runtime::Runtime::new()?.block_on(async {
        run_node(port, connect_list, &mut config, query_service).await
    })
}
/// 运行libp2p节点的核心异步函数
async fn run_node(
    port: u16,
    connect_list: Vec<String>,
    config: &mut NodeConfig,
    query_service: Option<String>,
) -> Result<(), Box<dyn Error>> {
    // 生成net连接节点列表
    let mut net_peer_list: Vec<PeerInfo> = [].to_vec();

    // 获取或生成节点的密钥对
    let keypair = get_key()?;
    let my_peer_id = PeerId::from(keypair.public());

    // 初始化服务发现
    let mut sd = ServiceDiscovery::new(&config.services.service_discovery, my_peer_id);
    
    // 初始化本地服务调度器（微内核+边车模式）
    let dispatcher = ServiceDispatcher::new(&config.services.dispatcher);
    if dispatcher.is_enabled() {
        let unhealthy = dispatcher.health_check();
        if unhealthy.is_empty() {
            let log = LogStruct {
                level: LogLevel::Debug,
                topic: "服务调度器".to_string(),
                content: "所有本地后端服务均可达".to_string(),
            };
            log.logout();
        } else {
            let log = LogStruct {
                level: LogLevel::Warning,
                topic: "服务调度器".to_string(),
                content: format!("以下后端服务不可达: {}", unhealthy.join(", ")),
            };
            log.logout();
        }
    }
    if sd.is_enabled() {
        let log = LogStruct {
            level: LogLevel::Preset,
            topic: "服务发现".to_string(),
            content: format!("已启用，本地服务: {:?}", config.services.service_discovery.services),
        };
        log.logout();
    }
    
    // 从公钥计算Peer ID（节点的唯一标识符）
    
    // 创建网络行为组合（包含ping、identify等协议）
    let behaviour = create_behaviour(&keypair, "/OAHD", &config)?;
    
    // 使用SwarmBuilder构建节点管理器
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|config| {config.with_idle_connection_timeout(Duration::from_secs(300))})
        .build();
    
    let (ipv4_address, ipv6_address) = get_network_addresses()?;
    // 根据配置启用IPv4监听
    if config.network.ipv4_enabled {
        let listen_addr_v4: Multiaddr = format!("/ip4/{}/tcp/{}", ipv4_address, port).parse()?;
        swarm.listen_on(listen_addr_v4)?;  // 绑定到所有IPv4接口的指定端口
    }
    
    // 根据配置启用IPv6监听
    if config.network.ipv6_enabled {
        let listen_addr_v6: Multiaddr = format!("/ip6/{}/tcp/{}", ipv6_address, port).parse()?;
        swarm.listen_on(listen_addr_v6)?;  // 绑定到所有IPv6接口的指定端口
    }
    
    // 如果有连接参数，尝试连接到指定的远程节点
    for connect_to in connect_list{
        match connect_to.parse::<Multiaddr>() {
            Ok(remote_addr) => {
                let log = LogStruct {
                    level: LogLevel::Preset,
                    topic: "尝试连接".to_string(),
                    content: format!("尝试连接到: {}", remote_addr),
                };
                log.logout();
                
                // 发起连接尝试
                match swarm.dial(remote_addr) {
                    Ok(_) => {}  // 连接已成功发起
                    Err(e) => {
                        let log = LogStruct {
                            level: LogLevel::Error,
                            topic: "连接失败".to_string(),
                            content: format!("无法连接: {:?}", e),
                        };
                        log.logout();
                    }
                }
            }
            Err(e) => {
                // 地址解析失败
                let log = LogStruct {
                    level: LogLevel::Error,
                    topic: "解析地址失败".to_string(),
                    content: format!("无法解析连接地址 '{}': {:?}", connect_to, e),
                };
                log.logout();
            }
        }
    }

    // 节点启动成功，输出状态信息
    let log = LogStruct {
        level: LogLevel::Important,
        topic: "节点启动".to_string(),
        content: format!("节点已启动，监听端口: {}\n\tpeer_id: {}", port, my_peer_id),
    };
    log.logout();

    // 阶段管理：监听就绪 → bootstrap → 服务宣告+查询
    let mut listeners_ready = false;
    let mut bootstrap_done = false;
    let mut sd_announced = false;
    let mut sd_query = query_service;

    // 主事件循环 - 持续处理Swarm产生的事件
    loop {
        match swarm.select_next_some().await {
            // 新的监听地址已添加
            SwarmEvent::NewListenAddr { address, .. } => {
                let log = LogStruct {
                    level: LogLevel::Debug,
                    topic: "监听地址".to_string(),
                    content: format!("监听于: {}", address),
                };
                log.logout();
                if sd.is_enabled() {
                    // 标记 listeners 已就绪
                    if !listeners_ready {
                        listeners_ready = true;
                    }
                    // 如果已宣告过，地址更新后重新宣告
                    if sd_announced {
                        let announce_addrs: Vec<String> = swarm
                            .listeners()
                            .map(|a| format!("{}/p2p/{}", a, my_peer_id))
                            .collect();
                        let records = sd.reannounce(&announce_addrs);
                        for record in records {
                            let key_str = String::from_utf8_lossy(&record.key.as_ref());
                            if let Some(svc_type) = key_str.strip_prefix(
                                std::str::from_utf8(SD_KEY_PREFIX).unwrap_or("")
                            ) {
                                let log = LogStruct {
                                    level: LogLevel::Debug,
                                    topic: "服务发现".to_string(),
                                    content: format!("更新宣告: {}", svc_type),
                                };
                                log.logout();
                            }
                            let _ = swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::One);
                        }
                    }
                }
            }
            // 传入连接错误事件
            SwarmEvent::IncomingConnectionError { connection_id, local_addr: _, send_back_addr, error, peer_id: _ } => {
                let log = LogStruct {
                    level: LogLevel::Error,
                    topic: "接收连接请求失败".to_string(),
                    content: format!("connection_id: {}\n\tconnection_id: {}\n\terror: {}", connection_id, send_back_addr, error),
                };
                log.logout();
            }
            // 连接已成功建立
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                let log = LogStruct {
                    level: LogLevel::Preset,
                    topic: "连接建立".to_string(),
                    content: format!("已连接到节点: {}", peer_id),
                };
                log.logout();
                let peer = peer_id;

                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer) {
                    net_peer_list[index].connection_status = ConnectionStatus::Connected;
                } else {
                    let new_peer_info = PeerInfo {
                        peer_id: peer ,
                        name_string: None,
                        addresses: None,
                        observed_addresses: None,
                        public_key: None,
                        rtt: None,
                        connection_status: ConnectionStatus::Connected,
                        supported_protocols: None,
                        agent_version: None,
                        score: Some(0),
                        tags: None,
                    };
                    net_peer_list.push(new_peer_info);
                }
            }
            // 连接已关闭
            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                let log = LogStruct {
                    level: LogLevel::Preset,
                    topic: "连接关闭".to_string(),
                    content: format!("连接关闭: {}\n\t原因: {:?}", peer_id, cause),
                };
                log.logout();
                // 修改状态
                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer_id) {
                    if net_peer_list[index].agent_version == format!("/OAHD/{}", env!("CARGO_PKG_VERSION")).into(){
                        net_peer_list[index].connection_status = ConnectionStatus::Disconnected;
                    } else {
                        net_peer_list.retain(|p| p.peer_id != peer_id);
                    }
                }
            }
            // 网络行为产生的事件
            SwarmEvent::Behaviour(event) => {
                match event {
                    // Ping协议事件
                    NetBehaviourEvent::Ping(event) => {
                        let libp2p::ping::Event { peer, result, connection: _ } = event;
                        match result {
                            Ok(rtt) => {
                                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer) {
                                    net_peer_list[index].rtt = Some(rtt);
                                }
                            }
                            Err(e) => {
                                let log = LogStruct {
                                    level: LogLevel::Warning,
                                    topic: "Ping 失败".to_string(),
                                    content: format!("Ping 失败 {}: {:?}", peer, e),
                                };
                                log.logout();
                                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer) {
                                    if net_peer_list[index].agent_version == format!("/OAHD/{}", env!("CARGO_PKG_VERSION")).into(){
                                        net_peer_list[index].connection_status = ConnectionStatus::Disconnected;
                                    } else {
                                        net_peer_list.retain(|p| p.peer_id != peer);
                                    }
                                }
                            }
                        }
                    },
                    // Identify协议事件
                    NetBehaviourEvent::Identify(event) => match event {
                        // 收到其他节点的身份信息
                        libp2p::identify::Event::Received { peer_id, info, connection_id: _ } => {
                            let log = LogStruct {
                                level: LogLevel::Preset,
                                topic: "身份交换".to_string(),
                                content: format!(
                                    "收到 {} 的身份信息\n\t地址: {:?}\n\t协议: {:?}\n\t版本: {}",
                                    peer_id, info.listen_addrs, info.protocols, info.agent_version
                                ),
                            };
                            log.logout();
                            // 确认身份并录入信息
                            if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer_id) {
                                net_peer_list[index].agent_version = Some(info.agent_version.clone());
                                let mut addr_with_peer_id = appropriate_address_filter(&info.listen_addrs, config).expect("");
                                addr_with_peer_id.push(libp2p::multiaddr::Protocol::P2p(peer_id));
                                net_peer_list[index].addresses = Some(addr_with_peer_id.clone());
                                net_peer_list[index].observed_addresses = Some(info.observed_addr.clone());
                                net_peer_list[index].public_key = Some(info.public_key.clone());
                                net_peer_list[index].supported_protocols = Some(info.protocols.clone());
                                // 若为我的节点则加入连接表中并开始kad协议
                                if config.services.kademlia.enabled
                                && format!("/OAHD/{}", env!("CARGO_PKG_VERSION")) == info.agent_version.clone()
                                && let Some(addr) = &net_peer_list[index].addresses 
                                && addr.iter().any(|proto| {matches!(proto, libp2p::multiaddr::Protocol::P2p(_))}){
                                    let addr_string = addr.to_string();
                                    config.insert_bootstrap_nodes(addr_string);
                                    // 加入节点列表
                                    swarm.behaviour_mut().kademlia.add_address(&peer_id, addr_with_peer_id.clone());
                                    let log = LogStruct {
                                        level: LogLevel::Preset,
                                        topic: format!("{}已被加入路由节点", peer_id),
                                        content: "".to_string(),
                                    };
                                    log.logout();
                                    println!("路由表总节点数: {}", swarm.behaviour_mut().kademlia.kbuckets().count());

                                    // 有同版本节点连接后，触发 bootstrap
                                    if !bootstrap_done {
                                        let _ = swarm.behaviour_mut().kademlia.bootstrap();
                                        bootstrap_done = true;
                                        let log = LogStruct {
                                            level: LogLevel::Preset,
                                            topic: "Kademlia".to_string(),
                                            content: "开始引导".to_string(),
                                        };
                                        log.logout();
                                    }

                                } else {
                                    let log = LogStruct {
                                        level: LogLevel::Warning,
                                        topic: "未知节点接入网络".to_string(),
                                        content: format!(
                                            "收到 {} 后未能创建列表内容！{}",
                                            peer_id,
                                            net_peer_list[index].addresses.as_ref().unwrap()
                                        ),
                                    };
                                    log.logout();
                                }
                            } else {
                                let log = LogStruct {
                                    level: LogLevel::Warning,
                                    topic: "异常".to_string(),
                                    content: "net_peer_list未找到节点信息？".to_string()
                                };
                                log.logout();
                            }
                        }

                        // // 已发送身份信息（无需处理）
                        // libp2p::identify::Event::Sent { peer_id, connection_id: _ } => {}

                        // 身份交换错误
                        libp2p::identify::Event::Error { peer_id, error, connection_id: _ } => {
                            let log = LogStruct {
                                level: LogLevel::Warning,
                                topic: "身份交换错误".to_string(),
                                content: format!("与 {} 身份交换失败: {:?}", peer_id, error),
                            };
                            log.logout();
                        }
                        // 其他Identify事件（忽略）
                        _ => {}
                    },
                    // Kademlia 事件处理
                    NetBehaviourEvent::Kademlia(event) => {
                        match event {
                            kad::Event::OutboundQueryProgressed {id, result, ..} => {
                                match result {
                                    QueryResult::Bootstrap(result) => match result {
                                        Ok(_) => {
                                            // Bootstrap 成功后，宣告本地服务并查询
                                            if sd.is_enabled() {
                                                if !sd_announced && sd.list_local_services().len() > 0 {
                                                    // 用 listeners 中已就绪的地址填充
                                                    let announce_addrs: Vec<String> = swarm
                                                        .listeners()
                                                        .map(|a| format!("{}/p2p/{}", a, my_peer_id))
                                                        .collect();
                                                    sd.set_addresses(&announce_addrs);
                                                    let records = sd.get_announce_records();
                                                    for record in records {
                                                        let key_str = String::from_utf8_lossy(&record.key.as_ref());
                                                        if let Some(svc_type) = key_str.strip_prefix(
                                                            std::str::from_utf8(SD_KEY_PREFIX).unwrap_or("")
                                                        ) {
                                                            let log = LogStruct {
                                                                level: LogLevel::Preset,
                                                                topic: "服务发现".to_string(),
                                                                content: format!("宣告服务: {}", svc_type),
                                                            };
                                                            log.logout();
                                                        }
                                                        let _ = swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::One);
                                                    }
                                                    sd_announced = true;
                                                }
                                                // 如果有查询请求，发起查询
                                                if let Some(ref svc_type) = sd_query {
                                                    let log = LogStruct {
                                                        level: LogLevel::Preset,
                                                        topic: "服务发现".to_string(),
                                                        content: format!("查询服务: {}", svc_type),
                                                    };
                                                    log.logout();
                                                    if let Some(key) = sd.query_service(svc_type) {
                                                        let _ = swarm.behaviour_mut().kademlia.get_record(key);
                                                    }
                                                    sd_query = None;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "Kademlia引导失败".to_string(),
                                                content: format!("引导失败，查询ID: {:?}，错误: {:?}", id, e),
                                            };
                                            log.logout();
                                        }
                                    },
                                    QueryResult::GetRecord(result) => match result {
                                        Ok(ok) => {
                                            let log = LogStruct {
                                                level: LogLevel::Debug,
                                                topic: "Kademlia查找详情".to_string(),
                                                content: "收到 GetRecord 结果".to_string(),
                                            };
                                            log.logout();
                                            // 将结果交给服务发现模块处理
                                            let old_count = sd.get_all_cached().len();
                                            sd.handle_get_record_ok(&ok);
                                            let new_count = sd.get_all_cached().len();
                                            if new_count > old_count {
                                                let log = LogStruct {
                                                    level: LogLevel::Preset,
                                                    topic: "服务发现".to_string(),
                                                    content: format!("发现 {} 个新服务提供者，共 {} 个", new_count - old_count, new_count),
                                                };
                                                log.logout();
                                                for svc in sd.get_all_cached() {
                                                    let log = LogStruct {
                                                        level: LogLevel::Debug,
                                                        topic: "服务提供者".to_string(),
                                                        content: format!("{} → {} (地址: {:?})", svc.service_type, svc.provider, svc.addrs),
                                                    };
                                                    log.logout();
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "Kademlia查找失败".to_string(),
                                                content: format!("查找失败: {:?}", e),
                                            };
                                            log.logout();
                                        }
                                    },
                                    QueryResult::PutRecord(result) => match result {
                                        Ok(kad::PutRecordOk { key, .. }) => {
                                            let log = LogStruct {
                                                level: LogLevel::Debug,
                                                topic: "Kademlia存储记录".to_string(),
                                                content: format!("成功存储记录: {:?}", key),
                                            };
                                            log.logout();
                                        }
                                        Err(e) => {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "Kademlia存储失败".to_string(),
                                                content: format!("存储失败: {:?}", e),
                                            };
                                            log.logout();
                                        }
                                    },
                                    QueryResult::GetClosestPeers(result) => match result {
                                        Ok(ok) => {
                                            if ! ok.peers.is_empty(){
                                                let log = LogStruct {
                                                    level: LogLevel::Debug,
                                                    topic: "Kademlia发现节点".to_string(),
                                                    content: format!("发现 {} 个最近节点", ok.peers.len()),
                                                };
                                                log.logout();
                                            }
                                            
                                            
                                            // 将发现的节点添加到net_peer_list
                                            for peer in &ok.peers {
                                                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer.peer_id) {
                                                    net_peer_list[index].addresses = Some(peer.addrs.to_vec()[0].clone());
                                                } else {
                                                    let new_peer = PeerInfo {
                                                        peer_id: peer.peer_id,
                                                        name_string: None,
                                                        addresses: Some(peer.addrs.to_vec()[0].clone()),
                                                        observed_addresses: None,
                                                        public_key: None,
                                                        rtt: None,
                                                        connection_status: ConnectionStatus::Disconnected,
                                                        supported_protocols: None,
                                                        agent_version: None,
                                                        score: Some(50), // 初始分数
                                                        tags: None,
                                                    };
                                                    net_peer_list.push(new_peer);
                                                }
                                                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer.peer_id) 
                                                && net_peer_list[index].connection_status == ConnectionStatus::Disconnected{
                                                    let log = LogStruct {
                                                        level: LogLevel::Preset,
                                                        topic: "尝试连接".to_string(),
                                                        content: format!("尝试连接到: {}", peer.addrs.to_vec()[0].clone()),
                                                    };
                                                    log.logout();
                                                    let _ = swarm.dial(peer.addrs.to_vec()[0].clone());
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "Kademlia节点发现失败".to_string(),
                                                content: format!("发现失败: {:?}", e),
                                            };
                                            log.logout();
                                        }
                                    },
                                    _ => {} // 其他查询结果类型
                                }
                            }
                            // kad::Event::RoutingUpdated { peer, .. } => {
                            // }
                            kad::Event::UnroutablePeer { peer, .. } => {
                                let log = LogStruct {
                                    level: LogLevel::Warning,
                                    topic: "Kademlia无法路由".to_string(),
                                    content: format!("无法路由到节点: {}", peer),
                                };
                                log.logout();
                            }
                            _ => {} // 其他Kademlia事件
                        }
                    }
                    // 地址检测事件
                    NetBehaviourEvent::AddrWatcher(event) => {
                        match event {
                            addr_watcher::AddrWatcherEvent::Changed(change) => {
                                let log = LogStruct {
                                    level: LogLevel::Preset,
                                    topic: "地址变更".to_string(),
                                    content: format!(
                                        "IP地址变化: {} → {}",
                                        change.old_addrs.join(", "),
                                        change.new_addrs.join(", ")
                                    ),
                                };
                                log.logout();

                                // 更新 announce_addresses
                                let announce_addrs: Vec<String> = swarm
                                    .listeners()
                                    .map(|a| format!("{}/p2p/{}", a, my_peer_id))
                                    .collect();

                                // 重新宣告服务
                                if sd.is_enabled() && sd_announced {
                                    let records = sd.reannounce(&announce_addrs);
                                    for record in records {
                                        let key_str = String::from_utf8_lossy(&record.key.as_ref());
                                        if let Some(svc_type) = key_str.strip_prefix(
                                            std::str::from_utf8(SD_KEY_PREFIX).unwrap_or("")
                                        ) {
                                            let log = LogStruct {
                                                level: LogLevel::Debug,
                                                topic: "服务发现".to_string(),
                                                content: format!("地址变更后重新宣告: {}", svc_type),
                                            };
                                            log.logout();
                                        }
                                        let _ = swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::One);
                                    }
                                }

                                // 更新配置
                                config.network.announce_addresses = announce_addrs;

                                // 重连所有已知节点
                                let known_nodes = config.services.kademlia.bootstrap_nodes.clone();
                                for node_addr in &known_nodes {
                                    match node_addr.parse::<Multiaddr>() {
                                        Ok(addr) => {
                                            let log = LogStruct {
                                                level: LogLevel::Debug,
                                                topic: "地址变更".to_string(),
                                                content: format!("尝试重连: {}", addr),
                                            };
                                            log.logout();
                                            let _ = swarm.dial(addr);
                                        }
                                        Err(e) => {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "地址变更".to_string(),
                                                content: format!("重连地址解析失败 {}: {:?}", node_addr, e),
                                            };
                                            log.logout();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // 其他事件（忽略）
            _ => {}
        }
    }
}