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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libp2p::kad;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::auth;
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;

const WITHDRAW_GRACE: Duration = Duration::from_secs(3);
const TYPES_KEY: &[u8] = b"/oahd/service/types";
const MAX_TYPES: usize = 4096;
const COMMAND_QUEUE_CAPACITY: usize = 256;
const ANNOUNCE_RETRY_INITIAL: Duration = Duration::from_secs(5);
const ANNOUNCE_RETRY_MAX: Duration = Duration::from_secs(60);
const RECONCILE_RETRY: Duration = Duration::from_secs(30);

fn types_key() -> kad::RecordKey {
    kad::RecordKey::new(&TYPES_KEY)
}

fn service_key(name: &str) -> kad::RecordKey {
    kad::RecordKey::new(&format!("/oahd/service/{name}"))
}

fn merge_type_set(known: &mut BTreeSet<String>, incoming: impl IntoIterator<Item = String>) {
    for name in incoming {
        if known.len() >= MAX_TYPES {
            break;
        }
        if auth::is_valid_service_name(&name) {
            known.insert(name);
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ServiceSnapshot {
    pub types: Vec<String>,
    pub announced: Vec<String>,
    pub backend_ready: Vec<String>,
    pub bootstrapped: bool,
}

#[derive(Debug, Clone)]
pub enum ServiceCommand {
    BackendReady(String),
    BackendDown(String),
    BootstrapCompleted,
    Tick,
    Reannounce,
    RefreshTypes,
    Reset,
}

#[derive(Debug, Clone)]
enum JobSpec {
    Announce {
        generation: u64,
        service: String,
        key: kad::RecordKey,
    },
    Withdraw {
        generation: u64,
        service: String,
        key: kad::RecordKey,
    },
    ReadTypes {
        generation: u64,
    },
    WriteTypes {
        generation: u64,
        value: Vec<u8>,
    },
}

enum JobResult {
    Announced {
        generation: u64,
        service: String,
        result: Result<(), String>,
    },
    Withdrawn {
        generation: u64,
        service: String,
        result: Result<(), String>,
    },
    TypesRead {
        generation: u64,
        result: Result<Vec<String>, String>,
    },
    TypesWritten {
        generation: u64,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileState {
    Idle,
    Reading,
    ReadyToWrite,
    Writing,
}

struct RegistryState {
    config: ConfigHandle,
    bootstrapped: bool,
    backend_ready: HashSet<String>,
    announced: HashSet<String>,
    withdraw_deadline: HashMap<String, Instant>,
    announce_inflight: HashSet<String>,
    withdraw_inflight: HashSet<String>,
    announce_failures: HashMap<String, u32>,
    announce_retry_at: HashMap<String, Instant>,
    known_types: BTreeSet<String>,
    reconcile: ReconcileState,
    reconcile_dirty: bool,
    next_types_sync: Option<Instant>,
    pending_types_write: Option<Vec<u8>>,
    generation: u64,
}

impl RegistryState {
    fn new(config: ConfigHandle) -> Self {
        Self {
            config,
            bootstrapped: false,
            backend_ready: HashSet::new(),
            announced: HashSet::new(),
            withdraw_deadline: HashMap::new(),
            announce_inflight: HashSet::new(),
            withdraw_inflight: HashSet::new(),
            announce_failures: HashMap::new(),
            announce_retry_at: HashMap::new(),
            known_types: BTreeSet::new(),
            reconcile: ReconcileState::Idle,
            reconcile_dirty: false,
            next_types_sync: None,
            pending_types_write: None,
            generation: 0,
        }
    }

    fn sync_interval(&self) -> Duration {
        Duration::from_secs(self.config.dispatcher_types_sync_interval() as u64)
    }

    fn retry_delay(failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(4);
        (ANNOUNCE_RETRY_INITIAL * (1u32 << shift)).min(ANNOUNCE_RETRY_MAX)
    }

    fn mark_types_dirty(&mut self, now: Instant) {
        self.reconcile_dirty = true;
        if self.reconcile == ReconcileState::Idle {
            self.next_types_sync = Some(now);
        }
    }

    fn handle(&mut self, command: ServiceCommand, now: Instant) {
        match command {
            ServiceCommand::BackendReady(service) => {
                self.backend_ready.insert(service.clone());
                self.withdraw_deadline.remove(&service);
                self.announce_failures.remove(&service);
                self.announce_retry_at.remove(&service);
            }
            ServiceCommand::BackendDown(service) => {
                self.backend_ready.remove(&service);
                self.announce_retry_at.remove(&service);
                if self.announced.contains(&service) {
                    self.withdraw_deadline
                        .entry(service)
                        .or_insert_with(|| now + WITHDRAW_GRACE);
                }
            }
            ServiceCommand::BootstrapCompleted => {
                self.bootstrapped = true;
                self.mark_types_dirty(now);
            }
            ServiceCommand::Tick => {}
            ServiceCommand::Reannounce => {
                self.mark_types_dirty(now);
            }
            ServiceCommand::RefreshTypes => {
                self.mark_types_dirty(now);
            }
            ServiceCommand::Reset => {
                self.generation = self.generation.wrapping_add(1);
                self.bootstrapped = false;
                self.announced.clear();
                self.withdraw_deadline.clear();
                self.announce_inflight.clear();
                self.withdraw_inflight.clear();
                self.announce_failures.clear();
                self.announce_retry_at.clear();
                self.reconcile = ReconcileState::Idle;
                self.reconcile_dirty = false;
                self.next_types_sync = None;
                self.pending_types_write = None;
            }
        }
    }

    fn plan(&mut self, now: Instant, slots: usize) -> Vec<JobSpec> {
        let mut jobs = Vec::new();
        let mut slots = slots;

        let mut due: Vec<String> = self
            .withdraw_deadline
            .iter()
            .filter(|(service, deadline)| {
                **deadline <= now && !self.backend_ready.contains(*service)
            })
            .map(|(service, _)| service.clone())
            .collect();
        due.sort();
        for service in due {
            if slots == 0 {
                break;
            }
            if self.withdraw_inflight.contains(&service)
                || self.announce_inflight.contains(&service)
            {
                continue;
            }
            self.withdraw_deadline.remove(&service);
            self.withdraw_inflight.insert(service.clone());
            jobs.push(JobSpec::Withdraw {
                generation: self.generation,
                service: service.clone(),
                key: service_key(&service),
            });
            slots -= 1;
        }

        let mut retries: Vec<String> = self
            .announce_retry_at
            .iter()
            .filter(|(service, at)| {
                **at <= now
                    && self.backend_ready.contains(*service)
                    && !self.announced.contains(*service)
            })
            .map(|(service, _)| service.clone())
            .collect();
        retries.sort();
        for service in retries {
            if slots == 0 {
                break;
            }
            if self.announce_inflight.contains(&service)
                || self.withdraw_inflight.contains(&service)
            {
                continue;
            }
            self.announce_retry_at.remove(&service);
            self.announce_inflight.insert(service.clone());
            jobs.push(JobSpec::Announce {
                generation: self.generation,
                service: service.clone(),
                key: service_key(&service),
            });
            slots -= 1;
        }

        if self.bootstrapped && slots > 0 {
            let local: Vec<String> = self
                .config
                .read()
                .services
                .dispatcher
                .local_services
                .iter()
                .filter(|service| auth::is_valid_service_name(&service.name))
                .map(|service| service.name.clone())
                .collect();
            for service in local {
                if slots == 0 {
                    break;
                }
                if !self.backend_ready.contains(&service)
                    || self.announced.contains(&service)
                    || self.announce_inflight.contains(&service)
                    || self.withdraw_inflight.contains(&service)
                {
                    continue;
                }
                if self
                    .announce_retry_at
                    .get(&service)
                    .is_some_and(|at| *at > now)
                {
                    continue;
                }
                self.announce_retry_at.remove(&service);
                self.announce_inflight.insert(service.clone());
                jobs.push(JobSpec::Announce {
                    generation: self.generation,
                    service: service.clone(),
                    key: service_key(&service),
                });
                slots -= 1;
            }
        }

        if slots > 0 {
            match self.reconcile {
                ReconcileState::Idle => {
                    let due =
                        self.reconcile_dirty || self.next_types_sync.is_some_and(|at| at <= now);
                    if due {
                        self.next_types_sync = None;
                        self.reconcile = ReconcileState::Reading;
                        self.reconcile_dirty = false;
                        jobs.push(JobSpec::ReadTypes {
                            generation: self.generation,
                        });
                    }
                }
                ReconcileState::ReadyToWrite => {
                    if let Some(value) = self.pending_types_write.take() {
                        self.reconcile = ReconcileState::Writing;
                        jobs.push(JobSpec::WriteTypes {
                            generation: self.generation,
                            value,
                        });
                    } else {
                        self.reconcile = ReconcileState::Idle;
                    }
                }
                ReconcileState::Reading | ReconcileState::Writing => {}
            }
        }

        jobs
    }

    fn apply(&mut self, result: JobResult, now: Instant) {
        match result {
            JobResult::Announced {
                generation,
                service,
                result,
            } => {
                self.announce_inflight.remove(&service);
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.announce_failures.remove(&service);
                        self.announce_retry_at.remove(&service);
                        self.announced.insert(service.clone());
                        self.mark_types_dirty(now);
                        if self.backend_ready.contains(&service) && self.bootstrapped {
                            LogStruct::new(
                                LogLevel::Preset,
                                format!("已注册服务: {}", service),
                                "",
                            )
                            .emit();
                        } else {
                            self.withdraw_deadline.insert(service, now);
                        }
                    }
                    Err(error) => {
                        if self.backend_ready.contains(&service) && self.bootstrapped {
                            let failures = {
                                let count =
                                    self.announce_failures.entry(service.clone()).or_insert(0);
                                *count = count.saturating_add(1);
                                *count
                            };
                            self.announce_retry_at
                                .insert(service.clone(), now + Self::retry_delay(failures));
                            LogStruct::new(
                                LogLevel::Warning,
                                "注册服务失败，稍后重试",
                                format!("{service}: {error}"),
                            )
                            .emit();
                        } else {
                            LogStruct::new(
                                LogLevel::Warning,
                                "注册服务失败",
                                format!("{service}: {error}"),
                            )
                            .emit();
                        }
                    }
                }
            }
            JobResult::Withdrawn {
                generation,
                service,
                result,
            } => {
                self.withdraw_inflight.remove(&service);
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.announced.remove(&service);
                        LogStruct::new(
                            LogLevel::Preset,
                            format!("已撤销服务宣告: {}", service),
                            "",
                        )
                        .emit();
                        if self.backend_ready.contains(&service) {
                            self.announce_retry_at.insert(service, now);
                        }
                    }
                    Err(error) => {
                        LogStruct::new(
                            LogLevel::Warning,
                            "撤销服务宣告失败",
                            format!("{service}: {error}"),
                        )
                        .emit();
                        if self.backend_ready.contains(&service) {
                            self.announce_retry_at.insert(service.clone(), now);
                        } else {
                            self.withdraw_deadline
                                .insert(service, now + ANNOUNCE_RETRY_INITIAL);
                        }
                    }
                }
            }
            JobResult::TypesRead { generation, result } => {
                if generation != self.generation || self.reconcile != ReconcileState::Reading {
                    return;
                }
                match result {
                    Ok(network_types) => {
                        let network_set: BTreeSet<String> = network_types.iter().cloned().collect();
                        merge_type_set(&mut self.known_types, network_types);
                        let announced: Vec<String> = self.announced.iter().cloned().collect();
                        merge_type_set(&mut self.known_types, announced);
                        if self.known_types.is_empty() || self.known_types == network_set {
                            self.reconcile = ReconcileState::Idle;
                            self.pending_types_write = None;
                            self.next_types_sync = Some(now + self.sync_interval());
                            if self.reconcile_dirty {
                                self.reconcile_dirty = false;
                                self.next_types_sync = Some(now);
                            }
                        } else {
                            let list: Vec<String> = self.known_types.iter().cloned().collect();
                            match serde_json::to_vec(&list) {
                                Ok(value) => {
                                    self.pending_types_write = Some(value);
                                    self.reconcile = ReconcileState::ReadyToWrite;
                                    self.reconcile_dirty = false;
                                }
                                Err(error) => {
                                    LogStruct::new(
                                        LogLevel::Warning,
                                        "序列化服务类型列表失败",
                                        error.to_string(),
                                    )
                                    .emit();
                                    self.reconcile = ReconcileState::Idle;
                                    self.pending_types_write = None;
                                    self.next_types_sync = Some(now + RECONCILE_RETRY);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        LogStruct::new(LogLevel::Warning, "读取服务类型列表失败", error).emit();
                        self.reconcile = ReconcileState::Idle;
                        self.pending_types_write = None;
                        self.next_types_sync = Some(now + RECONCILE_RETRY);
                        if self.reconcile_dirty {
                            self.reconcile_dirty = false;
                            self.next_types_sync = Some(now);
                        }
                    }
                }
            }
            JobResult::TypesWritten { generation, result } => {
                if generation != self.generation || self.reconcile != ReconcileState::Writing {
                    return;
                }
                let success = result.is_ok();
                if let Err(error) = result {
                    LogStruct::new(LogLevel::Warning, "更新服务类型列表失败", error).emit();
                }
                self.reconcile = ReconcileState::Idle;
                self.pending_types_write = None;
                self.next_types_sync = Some(
                    now + if success {
                        self.sync_interval()
                    } else {
                        RECONCILE_RETRY
                    },
                );
                if self.reconcile_dirty {
                    self.reconcile_dirty = false;
                    self.next_types_sync = Some(now);
                }
            }
        }
    }

    fn snapshot(&self) -> ServiceSnapshot {
        let mut types = self.known_types.clone();
        types.extend(self.announced.iter().cloned());
        let announced: Vec<String> = self
            .announced
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let backend_ready: Vec<String> = self
            .backend_ready
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        ServiceSnapshot {
            types: types.into_iter().collect(),
            announced,
            backend_ready,
            bootstrapped: self.bootstrapped,
        }
    }
}

#[derive(Clone)]
enum DhtClient {
    Real(NetworkHandle),
    #[cfg(test)]
    Mock(Arc<MockDht>),
}

impl DhtClient {
    async fn start_providing(&self, key: kad::RecordKey) -> Result<(), String> {
        match self {
            DhtClient::Real(swarm) => swarm.start_providing(key).await.map_err(|e| e.to_string()),
            #[cfg(test)]
            DhtClient::Mock(mock) => mock.start_providing().await,
        }
    }

    async fn stop_providing(&self, key: kad::RecordKey) -> Result<(), String> {
        match self {
            DhtClient::Real(swarm) => swarm.stop_providing(key).await.map_err(|e| e.to_string()),
            #[cfg(test)]
            DhtClient::Mock(mock) => mock.stop_providing().await,
        }
    }

    async fn get_record_network(&self, key: kad::RecordKey) -> Result<kad::GetRecordOk, String> {
        match self {
            DhtClient::Real(swarm) => swarm
                .get_record_network(key)
                .await
                .map_err(|e| e.to_string()),
            #[cfg(test)]
            DhtClient::Mock(mock) => mock.get_record_network(),
        }
    }

    async fn put_record(&self, key: kad::RecordKey, value: Vec<u8>) -> Result<(), String> {
        match self {
            DhtClient::Real(swarm) => swarm
                .put_record(key, value)
                .await
                .map_err(|e| e.to_string()),
            #[cfg(test)]
            DhtClient::Mock(mock) => mock.put_record(value),
        }
    }
}

pub struct ServiceRegistryHandle {
    pub tx: mpsc::Sender<ServiceCommand>,
    pub snapshot: watch::Receiver<Arc<ServiceSnapshot>>,
}

struct ServiceRegistryActor {
    config: ConfigHandle,
    dht: DhtClient,
    state: RegistryState,
    cmd_rx: mpsc::Receiver<ServiceCommand>,
    shutdown_rx: watch::Receiver<bool>,
    snapshot_tx: watch::Sender<Arc<ServiceSnapshot>>,
    jobs: JoinSet<JobResult>,
}

impl ServiceRegistryActor {
    fn snapshot(&self) {
        let _ = self.snapshot_tx.send(Arc::new(self.state.snapshot()));
    }

    fn pump(&mut self) {
        let max = self.config.registry_max_inflight_dht().clamp(1, 16);
        let slots = max.saturating_sub(self.jobs.len());
        let specs = self.state.plan(Instant::now(), slots);
        for spec in specs {
            self.spawn(spec);
        }
    }

    fn spawn(&mut self, spec: JobSpec) {
        let dht = self.dht.clone();
        match spec {
            JobSpec::Announce {
                generation,
                service,
                key,
            } => {
                self.jobs.spawn(async move {
                    let result = dht.start_providing(key).await;
                    JobResult::Announced {
                        generation,
                        service,
                        result,
                    }
                });
            }
            JobSpec::Withdraw {
                generation,
                service,
                key,
            } => {
                self.jobs.spawn(async move {
                    let result = dht.stop_providing(key).await;
                    JobResult::Withdrawn {
                        generation,
                        service,
                        result,
                    }
                });
            }
            JobSpec::ReadTypes { generation } => {
                self.jobs.spawn(async move {
                    let result = match dht.get_record_network(types_key()).await {
                        Ok(kad::GetRecordOk::FoundRecord(record)) => {
                            parse_types(&record.record.value).map_err(|e| e.to_string())
                        }
                        Ok(_) => Ok(Vec::new()),
                        Err(error) => Err(error),
                    };
                    JobResult::TypesRead { generation, result }
                });
            }
            JobSpec::WriteTypes { generation, value } => {
                self.jobs.spawn(async move {
                    let result = dht.put_record(types_key(), value).await;
                    JobResult::TypesWritten { generation, result }
                });
            }
        }
    }

    async fn run(mut self) {
        self.snapshot();
        self.pump();

        loop {
            tokio::select! {
                command = self.cmd_rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    if matches!(command, ServiceCommand::Reset) {
                        self.jobs.abort_all();
                    }
                    self.state.handle(command, Instant::now());
                    self.snapshot();
                    self.pump();
                }
                result = self.jobs.join_next() => {
                    match result {
                        Some(Ok(result)) => {
                            self.state.apply(result, Instant::now());
                            self.snapshot();
                        }
                        Some(Err(error)) if error.is_cancelled() => {}
                        Some(Err(error)) => {
                            LogStruct::new(
                                LogLevel::Error,
                                "服务注册任务异常退出",
                                error.to_string(),
                            )
                            .emit();
                        }
                        None => {}
                    }
                    self.pump();
                }
                _ = self.shutdown_rx.changed() => {
                    break;
                }
            }
        }

        self.jobs.shutdown().await;
    }
}

fn parse_types(value: &[u8]) -> Result<Vec<String>, serde_json::Error> {
    let types: Vec<String> = serde_json::from_slice(value)?;
    Ok(types
        .into_iter()
        .filter(|name| auth::is_valid_service_name(name))
        .take(MAX_TYPES)
        .collect())
}

pub fn spawn(
    config: ConfigHandle,
    swarm: NetworkHandle,
    shutdown_rx: watch::Receiver<bool>,
) -> ServiceRegistryHandle {
    spawn_with_dht(config, DhtClient::Real(swarm), shutdown_rx)
}

fn spawn_with_dht(
    config: ConfigHandle,
    dht: DhtClient,
    shutdown_rx: watch::Receiver<bool>,
) -> ServiceRegistryHandle {
    let (tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let (snapshot_tx, snapshot) = watch::channel(Arc::new(ServiceSnapshot::default()));
    let state = RegistryState::new(config.clone());
    let actor = ServiceRegistryActor {
        config,
        dht,
        state,
        cmd_rx,
        shutdown_rx,
        snapshot_tx,
        jobs: JoinSet::new(),
    };
    tokio::spawn(async move {
        actor.run().await;
    });
    ServiceRegistryHandle { tx, snapshot }
}

#[cfg(test)]
struct MockDht {
    start_delay: Duration,
    fail_start: std::sync::atomic::AtomicBool,
    types: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl MockDht {
    fn new(start_delay: Duration) -> Self {
        Self {
            start_delay,
            fail_start: std::sync::atomic::AtomicBool::new(false),
            types: std::sync::Mutex::new(Vec::new()),
        }
    }

    async fn start_providing(&self) -> Result<(), String> {
        tokio::time::sleep(self.start_delay).await;
        if self.fail_start.load(std::sync::atomic::Ordering::Relaxed) {
            Err("mock failure".to_string())
        } else {
            Ok(())
        }
    }

    async fn stop_providing(&self) -> Result<(), String> {
        Ok(())
    }

    fn get_record_network(&self) -> Result<kad::GetRecordOk, String> {
        let types = self.types.lock().expect("mock types poisoned");
        let value = serde_json::to_vec(&*types).map_err(|e| e.to_string())?;
        Ok(kad::GetRecordOk::FoundRecord(kad::PeerRecord {
            peer: None,
            record: kad::Record::new(types_key(), value),
        }))
    }

    fn put_record(&self, value: Vec<u8>) -> Result<(), String> {
        let types: Vec<String> = serde_json::from_slice(&value).map_err(|e| e.to_string())?;
        *self.types.lock().expect("mock types poisoned") = types;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalServiceEntry, NodeConfig};
    use std::sync::atomic::Ordering;

    fn config_with_services(names: &[&str]) -> ConfigHandle {
        let mut config = NodeConfig::default();
        config.services.dispatcher.local_services = names
            .iter()
            .enumerate()
            .map(|(index, name)| LocalServiceEntry {
                name: (*name).to_string(),
                port: 6000 + index as u16,
                require_auth: false,
            })
            .collect();
        ConfigHandle::new(config)
    }

    fn announce_services(state: &mut RegistryState, names: &[&str], now: Instant) -> Vec<JobSpec> {
        for name in names {
            state.handle(ServiceCommand::BackendReady((*name).to_string()), now);
        }
        state.handle(ServiceCommand::BootstrapCompleted, now);
        state.plan(now, 8)
    }

    #[test]
    fn waits_for_bootstrap_before_announcing() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        state.handle(ServiceCommand::BackendReady("cmd".to_string()), now);
        assert!(state.plan(now, 4).is_empty());

        state.handle(ServiceCommand::BootstrapCompleted, now);
        let jobs = state.plan(now, 4);
        let announces: Vec<_> = jobs
            .iter()
            .filter(|job| matches!(job, JobSpec::Announce { .. }))
            .collect();
        assert!(
            matches!(announces.as_slice(), [JobSpec::Announce { service, .. }] if service == "cmd")
        );
    }

    #[test]
    fn respects_max_inflight_slots() {
        let mut state = RegistryState::new(config_with_services(&["a", "b", "c"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["a", "b", "c"], now);
        assert_eq!(
            jobs.iter()
                .filter(|job| matches!(job, JobSpec::Announce { .. }))
                .count(),
            3
        );

        let mut state = RegistryState::new(config_with_services(&["a", "b", "c"]));
        let now = Instant::now();
        for name in ["a", "b", "c"] {
            state.handle(ServiceCommand::BackendReady(name.to_string()), now);
        }
        state.handle(ServiceCommand::BootstrapCompleted, now);
        assert_eq!(
            state
                .plan(now, 2)
                .iter()
                .filter(|job| matches!(job, JobSpec::Announce { .. }))
                .count(),
            2
        );
        assert_eq!(
            state
                .plan(now, 2)
                .iter()
                .filter(|job| matches!(job, JobSpec::Announce { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn grace_allows_reconnect_and_withdraws_after_deadline() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["cmd"], now);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected announce");
        };
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Ok(()),
            },
            now,
        );
        assert!(state.snapshot().announced.contains(&"cmd".to_string()));

        state.handle(ServiceCommand::BackendDown("cmd".to_string()), now);
        assert!(state.plan(now + Duration::from_secs(1), 4).is_empty());
        state.handle(
            ServiceCommand::BackendReady("cmd".to_string()),
            now + Duration::from_secs(2),
        );
        assert!(state.plan(now + Duration::from_secs(4), 4).is_empty());

        state.handle(
            ServiceCommand::BackendDown("cmd".to_string()),
            now + Duration::from_secs(5),
        );
        let jobs = state.plan(now + Duration::from_secs(10), 4);
        assert!(matches!(jobs.as_slice(), [JobSpec::Withdraw { service, .. }] if service == "cmd"));
    }

    #[test]
    fn announce_success_after_backend_down_is_withdrawn() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["cmd"], now);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected announce");
        };
        state.handle(ServiceCommand::BackendDown("cmd".to_string()), now);
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Ok(()),
            },
            now,
        );
        assert!(state.snapshot().announced.contains(&"cmd".to_string()));
        let jobs = state.plan(now, 4);
        assert!(matches!(jobs.as_slice(), [JobSpec::Withdraw { .. }]));
    }

    #[test]
    fn late_announce_schedules_type_sync() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        state.handle(ServiceCommand::BootstrapCompleted, now);
        let jobs = state.plan(now, 4);
        let generation = state.generation;
        assert!(
            jobs.iter()
                .any(|job| matches!(job, JobSpec::ReadTypes { .. }))
        );
        state.apply(
            JobResult::TypesRead {
                generation,
                result: Ok(Vec::new()),
            },
            now,
        );
        assert_eq!(state.reconcile, ReconcileState::Idle);

        state.handle(ServiceCommand::BackendReady("cmd".to_string()), now);
        let jobs = state.plan(now, 4);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs
            .into_iter()
            .find(|job| matches!(job, JobSpec::Announce { .. }))
            .unwrap()
        else {
            panic!("expected announce");
        };
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Ok(()),
            },
            now,
        );
        assert!(state.reconcile_dirty);
        assert!(state.next_types_sync.is_some_and(|at| at <= now));
    }

    #[test]
    fn failed_withdraw_is_retried() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["cmd"], now);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected announce");
        };
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Ok(()),
            },
            now,
        );
        state.handle(ServiceCommand::BackendDown("cmd".to_string()), now);
        let jobs = state.plan(now + Duration::from_secs(4), 4);
        let JobSpec::Withdraw {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected withdraw");
        };
        state.apply(
            JobResult::Withdrawn {
                generation,
                service,
                result: Err("nope".to_string()),
            },
            now + Duration::from_secs(4),
        );
        assert!(
            state
                .plan(now + Duration::from_secs(5), 4)
                .iter()
                .all(|job| !matches!(job, JobSpec::Withdraw { .. }))
        );
        assert!(matches!(
            state.plan(now + Duration::from_secs(10), 4).as_slice(),
            [JobSpec::Withdraw { .. }]
        ));
    }

    #[test]
    fn reconcile_dirty_reruns_after_write() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        state.known_types.insert("ocr".to_string());
        state.handle(ServiceCommand::BootstrapCompleted, now);
        let jobs = state.plan(now, 4);
        let JobSpec::ReadTypes { generation } = jobs
            .into_iter()
            .find(|job| matches!(job, JobSpec::ReadTypes { .. }))
            .unwrap()
        else {
            panic!("expected read");
        };
        state.apply(
            JobResult::TypesRead {
                generation,
                result: Ok(vec!["cmd".to_string()]),
            },
            now,
        );
        let jobs = state.plan(now, 4);
        let JobSpec::WriteTypes { generation, .. } = jobs
            .into_iter()
            .find(|job| matches!(job, JobSpec::WriteTypes { .. }))
            .unwrap()
        else {
            panic!("expected write");
        };
        state.handle(ServiceCommand::RefreshTypes, now);
        state.apply(
            JobResult::TypesWritten {
                generation,
                result: Ok(()),
            },
            now,
        );
        assert!(state.next_types_sync.is_some_and(|at| at <= now));
        assert!(
            state
                .plan(now, 4)
                .iter()
                .any(|job| matches!(job, JobSpec::ReadTypes { .. }))
        );
    }

    #[test]
    fn failed_announce_retries_with_backoff() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["cmd"], now);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected announce");
        };
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Err("nope".to_string()),
            },
            now,
        );
        assert!(state.plan(now, 4).is_empty());
        assert!(matches!(
            state.plan(now + ANNOUNCE_RETRY_INITIAL, 4).as_slice(),
            [JobSpec::Announce { .. }]
        ));
    }

    #[test]
    fn reset_ignores_stale_results() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        let jobs = announce_services(&mut state, &["cmd"], now);
        let JobSpec::Announce {
            generation,
            service,
            ..
        } = jobs.into_iter().next().unwrap()
        else {
            panic!("expected announce");
        };
        state.handle(ServiceCommand::Reset, now);
        state.apply(
            JobResult::Announced {
                generation,
                service,
                result: Ok(()),
            },
            now,
        );
        assert!(state.snapshot().announced.is_empty());
        assert!(state.snapshot().types.is_empty());
    }

    #[test]
    fn reconcile_is_single_flight_and_publishes_types() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        state.known_types.insert("ocr".to_string());
        state.handle(ServiceCommand::BootstrapCompleted, now);
        let jobs = state.plan(now, 4);
        let JobSpec::ReadTypes { generation } = jobs
            .iter()
            .find(|job| matches!(job, JobSpec::ReadTypes { .. }))
            .cloned()
            .unwrap()
        else {
            panic!("expected read");
        };
        assert!(
            state
                .plan(now, 4)
                .iter()
                .all(|job| !matches!(job, JobSpec::ReadTypes { .. }))
        );

        state.apply(
            JobResult::TypesRead {
                generation,
                result: Ok(vec!["cmd".to_string()]),
            },
            now,
        );
        let jobs = state.plan(now, 4);
        let JobSpec::WriteTypes { generation, .. } = jobs
            .iter()
            .find(|job| matches!(job, JobSpec::WriteTypes { .. }))
            .cloned()
            .unwrap()
        else {
            panic!("expected write");
        };
        assert_eq!(
            state.snapshot().types,
            vec!["cmd".to_string(), "ocr".to_string()]
        );
        state.apply(
            JobResult::TypesWritten {
                generation,
                result: Ok(()),
            },
            now,
        );
        assert_eq!(state.reconcile, ReconcileState::Idle);
    }

    #[tokio::test]
    async fn actor_processes_commands_while_announce_is_slow() {
        let config = config_with_services(&["cmd"]);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mock = Arc::new(MockDht::new(Duration::from_millis(500)));
        let handle = spawn_with_dht(config, DhtClient::Mock(mock), shutdown_rx);

        handle
            .tx
            .send(ServiceCommand::BackendReady("cmd".to_string()))
            .await
            .unwrap();
        handle
            .tx
            .send(ServiceCommand::BootstrapCompleted)
            .await
            .unwrap();
        handle
            .tx
            .send(ServiceCommand::BackendDown("cmd".to_string()))
            .await
            .unwrap();

        let snapshot = tokio::time::timeout(Duration::from_millis(100), async {
            let mut rx = handle.snapshot.clone();
            loop {
                let current = rx.borrow_and_update().clone();
                if current.bootstrapped && current.backend_ready.is_empty() {
                    break current;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("registry actor was blocked by the slow announce");
        assert!(snapshot.announced.is_empty());
    }

    #[tokio::test]
    async fn mock_can_fail_announce() {
        let mock = MockDht::new(Duration::ZERO);
        mock.fail_start.store(true, Ordering::Relaxed);
        assert!(mock.start_providing().await.is_err());
    }
}
