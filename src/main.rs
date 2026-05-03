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
use service_dispatcher::{ManagerCommand, ServiceDispatcher};

mod request_handler;
use request_handler::handle_incoming_request;
use request_handler::send_service_request;


use libp2p::kad::QueryResult;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, futures::StreamExt, kad, noise, swarm::SwarmEvent, tcp, yamux,
};
use net::{
    ConnectionStatus, NetBehaviourEvent, PeerInfo, appropriate_address_filter, create_behaviour,
    get_key, get_network_addresses,
};

use service_discovery::SD_KEY_PREFIX;
use tokio::sync::mpsc;

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
            "--port" | "-p" if i + 1 < args.len() => {
                port = args[i + 1].parse().unwrap_or(5000);
                i += 1;
            }
            "--connect" | "-c" if i + 1 < args.len() => {
                connect_list.clear();
                connect_list.push(args[i + 1].clone());
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    // 各个参数检查和报告
    if connect_list.is_empty() {
        let log = LogStruct {
            level: LogLevel::Warning,
            topic: "没有设置连接节点".to_string(),
            content: "节点将会被动监听……".to_string(),
        };
        log.logout();
    } else {
        let log = LogStruct {
            level: LogLevel::Preset,
            topic: format!("计划连接 {} 个节点", connect_list.len()),
            content: "".to_string(),
        };
        log.logout();
    }
    

    tokio::runtime::Runtime::new()?.block_on(async {
        let _ = run_node(port, connect_list, &mut config).await;
    });
    Ok(())
}
/// 运行libp2p节点的核心异步函数
async fn run_node(
    port: u16,
    connect_list: Vec<String>,
    config: &mut NodeConfig,
) -> Result<(), Box<dyn Error>> {
    // 获取或生成节点的密钥对
    let keypair = get_key()?;
    let my_peer_id = PeerId::from(keypair.public());

    // 生成net连接节点列表
    let mut net_peer_list: Vec<PeerInfo> = [].to_vec();

    // 初始化服务发现
    let mut sd = ServiceDiscovery::new(&config.services.service_discovery, my_peer_id);

    // 初始化本地服务调度器（微内核+边车模式）
    let dispatcher = ServiceDispatcher::new(&config.services.dispatcher);
    if dispatcher.is_enabled() {
        // 将 dispatcher 中注册的服务自动同步到服务发现注册表
        let all_services = dispatcher.get_service_names();
        let unhealthy = dispatcher.health_check();
        let verified_services: Vec<String> = all_services
            .into_iter()
            .filter(|name| !unhealthy.contains(name))
            .collect();
        sd.sync_from_dispatcher(&verified_services);
        if !unhealthy.is_empty() {
            let log = LogStruct {
                level: LogLevel::Warning,
                topic: "服务调度器".to_string(),
                content: format!("以下注册的后端服务不可达: \n    {}", unhealthy.join("\n    ")),
            };
            log.logout();
        }
    }

    // 创建网络行为组合（包含ping、identify等协议）
    let behaviour = create_behaviour(&keypair, "/OAHD", config)?;

    // 使用SwarmBuilder构建节点管理器
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    // 根据配置启用IPv4或IPv6监听
    {
        let mut listen_success = false;
        let (ipv4_address, ipv6_address) = get_network_addresses()?;
        if config.network.ipv4_enabled {
            let listen_addr_v4: Multiaddr = format!("/ip4/{}/tcp/{}", ipv4_address, port).parse()?;
            if let Err(e) = swarm.listen_on(listen_addr_v4) {
                let log = LogStruct {
                    level: LogLevel::Warning,
                    topic: "监听端口失败".to_string(),
                    content: format!("在试图监听地址时出现错误 {}", e),
                };
                log.logout();
            } else {
                listen_success = true;
            }
        }
        if config.network.ipv6_enabled {
            let listen_addr_v6: Multiaddr = format!("/ip6/{}/tcp/{}", ipv6_address, port).parse()?;
            if let Err(e) = swarm.listen_on(listen_addr_v6) {
                let log = LogStruct {
                    level: LogLevel::Warning,
                    topic: "监听端口失败".to_string(),
                    content: format!("在试图监听地址时出现错误 {}", e),
                };
                log.logout();
            } else {
                listen_success = true;
            }
        }
        if !listen_success {
            let log = LogStruct {
                level: LogLevel::Critical,
                topic: "网络监听完全失败".to_string(),
                content: "所有要求的网络地址均无法监听，程序退出".to_string(),
            };
            log.logout();
            std::process::exit(1);
        }
    }
    

    // 节点启动成功，输出状态信息
    let log = LogStruct {
        level: LogLevel::Important,
        topic: "节点启动".to_string(),
        content: format!("节点已启动，监听端口: {}\n\tpeer_id: {}", port, my_peer_id),
    };
    log.logout();

    // 如果有连接参数，尝试连接到指定的远程节点
    // [todo] 合并为统一的合并连接函数
    for connect_to in connect_list {{
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
                    Ok(_) => {} // 连接已成功发起
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

    if sd.is_enabled() {
        let log = LogStruct {
            level: LogLevel::Preset,
            topic: "服务发现".to_string(),
            content: format!(
                "已启用，本地服务: {:?}",
                sd.list_local_services()
            ),
        };
        log.logout();
    }
    
    // // 调度器管理命令通道：本地服务（如 CLI）通过 dispatcher 管理口发来的 P2P 请求
    let (dispatcher_cmd_tx, mut dispatcher_cmd_rx) = mpsc::channel::<ManagerCommand>(64);
    if config.services.dispatcher.enabled {
        let mgmt_config = config.services.dispatcher.clone();
        let cmd_tx = dispatcher_cmd_tx.clone();
        ServiceDispatcher::start_management(mgmt_config, cmd_tx);
    }

    // 阶段管理：监听就绪 → bootstrap → 服务宣告+查询
    let mut listeners_ready = false;
    let mut bootstrap_done = false;
    let mut sd_announced = false;

    // 主事件循环 - 持续处理Swarm产生的事件
    loop {
        tokio::select! {
                    // 处理本地服务通过 dispatcher 管理口发来的远程调用请求
                    Some(cmd) = dispatcher_cmd_rx.recv() => {
                        match cmd {
                            ManagerCommand::CallRemote { peer, service, payload, response_tx } => {
                                // 解析目标 PeerId
                                match peer.parse::<PeerId>() {
                                    Ok(peer_id) => {
                                        match send_service_request(&mut swarm, &peer_id, &service, payload) {
                                            Ok(request_id) => {
                                                let _ = response_tx.send(Ok(format!("请求已发送, request_id={}", request_id).into_bytes()));
                                            }
                                            Err(e) => {
                                                let _ = response_tx.send(Err(format!("发送请求失败: {}", e)));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let _ = response_tx.send(Err(format!("无效的 PeerId '{}': {}", peer, e)));
                                    }
                                }
                            }
                        }
                    }
                    // 处理 Swarm 事件
                    event = swarm.select_next_some() => {
                        match event {
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
                                    let key_str = String::from_utf8_lossy(record.key.as_ref());
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
                                addresses: None,
                                observed_addresses: None,
                                public_key: None,
                                rtt: None,
                                connection_status: ConnectionStatus::Connected,
                                supported_protocols: None,
                                agent_version: None,
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
                                        let entry = &mut net_peer_list[index];

                                        entry.agent_version = Some(info.agent_version.clone());
                                        entry.observed_addresses = Some(info.observed_addr.clone());
                                        entry.public_key = Some(info.public_key.clone());
                                        entry.supported_protocols = Some(info.protocols.clone());

                                        let filtered_addrs = appropriate_address_filter(&info.listen_addrs, config);
                                        if let Some(mut addrs) = filtered_addrs {
                                            addrs.push(libp2p::multiaddr::Protocol::P2p(peer_id));
                                            entry.addresses = Some(addrs);
                                        } else {
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "地址不兼容".to_string(),
                                                content: format!("{} 的监听地址与本地 IP 协议栈不匹配，跳过", peer_id),
                                            };
                                            log.logout();
                                        }

                                        let has_p2p_addr = entry
                                            .addresses
                                            .as_ref()
                                            .is_some_and(|addrs| addrs.iter().any(|proto| matches!(proto, libp2p::multiaddr::Protocol::P2p(_))));

                                        const AGENT_VERSION_PREFIX: &str = concat!("/OAHD/", env!("CARGO_PKG_VERSION"));
                                        let is_my_node = config.services.kademlia.enabled
                                            && info.agent_version == AGENT_VERSION_PREFIX
                                            && has_p2p_addr;

                                        if is_my_node {
                                            let addrs = entry.addresses.as_ref().unwrap();
                                            let addr_string = addrs.to_string();
                                            config.insert_bootstrap_nodes(addr_string);

                                            let kademlia = &mut swarm.behaviour_mut().kademlia;
                                            kademlia.add_address(&peer_id, addrs.clone());
                                            let log = LogStruct {
                                                level: LogLevel::Preset,
                                                topic: format!("{}已被加入路由节点", peer_id),
                                                content: String::new(),
                                            };
                                            log.logout();
                                            println!("路由表总节点数: {}", kademlia.kbuckets().count());

                                            if !bootstrap_done {
                                                let _ = kademlia.bootstrap();
                                                bootstrap_done = true;
                                                let log = LogStruct {
                                                    level: LogLevel::Preset,
                                                    topic: "Kademlia".to_string(),
                                                    content: "开始引导".to_string(),
                                                };
                                                log.logout();
                                            }
                                        } else if has_p2p_addr {
                                            let addrs = entry.addresses.as_ref().unwrap();
                                            let log = LogStruct {
                                                level: LogLevel::Warning,
                                                topic: "未知节点接入网络".to_string(),
                                                content: format!("收到 {} 后未能创建列表内容！{}", peer_id, addrs),
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
                                                    if sd.is_enabled()
                                                       && !sd_announced 
                                                       && !sd.list_local_services().is_empty() {
                                                        // 用 listeners 中已就绪的地址填充
                                                        let announce_addrs: Vec<String> = swarm
                                                            .listeners()
                                                            .map(|a| format!("{}/p2p/{}", a, my_peer_id))
                                                            .collect();
                                                        sd.set_addresses(&announce_addrs);
                                                        let records = sd.get_announce_records();
                                                        for record in records {
                                                            let key_str = String::from_utf8_lossy(record.key.as_ref());
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
                                                                addresses: Some(peer.addrs.to_vec()[0].clone()),
                                                                observed_addresses: None,
                                                                public_key: None,
                                                                rtt: None,
                                                                connection_status: ConnectionStatus::Disconnected,
                                                                supported_protocols: None,
                                                                agent_version: None,
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
                            // 请求/响应事件
                            NetBehaviourEvent::RequestResponse(event) => {
                                match event {
                                    libp2p::request_response::Event::Message { peer, message, .. } => {
                                        match message {
                                            libp2p::request_response::Message::Request {
                                                request, channel, ..
                                            } => {
                                                let log = LogStruct {
                                                    level: LogLevel::Debug,
                                                    topic: "P2P请求".to_string(),
                                                    content: format!(
                                                        "收到来自 {} 的服务请求: service={}, request_id={}",
                                                        peer, request.service, request.request_id
                                                    ),
                                                };
                                                log.logout();

                                                let response = handle_incoming_request(request, &dispatcher);
                                                let _ = swarm
                                                    .behaviour_mut()
                                                    .request_response
                                                    .send_response(channel, response);
                                            }
                                            libp2p::request_response::Message::Response {
                                                response, ..
                                            } => {
                                                let log = LogStruct {
                                                    level: LogLevel::Debug,
                                                    topic: "P2P响应".to_string(),
                                                    content: format!(
                                                        "收到来自 {} 的服务响应: status={}, request_id={}",
                                                        peer, response.status, response.request_id
                                                    ),
                                                };
                                                log.logout();
                                            }
                                        }
                                    }
                                    libp2p::request_response::Event::OutboundFailure {
                                        peer, error, ..
                                    } => {
                                        let log = LogStruct {
                                            level: LogLevel::Warning,
                                            topic: "P2P请求失败".to_string(),
                                            content: format!("向 {} 发送请求失败: {:?}", peer, error),
                                        };
                                        log.logout();
                                    }
                                    libp2p::request_response::Event::InboundFailure {
                                        peer, error, ..
                                    } => {
                                        let log = LogStruct {
                                            level: LogLevel::Warning,
                                            topic: "P2P响应失败".to_string(),
                                            content: format!("来自 {} 的请求处理失败: {:?}", peer, error),
                                        };
                                        log.logout();
                                    }
                                    _ => {}
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
                                                let key_str = String::from_utf8_lossy(record.key.as_ref());
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
    }
}
