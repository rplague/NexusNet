# Service Registry 异步设计

## 目标

- NodeController 不等待任何 service-registry DHT 操作。
- registry actor 不被单个 DHT 查询阻塞；DHT 调用作为受控 job 执行。
- `list_services` 通过 watch 快照立即返回，后台刷新类型索引。

## 结构

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

## 命令

| 命令 | 来源 | 说明 |
|---|---|---|
| `BootstrapCompleted` | controller | 启动 announce 与 reconcile |
| `Tick` | controller（1s） | 处理到期撤销/重试 |
| `Reannounce` | 控制命令 | 后台重新调度 |
| `RefreshTypes` | `list_services` | 后台刷新类型索引 |
| `Reset` | reload | generation +1，清空临时状态，保留 known_types |

## 状态机

`RegistryState::plan(now, slots)` 返回需要启动的 `JobSpec`；
`RegistryState::apply(result, now)` 处理 job 结果。actor 每次处理命令或结果后调用
`plan`。

调度优先级：

1. 到期撤销
2. 到期 announce 重试
3. 新 announce
4. types reconcile

### announce

- 成功且后端仍 ready：记入 `announced`。
- 成功但后端已 down：记入 `announced` 并立即调度撤销。
- 失败且仍 ready：指数退避重试（5s → 60s）。
- 失败且已 down：不再重试。

### withdraw

- 成功：从 `announced` 移除；若后端已恢复，重新调度 announce。
- 失败：重新设置重试 deadline。

### reconcile types

```text
Idle --(due / RefreshTypes)--> Reading
Reading --(merge known_types)--> Idle | ReadyToWrite
ReadyToWrite --(plan)--> Writing
Writing --(result)--> Idle
```

- single-flight；`reconcile_dirty` 在当前链结束后触发立即重跑。
- 失败后 30s 重试。
- `known_types` 保持单调并集，上限 `MAX_TYPES`。

## 并发与背压

- `services.registry.max_inflight_dht` 控制同时执行的 registry DHT job，
  默认 4，范围 1–16。
- 命令队列 256；状态类命令等待入队（不等待 DHT），
  `Tick`/`RefreshTypes`/`Reannounce` 在队列满时丢弃或返回 busy。
- reload 时 `generation` 使旧 job 结果失效；`JoinSet` 在 shutdown 时排空。

## 快照

`ServiceSnapshot` 包含：

- `types`：`known_types ∪ announced`
- `announced`
- `backend_ready`
- `bootstrapped`

`list_services` 读取 `types` 并触发后台 `RefreshTypes`。该命令是最终一致的：
如果 DHT 中刚出现新类型，下一次调用或后台刷新后可见。

## 测试

- 纯状态机测试：announce/withdraw 生命周期、grace、重试、reset、reconcile。
- mock DHT 测试：慢 announce 不阻塞 actor 命令；快照及时更新。
- 现有单元测试全部保留。
