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

//! Read-through DHT cache.
//!
//! The contract is deliberately small:
//!
//! - A cached value is returned immediately.
//! - If no refresh is in flight for that key, one is started in the background.
//! - The background result replaces the cached entry when it completes.
//! - If there is no cached value yet (cold start), the caller waits for the
//!   first refresh and receives either the value or the real fetch error.
//!
//! There are no freshness windows, per-cache permits, cache-level timeouts or
//! failure backoffs. Concurrency control is per key: at most one refresh.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use libp2p::futures::future::BoxFuture;
use tokio::sync::watch;

use crate::log::{LogLevel, LogStruct};

/// A value fetched from the network, already verified by the fetcher.
pub struct Fetched<V> {
    pub value: V,
    pub version: Option<u64>,
    /// Approximate size in bytes, used for cache accounting.
    pub size: usize,
}

/// Network access + validation for one cache namespace.
pub trait Fetcher<K, V>: Send + Sync + 'static {
    fn fetch<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<Option<Fetched<V>>, String>>;
}

/// Result of a cache read.
#[allow(dead_code)]
pub struct CacheRead<V> {
    pub value: Option<V>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CacheError {
    Unavailable(String),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Unavailable(reason) => write!(f, "DHT value unavailable: {reason}"),
        }
    }
}

impl std::error::Error for CacheError {}

#[derive(Clone)]
struct Entry<V> {
    /// `Some` = a value was found; `None` = a successful lookup found nothing.
    value: Option<V>,
    version: Option<u64>,
    last_error: Option<String>,
    last_access: Instant,
    size: usize,
}

type RefreshReceiver = watch::Receiver<Option<Result<(), String>>>;

/// Read-through DHT cache with per-key single-flight refreshes.
pub struct DhtCache<K, V, F> {
    entries: Mutex<HashMap<K, Entry<V>>>,
    inflight: Mutex<HashMap<K, RefreshReceiver>>,
    global_generation: AtomicU64,
    key_generation: Mutex<HashMap<K, u64>>,
    fetcher: Arc<F>,
    max_entries: usize,
    max_bytes: usize,
}

impl<K, V, F> DhtCache<K, V, F>
where
    K: Clone + Eq + Hash + fmt::Debug + Send + Sync + 'static,
    V: Clone + PartialEq + Send + Sync + 'static,
    F: Fetcher<K, V>,
{
    pub fn new(fetcher: Arc<F>, max_entries: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            global_generation: AtomicU64::new(0),
            key_generation: Mutex::new(HashMap::new()),
            fetcher,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
        })
    }

    /// Return the last cached value without starting a refresh.
    pub fn peek(&self, key: &K) -> Option<CacheRead<V>> {
        self.entry(key).map(read_from)
    }

    /// Return the last cached value and schedule a background refresh.
    ///
    /// If there is no cached value yet, wait for the first refresh.
    pub async fn get(self: &Arc<Self>, key: K) -> Result<CacheRead<V>, CacheError> {
        if let Some(entry) = self.entry(&key) {
            self.refresh(key);
            return Ok(read_from(entry));
        }

        let mut rx = self.ensure_refresh(key.clone());
        let outcome = rx.wait_for(|value| value.is_some()).await;
        let outcome = match outcome {
            Ok(value) => value.clone(),
            Err(_) => None,
        };

        match outcome {
            Some(Ok(())) => match self.entry(&key) {
                Some(entry) => Ok(read_from(entry)),
                None => Err(CacheError::Unavailable("cache invalidated".to_string())),
            },
            Some(Err(error)) => Err(CacheError::Unavailable(error)),
            None => Err(CacheError::Unavailable(
                "refresh task exited without a result".to_string(),
            )),
        }
    }

    /// Start a background refresh unless one is already running for this key.
    pub fn refresh(self: &Arc<Self>, key: K) {
        let _ = self.ensure_refresh(key);
    }

    /// Drop one key and invalidate any refresh currently in flight for it.
    pub fn invalidate(&self, key: &K) {
        {
            let mut keys = self
                .key_generation
                .lock()
                .expect("dht cache key generation poisoned");
            let generation = keys.entry(key.clone()).or_insert(0);
            *generation = generation.wrapping_add(1);
        }
        self.entries
            .lock()
            .expect("dht cache entries poisoned")
            .remove(key);
    }

    /// Drop everything, e.g. after a config reload or `refresh_cache`.
    pub fn invalidate_all(&self) {
        self.global_generation.fetch_add(1, Ordering::Relaxed);
        self.entries
            .lock()
            .expect("dht cache entries poisoned")
            .clear();
        self.key_generation
            .lock()
            .expect("dht cache key generation poisoned")
            .clear();
    }

    fn ensure_refresh(self: &Arc<Self>, key: K) -> RefreshReceiver {
        let mut inflight = self.inflight.lock().expect("dht cache inflight poisoned");
        if let Some(rx) = inflight.get(&key) {
            return rx.clone();
        }

        let (tx, rx) = watch::channel(None);
        inflight.insert(key.clone(), rx.clone());
        drop(inflight);

        let generation = self.generation(&key);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_refresh(key, tx, generation).await;
        });
        rx
    }

    async fn run_refresh(
        self: Arc<Self>,
        key: K,
        tx: watch::Sender<Option<Result<(), String>>>,
        generation: (u64, u64),
    ) {
        let guard_cache = Arc::clone(&self);
        let guard_key = key.clone();
        scopeguard::defer! {
            guard_cache
                .inflight
                .lock()
                .expect("dht cache inflight poisoned")
                .remove(&guard_key);
        }

        let result = self.fetcher.fetch(&key).await;

        if generation != self.generation(&key) {
            let _ = tx.send(Some(Err("cache invalidated".to_string())));
            return;
        }

        match result {
            Ok(Some(fetched)) => {
                self.store(&key, fetched);
                let _ = tx.send(Some(Ok(())));
            }
            Ok(None) => {
                self.store_missing(&key);
                let _ = tx.send(Some(Ok(())));
            }
            Err(error) => {
                self.mark_error(&key, &error);
                let _ = tx.send(Some(Err(error)));
            }
        }
    }

    fn entry(&self, key: &K) -> Option<Entry<V>> {
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        let entry = entries.get_mut(key)?;
        entry.last_access = Instant::now();
        Some(entry.clone())
    }

    fn generation(&self, key: &K) -> (u64, u64) {
        let global = self.global_generation.load(Ordering::Relaxed);
        let key = self
            .key_generation
            .lock()
            .expect("dht cache key generation poisoned")
            .get(key)
            .copied()
            .unwrap_or(0);
        (global, key)
    }

    fn store(&self, key: &K, fetched: Fetched<V>) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");

        let mut rollback = false;
        let mut conflict = false;
        if let Some(existing) = entries.get(key)
            && let (Some(old), Some(new)) = (existing.version, fetched.version)
        {
            if new < old {
                rollback = true;
            } else if new == old
                && existing
                    .value
                    .as_ref()
                    .is_some_and(|value| value != &fetched.value)
            {
                conflict = true;
            }
        }
        if rollback {
            return;
        }
        if conflict {
            let entry = entries.get_mut(key).expect("entry exists");
            entry.last_error = Some("conflicting content for the same version".to_string());
            entry.last_access = now;
            drop(entries);
            LogStruct::new(LogLevel::Error, "DHT 刷新内容冲突", format!("{key:?}")).emit();
            return;
        }

        entries.insert(
            key.clone(),
            Entry {
                value: Some(fetched.value),
                version: fetched.version,
                last_error: None,
                last_access: now,
                size: fetched.size,
            },
        );
        evict(&mut entries, self.max_entries, self.max_bytes);
    }

    fn store_missing(&self, key: &K) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        entries.insert(
            key.clone(),
            Entry {
                value: None,
                version: None,
                last_error: None,
                last_access: now,
                size: 0,
            },
        );
        evict(&mut entries, self.max_entries, self.max_bytes);
    }

    fn mark_error(&self, key: &K, error: &str) {
        let should_log = {
            let mut entries = self.entries.lock().expect("dht cache entries poisoned");
            match entries.get_mut(key) {
                Some(entry) => {
                    let changed = entry.last_error.as_deref() != Some(error);
                    entry.last_error = Some(error.to_string());
                    entry.last_access = Instant::now();
                    changed
                }
                None => true,
            }
        };
        if should_log {
            LogStruct::new(LogLevel::Error, "DHT 刷新失败", format!("{key:?}: {error}")).emit();
        }
    }
}

fn read_from<V: Clone>(entry: Entry<V>) -> CacheRead<V> {
    CacheRead {
        value: entry.value,
        last_error: entry.last_error,
    }
}

fn evict<K: Eq + Hash + Clone, V>(
    entries: &mut HashMap<K, Entry<V>>,
    max_entries: usize,
    max_bytes: usize,
) {
    while entries.len() > max_entries
        || entries.values().map(|entry| entry.size).sum::<usize>() > max_bytes
    {
        let Some(victim) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if entries.len() == 1 {
            break;
        }
        entries.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct MockFetcher {
        responses: Mutex<VecDeque<Result<Option<Fetched<i32>>, String>>>,
        calls: AtomicUsize,
        delay: Duration,
    }

    impl MockFetcher {
        fn new(responses: Vec<Result<Option<Fetched<i32>>, String>>, delay: Duration) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
                delay,
            }
        }
    }

    impl Fetcher<(), i32> for MockFetcher {
        fn fetch<'a>(
            &'a self,
            _key: &'a (),
        ) -> BoxFuture<'a, Result<Option<Fetched<i32>>, String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                self.responses
                    .lock()
                    .expect("mock responses poisoned")
                    .pop_front()
                    .unwrap_or(Ok(None))
            })
        }
    }

    fn fetched(value: i32, version: u64) -> Option<Fetched<i32>> {
        Some(Fetched {
            value,
            version: Some(version),
            size: 4,
        })
    }

    #[tokio::test]
    async fn cached_value_is_returned_immediately_while_refreshing() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1)), Ok(fetched(2, 2))],
            Duration::from_millis(50),
        ));
        let cache = DhtCache::new(fetcher.clone(), 16, 1 << 20);

        assert_eq!(cache.get(()).await.unwrap().value, Some(1));

        let started = std::time::Instant::now();
        let cached = cache.get(()).await.unwrap();
        assert_eq!(cached.value, Some(1));
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "cached read waited for the background refresh"
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cache.peek(&()).unwrap().value, Some(2));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cold_start_concurrent_reads_share_one_refresh() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1))],
            Duration::from_millis(30),
        ));
        let cache = DhtCache::new(fetcher.clone(), 16, 1 << 20);

        let (a, b) = tokio::join!(cache.get(()), cache.get(()));
        assert_eq!(a.unwrap().value, Some(1));
        assert_eq!(b.unwrap().value, Some(1));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn background_failure_keeps_old_value() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1)), Err("boom".to_string())],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(fetcher.clone(), 16, 1 << 20);

        assert_eq!(cache.get(()).await.unwrap().value, Some(1));
        assert_eq!(cache.get(()).await.unwrap().value, Some(1));

        tokio::time::sleep(Duration::from_millis(20)).await;
        let entry = cache.peek(&()).expect("old value must remain");
        assert_eq!(entry.value, Some(1));
        assert_eq!(entry.last_error.as_deref(), Some("boom"));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cold_start_failure_returns_real_error() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Err("boom".to_string())],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(fetcher, 16, 1 << 20);

        let error = match cache.get(()).await {
            Ok(_) => panic!("cold failure must surface"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "DHT value unavailable: boom");
    }

    #[tokio::test]
    async fn missing_result_overwrites_old_value() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1)), Ok(None)],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(fetcher.clone(), 16, 1 << 20);

        assert_eq!(cache.get(()).await.unwrap().value, Some(1));
        assert_eq!(cache.get(()).await.unwrap().value, Some(1));

        tokio::time::sleep(Duration::from_millis(20)).await;
        let entry = cache.peek(&()).expect("missing result is cached");
        assert!(entry.value.is_none());
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn lower_version_does_not_replace_higher() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(2, 2)), Ok(fetched(1, 1))],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(fetcher, 16, 1 << 20);

        assert_eq!(cache.get(()).await.unwrap().value, Some(2));
        assert_eq!(cache.get(()).await.unwrap().value, Some(2));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(cache.peek(&()).unwrap().value, Some(2));
    }

    #[tokio::test]
    async fn invalidate_discards_an_inflight_refresh() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1)), Ok(fetched(2, 2))],
            Duration::from_millis(30),
        ));
        let cache = DhtCache::new(fetcher.clone(), 16, 1 << 20);

        cache.refresh(());
        cache.invalidate(&());
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cache.peek(&()).is_none());

        assert_eq!(cache.get(()).await.unwrap().value, Some(2));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }
}
