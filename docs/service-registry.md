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
| `RelayStateChanged` | controller | relay 就绪/失效；控制 announce 门控与撤销 |

## 状态机

`RegistryState::plan(now, slots)` 返回需要启动的 `JobSpec`；
`RegistryState::apply(result, now)` 处理 job 结果。actor 每次处理命令或结果后调用
`plan`。

调度优先级：

1. 到期撤销（含 relay 丢失后的强制撤销）
2. 到期 announce 重试
3. 新 announce
4. types reconcile

### relay 门控

- `need_relay = true` 且 `active_relays == 0` 时，不调度任何新 announce。
- 第一个 relay 就绪：清空 `announced`，重新 `start_providing`。
- relay 数量变化但仍大于 0：不重新宣告。
- 最后一个 relay 失效：把已宣告/正在宣告的服务加入强制撤销集合，
  逐个 `stop_providing`；在 relay 恢复前不再宣告。
- `need_relay = false` 的公网双栈节点不受影响。

### announce

- 成功且后端仍 ready：记入 `announced`。
- 成功但 relay 已不可用：记入 `announced`，并加入强制撤销集合。
- 成功但后端已 down：记入 `announced` 并立即调度撤销。
- 失败且仍 ready：指数退避重试（5s → 60s）。
- 失败且已 down：不再重试。

### withdraw

- 成功：从 `announced` 移除；若后端已恢复，重新调度 announce。
- 失败：重新设置重试 deadline。

### provider 记录生命周期

- provider 记录由 libp2p Kademlia 的周期性 provider job 维护，TTL 与重发间隔
  来自 `services.kademlia.provider_record_ttl_seconds`（默认 3600s）和
  `services.kademlia.provider_publication_interval_seconds`（默认 1800s）。
- 活跃 provider 在 TTL 前按发布间隔重发，不会因记录过期而从
  `discover_providers` 闪断。
- 停止或崩溃的 provider 不再重发，其记录最迟约一个 TTL 后从 DHT 发现中
  消失；本机制不按消费者活跃度回收，也不主动探测 provider。
- 这两个配置必须大于 0，且 TTL 不能小于 2 倍发布间隔。

### bootstrap / reannounce

`BootstrapCompleted` 与 `Reannounce` 都会清空 `announced` 和
`announce_retry_at`，因此下一次 `plan` 会重新执行 `start_providing`。
`BootstrapCompleted` 每次 DHT bootstrap 成功都会触发（actor 侧 30 秒去抖），
避免网络分区或 DHT 重建后永久停留在“已宣告”状态。

### reconcile types

```text
Idle --(due / RefreshTypes)--> Reading
Reading --(merge + renew leases)--> Idle | ReadyToWrite
ReadyToWrite --(plan)--> Writing
Writing --(result)--> Idle
```

- single-flight；`reconcile_dirty` 在当前链结束后触发立即重跑。
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

### types 迁移

`parse_types` 同时接受旧 `["ocr","cmd"]` 与新 tuple 数组。旧字符串条目获得
一次迁移期 expiry（`now + services.registry.type_ttl_secs`），之后必须由本地
provider 续期。新节点一律写回 tuple 格式。

这是无版本格式变更，**不做旧节点滚动兼容**：升级前应停止旧节点写入，再升级
全部节点。

## 并发与背压

- `services.registry.max_inflight_dht` 控制同时执行的 registry DHT job，
  默认 4，范围 1–16。
- 命令队列 256；状态类命令等待入队（不等待 DHT），
  `Tick`/`RefreshTypes`/`Reannounce` 在队列满时丢弃或返回 busy。
- reload 时 `generation` 使旧 job 结果失效；`JoinSet` 在 shutdown 时排空。

## 快照

`ServiceSnapshot` 包含：

- `types`：未过期的 `known_types` 名字 ∪ `announced`
- `announced`
- `backend_ready`
- `bootstrapped`

`list_services` 读取 `types` 并触发后台 `RefreshTypes`。该命令是最终一致的：
如果 DHT 中刚出现新类型，下一次调用或后台刷新后可见；过期类型不会出现在
快照中。

## 测试

- 纯状态机测试：announce/withdraw 生命周期、grace、重试、reset、reconcile。
- mock DHT 测试：慢 announce 不阻塞 actor 命令；快照及时更新。
- 现有单元测试全部保留。
