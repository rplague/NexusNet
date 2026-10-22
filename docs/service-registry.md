# Service Registry 异步设计

## 目标

- NodeController 不等待 service-registry DHT 操作。
- registry actor 不被单个 DHT 查询阻塞；DHT 调用作为受控 job 执行。
- 服务类型索引不再由 registry 维护，改为独立的签名文档 SWR 缓存。

## 结构

```text
NodeController
  ├─ services_tx: mpsc::Sender<ServiceCommand>        (bounded 256)
  ├─ services_snapshot: watch::Receiver<Arc<ServiceSnapshot>>
  └─ types_cache: DhtCache<(), BTreeSet<String>, TypesFetcher>

                 ServiceCommand
                       │
                       ▼
ServiceRegistryActor
  ├─ RegistryState（纯状态机）
  ├─ JoinSet<JobResult>（最多 services.registry.max_inflight_dht 个）
  │    ├─ start_providing
  │    └─ stop_providing
  └─ snapshot_tx: watch::Sender<Arc<ServiceSnapshot>>
```

`SwarmActor` 仍是唯一拥有 `Swarm` 的任务。registry actor 通过
`NetworkHandle` 发命令；Kademlia 的多个 `QueryId` 提供 DHT 并发。

`ServiceDispatcher` 持有唯一的 `backend_ready` 集合并通过 watch 发布；
registry actor 订阅该 watch，用完整集合差分处理就绪/断开。

## 命令

| 命令 | 来源 | 说明 |
|---|---|---|
| `BootstrapCompleted` | controller | 清空 `announced`，让规划器重新宣告 |
| `Tick` | controller（1s） | 处理到期撤销/重试 |
| `Reannounce` | 控制命令 | 清空 `announced` 并重新调度 |
| `Reset` | reload | generation +1，清空临时状态 |

## 状态机

`RegistryState::plan(now, slots)` 返回需要启动的 `JobSpec`；
`RegistryState::apply(result, now)` 处理 job 结果。actor 每次处理命令或结果后调用
`plan`。

调度优先级：

1. 到期撤销
2. 到期 announce 重试
3. 新 announce

### announce

- 成功且后端仍 ready：记入 `announced`。
- 成功但后端已 down：记入 `announced` 并立即调度撤销。
- 失败且仍 ready：指数退避重试（5s → 60s）。
- 失败且已 down：不再重试。

### withdraw

- 成功：从 `announced` 移除；若后端已恢复，重新调度 announce。
- 失败：重新设置重试 deadline。

### bootstrap / reannounce

`BootstrapCompleted` 与 `Reannounce` 都会清空 `announced` 和
`announce_retry_at`，因此下一次 `plan` 会重新执行 `start_providing`。
`BootstrapCompleted` 每次 DHT bootstrap 成功都会触发（actor 侧 30 秒去抖），
避免网络分区或 DHT 重建后永久停留在“已宣告”状态。

## 类型索引

类型索引不属于 registry 状态机：

- DHT key：`/oahd/service/authtypes/`。
- 值：authority 签名的 `TypesDoc`（COSE_Sign1）。
- 读取：`DhtCache<(), BTreeSet<String>, TypesFetcher>`。
  - `fresh_for = services.dispatcher.types_sync_secs`
  - stale 允许到文档 `expires_at`
  - 只接受最高合法 `version`，整体替换
- 发布：`publish_types` 控制指令，`Quorum::One`（旧网络兼容）。
- 未配置 `services.registry.types_authority` 时进入旧版兼容模式（见下）。

`list_services` = 类型缓存 ∪ `announced` ∪ `backend_ready`，立即返回并触发后台刷新。

## 并发与背压

- `services.registry.max_inflight_dht` 控制同时执行的 registry DHT job，
  默认 4，范围 1–16。
- 命令队列 256；状态类命令等待入队（不等待 DHT），
  `Tick`/`Reannounce` 在队列满时丢弃或返回 busy。
- reload 时 `generation` 使旧 job 结果失效；`JoinSet` 在 shutdown 时排空。

## 快照

`ServiceSnapshot` 包含：

- `announced`
- `backend_ready`
- `bootstrapped`

`ServiceSnapshot.types` 已移除；全局类型由 NodeController 的 `types_cache` 提供。

## 测试

- 纯状态机测试：announce/withdraw 生命周期、grace、重试、reset。
- bootstrap/reannounce 清空 `announced` 并重新宣告。
- mock DHT 测试：慢 announce 不阻塞 actor 命令；快照及时更新。
- 类型签名/版本/过期/冲突测试位于 `types_doc` 与 `dht_cache`。

## 旧版兼容

- 旧 key：`/oahd/service/types`，值为未签名 JSON 数组 `["cmd","ocr"]`。
- 配置了 `types_authority`：
  - 新节点使用 `/oahd/service/authtypes/` 的签名文档；
  - 默认 `compat.legacy_types_merge = true`，`list_services` 同时合并旧 key 类型；
  - `publish_types` 把“签名类型 ∪ 旧 key 类型”镜像写入旧 key，供旧节点读取；
  - 签名文档缺失时，新节点仍然回退读取旧 key。
- 未配置 `types_authority`：
  - 进入旧版模式，读取旧 key 并合并所有记录；
  - 周期（`types_sync_secs`）把 `旧 key 类型 ∪ 本地 announced` 发布回旧 key；
  - 启动日志记录 Warning，提示配置 `types_authority` 以启用签名索引。
- 旧 key 与新 key 不冲突；`/oahd/service/authtypes/` 末尾的 `/` 也避免了与服务名 `authtypes` 的 provider key 冲突。
- `compat.max_record_bytes` 默认 14 KiB；旧 key 镜像超过上限时按前缀裁剪并记录 Warning。

## 签名类型文档

`TypesDoc` 的 CBOR payload：

```text
{
  "version":    uint,      # 单调递增；续期也需递增
  "issued_at":  uint,      # Unix 秒
  "expires_at": uint,      # Unix 秒，issued_at < expires_at
  "services":   [tstr],    # 去重后的服务名
}
```

签名规则与鉴权记录相同：

- `COSE_Sign1`，保护头 `alg = EdDSA`；
- `external_aad = "/oahd/service/authtypes/"` 的 UTF-8 字节；
- 使用 `services.registry.types_authority` 对应的 Ed25519 私钥签名；
- 文档生命周期不得超过 `services.registry.types_ttl_seconds`。

发布：

```text
publish_types { t:"publish_types", id:<uuid>, value:<COSE_Sign1 bstr> }
```

节点验签、检查版本/过期/名称后，以 `Quorum::All` 写入
`/oahd/service/authtypes/`。读取端只接受最高合法版本，整体替换旧列表。
