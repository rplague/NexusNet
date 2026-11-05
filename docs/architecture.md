# 架构与内部实现 / Architecture and internals

**语言 / Language:** [中文](#中文) · [English](#english)

<a id="中文"></a>
## 中文

本文描述 NexusNet 节点在运行时的组成。协议契约见 [sidecar.cddl](./sidecar.cddl)、
[service-registry.md](./service-registry.md)、[nat-relay.md](./nat-relay.md) 与
[auth.md](./auth.md)。

### 进程模型

节点是微内核加边车：

| 组件 | 职责 |
|---|---|
| `main` / `runtime` | 启动编排与信号处理。`runtime::start_node` 由桌面二进制和移动端共用。 |
| `SwarmActor` | 唯一持有 libp2p `Swarm` 的任务。所有网络命令经它串行化执行。 |
| `NodeController` | 单事件循环，处理网络事件、后端命令、服务 tick、relay tick 和关闭。 |
| `ServiceDispatcher` | 持有后端 TCP 连接、就绪状态、重连和请求转发。 |
| `ServiceRegistryActor` | 纯状态机加有界 DHT job，负责宣告、撤销和类型索引。 |
| `AuthGate` | 验证签名鉴权记录并执行 fail-closed 判定。 |
| `DhtCache` | 读穿透缓存，每 key 单飞刷新。 |

桌面二进制只是在 `runtime::start_node` 外层加了信号处理和日志模式；移动端桥接
用同一 runtime，并传入显式路径和日志 sink。

### 启动

```text
start_node(options)
  ├─ 安装日志 sink（可选）
  ├─ 读取 config.toml，补全默认值并原子写回
  ├─ 加载或生成 keypair.bin（Ed25519 身份）
  ├─ 启用 PQ 时加载 keypair.pq.bin（可选）
  ├─ 等待配置的地址族（桌面）
  ├─ 探测公网地址并更新配置（桌面）
  ├─ Network::start → 构建 Swarm，启动 SwarmActor
  ├─ 拨号 bootstrap 节点
  ├─ 启动 ServiceDispatcher
  └─ 启动 NodeController
       tokio::select! {
           event_rx          → 网络事件
           to_controller_rx  → 后端命令与就绪状态
           service_tick      → 到期撤销 / 重试（1 秒）
           relay_tick        → relay 补约与 bootstrap 宣告
           shutdown          → 优雅退出
       }
```

### 事件循环与背压

所有队列都有界。过载时返回明确错误，而不是无限制增长内存。

| 队列 | 上限 | 满时行为 |
|---|---|---|
| 鉴权 worker | 4 个 worker × 32 | 入站请求返回 `authentication overloaded` |
| dispatcher 入站 | 256 | 入站请求返回 `backend overloaded` |
| controller 控制 | 128 | 控制命令失败或按操作等待 |
| 每后端命令 / 事件 | 各 64 | 后端任务施加背压或报错 |
| 服务注册命令 | 256 | 状态类命令等待；`Tick`/`RefreshTypes`/`Reannounce` 丢弃或返回 busy |

DHT 工作与 controller 隔离：

- dispatcher 的 DHT 命令在有界 worker 池中执行（`services.dispatcher.max_inflight_dht`，默认 4，范围 1–16）。
- registry 的 DHT job 在 `JoinSet` 中执行，上限为 `services.registry.max_inflight_dht`。
- `whoami`、`relay_status`、`pq_status`、`auth_status` 和 `list_services` 从不等待 DHT。

### 服务注册生命周期

本地服务只有在节点确实能提供服务后才对外可见：

1. 后端完成 `hello` 握手。`ServiceDispatcher` 通过 watch 通道发布唯一的 `backend_ready` 集合。
2. DHT bootstrap 完成。
3. 节点可达。需要 relay 的节点在至少一条 circuit 就绪前不宣告。

`ServiceRegistryActor` 按以下优先级 reconcile 宣告：到期撤销、到期重试、新宣告，
最后是类型索引 reconcile。
后端断开后进入短暂宽限期（约 3–4 秒，由 1 秒 tick 检查）；重连则取消撤销。
宣告成功记入 `announced`；失败按指数退避重试（5 秒 → 60 秒）。

`BootstrapCompleted` 和 `reannounce_services` 会清空 `announced` 缓存，使节点在
DHT 重建后重新宣告，而不是停留在旧状态。

### relay 门控

除非节点是双栈**且**有公网地址，否则 `needs_relay` 为 true。需要 relay 时：

- 第一个 active relay 出现前：不调度任何宣告。
- 第一个 relay 就绪：清空 `announced` 并重新宣告服务。
- relay 数量变化但仍大于 0：不重新宣告。
- 最后一个 relay 消失：撤销所有已宣告服务，并在 relay 恢复前阻止新的宣告。

provider 记录由 libp2p 的周期性 provider job 维护。活跃 provider 按
`provider_publication_interval_seconds`（默认 1800 秒）重发；provider 停止后，
记录最迟约一个 `provider_record_ttl_seconds`（默认 3600 秒）后消失。

### 请求路径

```text
backend → control(service_request)
  → ServiceDispatcher backend_read_loop
  → ControlRequest → NodeController
  → discover_providers（DHT get_providers）
  → 按 RTT 排序候选
  → send_request_to_peer
  → 失败时：get_closest_peers(peer) 解析 relay 地址后重试
  → CBOR request-response
  → 远端 NodeController → InboundServiceRequest
  → 远端 ServiceDispatcher → backend 任务
  → request 帧 → 远端后端 → reply 沿原路返回
```

`service_request` 优先使用就绪的本地后端，跳过 DHT、PQ 和鉴权门。
`service_request_to(self, service)` 同样走本地。本地后端未就绪时，
`service_request` 回退到远端 provider，而 `service_request_to(self)` 返回
`backend_not_ready`。

### DHT 读缓存

`DhtCache` 是读穿透的，并且每个 key 只允许一个进行中的刷新：

- 有旧结果时立即返回，若没有进行中的刷新则启动后台刷新。
- 刷新完成后覆盖缓存；成功查到“不存在”也会覆盖为空结果。
- 冷 key 等待第一次查询，并返回其结果或真实错误。
- 刷新失败保留旧值并记录日志；下一次读取会重试。
- 没有新鲜度窗口、全局 permit、缓存超时或失败退避。

鉴权使用同一缓存，但每次判定仍检查 `expires_at` 和 `version`，因此过期或回滚的
文档即使来自缓存也会被拒绝。`/oahd/service/types` 绕过缓存，由 registry 的租约
逻辑处理。

### 鉴权路径

对每个入站请求，鉴权门按顺序检查：

1. `auth.network == "none"` → 放行。
2. 服务不要求鉴权 → 放行。
3. 索引记录缺失或过期 → 拒绝。
4. 服务不在索引中 → 本次放行并记 Warning。
5. 白名单缺失 → 拒绝；peer 不在 members 中 → 拒绝。

文档是 COSE_Sign1 + EdDSA，并通过 `external_aad` 绑定到其 DHT key。版本必须递增，
hash 必须与索引条目一致。字节级契约见 [auth.md](./auth.md)。

### 后量子层

启用后，服务调用使用 `/oahd/service_req/2.0.0`：调用方封装到响应方的混合 KEM
公钥，双方派生同一个共享密钥，用于请求和响应。只有在确认对端不支持 PQ 时才使用
明文协议；能力未知时返回可重试错误，不会静默降级。`crypto.pq_required` 会直接
拒绝非 PQ 对端。

`pq_identity_enabled` 额外要求 ML-DSA-65 签名，并把签名身份绑定到传输层 PeerId。

### 大小限制

- `services.dispatcher.max_request_bytes` / `max_response_bytes`：默认 16 MiB，每条消息范围 1–64 MiB。
- `compat.max_record_bytes`：默认 14 KiB，保证 DHT 记录仍适配旧版 16 KiB Kademlia 包。
- `sidecar_protocol::MAX_FRAME`：每个后端帧 16 MiB。
- PQ 请求额外增加约 8 KiB 的 KEM、nonce 和可选身份数据。

relay server 的 `max_circuit_bytes` 取 `max_request_bytes + max_response_bytes`，
因此 bootstrap 的上限必须不小于客户端。

---

<a id="english"></a>
## English

This document describes how a NexusNet node is put together at runtime. For the
protocol contracts see [sidecar.cddl](./sidecar.cddl), [service-registry.md](./service-registry.md),
[nat-relay.md](./nat-relay.md) and [auth.md](./auth.md).

### Process model

A node is a microkernel surrounded by sidecars:

| Component | Responsibility |
|---|---|
| `main` / `runtime` | Startup orchestration and signal handling. `runtime::start_node` is shared by the desktop binary and mobile hosts. |
| `SwarmActor` | The only task that owns the libp2p `Swarm`. All network commands are serialized through it. |
| `NodeController` | Single event loop that handles network events, backend commands, service ticks, relay ticks and shutdown. |
| `ServiceDispatcher` | Owns backend TCP connections, readiness, reconnects and request forwarding. |
| `ServiceRegistryActor` | Pure state machine plus bounded DHT jobs for announcements, withdrawals and the type index. |
| `AuthGate` | Verifies signed authorization records and applies fail-closed decisions. |
| `DhtCache` | Read-through cache with per-key single-flight refresh. |

The desktop binary only adds signal handling and logging mode around
`runtime::start_node`; mobile bridges call the same runtime with explicit paths
and a log sink.

### Startup

```text
start_node(options)
  ├─ install log sink (optional)
  ├─ read config.toml, filling defaults and writing them back
  ├─ load or create keypair.bin (Ed25519 identity)
  ├─ load keypair.pq.bin when PQ is enabled (optional)
  ├─ wait for configured address families (desktop)
  ├─ probe public addresses and update config (desktop)
  ├─ Network::start → build Swarm, spawn SwarmActor
  ├─ dial bootstrap peers
  ├─ spawn ServiceDispatcher
  └─ spawn NodeController
       tokio::select! {
           event_rx          → network events
           to_controller_rx  → backend commands and readiness
           service_tick      → due withdrawals / retries (1 s)
           relay_tick        → relay top-up and bootstrap announce
           shutdown          → graceful exit
       }
```

### Event loop and backpressure

Every queue is bounded. Overload produces an explicit error instead of unbounded
memory growth.

| Queue | Bound | Behavior when full |
|---|---|---|
| Authentication workers | 4 workers × 32 | Inbound request returns `authentication overloaded` |
| Dispatcher inbound | 256 | Inbound request returns `backend overloaded` |
| Controller control | 128 | Control command fails or waits, depending on op |
| Per-backend command / event | 64 each | Backend task applies backpressure or errors |
| Service registry commands | 256 | State commands wait; `Tick`/`RefreshTypes`/`Reannounce` drop or report busy |

DHT work is isolated from the controller:

- Dispatcher DHT commands run in a bounded worker pool (`services.dispatcher.max_inflight_dht`, default 4, range 1–16).
- Registry DHT jobs run in a `JoinSet` capped by `services.registry.max_inflight_dht`.
- `whoami`, `relay_status`, `pq_status`, `auth_status` and `list_services` never wait on DHT.

### Service registration lifecycle

A local service becomes visible only when the node can actually serve it:

1. The backend completes the `hello` handshake. `ServiceDispatcher` publishes the single `backend_ready` set over a watch channel.
2. DHT bootstrap completes.
3. The node is reachable. A node that needs a relay does not announce until at least one circuit is active.

Announcements are reconciled by `ServiceRegistryActor` in this priority order:
due withdrawals, due retries, new announcements, then type-index reconciliation.
Backend disconnects enter a short grace window (about 3–4 seconds, checked on a
1-second tick); a reconnect cancels the withdrawal. A successful announcement is
recorded in `announced`; failures retry with exponential backoff (5 s → 60 s).

`BootstrapCompleted` and `reannounce_services` clear the `announced` cache so the
node re-announces after a DHT rebuild instead of staying in a stale state.

### Relay gating

`needs_relay` is true unless the node is dual-stack **and** has a public address.
When a relay is required:

- Before the first active relay: no announcements are scheduled.
- First relay becomes active: `announced` is cleared and services are announced again.
- Relay count changes but stays above zero: no re-announcement.
- Last relay disappears: every announced service is withdrawn and further announcements are blocked until a relay returns.

Provider records are maintained by libp2p's periodic provider job. Active
providers republish every `provider_publication_interval_seconds` (default
1800 s); records disappear at most about one `provider_record_ttl_seconds`
(default 3600 s) after a provider stops.

### Request path

```text
backend → control(service_request)
  → ServiceDispatcher backend_read_loop
  → ControlRequest → NodeController
  → discover_providers (DHT get_providers)
  → rank candidates by RTT
  → send_request_to_peer
  → on failure: get_closest_peers(peer) to resolve relay addresses, retry
  → CBOR request-response
  → remote NodeController → InboundServiceRequest
  → remote ServiceDispatcher → backend task
  → request frame → remote backend → reply travels back
```

`service_request` prefers a ready local backend and skips DHT, PQ and the auth
gate. `service_request_to(self, service)` also stays local. If the local backend
is not ready, `service_request` falls back to a remote provider, while
`service_request_to(self)` returns `backend_not_ready`.

### DHT read cache

`DhtCache` is read-through with one in-flight refresh per key:

- A previous result is returned immediately and a background refresh starts if none is running.
- The refresh overwrites the cached value; a successful "not found" overwrites with an empty result.
- A cold key waits for the first query and returns its result or a real error.
- Refresh failures keep the previous value and are logged; the next read retries.
- There is no freshness window, global permit, cache timeout or failure backoff.

Authorization uses the same cache but still checks `expires_at` and `version` on
every decision, so expired or rolled-back documents are rejected even when served
from cache. `/oahd/service/types` bypasses the cache and is handled by the
registry lease logic.

### Authorization path

For each inbound request the auth gate checks, in order:

1. `auth.network == "none"` → allow.
2. Service does not require auth → allow.
3. Index record missing or expired → deny.
4. Service absent from the index → allow this request with a warning.
5. Whitelist missing → deny; peer not in members → deny.

Documents are COSE_Sign1 with EdDSA, bound to their DHT key through
`external_aad`. Versions must increase and hashes must match the index entry.
See [auth.md](./auth.md) for the byte-level contract.

### Post-quantum layer

When enabled, service calls use `/oahd/service_req/2.0.0`: the caller encapsulates
to the responder's hybrid KEM public key, and both sides derive one shared key
used for the request and the response. The plaintext protocol is used only when
the peer is confirmed not to support PQ; unknown capability returns a retryable
error rather than silently downgrading. `crypto.pq_required` rejects non-PQ peers
outright.

`pq_identity_enabled` additionally requires ML-DSA-65 signatures and binds the
signing identity to the transport PeerId.

### Size limits

- `services.dispatcher.max_request_bytes` / `max_response_bytes`: default 16 MiB, range 1–64 MiB per message.
- `compat.max_record_bytes`: default 14 KiB, keeps DHT records inside the legacy 16 KiB Kademlia packet.
- `sidecar_protocol::MAX_FRAME`: 16 MiB per backend frame.
- PQ requests add roughly 8 KiB of KEM, nonce and optional identity data.

Relay servers size `max_circuit_bytes` as `max_request_bytes + max_response_bytes`,
so bootstrap limits must be at least as large as their clients'.
