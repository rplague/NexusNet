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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use libp2p::{PeerId, ping};

/// Tracks per-peer round-trip times and consecutive ping failures.
#[derive(Default)]
pub struct PeerTracker {
    rtts: HashMap<PeerId, Duration>,
    failures: HashMap<PeerId, u32>,
}

impl PeerTracker {
    /// Seed the local peer with a zero RTT.
    pub fn new(my_peer_id: PeerId) -> Self {
        let mut rtts = HashMap::new();
        rtts.insert(my_peer_id, Duration::ZERO);
        Self {
            rtts,
            failures: HashMap::new(),
        }
    }

    /// Update tracking from a ping event.
    ///
    /// Returns the peer to disconnect once its consecutive failures reach
    /// `max_failures`; `max_failures == 0` disables disconnection.
    pub fn on_ping(&mut self, event: ping::Event, max_failures: u32) -> Option<PeerId> {
        match event {
            ping::Event {
                peer,
                result: Ok(rtt),
                ..
            } => {
                self.rtts.insert(peer, rtt);
                self.failures.remove(&peer);
                None
            }
            ping::Event { peer, .. } => {
                self.rtts.remove(&peer);
                if max_failures == 0 {
                    return None;
                }
                let count = self.failures.entry(peer).or_insert(0);
                *count += 1;
                if *count >= max_failures {
                    self.failures.remove(&peer);
                    Some(peer)
                } else {
                    None
                }
            }
        }
    }

    /// Rank providers by known RTT, then keep unknown peers in input order.
    pub fn ranked_peers(&self, providers: &[PeerId]) -> Vec<PeerId> {
        let mut ranked: Vec<PeerId> = Vec::new();
        let mut seen: HashSet<PeerId> = HashSet::new();
        for peer in providers {
            if seen.insert(*peer) {
                ranked.push(*peer);
            }
        }
        ranked.sort_by_key(|peer| match self.rtts.get(peer) {
            Some(rtt) => (0u8, *rtt),
            None => (1u8, Duration::ZERO),
        });
        ranked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtt(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn ranked_peers_picks_lowest_rtt_first() {
        let mut tracker = PeerTracker::default();
        let a = PeerId::random();
        let b = PeerId::random();
        let c = PeerId::random();
        tracker.rtts.insert(a, rtt(50));
        tracker.rtts.insert(b, rtt(10));
        assert_eq!(tracker.ranked_peers(&[a, b, c]), vec![b, a, c]);
    }

    #[test]
    fn ranked_peers_keeps_unknown_order() {
        let tracker = PeerTracker::default();
        let a = PeerId::random();
        let b = PeerId::random();
        assert_eq!(tracker.ranked_peers(&[a, b]), vec![a, b]);
    }

    #[test]
    fn ranked_peers_orders_known_rtt_then_unknown() {
        let mut tracker = PeerTracker::default();
        let fast = PeerId::random();
        let slow = PeerId::random();
        let unknown_a = PeerId::random();
        let unknown_b = PeerId::random();
        tracker.rtts.insert(fast, rtt(10));
        tracker.rtts.insert(slow, rtt(100));

        assert_eq!(
            tracker.ranked_peers(&[unknown_a, slow, unknown_b, fast]),
            vec![fast, slow, unknown_a, unknown_b]
        );
    }

    #[test]
    fn ranked_peers_deduplicates() {
        let mut tracker = PeerTracker::default();
        let peer = PeerId::random();
        tracker.rtts.insert(peer, rtt(10));
        assert_eq!(tracker.ranked_peers(&[peer, peer]), vec![peer]);
    }
}
