//! Two nodes talking to each other over the sidecar protocol.
//!
//! Node B runs a tiny in-process `echo` backend. Node A dials B as its
//! bootstrap peer and calls the service with `service_request_to`. The call
//! returns the backend's reply.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example two_nodes
//! ```

use std::io;
use std::path::Path;
use std::time::Duration;

use nexusnet::config::{ConfigHandle, LocalServiceEntry, NodeConfig};
use nexusnet::network::KeyManager;
use nexusnet::runtime::{NodeOptions, start_node};
use nexusnet::sidecar_protocol::{
    MAX_FRAME, Message, PROTOCOL_VERSION, decode_frame, encode_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the ephemeral port")
        .port()
}

fn node_config(name: &str, port: u16) -> NodeConfig {
    let mut config = NodeConfig::default();
    config.node.name = name.to_string();
    config.node.description = "NexusNet two-node example".to_string();
    config.network.ipv4_enabled = true;
    config.network.ipv4_address = Some("127.0.0.1".parse().expect("valid loopback address"));
    config.network.port = u32::from(port);
    config.crypto.pq_transport_enabled = false;
    config.crypto.pq_identity_enabled = false;
    config
}

fn node_options(dir: &Path) -> NodeOptions {
    NodeOptions {
        config_path: dir.join("config.toml"),
        keypair_path: dir.join("keypair.bin"),
        log_sink: None,
        update_public_ip: false,
        wait_for_addresses: false,
    }
}

fn save_config(config: NodeConfig, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    ConfigHandle::new(config)
        .save_to_path(path)
        .map_err(io::Error::other)?;
    Ok(())
}

async fn write_message(stream: &mut TcpStream, msg: &Message) -> io::Result<()> {
    let frame = encode_frame(msg).map_err(io::Error::other)?;
    stream.write_all(&frame).await
}

async fn read_message(stream: &mut TcpStream) -> io::Result<Option<Message>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(format!("frame too large: {len} bytes")));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    decode_frame(&body).map(Some).map_err(io::Error::other)
}

async fn serve_echo(listener: TcpListener) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            if write_message(
                &mut stream,
                &Message::Hello {
                    version: PROTOCOL_VERSION,
                },
            )
            .await
            .is_err()
            {
                return;
            }
            match read_message(&mut stream).await {
                Ok(Some(Message::Hello { version })) if version == PROTOCOL_VERSION => {}
                _ => return,
            }
            while let Ok(Some(msg)) = read_message(&mut stream).await {
                let Message::Request { id, payload, .. } = msg else {
                    continue;
                };
                let reply = Message::Reply {
                    id,
                    ok: true,
                    result: Some(payload),
                    error: None,
                };
                if write_message(&mut stream, &reply).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join("nexusnet-two-nodes");
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::create_dir_all(root.join("b"))?;

    let backend_listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let backend_port = backend_listener.local_addr()?.port();
    tokio::spawn(serve_echo(backend_listener));

    let bootstrap_port = free_port();
    let caller_port = free_port();
    let provider_keypair = root.join("b/keypair.bin");
    let provider_peer = KeyManager::load_or_create(&provider_keypair)?.peer_id();

    let mut provider_config = node_config("nexusnet-two-nodes-b", bootstrap_port);
    provider_config.services.dispatcher.local_services = vec![LocalServiceEntry {
        name: "echo".to_string(),
        port: backend_port,
        require_auth: false,
        publish: true,
    }];
    save_config(provider_config, &root.join("b/config.toml"))?;

    let mut caller_config = node_config("nexusnet-two-nodes-a", caller_port);
    caller_config.services.kademlia.bootstrap_nodes =
        vec![format!("/ip4/127.0.0.1/tcp/{bootstrap_port}/ws/p2p/{provider_peer}").parse()?];
    save_config(caller_config, &root.join("a/config.toml"))?;

    let provider = start_node(node_options(&root.join("b"))).await?;
    let caller = start_node(node_options(&root.join("a"))).await?;

    println!("provider peer: {}", provider.peer_id());
    println!("caller peer:   {}", caller.peer_id());
    println!("calling echo service on the provider...");

    let payload = b"hello from nexusnet".to_vec();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut echoed = None;
    let mut last_error = None;

    while tokio::time::Instant::now() < deadline {
        let request = Message::ServiceRequestTo {
            id: Uuid::new_v4(),
            service: "echo".to_string(),
            peer: provider.peer_id().to_string(),
            payload: payload.clone(),
        };
        match tokio::time::timeout(Duration::from_secs(10), caller.control(request)).await {
            Ok(Ok(reply)) if reply == payload => {
                echoed = Some(reply);
                break;
            }
            Ok(Ok(reply)) => {
                last_error = Some(format!("unexpected reply: {} bytes", reply.len()));
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => last_error = Some("request timed out".to_string()),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    caller.shutdown().await;
    provider.shutdown().await;

    match echoed {
        Some(reply) => {
            println!("echo reply:    {}", String::from_utf8_lossy(&reply));
            Ok(())
        }
        None => Err(io::Error::other(
            last_error.unwrap_or_else(|| "no reply before timeout".to_string()),
        )
        .into()),
    }
}
