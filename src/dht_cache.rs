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

//! Stale-while-revalidate cache for DHT reads.
//!
//! Every application-level DHT read cache uses this module. A read returns the
//! last validated value immediately, schedules at most one background refresh
//! per key, and replaces the entry when the refresh completes. Security
//! sensitive callers configure a short stale window and fail closed once it is
//! exceeded.
//!
//! The cache never stores raw network bytes: the [`Fetcher`] is responsible for
//! verifying and selecting a value (highest version for signed documents,
//! remote over local store copies for plain records).

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libp2p::futures::future::BoxFuture;
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

/// A value fetched from the network, already verified by the fetcher.
pub struct Fetched<V> {
    pub value: V,
    pub version: Option<u64>,
    /// Unix seconds after which the value must not be used.
    pub expires_at: Option<u64>,
    /// Approximate size in bytes, used for cache accounting.
    pub size: usize,
}

/// Network access + validation for one cache namespace.
pub trait Fetcher<K, V>: Send + Sync + 'static {
    fn fetch<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<Option<Fetched<V>>, String>>;
}

/// Policy for one cache namespace.
#[derive(Debug, Clone)]
pub struct CachePolicy {
    /// Entries younger than this are returned without a refresh.
    pub fresh_for: Duration,
    /// Values may be served while refreshing until this age is reached.
    pub max_stale: Duration,
    /// How long a "record not found" result is considered fresh.
    pub missing_fresh_for: Duration,
    /// Upper bound for one background refresh.
    pub refresh_timeout: Duration,
    /// Delay before retrying a failed refresh.
    pub refresh_backoff: Duration,
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl CachePolicy {
    pub fn auth() -> Self {
        Self {
            fresh_for: Duration::from_secs(2),
            max_stale: Duration::from_secs(5),
            missing_fresh_for: Duration::from_secs(2),
            refresh_timeout: Duration::from_secs(10),
            refresh_backoff: Duration::from_secs(5),
            max_entries: 1024,
            max_bytes: 64 * 1024 * 1024,
        }
    }

    pub fn types(sync_interval: Duration) -> Self {
        Self {
            fresh_for: sync_interval,
            max_stale: Duration::from_secs(24 * 60 * 60),
            missing_fresh_for: Duration::from_secs(30),
            refresh_timeout: Duration::from_secs(15),
            refresh_backoff: Duration::from_secs(30),
            max_entries: 4,
            max_bytes: 16 * 1024 * 1024,
        }
    }

    pub fn generic() -> Self {
        Self {
            fresh_for: Duration::from_secs(60),
            max_stale: Duration::from_secs(60),
            missing_fresh_for: Duration::from_secs(60),
            refresh_timeout: Duration::from_secs(15),
            refresh_backoff: Duration::from_secs(30),
            max_entries: 1024,
            max_bytes: 64 * 1024 * 1024,
        }
    }

    pub fn providers() -> Self {
        Self {
            fresh_for: Duration::from_secs(30),
            max_stale: Duration::from_secs(60),
            missing_fresh_for: Duration::from_secs(30),
            refresh_timeout: Duration::from_secs(15),
            refresh_backoff: Duration::from_secs(30),
            max_entries: 1024,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Result of a cache read. `value == None` means the record was absent at the
/// last successful refresh.
#[allow(dead_code)]
pub struct CacheRead<V> {
    pub value: Option<V>,
    pub version: Option<u64>,
    pub age: Duration,
    pub stale: bool,
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
    value: Option<V>,
    fetched_at: Instant,
    expires_at: Option<u64>,
    version: Option<u64>,
    last_error: Option<String>,
    next_refresh_at: Instant,
    last_access: Instant,
    size: usize,
}

/// Concurrent, policy-driven DHT read cache.
pub struct DhtCache<K, V, F> {
    entries: Mutex<HashMap<K, Entry<V>>>,
    /// Per-key completion signal for a running refresh.
    inflight: Mutex<HashMap<K, watch::Receiver<bool>>>,
    global_generation: AtomicU64,
    key_generation: Mutex<HashMap<K, u64>>,
    fetcher: Arc<F>,
    policy: CachePolicy,
    permits: Arc<Semaphore>,
}

impl<K, V, F> DhtCache<K, V, F>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Clone + PartialEq + Send + Sync + 'static,
    F: Fetcher<K, V>,
{
    pub fn new(fetcher: Arc<F>, policy: CachePolicy, refresh_permits: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            global_generation: AtomicU64::new(0),
            key_generation: Mutex::new(HashMap::new()),
            fetcher,
            policy,
            permits: Arc::new(Semaphore::new(refresh_permits.max(1))),
        })
    }

    /// Return a cached value without starting a refresh.
    pub fn peek(&self, key: &K) -> Option<CacheRead<V>> {
        let now = Instant::now();
        let entry = self.entry(key)?;
        if is_expired(&entry, unix_now()) {
            return None;
        }
        let age = now.saturating_duration_since(entry.fetched_at);
        let horizon = if entry.value.is_some() {
            self.policy.max_stale
        } else {
            self.policy.missing_fresh_for
        };
        if age > horizon {
            return None;
        }
        Some(read_from(entry, age, self.policy.fresh_for))
    }

    /// Read with stale-while-revalidate semantics.
    pub async fn get(self: &Arc<Self>, key: K) -> Result<CacheRead<V>, CacheError> {
        let now = Instant::now();
        let unix = unix_now();

        if let Some(entry) = self.entry(&key)
            && !is_expired(&entry, unix)
        {
            let age = now.saturating_duration_since(entry.fetched_at);
            let horizon = if entry.value.is_some() {
                self.policy.fresh_for
            } else {
                self.policy.missing_fresh_for
            };
            if age <= horizon {
                return Ok(read_from(entry, age, self.policy.fresh_for));
            }
            if entry.value.is_some() && age <= self.policy.max_stale {
                self.refresh(key);
                return Ok(read_from(entry, age, self.policy.fresh_for));
            }
        }

        // Cold start, expired value, or stale beyond the allowed window: wait
        // for one refresh. Concurrent readers share the same in-flight query.
        for _ in 0..3 {
            if let Some(mut rx) = self.inflight_receiver(&key) {
                let _ = timeout(
                    self.policy.refresh_timeout + Duration::from_secs(3),
                    rx.wait_for(|done| *done),
                )
                .await;
            } else {
                self.refresh(key.clone());
                if let Some(mut rx) = self.inflight_receiver(&key) {
                    let _ = timeout(
                        self.policy.refresh_timeout + Duration::from_secs(3),
                        rx.wait_for(|done| *done),
                    )
                    .await;
                }
            }

            let now = Instant::now();
            let unix = unix_now();
            if let Some(entry) = self.entry(&key) {
                if !is_expired(&entry, unix) {
                    let age = now.saturating_duration_since(entry.fetched_at);
                    let horizon = if entry.value.is_some() {
                        self.policy.max_stale
                    } else {
                        self.policy.missing_fresh_for
                    };
                    if age <= horizon {
                        return Ok(read_from(entry, age, self.policy.fresh_for));
                    }
                }
                if let Some(error) = entry.last_error {
                    return Err(CacheError::Unavailable(error));
                }
            }
        }

        Err(CacheError::Unavailable(
            "refresh did not produce a usable value".to_string(),
        ))
    }

    /// Schedule a background refresh unless one is already running.
    pub fn refresh(self: &Arc<Self>, key: K) {
        if let Some(entry) = self.entry(&key)
            && entry.last_error.is_some()
            && entry.next_refresh_at > Instant::now()
        {
            return;
        }
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return,
        };

        let (tx, rx) = watch::channel(false);
        {
            let mut inflight = self.inflight.lock().expect("dht cache inflight poisoned");
            if inflight.contains_key(&key) {
                return;
            }
            inflight.insert(key.clone(), rx);
        }

        let this = Arc::clone(self);
        let key_for_task = key.clone();
        tokio::spawn(async move {
            let generation = this.generation(&key_for_task);
            let fetched = timeout(
                this.policy.refresh_timeout,
                this.fetcher.fetch(&key_for_task),
            )
            .await;

            if generation == this.generation(&key_for_task) {
                match fetched {
                    Ok(Ok(Some(value))) => this.store(&key_for_task, value),
                    Ok(Ok(None)) => this.store_missing(&key_for_task),
                    Ok(Err(error)) => this.store_error(&key_for_task, error),
                    Err(_) => this.store_error(
                        &key_for_task,
                        format!(
                            "refresh timed out after {}s",
                            this.policy.refresh_timeout.as_secs()
                        ),
                    ),
                }
            }

            let mut inflight = this.inflight.lock().expect("dht cache inflight poisoned");
            inflight.remove(&key_for_task);
            drop(inflight);
            let _ = tx.send(true);
            drop(permit);
        });
    }

    /// Refresh now if the entry is stale, missing, or its retry deadline passed.
    pub fn refresh_if_due(self: &Arc<Self>, key: K) {
        let now = Instant::now();
        match self.entry(&key) {
            Some(entry) => {
                let age = now.saturating_duration_since(entry.fetched_at);
                let horizon = if entry.value.is_some() {
                    self.policy.fresh_for
                } else {
                    self.policy.missing_fresh_for
                };
                if age > horizon || entry.next_refresh_at <= now {
                    self.refresh(key);
                }
            }
            None => self.refresh(key),
        }
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

    /// Drop everything, e.g. after a config/trust-domain reload.
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

    /// Keep the value but make the next read schedule a refresh.
    pub fn mark_stale(&self, key: &K) {
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        if let Some(entry) = entries.get_mut(key) {
            entry.fetched_at = Instant::now()
                .checked_sub(self.policy.fresh_for + Duration::from_millis(1))
                .unwrap_or_else(Instant::now);
            entry.next_refresh_at = Instant::now();
        }
    }

    pub fn mark_all_stale(&self) {
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        let fetched_at = Instant::now()
            .checked_sub(self.policy.fresh_for + Duration::from_millis(1))
            .unwrap_or_else(Instant::now);
        for entry in entries.values_mut() {
            entry.fetched_at = fetched_at;
            entry.next_refresh_at = Instant::now();
        }
    }

    fn entry(&self, key: &K) -> Option<Entry<V>> {
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        let entry = entries.get_mut(key)?;
        entry.last_access = Instant::now();
        Some(entry.clone())
    }

    fn inflight_receiver(&self, key: &K) -> Option<watch::Receiver<bool>> {
        self.inflight
            .lock()
            .expect("dht cache inflight poisoned")
            .get(key)
            .cloned()
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
            entry.last_error = Some("conflicting content for the same version".into());
            entry.next_refresh_at = now + self.policy.refresh_backoff;
            return;
        }

        entries.insert(
            key.clone(),
            Entry {
                value: Some(fetched.value),
                fetched_at: now,
                expires_at: fetched.expires_at,
                version: fetched.version,
                last_error: None,
                next_refresh_at: now + self.policy.fresh_for,
                last_access: now,
                size: fetched.size,
            },
        );
        evict(&mut entries, &self.policy);
    }

    fn store_missing(&self, key: &K) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        entries.insert(
            key.clone(),
            Entry {
                value: None,
                fetched_at: now,
                expires_at: None,
                version: None,
                last_error: None,
                next_refresh_at: now + self.policy.missing_fresh_for,
                last_access: now,
                size: 0,
            },
        );
        evict(&mut entries, &self.policy);
    }

    fn store_error(&self, key: &K, error: String) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("dht cache entries poisoned");
        match entries.get_mut(key) {
            Some(entry) => {
                entry.last_error = Some(error);
                entry.next_refresh_at = now + self.policy.refresh_backoff;
            }
            None => {
                entries.insert(
                    key.clone(),
                    Entry {
                        value: None,
                        fetched_at: now
                            .checked_sub(self.policy.missing_fresh_for + Duration::from_millis(1))
                            .unwrap_or(now),
                        expires_at: None,
                        version: None,
                        last_error: Some(error),
                        next_refresh_at: now + self.policy.refresh_backoff,
                        last_access: now,
                        size: 0,
                    },
                );
            }
        }
        evict(&mut entries, &self.policy);
    }
}

fn read_from<V: Clone>(entry: Entry<V>, age: Duration, fresh_for: Duration) -> CacheRead<V> {
    CacheRead {
        value: entry.value,
        version: entry.version,
        age,
        stale: age > fresh_for,
        last_error: entry.last_error,
    }
}

fn is_expired<V>(entry: &Entry<V>, now_unix: u64) -> bool {
    entry.expires_at.is_some_and(|expires| now_unix > expires)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn evict<K: Eq + Hash + Clone, V>(entries: &mut HashMap<K, Entry<V>>, policy: &CachePolicy) {
    if entries.len() <= policy.max_entries {
        let total: usize = entries.values().map(|entry| entry.size).sum();
        if total <= policy.max_bytes {
            return;
        }
    }

    while entries.len() > policy.max_entries
        || entries.values().map(|entry| entry.size).sum::<usize>() > policy.max_bytes
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
            expires_at: None,
            size: 4,
        })
    }

    fn policy(fresh: Duration, stale: Duration) -> CachePolicy {
        CachePolicy {
            fresh_for: fresh,
            max_stale: stale,
            missing_fresh_for: Duration::from_millis(5),
            refresh_timeout: Duration::from_secs(2),
            refresh_backoff: Duration::from_millis(5),
            max_entries: 16,
            max_bytes: 1 << 20,
        }
    }

    #[tokio::test]
    async fn fresh_value_is_served_without_refresh() {
        let fetcher = Arc::new(MockFetcher::new(vec![Ok(fetched(1, 1))], Duration::ZERO));
        let cache = DhtCache::new(
            fetcher.clone(),
            policy(Duration::from_secs(60), Duration::from_secs(60)),
            2,
        );
        assert_eq!(cache.get(()).await.unwrap().value, Some(1));
        assert_eq!(cache.get(()).await.unwrap().value, Some(1));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stale_value_is_returned_while_refreshing() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1)), Ok(fetched(2, 2))],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(
            fetcher.clone(),
            policy(Duration::ZERO, Duration::from_secs(5)),
            2,
        );
        assert_eq!(cache.get(()).await.unwrap().value, Some(1));
        let stale = cache.get(()).await.unwrap();
        assert_eq!(stale.value, Some(1));
        assert!(stale.stale);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(cache.get(()).await.unwrap().value, Some(2));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn concurrent_cold_reads_share_one_refresh() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(1, 1))],
            Duration::from_millis(30),
        ));
        let cache = DhtCache::new(
            fetcher.clone(),
            policy(Duration::from_secs(60), Duration::from_secs(60)),
            2,
        );
        let a = cache.clone();
        let b = cache.clone();
        let (ra, rb) = tokio::join!(a.get(()), b.get(()));
        assert_eq!(ra.unwrap().value, Some(1));
        assert_eq!(rb.unwrap().value, Some(1));
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn lower_version_does_not_replace_higher() {
        let fetcher = Arc::new(MockFetcher::new(
            vec![Ok(fetched(2, 2)), Ok(fetched(1, 1))],
            Duration::ZERO,
        ));
        let cache = DhtCache::new(
            fetcher.clone(),
            policy(Duration::ZERO, Duration::from_secs(5)),
            2,
        );
        assert_eq!(cache.get(()).await.unwrap().value, Some(2));
        let _ = cache.get(()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(cache.get(()).await.unwrap().value, Some(2));
    }

    #[tokio::test]
    async fn missing_record_is_reported() {
        let fetcher = Arc::new(MockFetcher::new(vec![Ok(None)], Duration::ZERO));
        let cache = DhtCache::new(
            fetcher,
            policy(Duration::from_secs(60), Duration::from_secs(60)),
            2,
        );
        assert!(cache.get(()).await.unwrap().value.is_none());
    }
}
