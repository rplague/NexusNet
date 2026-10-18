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

use crate::{
    LogLevel, LogStruct,
    config::{ConfigHandle, NodeConfig},
};
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

/// Probe public IPs and write them back to the config.
pub fn update_config_with_public_ip(
    config: &ConfigHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let public_ips = get_public_ips();
    let ipv4 = public_ips.iter().find(|ip| ip.is_ipv4()).copied();
    let ipv6 = public_ips.iter().find(|ip| ip.is_ipv6()).copied();

    if ipv4.is_none() && ipv6.is_none() {
        LogStruct::new(LogLevel::Warning, "未发现公网IP", "保留现有网络配置").emit();
        return Ok(());
    }

    let changed = {
        let mut cfg = config.write();
        apply_public_ips(&mut cfg, ipv4, ipv6)
    };
    if !changed {
        return Ok(());
    }

    config.save_to_default();
    LogStruct::new(
        LogLevel::Preset,
        "网络配置已自动更新",
        format!("IPv4: {ipv4:?}, IPv6: {ipv6:?}"),
    )
    .emit();
    Ok(())
}

fn apply_public_ips(config: &mut NodeConfig, ipv4: Option<IpAddr>, ipv6: Option<IpAddr>) -> bool {
    let before = (
        config.network.ipv4_enabled,
        config.network.ipv4_address,
        config.network.ipv6_enabled,
        config.network.ipv6_address,
    );

    if let Some(ip) = ipv4
        && config.network.ipv4_address.is_none()
    {
        config.network.ipv4_address = Some(ip);
        config.network.ipv4_enabled = true;
    }
    if let Some(ip) = ipv6
        && config.network.ipv6_address.is_none()
    {
        config.network.ipv6_address = Some(ip);
        config.network.ipv6_enabled = true;
    }

    before
        != (
            config.network.ipv4_enabled,
            config.network.ipv4_address,
            config.network.ipv6_enabled,
            config.network.ipv6_address,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeConfig;

    fn ipv4(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, last))
    }

    fn ipv6(last: u16) -> IpAddr {
        IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last))
    }

    #[test]
    fn explicit_address_is_not_overwritten() {
        let mut config = NodeConfig::default();
        config.network.ipv4_enabled = true;
        config.network.ipv4_address = Some(ipv4(10));
        assert!(!apply_public_ips(&mut config, Some(ipv4(20)), None));
        assert_eq!(config.network.ipv4_address, Some(ipv4(10)));
    }

    #[test]
    fn unset_address_is_filled_and_enabled() {
        let mut config = NodeConfig::default();
        assert!(apply_public_ips(&mut config, Some(ipv4(20)), None));
        assert!(config.network.ipv4_enabled);
        assert_eq!(config.network.ipv4_address, Some(ipv4(20)));
    }

    #[test]
    fn missing_family_does_not_disable_existing_config() {
        let mut config = NodeConfig::default();
        config.network.ipv4_enabled = true;
        config.network.ipv4_address = Some(ipv4(10));
        config.network.ipv6_enabled = true;
        config.network.ipv6_address = Some(ipv6(1));
        assert!(!apply_public_ips(&mut config, None, None));
        assert!(config.network.ipv4_enabled);
        assert!(config.network.ipv6_enabled);
    }

    #[test]
    fn one_family_can_be_updated_without_touching_the_other() {
        let mut config = NodeConfig::default();
        config.network.ipv4_enabled = true;
        config.network.ipv4_address = Some(ipv4(10));
        assert!(apply_public_ips(&mut config, Some(ipv4(20)), Some(ipv6(2))));
        assert_eq!(config.network.ipv4_address, Some(ipv4(10)));
        assert!(config.network.ipv6_enabled);
        assert_eq!(config.network.ipv6_address, Some(ipv6(2)));
    }
}
