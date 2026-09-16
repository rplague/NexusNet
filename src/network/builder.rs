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

//! Swarm 构建：传输层 + 行为装配 + 监听地址。

use crate::config::ConfigHandle;
use crate::network::NetworkError;
use crate::network::addr::to_multiaddr;
use crate::network::behaviour::NetBehaviour;
use libp2p::{Multiaddr, Swarm, SwarmBuilder, identity, noise, tcp, yamux};
use std::net::IpAddr;
use std::time::Duration;

/// 依据当前配置构建一个全新的 Swarm 并开始监听。
///
/// 该函数同时用于首次启动与 `reload()` 重建，因此不持有任何长期状态。
pub fn build_swarm(
    config: &ConfigHandle,
    keypair: &identity::Keypair,
) -> Result<Swarm<NetBehaviour>, NetworkError> {
    let config_clone = config.clone();

    let listen_addrs: Vec<Multiaddr> = {
        let cfg = config.read();
        let mut addrs = Vec::new();
        if cfg.network.ipv4_enabled {
            let ip = cfg
                .network
                .ipv4_address
                .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
            addrs.push(to_multiaddr(ip, cfg.network.port as u16));
        }
        if cfg.network.ipv6_enabled {
            let ip = cfg
                .network
                .ipv6_address
                .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
            addrs.push(to_multiaddr(ip, cfg.network.port as u16));
        }
        addrs
    };

    let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| NetworkError::Build(e.to_string()))?
        .with_relay_client(noise::Config::new, yamux::Config::default)
        .map_err(|e| NetworkError::Build(e.to_string()))?
        .with_behaviour(|keypair, relay_client| {
            NetBehaviour::new(&config_clone, keypair, relay_client)
        })
        .map_err(|e| NetworkError::Build(e.to_string()))?
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    for addr in listen_addrs {
        swarm
            .listen_on(addr)
            .map_err(|e| NetworkError::Build(e.to_string()))?;
    }

    Ok(swarm)
}
