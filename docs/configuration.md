# 配置参考 / Configuration reference

**语言 / Language:** [中文](#中文) · [English](#english)

<a id="中文"></a>
## 中文

NexusNet 读取 TOML 配置文件 `config.toml`。省略的可选字段会取默认值；启动时缺失字段会被补全并原子写回，已有值、未知字段、注释和数组表条目保持不变。补全写回失败会记录 Critical 并退出。

### 最小配置

```toml
[node]
name = "my-node"

[network]
ipv4_enabled = true

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
```

### 完整示例

```toml
[node]
name = "未设置的p2p节点"
description = "无详细描述"
allow_bootstrap = true
accept_bootstrap_providers = false   # 是否自动采纳 Identify 发现的 bootstrap 节点

[network]
# 启动时会探测公网地址并写回；ipv4_enabled/ipv6_enabled 随之更新
ipv4_enabled = false
ipv4_address = "x.x.x.x"       # 可选，但会被自动探测覆盖
ipv6_enabled = false
ipv6_address = "x:x::x"         # 可选，但会被自动探测覆盖
port = 5000                     # WebSocket TCP 端口
force_relay = false             # 即使探测到公网地址也强制使用中继

[services.ping]
enabled = true
interval_secs = 15
with_timeout = 10
max_failures = 2

[services.kademlia]
enabled = true
record_ttl_seconds = 3600
provider_record_ttl_seconds = 3600             # provider 记录 TTL；必须 >= 2 * 发布间隔
provider_publication_interval_seconds = 1800   # 活跃 provider 重发间隔
replication_factor = 20
query_timeout_seconds = 60
bootstrap_nodes = []

[services.dispatcher]
enabled = true
query_timeout_secs = 60
types_sync_secs = 300
max_inflight_dht = 4             # 1..=16，DHT 控制指令并发上限
max_request_bytes = 16777216     # P2P 服务请求上限，默认 16 MiB，范围 1..=64 MiB
max_response_bytes = 16777216    # P2P 服务响应上限，默认 16 MiB，范围 1..=64 MiB

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
require_auth = false
publish = true                   # false = 仅本地，不发布到网络

[services.registry]
max_inflight_dht = 4             # 1..=16，registry DHT 作业并发上限
type_ttl_secs = 1800             # 服务类型租约；必须 > 2 * types_sync_secs

[services.relay]
retry_interval_secs = 60
max_failures = 3

[compat]
max_record_bytes = 14336         # 默认适配旧版 16 KiB Kademlia 包

[crypto]
pq_transport_enabled = true
pq_identity_enabled = true
pq_required = false

[auth]
network = "none"                 # "none" = 不鉴权；启用时改为 myorg

# [auth.networks.myorg]          # 每个鉴权网络的信任锚
# authority = "<base64 ed25519 公钥>"
```

### 字段说明

#### `[node]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `name` | `未设置的p2p节点` | 节点显示名 |
| `description` | `无详细描述` | 节点描述 |
| `allow_bootstrap` | `true` | 双栈节点是否可作为 bootstrap/中继并声明 `/oahd/bootstrap` provider |
| `accept_bootstrap_providers` | `false` | 是否自动采纳 Identify 发现的 bootstrap provider |

#### `[network]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `ipv4_enabled` | `false` | 是否监听/使用 IPv4 |
| `ipv4_address` | 未设置 | 可选 IPv4 地址；启动探测会覆盖 |
| `ipv6_enabled` | `false` | 是否监听/使用 IPv6 |
| `ipv6_address` | 未设置 | 可选 IPv6 地址；启动探测会覆盖 |
| `port` | `5000` | WebSocket TCP 端口 |
| `force_relay` | `false` | 即使有公网地址也强制中继预约（移动网络常用） |

#### `[services.ping]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `true` | 启用 Ping 保活 |
| `interval_secs` | `15` | 发送间隔 |
| `with_timeout` | `10` | 单次超时 |
| `max_failures` | `2` | 连续失败上限 |

#### `[services.kademlia]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `true` | 启用 Kademlia DHT |
| `record_ttl_seconds` | `3600` | 普通 DHT 记录 TTL |
| `provider_record_ttl_seconds` | `3600` | provider 记录 TTL；必须 >= 2 × 发布间隔 |
| `provider_publication_interval_seconds` | `1800` | 活跃 provider 重发间隔 |
| `replication_factor` | `20` | DHT 复制因子 |
| `query_timeout_seconds` | `60` | DHT 查询超时 |
| `bootstrap_nodes` | `[]` | 引导节点；地址必须带 `/ws`，例如 `/ip6/.../tcp/5000/ws/p2p/<PeerId>` |

#### `[services.dispatcher]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `true` | 启用服务分发 |
| `query_timeout_secs` | `60` | 控制指令基础超时；涉及 DHT 的命令会自动延长 |
| `types_sync_secs` | `300` | 服务类型索引同步间隔 |
| `max_inflight_dht` | `4` | 并发 DHT 控制指令上限，范围 1–16 |
| `max_request_bytes` | `16777216` | 单个 P2P 请求（CBOR 编码后）上限，范围 1–64 MiB |
| `max_response_bytes` | `16777216` | 单个 P2P 响应上限，范围 1–64 MiB |
| `local_services` | `[]` | 本地后端列表，见下 |

#### `[[services.dispatcher.local_services]]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `name` | 必填 | 服务名；在列表中必须唯一 |
| `port` | 必填 | 本地后端端口；节点固定连接 `127.0.0.1:<port>` |
| `require_auth` | `false` | 该服务是否要求鉴权 |
| `publish` | `true` | 是否发布到网络；`false` 时不写 provider 记录与服务类型索引，但保留本地后端和显式 peer 调用 |

修改 `local_services` 后发送 `SIGHUP` 或调用 `reload_config`：节点会重新读取
`config.toml`，按新列表建立/停止后端连接，并撤销与重新宣告服务，无需重启。

#### `[services.registry]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `max_inflight_dht` | `4` | 并发 registry DHT 作业上限，范围 1–16 |
| `type_ttl_secs` | `1800` | 服务类型租约；必须 > 2 × `types_sync_secs` |

#### `[services.relay]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `retry_interval_secs` | `60` | 中继补约检查间隔 |
| `max_failures` | `3` | 连续失败多少次后从 bootstrap 列表移除 |

#### `[compat]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `max_record_bytes` | `14336` | DHT 记录上限；默认 14 KiB，适配旧版 16 KiB Kademlia 包 |

#### `[crypto]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `pq_transport_enabled` | `true` | 启用混合后量子加密服务调用 |
| `pq_identity_enabled` | `true` | 要求并校验 ML-DSA-65 签名（需启用 transport） |
| `pq_required` | `false` | 强制 PQ，拒绝非 PQ 对端 |

没有 `[crypto]` 的旧配置保留历史行为（PQ 关闭）；新生成的配置默认启用。

#### `[auth]`

| 字段 | 默认值 | 说明 |
|---|---|---|
| `network` | `none` | 鉴权网络名；`"none"` 表示不鉴权 |
| `networks.<name>.authority` | 无 | 该网络的权威 ed25519 公钥（标准 base64） |

### 行为说明

- **自动补全**：启动时缺失字段取默认值并原子写回；写回失败记录 Critical 并退出。
- **公网地址**：`network` 段会被公网 IP 探测整体重写为规范格式。
- **本地后端**：节点固定连接 `127.0.0.1:<port>`，不支持自定义 host。
- **超时**：`services.dispatcher.query_timeout_secs` 是控制指令的基础超时；涉及 DHT 查询的命令按实际需要自动延长。
- **报文上限**：`max_request_bytes` / `max_response_bytes` 限制单个 P2P 服务报文（CBOR 编码后）的大小。启用 PQ 时约增加 8 KiB KEM/nonce/可选身份签名开销；接近上限时需调大。发送端会先做本地检查；两端都需升级并配置足够的上限。
- **记录上限**：超过 `compat.max_record_bytes` 的 auth/service 写入会被拒绝。全量升级后再调大。
- **TTL 约束**：`provider_record_ttl_seconds` 与 `provider_publication_interval_seconds` 必须大于 0，且 TTL 不能小于 2 × 发布间隔；`type_ttl_secs` 必须大于 2 × `types_sync_secs`。

### 环境变量

| 变量 | 含义 |
|---|---|
| `NEXUSNET_CONFIG_PATH` | 配置目录；读取 `<dir>/config.toml` |
| `NEXUSNET_KEYPAIR_PATH` | 密钥目录；读取 `<dir>/keypair.bin` 与 `keypair.pq.bin` |
| `NEXUSNET_LOG_PATH` | 日志目录；仅文件输出模式使用，systemd 下日志走 journald |
| `NEXUSNET_LOG_MODE` | 日志输出模式 |

未设置时回退当前目录。完整路径解析见 `src/paths.rs`。

### 安装器配置模式

正常服务不解析命令行参数。安装器使用内部模式生成和检查配置：

| 模式 | 用途 |
|---|---|
| `--write-config` | 从安装器输入生成配置 |
| `--defaults` | 生成默认配置 |
| `--check-config` | 检查现有配置 |

详见 [deploy/README.md](../deploy/README.md)。

---

<a id="english"></a>
## English

NexusNet reads a TOML configuration file, `config.toml`. Optional fields fall back to defaults; on startup, missing fields are filled in and written back atomically while existing values, unknown fields, comments and array-of-table entries are preserved. A failed write-back logs Critical and exits.

### Minimal configuration

```toml
[node]
name = "my-node"

[network]
ipv4_enabled = true

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
```

### Full example

```toml
[node]
name = "未设置的p2p节点"   # default: unconfigured p2p node
description = "无详细描述" # default: no description
allow_bootstrap = true
accept_bootstrap_providers = false   # adopt bootstrap nodes discovered via Identify

[network]
# Public addresses are probed and written back on startup; the enabled flags follow.
ipv4_enabled = false
ipv4_address = "x.x.x.x"       # optional; overwritten by probing
ipv6_enabled = false
ipv6_address = "x:x::x"         # optional; overwritten by probing
port = 5000                     # WebSocket TCP port
force_relay = false             # reserve relays even when a public address is detected

[services.ping]
enabled = true
interval_secs = 15
with_timeout = 10
max_failures = 2

[services.kademlia]
enabled = true
record_ttl_seconds = 3600
provider_record_ttl_seconds = 3600             # must be >= 2 * publication interval
provider_publication_interval_seconds = 1800   # active provider republish interval
replication_factor = 20
query_timeout_seconds = 60
bootstrap_nodes = []

[services.dispatcher]
enabled = true
query_timeout_secs = 60
types_sync_secs = 300
max_inflight_dht = 4             # 1..=16, concurrent DHT control commands
max_request_bytes = 16777216     # P2P request limit, default 16 MiB, range 1..=64 MiB
max_response_bytes = 16777216    # P2P response limit, default 16 MiB, range 1..=64 MiB

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
require_auth = false
publish = true                   # false = keep local only

[services.registry]
max_inflight_dht = 4             # 1..=16, concurrent registry DHT jobs
type_ttl_secs = 1800             # service type lease; must be > 2 * types_sync_secs

[services.relay]
retry_interval_secs = 60
max_failures = 3

[compat]
max_record_bytes = 14336         # fits the legacy 16 KiB Kademlia packet

[crypto]
pq_transport_enabled = true
pq_identity_enabled = true
pq_required = false

[auth]
network = "none"                 # "none" = no authorization; use a network name to enable

# [auth.networks.myorg]          # trust anchor for each authorization network
# authority = "<base64 ed25519 public key>"
```

### Fields

#### `[node]`

| Field | Default | Meaning |
|---|---|---|
| `name` | `未设置的p2p节点` (unconfigured p2p node) | Display name |
| `description` | `无详细描述` (no description) | Description |
| `allow_bootstrap` | `true` | Whether a dual-stack node may act as bootstrap/relay and advertise `/oahd/bootstrap` |
| `accept_bootstrap_providers` | `false` | Adopt bootstrap providers discovered via Identify |

#### `[network]`

| Field | Default | Meaning |
|---|---|---|
| `ipv4_enabled` | `false` | Listen on and use IPv4 |
| `ipv4_address` | unset | Optional IPv4 address; overwritten by probing |
| `ipv6_enabled` | `false` | Listen on and use IPv6 |
| `ipv6_address` | unset | Optional IPv6 address; overwritten by probing |
| `port` | `5000` | WebSocket TCP port |
| `force_relay` | `false` | Reserve relays even with a public address (common on mobile networks) |

#### `[services.ping]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Enable Ping keepalive |
| `interval_secs` | `15` | Send interval |
| `with_timeout` | `10` | Per-request timeout |
| `max_failures` | `2` | Consecutive failure threshold |

#### `[services.kademlia]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Enable the Kademlia DHT |
| `record_ttl_seconds` | `3600` | Ordinary DHT record TTL |
| `provider_record_ttl_seconds` | `3600` | Provider record TTL; must be >= 2 × publication interval |
| `provider_publication_interval_seconds` | `1800` | Active provider republish interval |
| `replication_factor` | `20` | DHT replication factor |
| `query_timeout_seconds` | `60` | DHT query timeout |
| `bootstrap_nodes` | `[]` | Bootstrap nodes; addresses must use `/ws`, e.g. `/ip6/.../tcp/5000/ws/p2p/<PeerId>` |

#### `[services.dispatcher]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Enable the service dispatcher |
| `query_timeout_secs` | `60` | Base control-command timeout; DHT commands extend it automatically |
| `types_sync_secs` | `300` | Service type index sync interval |
| `max_inflight_dht` | `4` | Concurrent DHT control commands, range 1–16 |
| `max_request_bytes` | `16777216` | Encoded P2P request limit, range 1–64 MiB |
| `max_response_bytes` | `16777216` | Encoded P2P response limit, range 1–64 MiB |
| `local_services` | `[]` | Local backends, see below |

#### `[[services.dispatcher.local_services]]`

| Field | Default | Meaning |
|---|---|---|
| `name` | required | Service name; must be unique in the list |
| `port` | required | Local backend port; the node always connects to `127.0.0.1:<port>` |
| `require_auth` | `false` | Whether this service requires authorization |
| `publish` | `true` | Publish to the network; when `false` no provider record or service-type entry is written, while the local backend and explicit peer calls remain available |

Send `SIGHUP` or call `reload_config` after changing `local_services`: the node
re-reads `config.toml`, starts/stops backend connections to match the new list,
and withdraws and re-announces services without a restart.

#### `[services.registry]`

| Field | Default | Meaning |
|---|---|---|
| `max_inflight_dht` | `4` | Concurrent registry DHT jobs, range 1–16 |
| `type_ttl_secs` | `1800` | Service type lease; must be > 2 × `types_sync_secs` |

#### `[services.relay]`

| Field | Default | Meaning |
|---|---|---|
| `retry_interval_secs` | `60` | Relay top-up check interval |
| `max_failures` | `3` | Consecutive failures before removing a node from the bootstrap list |

#### `[compat]`

| Field | Default | Meaning |
|---|---|---|
| `max_record_bytes` | `14336` | DHT record limit; 14 KiB fits the legacy 16 KiB Kademlia packet |

#### `[crypto]`

| Field | Default | Meaning |
|---|---|---|
| `pq_transport_enabled` | `true` | Enable hybrid post-quantum encryption for service calls |
| `pq_identity_enabled` | `true` | Require and verify ML-DSA-65 signatures (requires transport) |
| `pq_required` | `false` | Reject non-PQ peers |

Legacy configs without `[crypto]` keep the historical behavior (PQ disabled); newly generated configs enable it by default.

#### `[auth]`

| Field | Default | Meaning |
|---|---|---|
| `network` | `none` | Authorization network name; `"none"` disables authorization |
| `networks.<name>.authority` | none | Authority ed25519 public key for that network (standard base64) |

### Behavior

- **Auto-completion**: missing fields are filled with defaults and written back atomically on startup; a failed write logs Critical and exits.
- **Public addresses**: the `network` section is rewritten in canonical form by public-IP probing.
- **Local backends**: the node always connects to `127.0.0.1:<port>`; custom hosts are not supported.
- **Timeouts**: `services.dispatcher.query_timeout_secs` is the base control-command timeout; DHT queries extend it as needed.
- **Message limits**: `max_request_bytes` / `max_response_bytes` bound one encoded P2P message. PQ adds roughly 8 KiB of KEM, nonce and optional identity data; raise the limits when close to the cap. The sender checks locally, and both ends must be upgraded with sufficient limits.
- **Record limit**: auth/service writes above `compat.max_record_bytes` are rejected. Raise it only after every node is upgraded.
- **TTL constraints**: `provider_record_ttl_seconds` and `provider_publication_interval_seconds` must be greater than 0, and the TTL must be at least 2 × the publication interval; `type_ttl_secs` must be greater than 2 × `types_sync_secs`.

### Environment variables

| Variable | Meaning |
|---|---|
| `NEXUSNET_CONFIG_PATH` | Configuration directory; reads `<dir>/config.toml` |
| `NEXUSNET_KEYPAIR_PATH` | Key directory; reads `<dir>/keypair.bin` and `keypair.pq.bin` |
| `NEXUSNET_LOG_PATH` | Log directory; file-output mode only (systemd uses journald) |
| `NEXUSNET_LOG_MODE` | Log output mode |

Unset variables fall back to the working directory. See `src/paths.rs` for the full resolution rules.

### Installer configuration modes

The service itself does not parse command-line arguments. The installer uses internal modes to generate and check configuration:

| Mode | Purpose |
|---|---|
| `--write-config` | Generate configuration from installer input |
| `--defaults` | Generate a default configuration |
| `--check-config` | Check the existing configuration |

See [deploy/README.md](../deploy/README.md) for details.
