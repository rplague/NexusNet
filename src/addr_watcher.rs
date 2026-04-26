use crate::config::AddressWatcherConfig;
use crate::net::get_network_addresses;
use libp2p::core::Endpoint;
use libp2p::swarm::{
    ConnectionDenied, ConnectionId, FromSwarm, THandler, THandlerInEvent, THandlerOutEvent,
    behaviour::ToSwarm, dummy,
};
use libp2p::{Multiaddr, PeerId};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// 地址变化事件
#[derive(Debug, Clone)]
pub struct AddressChangeEvent {
    pub new_addrs: Vec<String>,
    pub old_addrs: Vec<String>,
}

/// AddrWatcher 的行为事件
#[derive(Debug)]
pub enum AddrWatcherEvent {
    Changed(AddressChangeEvent),
}

/// 自定义 libp2p behaviour：周期性检测本机 IP，发生变化时上报事件
///
/// 实现原理：
/// - 内部维护一个 interval timer（每 check_interval 触发一次）
/// - 触发时调用 get_network_addresses() 获取当前 IP
/// - 与 last_addrs 比较
/// - 变化时通过 GenerateEvent(AddrWatcherEvent::Changed(...)) 上报
pub struct Behaviour {
    enabled: bool,
    check_interval: Duration,
    last_check: Instant,
    last_addrs: Vec<String>,
}

impl Behaviour {
    pub fn new(config: &AddressWatcherConfig) -> Self {
        let initial = if config.enabled {
            let (v4, v6) = get_network_addresses().unwrap_or_default();
            let mut addrs = Vec::new();
            if !v4.is_empty() {
                addrs.push(v4);
            }
            if !v6.is_empty() {
                addrs.push(v6);
            }
            addrs
        } else {
            Vec::new()
        };

        Behaviour {
            enabled: config.enabled,
            check_interval: Duration::from_secs(config.check_interval_secs),
            last_check: Instant::now(),
            last_addrs: initial,
        }
    }
}

impl libp2p::swarm::NetworkBehaviour for Behaviour {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = AddrWatcherEvent;

    fn handle_established_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _peer: PeerId,
        _local_addr: &Multiaddr,
        _remote_addr: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn handle_established_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _peer: PeerId,
        _addr: &Multiaddr,
        _endpoint: Endpoint,
        _port_use: libp2p::core::transport::PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn on_swarm_event(&mut self, _event: FromSwarm) {}

    fn on_connection_handler_event(
        &mut self,
        _peer_id: PeerId,
        _connection_id: ConnectionId,
        _event: THandlerOutEvent<Self>,
    ) {
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        if !self.enabled {
            return Poll::Pending;
        }

        let now = Instant::now();
        if now < self.last_check + self.check_interval {
            let remaining = self.last_check + self.check_interval - now;
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                tokio::time::sleep(remaining).await;
                waker.wake();
            });
            return Poll::Pending;
        }

        self.last_check = now;

        let (v4, v6) = match get_network_addresses() {
            Ok(addrs) => addrs,
            Err(_) => return Poll::Pending,
        };

        let mut current_addrs = Vec::new();
        if !v4.is_empty() {
            current_addrs.push(v4);
        }
        if !v6.is_empty() {
            current_addrs.push(v6);
        }

        if current_addrs == self.last_addrs {
            return Poll::Pending;
        }

        let old_addrs = std::mem::replace(&mut self.last_addrs, current_addrs.clone());

        Poll::Ready(ToSwarm::GenerateEvent(AddrWatcherEvent::Changed(
            AddressChangeEvent {
                new_addrs: current_addrs,
                old_addrs,
            },
        )))
    }

    fn handle_pending_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _local_addr: &Multiaddr,
        _remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        Ok(())
    }

    fn handle_pending_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        _maybe_peer: Option<PeerId>,
        _addresses: &[Multiaddr],
        _effective_role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        Ok(vec![])
    }
}
