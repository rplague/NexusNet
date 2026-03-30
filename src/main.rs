mod log;
use log::LogLevel;
use log::LogStruct;

mod config;
use config::read_config_file;

mod net;
use net::get_network_addresses;

use libp2p::{
    identity,
    tcp,
    noise,
    yamux,
    swarm::SwarmEvent,
    SwarmBuilder,
    PeerId,
    Multiaddr,
    Transport,
};
use libp2p::futures::StreamExt;

use std::env;
use std::error::Error;
use std::fs;
use tokio;

fn main() -> Result<(), Box<dyn Error>> {
    // 读取设置
    let mut config = read_config_file().unwrap();

    // 录入设置参数
    let mut port = config.network.port;
    let mut connect_list:Option<&mut Vec<String>> = None;
    if config.services.discovery.enabled {
        connect_list = Some(&mut config.services.discovery.bootstrap_nodes);
    };
    let mut connect_to: Option<String> = None;
    if config.services.discovery.enabled {
        connect_to = connect_list.and_then(|list| {
            if !list.is_empty() {
                Some(list[0].clone())
            } else {
                None
            }
        });
    };

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
                    connect_to = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    // 各个参数检查和报告
    if connect_to == None {
        let log = LogStruct {
            level: LogLevel::Warning,
            topic: "没有设置连接目标".to_string(),
            content: "节点将会被动监听……".to_string(),
        };
        log.logout();
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        run_node(port, connect_to).await
    })
}

async fn run_node(port: u16, connect_to: Option<String>) -> Result<(), Box<dyn Error>> {
    let keypair = if std::path::Path::new("keypair.bin").exists() {
        let bytes = fs::read("keypair.bin")?;
        identity::Keypair::from_protobuf_encoding(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    } else {
        let keypair = identity::Keypair::generate_ed25519();
        let encoded = keypair.to_protobuf_encoding()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write("keypair.bin", &encoded)?;
        
        keypair
    };
    
    let peer_id = PeerId::from(keypair.public());
    let log = LogStruct {
        level: LogLevel::Important,
        topic: format!("节点 Peer ID: {}", peer_id),
        content: "".to_string(),
    };
    log.logout();
    
    // 创建网络传输层
    let transport = tcp::tokio::Transport::default()
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(noise::Config::new(&keypair)?)
        .multiplex(yamux::Config::default())
        .boxed();
    
    // 创建网络行为
    let behaviour = libp2p::ping::Behaviour::new(libp2p::ping::Config::new());
    
    // 创建 Swarm
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|_| behaviour)?
        .build();
    
    // 监听指定端口
    let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", port).parse()?;
    swarm.listen_on(listen_addr.clone())?;
    
    // 如果有连接参数，尝试连接
    if let Some(remote_addr_str) = connect_to {
        match remote_addr_str.parse::<Multiaddr>() {
            Ok(remote_addr) => {
                let log = LogStruct {
                    level: LogLevel::Important,
                    topic: "尝试连接".to_string(),
                    content: format!("尝试连接到: {}", remote_addr),
                };
                log.logout();
                
                match swarm.dial(remote_addr) {
                    Ok(_) => {
                        let log = LogStruct {
                            level: LogLevel::Important,
                            topic: "连接请求已发送".to_string(),
                            content: "连接请求已发送，等待对方接受...".to_string(),
                        };
                        log.logout();
                    }
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
                let log = LogStruct {
                    level: LogLevel::Error,
                    topic: "解析地址失败".to_string(),
                    content: format!("无法解析连接地址 '{}': {:?}", remote_addr_str, e),
                };
                log.logout();
            }
        }
    }
    
    // 主事件循环
    let log = LogStruct {
        level: LogLevel::Important,
        topic: "节点启动".to_string(),
        content: format!("节点已启动，监听端口: {}", port),
    };
    log.logout();
    
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => {
                let log = LogStruct {
                    level: LogLevel::Preset,
                    topic: format!("新增监听地址: {}", address),
                    content: "".to_string(),
                };
                log.logout();
            }
            
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                let log = LogStruct {
                    level: LogLevel::Important,
                    topic: "连接建立".to_string(),
                    content: format!("已连接到节点: {}", peer_id),
                };
                log.logout();
            }
            
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                let log = LogStruct {
                    level: LogLevel::Warning,
                    topic: "连接关闭".to_string(),
                    content: format!("连接关闭: {}", peer_id),
                };
                log.logout();
            }
            
            SwarmEvent::Behaviour(event) => {
                match event {
                    libp2p::ping::Event { peer, result, connection: _ } => match result {
                        Ok(rtt) => {
                            let log = LogStruct {
                                level: LogLevel::Debug,
                                topic: "Ping 响应".to_string(),
                                content: format!("收到 {} 的 Ping，延迟: {:?}", peer, rtt),
                            };
                            log.logout();
                        }
                        Err(e) => {
                            let log = LogStruct {
                                level: LogLevel::Warning,
                                topic: "Ping 失败".to_string(),
                                content: format!("Ping 失败 {}: {:?}", peer, e),
                            };
                            log.logout();
                        }
                    },
                }
            }
            
            _ => {}
        }
    }
}