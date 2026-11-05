//! Minimal NexusNet node.
//!
//! Starts one node with a generated identity, prints its PeerId, and runs
//! until Ctrl-C. All files live in a temporary directory.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example basic_node
//! ```

use std::path::PathBuf;

use nexusnet::config::{ConfigHandle, NodeConfig};
use nexusnet::runtime::{NodeOptions, start_node};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read the ephemeral port")
        .port()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf = std::env::temp_dir().join("nexusnet-basic-node");
    std::fs::create_dir_all(&dir)?;

    let mut config = NodeConfig::default();
    config.node.name = "nexusnet-basic-node".to_string();
    config.node.description = "NexusNet basic example".to_string();
    config.network.ipv4_enabled = true;
    config.network.ipv4_address = Some("127.0.0.1".parse()?);
    config.network.port = u32::from(free_port());
    config.crypto.pq_transport_enabled = false;
    config.crypto.pq_identity_enabled = false;

    let config_path = dir.join("config.toml");
    let keypair_path = dir.join("keypair.bin");
    ConfigHandle::new(config)
        .save_to_path(&config_path)
        .map_err(std::io::Error::other)?;

    let node = start_node(NodeOptions {
        config_path,
        keypair_path,
        log_sink: None,
        update_public_ip: false,
        wait_for_addresses: false,
    })
    .await?;

    println!("node started");
    println!("  peer id: {}", node.peer_id());
    println!("  data:    {}", dir.display());
    println!("press Ctrl-C to stop");

    tokio::signal::ctrl_c().await?;
    node.shutdown().await;
    Ok(())
}
