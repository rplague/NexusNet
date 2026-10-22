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

//! Legacy unsigned type-index mirror.
//!
//! When no `types_authority` is configured, NexusNet keeps publishing the old
//! `/oahd/service/types` JSON array so older nodes can still discover service
//! types. The signed `/oahd/service/authtypes/` index remains the preferred
//! path when an authority is configured.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libp2p::kad;

use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;
use crate::types_doc;

struct MirrorState {
    last: Option<Vec<String>>,
    next_publish: Instant,
}

pub struct LegacyTypesPublisher {
    swarm: NetworkHandle,
    interval: Duration,
    max_bytes: usize,
    state: Mutex<MirrorState>,
    inflight: AtomicBool,
}

impl LegacyTypesPublisher {
    pub fn new(swarm: NetworkHandle, interval: Duration, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            swarm,
            interval,
            max_bytes,
            state: Mutex::new(MirrorState {
                last: None,
                next_publish: Instant::now(),
            }),
            inflight: AtomicBool::new(false),
        })
    }

    /// Publish the legacy mirror when the set changed or the refresh interval
    /// elapsed. Returns immediately; the write runs in the background.
    pub fn maybe_publish(self: &Arc<Self>, services: BTreeSet<String>) {
        let list: Vec<String> = services.into_iter().collect();
        let Some((value, included)) = types_doc::encode_legacy_types_capped(&list, self.max_bytes)
        else {
            return;
        };
        let published: Vec<String> = list[..included].to_vec();
        let now = Instant::now();
        {
            let state = self.state.lock().expect("legacy types state poisoned");
            if state.last.as_deref() == Some(published.as_slice()) && now < state.next_publish {
                return;
            }
            if published.is_empty() && state.last.is_none() {
                return;
            }
        }
        if self.inflight.swap(true, Ordering::SeqCst) {
            return;
        }
        if included < list.len() {
            LogStruct::new(
                LogLevel::Warning,
                "旧版类型镜像已裁剪",
                format!(
                    "{included}/{} 个服务，超出 compat.max_record_bytes",
                    list.len()
                ),
            )
            .emit();
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this
                .swarm
                .put_record(
                    kad::RecordKey::new(&types_doc::LEGACY_TYPES_KEY),
                    value,
                    kad::Quorum::One,
                )
                .await;
            match result {
                Ok(()) => {
                    let mut state = this.state.lock().expect("legacy types state poisoned");
                    state.last = Some(published);
                    state.next_publish = Instant::now() + this.interval;
                }
                Err(error) => {
                    let mut state = this.state.lock().expect("legacy types state poisoned");
                    state.next_publish = Instant::now() + Duration::from_secs(30);
                    drop(state);
                    LogStruct::new(LogLevel::Warning, "旧版类型镜像发布失败", error.to_string())
                        .emit();
                }
            }
            this.inflight.store(false, Ordering::SeqCst);
        });
    }
}
