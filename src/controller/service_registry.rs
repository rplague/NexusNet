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
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use libp2p::kad;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::auth;
use crate::config::ConfigHandle;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;
use crate::service_dispatcher::BackendReadiness;

const WITHDRAW_GRACE: Duration = Duration::from_secs(3);
const COMMAND_QUEUE_CAPACITY: usize = 256;
const ANNOUNCE_RETRY_INITIAL: Duration = Duration::from_secs(5);
const ANNOUNCE_RETRY_MAX: Duration = Duration::from_secs(60);

fn service_key(name: &str) -> kad::RecordKey {
    kad::RecordKey::new(&format!("/oahd/service/{name}"))
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ServiceSnapshot {
    pub announced: Vec<String>,
    pub backend_ready: Vec<String>,
    pub bootstrapped: bool,
}

#[derive(Debug, Clone)]
pub enum ServiceCommand {
    BootstrapCompleted,
    Tick,
    Reannounce,
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
            generation: 0,
        }
    }

    fn retry_delay(failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(4);
        (ANNOUNCE_RETRY_INITIAL * (1u32 << shift)).min(ANNOUNCE_RETRY_MAX)
    }

    fn replace_backend_ready(&mut self, ready: &HashSet<String>, now: Instant) {
        let previous = std::mem::replace(&mut self.backend_ready, ready.clone());
        for service in ready.difference(&previous) {
            self.withdraw_deadline.remove(service);
            self.announce_failures.remove(service);
            self.announce_retry_at.remove(service);
        }
        for service in previous.difference(ready) {
            self.announce_retry_at.remove(service);
            if self.announced.contains(service) {
                self.withdraw_deadline
                    .entry(service.clone())
                    .or_insert_with(|| now + WITHDRAW_GRACE);
            }
        }
    }

    #[cfg(test)]
    fn set_backend_ready(&mut self, service: &str, ready: bool, now: Instant) {
        let mut set = self.backend_ready.clone();
        if ready {
            set.insert(service.to_string());
        } else {
            set.remove(service);
        }
        self.replace_backend_ready(&set, now);
    }

    fn handle(&mut self, command: ServiceCommand, _now: Instant) {
        match command {
            ServiceCommand::BootstrapCompleted => {
                self.bootstrapped = true;
                // The DHT may have been rebuilt or partitioned. Drop the local
                // "already announced" cache and let the planner re-announce.
                self.announced.clear();
                self.announce_retry_at.clear();
            }
            ServiceCommand::Tick => {}
            ServiceCommand::Reannounce => {
                // Re-announce for real: clearing `announced` makes `plan`
                // schedule `start_providing` again.
                self.announced.clear();
                self.announce_retry_at.clear();
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
        }
    }

    fn snapshot(&self) -> ServiceSnapshot {
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
}

pub struct ServiceRegistryHandle {
    pub tx: mpsc::Sender<ServiceCommand>,
    pub snapshot: watch::Receiver<Arc<ServiceSnapshot>>,
    #[cfg(test)]
    pub loops: Arc<AtomicUsize>,
}

struct ServiceRegistryActor {
    config: ConfigHandle,
    dht: DhtClient,
    state: RegistryState,
    cmd_rx: mpsc::Receiver<ServiceCommand>,
    readiness_rx: watch::Receiver<Arc<BackendReadiness>>,
    shutdown_rx: watch::Receiver<bool>,
    snapshot_tx: watch::Sender<Arc<ServiceSnapshot>>,
    jobs: JoinSet<JobResult>,
    #[cfg(test)]
    loops: Arc<AtomicUsize>,
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
        }
    }

    async fn run(mut self) {
        let initial = self.readiness_rx.borrow().clone();
        self.state
            .replace_backend_ready(&initial.ready, Instant::now());
        self.snapshot();
        self.pump();

        loop {
            #[cfg(test)]
            self.loops.fetch_add(1, Ordering::Relaxed);
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
                Some(result) = self.jobs.join_next() => {
                    match result {
                        Ok(result) => {
                            self.state.apply(result, Instant::now());
                            self.snapshot();
                        }
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => {
                            LogStruct::new(
                                LogLevel::Error,
                                "服务注册任务异常退出",
                                error.to_string(),
                            )
                            .emit();
                        }
                    }
                    self.pump();
                }
                changed = self.readiness_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let ready = self.readiness_rx.borrow_and_update().clone();
                    self.state
                        .replace_backend_ready(&ready.ready, Instant::now());
                    self.snapshot();
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

pub fn spawn(
    config: ConfigHandle,
    swarm: NetworkHandle,
    readiness_rx: watch::Receiver<Arc<BackendReadiness>>,
    shutdown_rx: watch::Receiver<bool>,
) -> ServiceRegistryHandle {
    spawn_with_dht(config, DhtClient::Real(swarm), readiness_rx, shutdown_rx)
}

fn spawn_with_dht(
    config: ConfigHandle,
    dht: DhtClient,
    readiness_rx: watch::Receiver<Arc<BackendReadiness>>,
    shutdown_rx: watch::Receiver<bool>,
) -> ServiceRegistryHandle {
    let (tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let (snapshot_tx, snapshot) = watch::channel(Arc::new(ServiceSnapshot::default()));
    #[cfg(test)]
    let loops = Arc::new(AtomicUsize::new(0));
    let state = RegistryState::new(config.clone());
    let actor = ServiceRegistryActor {
        config,
        dht,
        state,
        cmd_rx,
        readiness_rx,
        shutdown_rx,
        snapshot_tx,
        jobs: JoinSet::new(),
        #[cfg(test)]
        loops: loops.clone(),
    };
    tokio::spawn(async move {
        actor.run().await;
    });
    ServiceRegistryHandle {
        tx,
        snapshot,
        #[cfg(test)]
        loops,
    }
}

#[cfg(test)]
struct MockDht {
    start_delay: Duration,
    fail_start: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl MockDht {
    fn new(start_delay: Duration) -> Self {
        Self {
            start_delay,
            fail_start: std::sync::atomic::AtomicBool::new(false),
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
            state.set_backend_ready(name, true, now);
        }
        state.handle(ServiceCommand::BootstrapCompleted, now);
        state.plan(now, 8)
    }

    #[test]
    fn waits_for_bootstrap_before_announcing() {
        let mut state = RegistryState::new(config_with_services(&["cmd"]));
        let now = Instant::now();
        state.set_backend_ready("cmd", true, now);
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
            state.set_backend_ready(name, true, now);
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

        state.set_backend_ready("cmd", false, now);
        assert!(state.plan(now + Duration::from_secs(1), 4).is_empty());
        state.set_backend_ready("cmd", true, now + Duration::from_secs(2));
        assert!(state.plan(now + Duration::from_secs(4), 4).is_empty());

        state.set_backend_ready("cmd", false, now + Duration::from_secs(5));
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
        state.set_backend_ready("cmd", false, now);
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
        state.set_backend_ready("cmd", false, now);
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
    }

    #[tokio::test]
    async fn idle_actor_does_not_spin() {
        let config = config_with_services(&[]);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (_readiness_tx, readiness_rx) = watch::channel(Arc::new(BackendReadiness::default()));
        let mock = Arc::new(MockDht::new(Duration::ZERO));
        let handle = spawn_with_dht(config, DhtClient::Mock(mock), readiness_rx, shutdown_rx);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let loops = handle.loops.load(Ordering::Relaxed);
        assert!(loops < 100, "registry actor spun {loops} times while idle");
    }

    #[tokio::test]
    async fn actor_processes_commands_while_announce_is_slow() {
        let config = config_with_services(&["cmd"]);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (readiness_tx, readiness_rx) = watch::channel(Arc::new(BackendReadiness::default()));
        let mock = Arc::new(MockDht::new(Duration::from_millis(500)));
        let handle = spawn_with_dht(config, DhtClient::Mock(mock), readiness_rx, shutdown_rx);

        readiness_tx
            .send(Arc::new(BackendReadiness {
                ready: HashSet::from(["cmd".to_string()]),
            }))
            .unwrap();
        handle
            .tx
            .send(ServiceCommand::BootstrapCompleted)
            .await
            .unwrap();
        readiness_tx
            .send(Arc::new(BackendReadiness::default()))
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
