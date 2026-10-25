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

//! Local public-address probing and Multiaddr conversion.

use crate::{LogLevel, LogStruct, config::ConfigHandle};
use libp2p::Multiaddr;
use std::net::IpAddr;

/// Collect this host's public IP addresses.
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

/// Convert an IP address and port into a Multiaddr.
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

/// Clear configured public addresses while preserving the listen-family flags.
fn clear_stale_public_addresses(network: &mut crate::config::NetworkConfig) -> bool {
    if network.ipv4_address.is_none() && network.ipv6_address.is_none() {
        return false;
    }
    network.ipv4_address = None;
    network.ipv6_address = None;
    true
}

/// Probe public IPs and write them back to the config.
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

    config.set_has_public_address(!public_ips.is_empty());

    if ipv4_addrs.is_empty() && ipv6_addrs.is_empty() {
        // Do not keep stale public addresses: binding them at startup fails
        // with AddrNotAvailable. Keep the enabled flags so the swarm can fall
        // back to a wildcard listener.
        let cleared = {
            let mut cfg = config.write();
            clear_stale_public_addresses(&mut cfg.network)
        };
        if cleared {
            config.save_to_default();
            LogStruct::new(
                LogLevel::Warning,
                "未发现公网IP",
                "已清空旧公网地址，将监听通配地址",
            )
            .emit();
        } else {
            LogStruct::new(LogLevel::Warning, "未发现公网IP", "无法自动更新网络配置").emit();
        }
        return Ok(());
    }

    {
        let cfg = config.read();
        if cfg.network.ipv4_address == ipv4_addrs.first().copied()
            && cfg.network.ipv6_address == ipv6_addrs.first().copied()
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
    }

    // Atomically persist to disk.
    config.save_to_default();
    LogStruct::new(
        LogLevel::Preset,
        "网络配置已自动更新",
        format!("IPv4: {:?}, IPv6: {:?}", ipv4_addrs, ipv6_addrs),
    )
    .emit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkConfig;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn clear_stale_public_addresses_keeps_listen_flags() {
        let mut network = NetworkConfig {
            ipv4_enabled: true,
            ipv6_enabled: true,
            ipv4_address: Some(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))),
            ipv6_address: Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            ..Default::default()
        };
        assert!(clear_stale_public_addresses(&mut network));
        assert!(network.ipv4_enabled);
        assert!(network.ipv6_enabled);
        assert!(network.ipv4_address.is_none());
        assert!(network.ipv6_address.is_none());
        assert!(!clear_stale_public_addresses(&mut network));
    }
}
