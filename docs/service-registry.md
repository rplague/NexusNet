# 服务注册异步设计 / Service registry asynchronous design

**语言 / Language:** [中文](#中文) · [English](#english)

<a id="中文"></a>
## 中文

### 目标

- NodeController 不等待任何服务注册（service registry）DHT 操作。
- 服务注册 actor（registry actor）不被单个 DHT 查询阻塞；DHT 调用作为受控作业执行。
- `list_services` 通过 watch 快照立即返回，后台刷新类型索引。

### 结构

```text
NodeController
  ├─ services_tx: mpsc::Sender<ServiceCommand>        (bounded 256)
  └─ services_snapshot: watch::Receiver<Arc<ServiceSnapshot>>

                 ServiceCommand
                       │
                       ▼
ServiceRegistryActor
  ├─ RegistryState（纯状态机）
  ├─ JoinSet<JobResult>（最多 services.registry.max_inflight_dht 个）
  │    ├─ start_providing
  │    ├─ stop_providing
  │    ├─ get_record_network
  │    └─ put_record
  └─ snapshot_tx: watch::Sender<Arc<ServiceSnapshot>>
```

`SwarmActor` 仍是唯一拥有 `Swarm` 的任务。registry actor 通过
`NetworkHandle` 发命令；Kademlia 的多个 `QueryId` 提供 DHT 并发。

`ServiceDispatcher` 持有唯一的 `backend_ready` 集合并通过 watch 发布；
registry actor 订阅该 watch，用完整集合差分处理就绪/断开，不再接收
`BackendReady` / `BackendDown` 命令。

### 命令

| 命令 | 来源 | 说明 |
|---|---|---|
| `BootstrapCompleted` | controller | 启动 announce 与 reconcile |
| `Tick` | controller（1s） | 处理到期撤销/重试 |
| `Reannounce` | 控制指令 | 后台重新调度 |
| `RefreshTypes` | `list_services` | 后台刷新类型索引 |
| `Reset` | reload | generation +1，清空临时状态，保留 known_types |
| `RelayStateChanged` | controller | 中继就绪/失效；控制 announce 门控与撤销 |

### 状态机

`RegistryState::plan(now, slots)` 返回需要启动的 `JobSpec`；
`RegistryState::apply(result, now)` 处理作业结果。actor 每次处理命令或结果后调用
`plan`。

调度优先级：

1. 到期撤销（含中继丢失后的强制撤销）
2. 到期 announce 重试
3. 新 announce
4. types reconcile

#### 中继门控

- `need_relay = true` 且 `active_relays == 0` 时，不调度任何新 announce。
- 第一个中继就绪：清空 `announced`，重新 `start_providing`。
- 中继数量变化但仍大于 0：不重新宣告。
- 最后一个中继失效：把已宣告/正在宣告的服务加入强制撤销集合，
  逐个 `stop_providing`；在中继恢复前不再宣告。
- `need_relay = false` 的公网双栈节点不受影响。

#### announce

- 成功且后端仍 ready：记入 `announced`。
- 成功但中继已不可用：记入 `announced`，并加入强制撤销集合。
- 成功但后端已 down：记入 `announced` 并立即调度撤销。
- 失败且仍 ready：指数退避重试（5s → 60s）。
- 失败且已 down：不再重试。

#### withdraw

- 成功：从 `announced` 移除；若后端已恢复，重新调度 announce。
- 失败：重新设置重试 deadline。

#### provider 记录生命周期

- provider 记录由 libp2p Kademlia 的周期性 provider 作业维护，TTL 与重发间隔
  来自 `services.kademlia.provider_record_ttl_seconds`（默认 3600s）和
  `services.kademlia.provider_publication_interval_seconds`（默认 1800s）。
- 活跃 provider 在 TTL 前按发布间隔重发，不会因记录过期而从
  `discover_providers` 闪断。
- 停止或崩溃的 provider 不再重发，其记录最迟约一个 TTL 后从 DHT 发现中
  消失；本机制不按消费者活跃度回收，也不主动探测 provider。
- 这两个配置必须大于 0，且 TTL 不能小于 2 倍发布间隔。

#### bootstrap / reannounce

`BootstrapCompleted` 与 `Reannounce` 都会清空 `announced` 和
`announce_retry_at`，因此下一次 `plan` 会重新执行 `start_providing`。
`BootstrapCompleted` 每次 DHT bootstrap 成功都会触发（actor 侧 30 秒去抖），
避免网络分区或 DHT 重建后永久停留在“已宣告”状态。

#### reconcile types

```text
Idle --(due / RefreshTypes)--> Reading
Reading --(merge + renew leases)--> Idle | ReadyToWrite
ReadyToWrite --(plan)--> Writing
Writing --(result)--> Idle
```

- 单飞（single-flight）；`reconcile_dirty` 在当前链结束后触发立即重跑。
- 失败后 30s 重试。
- `known_types` 为 `BTreeMap<String, u64>`（服务名 -> `expires_at`），上限
  `MAX_TYPES`。
- DHT 值是无版本 tuple 数组：`[["ocr",1700003600], ...]`；单条
  `/oahd/service/types`，不分片、不加版本字段。
- reconcile 读取后合并未过期条目并取 `max(expires_at)`，丢弃
  `expires_at <= now` 的条目，写回时不再包含过期条目。
- 本地 `announced` 服务在每次 reconcile 写入新的
  `now + services.registry.type_ttl_secs`；没有本地 provider 的条目只保留
  远端 expiry，不会被续期。
- `Announced` 成功后立即写入本地 expiry 并标记 dirty。
- `stop_providing` 后不再续期，但不会立即删除；最后一个 provider 停止刷新
  后由租约自然过期。
- 不主动探测 provider，不扫描 `get_providers`。

#### types 迁移

`parse_types` 同时接受旧 `["ocr","cmd"]` 与新 tuple 数组。旧字符串条目获得
一次迁移期 expiry（`now + services.registry.type_ttl_secs`），之后必须由本地
provider 续期。新节点一律写回 tuple 格式。

这是无版本格式变更，**不做旧节点滚动兼容**：升级前应停止旧节点写入，再升级
全部节点。

### 并发与背压

- `services.registry.max_inflight_dht` 控制同时执行的 registry DHT 作业，
  默认 4，范围 1–16。
- 命令队列 256；状态类命令等待入队（不等待 DHT），
  `Tick`/`RefreshTypes`/`Reannounce` 在队列满时丢弃或返回 busy。
- reload 时 `generation` 使旧作业结果失效；`JoinSet` 在 shutdown 时排空。

### 快照

`ServiceSnapshot` 包含：

- `types`：未过期的 `known_types` 名字 ∪ `announced`
- `announced`
- `backend_ready`
- `bootstrapped`

`list_services` 读取 `types` 并触发后台 `RefreshTypes`。该命令是最终一致的：
如果 DHT 中刚出现新类型，下一次调用或后台刷新后可见；过期类型不会出现在
快照中。

### 测试

- 纯状态机测试：announce/withdraw 生命周期、grace、重试、reset、reconcile。
- mock DHT 测试：慢 announce 不阻塞 actor 命令；快照及时更新。
- 现有单元测试全部保留。

---

<a id="english"></a>
## English

### Goals

- NodeController never waits for a service-registry DHT operation.
- The registry actor is never blocked by a single DHT query; DHT calls run as controlled jobs.
- `list_services` returns a watch snapshot immediately and refreshes the type index in the background.

### Structure

```text
NodeController
  ├─ services_tx: mpsc::Sender<ServiceCommand>        (bounded 256)
  └─ services_snapshot: watch::Receiver<Arc<ServiceSnapshot>>

                 ServiceCommand
                       │
                       ▼
ServiceRegistryActor
  ├─ RegistryState (pure state machine)
  ├─ JoinSet<JobResult> (at most services.registry.max_inflight_dht)
  │    ├─ start_providing
  │    ├─ stop_providing
  │    ├─ get_record_network
  │    └─ put_record
  └─ snapshot_tx: watch::Sender<Arc<ServiceSnapshot>>
```

`SwarmActor` remains the only task that owns the `Swarm`. The registry actor
sends commands through `NetworkHandle`; multiple Kademlia `QueryId`s provide DHT
concurrency.

`ServiceDispatcher` owns the single `backend_ready` set and publishes it over a
watch channel. The registry actor subscribes to that watch and diffs the complete
set on readiness/teardown, so it no longer receives `BackendReady` / `BackendDown`
commands.

### Commands

| Command | Source | Purpose |
|---|---|---|
| `BootstrapCompleted` | controller | Start announcements and reconciliation |
| `Tick` | controller (1 s) | Handle due withdrawals and retries |
| `Reannounce` | control command | Reschedule in the background |
| `RefreshTypes` | `list_services` | Refresh the type index in the background |
| `Reset` | reload | Increment generation, clear transient state, keep known_types |
| `RelayStateChanged` | controller | Relay ready/lost; controls announce gating and withdrawals |

### State machine

`RegistryState::plan(now, slots)` returns the `JobSpec`s that should start;
`RegistryState::apply(result, now)` processes job results. The actor calls `plan`
after every command or result.

Scheduling priority:

1. Due withdrawals (including forced withdrawals after relay loss)
2. Due announce retries
3. New announcements
4. Type reconciliation

#### Relay gating

- When `need_relay = true` and `active_relays == 0`, no new announcement is scheduled.
- First relay ready: clear `announced` and `start_providing` again.
- Relay count changes but stays above zero: no re-announcement.
- Last relay lost: add announced and in-progress services to the forced-withdrawal set and `stop_providing` them one by one; no announcements until a relay returns.
- Dual-stack public nodes with `need_relay = false` are unaffected.

#### announce

- Success and backend still ready: record it in `announced`.
- Success but the relay is no longer available: record it in `announced` and add it to the forced-withdrawal set.
- Success but the backend is down: record it in `announced` and schedule withdrawal immediately.
- Failure and still ready: retry with exponential backoff (5 s → 60 s).
- Failure and down: stop retrying.

#### withdraw

- Success: remove from `announced`; if the backend has recovered, schedule an announcement again.
- Failure: set a new retry deadline.

#### Provider record lifecycle

- Provider records are maintained by libp2p Kademlia's periodic provider job. TTL and republish interval come from `services.kademlia.provider_record_ttl_seconds` (default 3600 s) and `services.kademlia.provider_publication_interval_seconds` (default 1800 s).
- An active provider republishes before the TTL, so `discover_providers` does not briefly lose it when a record expires.
- A stopped or crashed provider stops republishing, and its record disappears from DHT discovery at most about one TTL later. This mechanism does not reclaim based on consumer activity and does not probe providers.
- Both settings must be greater than 0, and the TTL must be at least twice the publication interval.

#### bootstrap / reannounce

`BootstrapCompleted` and `Reannounce` both clear `announced` and
`announce_retry_at`, so the next `plan` runs `start_providing` again.
`BootstrapCompleted` fires after every successful DHT bootstrap (debounced by 30
seconds in the actor), which prevents a node from staying permanently
"announced" after a partition or DHT rebuild.

#### reconcile types

```text
Idle --(due / RefreshTypes)--> Reading
Reading --(merge + renew leases)--> Idle | ReadyToWrite
ReadyToWrite --(plan)--> Writing
Writing --(result)--> Idle
```

- Single-flight; `reconcile_dirty` triggers an immediate rerun after the current chain finishes.
- Retry 30 s after a failure.
- `known_types` is a `BTreeMap<String, u64>` (service name -> `expires_at`), capped at `MAX_TYPES`.
- The DHT value is an unversioned tuple array: `[["ocr",1700003600], ...]`, stored as a single `/oahd/service/types` record with no sharding and no version field.
- After reading, reconcile merges unexpired entries and keeps `max(expires_at)`, drops entries with `expires_at <= now`, and writes back without expired entries.
- Locally `announced` services get a fresh `now + services.registry.type_ttl_secs` on every reconcile; entries without a local provider keep only the remote expiry and are not renewed.
- After a successful `Announced`, the local expiry is written immediately and the state is marked dirty.
- After `stop_providing` the entry is no longer renewed but is not deleted immediately; once the last provider stops refreshing, the lease expires naturally.
- Providers are not probed proactively, and `get_providers` is not scanned.

#### types migration

`parse_types` accepts both the old `["ocr","cmd"]` form and the new tuple array.
Old string entries receive a one-time migration expiry
(`now + services.registry.type_ttl_secs`) and must then be renewed by a local
provider. New nodes always write the tuple format.

This is an unversioned format change with **no rolling compatibility**: stop old
writers before upgrading the remaining nodes.

### Concurrency and backpressure

- `services.registry.max_inflight_dht` controls concurrent registry DHT jobs; default 4, range 1–16.
- The command queue holds 256 entries. State commands wait to enqueue (without waiting on DHT); `Tick`/`RefreshTypes`/`Reannounce` are dropped or return busy when the queue is full.
- On reload, `generation` invalidates old job results; the `JoinSet` is drained on shutdown.

### Snapshot

`ServiceSnapshot` contains:

- `types`: unexpired `known_types` names ∪ `announced`
- `announced`
- `backend_ready`
- `bootstrapped`

`list_services` reads `types` and triggers a background `RefreshTypes`. The
command is eventually consistent: a type that just appeared in the DHT becomes
visible on the next call or background refresh; expired types never appear in
the snapshot.

### Tests

- Pure state-machine tests: announce/withdraw lifecycle, grace, retries, reset, reconcile.
- Mock DHT tests: a slow announce does not block actor commands; snapshots update promptly.
- All existing unit tests are kept.
