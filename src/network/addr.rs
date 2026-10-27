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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// Maximum time to wait for the configured address families at startup.
pub const ADDR_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for configured address families.
const ADDR_READY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Whether `ip` is a globally usable IPv4 address (not loopback, private or
/// link-local).
fn is_ready_ipv4(v4: Ipv4Addr) -> bool {
    !v4.is_unspecified()
        && !v4.is_loopback()
        && !v4.is_private()
        && !v4.is_link_local()
        && !v4.is_multicast()
        && !v4.is_broadcast()
}

/// Whether `ip` is a globally usable IPv6 address (not loopback, unspecified,
/// multicast, link-local or unique-local).
fn is_ready_ipv6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return false;
    }
    let octets = v6.octets();
    let link_local = octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80;
    let unique_local = (octets[0] & 0xfe) == 0xfc;
    !link_local && !unique_local
}

fn is_ready_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ready_ipv4(v4),
        IpAddr::V6(v6) => is_ready_ipv6(v6),
    }
}

/// Whether the addresses satisfy every address family enabled in the config.
pub fn has_ready_address(addrs: &[IpAddr], ipv4_enabled: bool, ipv6_enabled: bool) -> bool {
    if !ipv4_enabled && !ipv6_enabled {
        return true;
    }
    if ipv4_enabled
        && !addrs
            .iter()
            .any(|ip| matches!(ip, IpAddr::V4(v4) if is_ready_ipv4(*v4)))
    {
        return false;
    }
    if ipv6_enabled
        && !addrs
            .iter()
            .any(|ip| matches!(ip, IpAddr::V6(v6) if is_ready_ipv6(*v6)))
    {
        return false;
    }
    true
}

/// Poll `ready` until it succeeds or `timeout` elapses.
async fn wait_until<F>(mut ready: F, timeout: Duration, interval: Duration) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(interval.min(deadline - now)).await;
    }
}

/// Wait until every configured address family has a globally usable address.
///
/// Times out after [`ADDR_READY_TIMEOUT`] and lets startup continue: the
/// address probe and the swarm can still fall back to wildcard listeners.
pub async fn wait_for_configured_addresses(config: &ConfigHandle) -> bool {
    let (ipv4_enabled, ipv6_enabled) = {
        let cfg = config.read();
        (cfg.network.ipv4_enabled, cfg.network.ipv6_enabled)
    };
    if !ipv4_enabled && !ipv6_enabled {
        return true;
    }

    LogStruct::new(
        LogLevel::Preset,
        "等待网络地址族就绪",
        format!("IPv4: {ipv4_enabled}, IPv6: {ipv6_enabled}"),
    )
    .emit();

    let ready = wait_until(
        || has_ready_address(&get_public_ips(), ipv4_enabled, ipv6_enabled),
        ADDR_READY_TIMEOUT,
        ADDR_READY_POLL_INTERVAL,
    )
    .await;

    if ready {
        LogStruct::new(LogLevel::Preset, "网络地址族已就绪", "").emit();
    } else {
        LogStruct::new(
            LogLevel::Warning,
            "等待网络地址族超时",
            format!(
                "{}s 内未就绪 (IPv4: {ipv4_enabled}, IPv6: {ipv6_enabled})，继续启动",
                ADDR_READY_TIMEOUT.as_secs()
            ),
        )
        .emit();
    }
    ready
}

/// Collect this host's public IP addresses.
pub fn get_public_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    if let Ok(ifaces) = get_if_addrs::get_if_addrs() {
        for iface in ifaces {
            let ip = iface.addr.ip();
            if is_ready_ip(ip) {
                ips.push(ip);
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
    use crate::config::{NetworkConfig, NodeConfig};
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv6_only_accepts_global_address() {
        let addrs = [ip("2001:db8::1")];
        assert!(has_ready_address(&addrs, false, true));
    }

    #[test]
    fn ipv6_only_rejects_link_local_and_ula() {
        let link_local = [ip("fe80::1")];
        assert!(!has_ready_address(&link_local, false, true));

        let ula = [ip("fd00::1")];
        assert!(!has_ready_address(&ula, false, true));

        let loopback = [ip("::1")];
        assert!(!has_ready_address(&loopback, false, true));
    }

    #[test]
    fn ipv4_requires_non_private_address() {
        let public = [ip("8.8.8.8")];
        assert!(has_ready_address(&public, true, false));

        let private = [ip("192.168.1.2")];
        assert!(!has_ready_address(&private, true, false));

        let link_local = [ip("169.254.1.1")];
        assert!(!has_ready_address(&link_local, true, false));

        let loopback = [ip("127.0.0.1")];
        assert!(!has_ready_address(&loopback, true, false));
    }

    #[test]
    fn dual_stack_requires_both_families() {
        assert!(!has_ready_address(&[ip("2001:db8::1")], true, true));
        assert!(!has_ready_address(&[ip("8.8.8.8")], true, true));
        assert!(has_ready_address(
            &[ip("8.8.8.8"), ip("2001:db8::1")],
            true,
            true
        ));
    }

    #[test]
    fn no_configured_family_is_immediately_ready() {
        assert!(has_ready_address(&[], false, false));
    }

    #[tokio::test]
    async fn no_configured_family_skips_waiting() {
        let handle = ConfigHandle::new(NodeConfig::default());
        assert!(wait_for_configured_addresses(&handle).await);
    }

    #[tokio::test]
    async fn wait_returns_as_soon_as_address_family_is_ready() {
        let addrs = [ip("2001:db8::1")];
        assert!(
            wait_until(
                || has_ready_address(&addrs, false, true),
                Duration::from_secs(1),
                Duration::from_millis(5),
            )
            .await
        );
    }

    #[tokio::test]
    async fn wait_times_out_for_link_local_only() {
        let addrs = [ip("fe80::1")];
        assert!(
            !wait_until(
                || has_ready_address(&addrs, false, true),
                Duration::from_millis(20),
                Duration::from_millis(2),
            )
            .await
        );
    }

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
