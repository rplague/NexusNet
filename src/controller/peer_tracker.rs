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

use std::collections::HashMap;
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

    /// Pick the provider with the lowest known RTT.
    pub fn best_peer(&self, providers: &[PeerId]) -> Option<PeerId> {
        providers
            .iter()
            .filter_map(|p| self.rtts.get(p).map(|rtt| (p, rtt)))
            .min_by_key(|(_, rtt)| *rtt)
            .map(|(p, _)| *p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtt(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn best_peer_picks_lowest_rtt() {
        let mut tracker = PeerTracker::default();
        let a = PeerId::random();
        let b = PeerId::random();
        let c = PeerId::random();
        tracker.rtts.insert(a, rtt(50));
        tracker.rtts.insert(b, rtt(10));
        // c is unknown and must be ignored.
        assert_eq!(tracker.best_peer(&[a, b, c]), Some(b));
    }

    #[test]
    fn best_peer_none_when_all_unknown() {
        let tracker = PeerTracker::default();
        assert_eq!(tracker.best_peer(&[PeerId::random()]), None);
    }
}
