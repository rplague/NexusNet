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

use std::collections::BTreeSet;

use libp2p::PeerId;
use libp2p::futures::future::BoxFuture;
use libp2p::identity::ed25519;
use libp2p::kad;

use crate::dht_cache::{Fetched, Fetcher};
use crate::network::NetworkHandle;
use crate::types_doc;

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
                expires_at: None,
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
                expires_at: None,
                size,
            }))
        })
    }
}

/// Fetch and verify the signed global service-type index.
pub struct TypesFetcher {
    pub swarm: NetworkHandle,
    pub authority: ed25519::PublicKey,
    pub max_ttl_seconds: u64,
}

impl Fetcher<(), BTreeSet<String>> for TypesFetcher {
    fn fetch<'a>(
        &'a self,
        _key: &'a (),
    ) -> BoxFuture<'a, Result<Option<Fetched<BTreeSet<String>>>, String>> {
        Box::pin(async move {
            let records = self
                .swarm
                .get_record_all(kad::RecordKey::new(&types_doc::TYPES_KEY))
                .await
                .map_err(|e| e.to_string())?;
            let selected = types_doc::select_types(
                &records,
                &self.authority,
                crate::auth::now_unix(),
                self.max_ttl_seconds,
            )
            .map_err(|e| e.to_string())?;
            Ok(selected.map(|(doc, size)| Fetched {
                value: doc.services.into_iter().collect(),
                version: Some(doc.version),
                expires_at: Some(doc.expires_at),
                size,
            }))
        })
    }
}

/// Read the legacy unsigned `/oahd/service/types` JSON array.
///
/// This is a compatibility path: new nodes use it only when the signed index is
/// unavailable or `types_authority` is not configured. All records are unioned
/// to match the old nodes' monotonic-union behavior.
pub struct LegacyTypesFetcher {
    pub swarm: NetworkHandle,
}

impl Fetcher<(), BTreeSet<String>> for LegacyTypesFetcher {
    fn fetch<'a>(
        &'a self,
        _key: &'a (),
    ) -> BoxFuture<'a, Result<Option<Fetched<BTreeSet<String>>>, String>> {
        Box::pin(async move {
            let records = self
                .swarm
                .get_record_all(kad::RecordKey::new(&types_doc::LEGACY_TYPES_KEY))
                .await
                .map_err(|e| e.to_string())?;
            let mut services = BTreeSet::new();
            let mut saw_valid = false;
            let mut size = 0;
            for record in &records {
                let Some(names) = types_doc::parse_legacy_types(&record.record.value) else {
                    continue;
                };
                saw_valid = true;
                size = size.max(record.record.value.len());
                for name in names {
                    if services.len() >= types_doc::MAX_TYPES {
                        break;
                    }
                    services.insert(name);
                }
            }
            if !saw_valid {
                return Ok(None);
            }
            Ok(Some(Fetched {
                value: services,
                version: None,
                expires_at: None,
                size,
            }))
        })
    }
}
