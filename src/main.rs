mod log;
use log::LogLevel;
use log::LogStruct;

mod config;
use config::read_config_file;

mod net;
use net::{
    get_network_addresses,
    get_key,
    create_behaviour,
    NetBehaviourEvent,
    PeerInfo,
    ConnectionStatus,
};
use libp2p::kad::QueryResult;
use libp2p::{
    tcp,
    kad,
    noise,
    yamux,
    swarm::SwarmEvent,
    SwarmBuilder,
    PeerId,
    Multiaddr,
};
use libp2p::futures::StreamExt;

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
                    // 命令行参数覆盖配置，清空原有列表，只使用命令行指定的
                    connect_list.clear();
                    connect_list.push(args[i + 1].clone());
                    i += 1;
                }
            }
            // 可以添加多连接参数支持
            "--connect-multi" => {
                // 如果需要支持多个 --connect 参数
                // 可以收集多个地址
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
    tokio::runtime::Runtime::new()?.block_on(async {
        run_node(port, connect_list, &mut config).await
    })
}
/// 运行libp2p节点的核心异步函数
///
/// 该函数负责：
/// 1. 生成或加载节点密钥
/// 2. 创建网络行为组合
/// 3. 构建并配置Swarm管理器
/// 4. 设置网络监听端口
/// 5. 尝试连接到指定节点
/// 6. 处理网络事件循环
///
/// # 参数
/// - `port`: 监听端口号
/// - `connect_to`: 可选的要连接的远程节点地址（Multiaddr格式字符串）
/// - `config`: 节点配置的引用，用于控制IPv4/IPv6监听设置
///
/// # 返回值
/// - `Result<(), Box<dyn Error>>`: 成功时返回Ok(())，失败时返回错误
async fn run_node(port: u16, connect_list: Vec<String>, config: &mut NodeConfig) -> Result<(), Box<dyn Error>> {
    // 生成net连接节点列表
    let mut net_peer_list: Vec<PeerInfo> = [].to_vec();

    // 获取或生成节点的密钥对
    let keypair = get_key()?;
    
    // 从公钥计算Peer ID（节点的唯一标识符）
    let peer_id = PeerId::from(keypair.public());
    
    // 输出节点的Peer ID
    let log = LogStruct {
        level: LogLevel::Important,
        topic: format!("节点 Peer ID: {}", peer_id),
        content: "".to_string(),
    };
    log.logout();
    
    // 创建网络行为组合（包含ping、identify等协议）
    let behaviour = create_behaviour(&keypair, "/OAHD")?;
    
    // 使用SwarmBuilder构建节点管理器
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|config| {config.with_idle_connection_timeout(Duration::from_secs(30))})
        .build();

    // 根据配置启用IPv4监听
    if config.network.ipv4_enabled {
        let listen_addr_v4: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", port).parse()?;
        swarm.listen_on(listen_addr_v4)?;  // 绑定到所有IPv4接口的指定端口
    }
    
    // 根据配置启用IPv6监听
    if config.network.ipv6_enabled {
        let listen_addr_v6: Multiaddr = format!("/ip6/::/tcp/{}", port).parse()?;
        swarm.listen_on(listen_addr_v6)?;  // 绑定到所有IPv6接口的指定端口
    }
    
    // 如果有连接参数，尝试连接到指定的远程节点

    for connect_to in connect_list{
        match connect_to.parse::<Multiaddr>() {
            Ok(remote_addr) => {
                let log = LogStruct {
                    level: LogLevel::Important,
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
        content: format!("节点已启动，监听端口: {}", port),
    };
    log.logout();
    
    // 主事件循环 - 持续处理Swarm产生的事件
    loop {
        match swarm.select_next_some().await {
            // // 新的监听地址已添加
            // SwarmEvent::NewListenAddr { .. } => {}
            
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
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                let log = LogStruct {
                    level: LogLevel::Preset,
                    topic: "连接建立".to_string(),
                    content: format!("已连接到节点: {}", peer_id),
                };
                log.logout();
                
                let remote_addr = endpoint.get_remote_address();
                swarm.behaviour_mut().kademlia.add_address(&peer_id, remote_addr.clone());
                if config.services.kademlia.bootstrap_nodes.iter().any(|addr| 
                addr.contains(&peer_id.to_string())) {
                    match swarm.behaviour_mut().kademlia.bootstrap() {
                        Ok(query_id) => {
                            let log = LogStruct {
                                level: LogLevel::Important,
                                topic: "Kademlia引导".to_string(),
                                content: format!("启动Kademlia引导，查询ID: {:?}", query_id),
                            };
                            log.logout();

                            swarm.behaviour_mut().kademlia.get_closest_peers(peer_id);
                        }
                        Err(e) => {
                            let log = LogStruct {
                                level: LogLevel::Warning,
                                topic: "Kademlia引导失败".to_string(),
                                content: format!("无法启动引导: {:?}", e),
                            };
                            log.logout();
                        }
                    }
                }

                if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer_id) {
                    net_peer_list[index].addresses = Some(remote_addr.clone());
                    net_peer_list[index].connection_status = ConnectionStatus::Connected;
                } else {
                    let new_peer_info = PeerInfo {
                        peer_id,
                        name_string: None,
                        addresses: Some(remote_addr.clone()),
                        observed_addresses: None,
                        public_key: None,
                        rtt: None,
                        connection_status: ConnectionStatus::Connected,
                        supported_protocols: None,
                        agent_version: None,
                        score: Some(50),
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
                        net_peer_list.retain(|peer| peer.peer_id != peer_id);
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
                                    net_peer_list[index].connection_status = ConnectionStatus::Disconnected;
                                }
                            }
                        }
                    },
                    // Identify协议事件
                    NetBehaviourEvent::Identify(event) => match event {
                        // 收到其他节点的身份信息
                        libp2p::identify::Event::Received { peer_id, info, connection_id: _ } => {
                            let log = LogStruct {
                                level: LogLevel::Important,
                                topic: "身份交换".to_string(),
                                content: format!(
                                    "收到 {} 的身份信息\n监听地址: {:?}\n协议: {:?}\n版本: {}",
                                    peer_id, info.listen_addrs, info.protocols, info.agent_version
                                ),
                            };
                            log.logout();
                            // 确认身份并录入信息
                            if let Some(index) = net_peer_list.iter().position(|p| p.peer_id == peer_id) {
                                net_peer_list[index].agent_version = Some(info.agent_version.clone());
                                net_peer_list[index].observed_addresses = Some(info.observed_addr.clone());
                                net_peer_list[index].public_key = Some(info.public_key.clone());
                                net_peer_list[index].supported_protocols = Some(info.protocols.clone());
                                // 若为我的节点则加入连接表中
                                if config.services.kademlia.enabled
                                && format!("/OAHD/{}", env!("CARGO_PKG_VERSION")) == info.agent_version.clone()
                                && let Some(addr) = &net_peer_list[index].addresses 
                                && addr.iter().any(|proto| {matches!(proto, libp2p::multiaddr::Protocol::P2p(_))}){
                                    let addr_string = addr.to_string();
                                    config.insert_bootstrap_nodes(addr_string);
                                }
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
                                        Ok(_ok) => {}
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
                                        Ok(_ok) => {}
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
                                            let log = LogStruct {
                                                level: LogLevel::Debug,
                                                topic: "Kademlia发现节点".to_string(),
                                                content: format!("发现 {} 个最近节点", ok.peers.len()),
                                            };
                                            log.logout();
                                            
                                            // 将发现的节点添加到net_peer_list
                                            for peer_id in &ok.peers {
                                                if net_peer_list.iter().all(|p| p.peer_id != peer_id.peer_id) {
                                                    let new_peer = PeerInfo {
                                                        peer_id: peer_id.peer_id,
                                                        name_string: None,
                                                        addresses: None,
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
                }
            }
            // 其他事件（忽略）
            _ => {}
        }
    }
}