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

//! 本地公网地址探测与 Multiaddr 转换。

use crate::{LogLevel, LogStruct, config::ConfigHandle};
use libp2p::Multiaddr;
use std::net::IpAddr;

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

/// 探测公网 IP 并写回配置，同时更新 announce_addresses。
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
    LogStruct::new(
        LogLevel::Preset,
        "网络配置已自动更新",
        format!(
            "IPv4: {:?}, IPv6: {:?}, 公告地址: {:?}",
            ipv4_addrs, ipv6_addrs, announce_addrs
        ),
    )
    .emit();
    Ok(())
}
