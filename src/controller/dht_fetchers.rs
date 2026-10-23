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

//! Cache fetchers for plain DHT records and provider lists.

use libp2p::PeerId;
use libp2p::futures::future::BoxFuture;
use libp2p::kad;

use crate::dht_cache::{Fetched, Fetcher};
use crate::network::NetworkHandle;

/// Fetch a plain record, preferring values obtained from the network over the
/// local Kademlia store copy.
pub struct GenericRecordFetcher {
    pub swarm: NetworkHandle,
}

impl Fetcher<kad::RecordKey, Vec<u8>> for GenericRecordFetcher {
    fn fetch<'a>(
        &'a self,
        key: &'a kad::RecordKey,
    ) -> BoxFuture<'a, Result<Option<Fetched<Vec<u8>>>, String>> {
        Box::pin(async move {
            let records = self
                .swarm
                .get_record_all(key.clone())
                .await
                .map_err(|e| e.to_string())?;
            let selected = records
                .iter()
                .rev()
                .find(|record| record.peer.is_some())
                .or_else(|| records.last());
            Ok(selected.map(|record| Fetched {
                value: record.record.value.clone(),
                version: None,
                size: record.record.value.len(),
            }))
        })
    }
}

/// Fetch the provider list for a key.
pub struct ProviderFetcher {
    pub swarm: NetworkHandle,
}

impl Fetcher<kad::RecordKey, Vec<PeerId>> for ProviderFetcher {
    fn fetch<'a>(
        &'a self,
        key: &'a kad::RecordKey,
    ) -> BoxFuture<'a, Result<Option<Fetched<Vec<PeerId>>>, String>> {
        Box::pin(async move {
            let providers = self
                .swarm
                .get_providers(key.clone())
                .await
                .map_err(|e| e.to_string())?;
            let size = providers.len() * std::mem::size_of::<PeerId>();
            Ok(Some(Fetched {
                value: providers,
                version: None,
                size,
            }))
        })
    }
}
