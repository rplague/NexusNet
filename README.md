# NexusNet

**OAHD 计划的核心网络层** — 基于 libp2p 的 P2P 节点网络。

NexusNet 将一个对等节点接入到去中心化 P2P 网络，通过 Kademlia DHT 实现节点自动发现、服务注册与查询，通过 TCP 边车代理将远程请求转发到本地业务进程。

## 架构

```
                   ┌──────────────────────────────────────┐
                   │           NexusNet 节点                 │
                   │                                        │
                   │  ┌─ P2P 通信层 ──────────────────────┐ │
                   │  │  Ping（保活检测，15s 间隔）       │ │
                   │  │  Identify（协议/版本握手）         │ │
                   │  │  Kademlia（DHT 路由表存储）        │ │
                   │  │    ├ 路由表（kbuckets）            │ │
                   │  │    ├ 服务宣告的 DHT 记录           │ │
                   │  │    └ 周期性 bootstrap（20s 间隔）  │ │
                   │  │  Request-Response（消息/服务请求） │ │
                   │  │  AddrWatcher（IP 变化检测，60s）   │ │
                   │  └─────────────────────────────────┘   │
                   │                                        │
                   │  ┌─ 边车调度与发现 ──────────────────┐ │
                   │  │  ServiceDiscovery（DHT 宣告/查询） │ │
                   │  │  ServiceDispatcher（TCP 本地转发） │ │
                   │  │  OutboundProxy（本地 TCP 控制口） │ │
                   │  └─────────────────────────────────┘   │
                   └──────────┬──────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────────────┐
        ▼                     ▼                              ▼
 ┌──────────────┐     ┌──────────────┐            ┌──────────────┐
 │ OCR 服务     │     │ 冷存储服务    │            │ 其他业务进程  │
 │ 本地进程     │     │ 本地进程      │            │ 任何语言      │
 └──────────────┘     └──────────────┘            └──────────────┘
```

### 设计原则：微内核 + 边车

- **P2P 通信层**（主循环）— 连接管理、路由维护、协议处理
- **ServiceDispatcher** — 收到远程请求后通过 TCP 转发到本地 `127.0.0.1:port`，业务进程语言无关
- **OutboundProxy** — 本地进程可通过 JSON 行协议对控制端口发命令，经 event loop 操作 P2P 网络
- **ServiceDiscovery** — 基于 Kademlia DHT 的 `put_record`/`get_record` 实现去中心化服务注册与查询

## 启动与配置

### 首次运行

```bash
cargo run
```

自动生成 `config.toml`（TOML 格式，支持注释）和 `keypair.bin`（节点身份，不可丢失）。

### 配置参考

```toml
[node]
name = "节点名称"
description = "节点描述"

[network]
port = 5000                        # 监听端口（默认）
ipv4_enabled = true                # 默认自动检测
ipv6_enabled = false               # 默认关闭

[services.ping]
enabled = true
interval_secs = 15
with_timeout = 10

[services.kademlia]
enabled = true
bootstrap_nodes = []               # 启动时尝试连接这些节点

[services.service_discovery]
enabled = true
services = ["ocr"]                 # 本节点宣告的服务
record_ttl_secs = 1800

[services.dispatcher]
enabled = false                    # 默认关闭，需手动启用
[[services.dispatcher.local_services]]
name = "ocr"
host = "127.0.0.1"
port = 5013

[services.address_watcher]
enabled = true
check_interval_secs = 60           # 每 60s 检测 IP 是否变化

[services.outbound_proxy]
enabled = false                    # 默认关闭
bind = "127.0.0.1"
port = 5200
```

所有非根配置段均有 `#[serde(default)]`，可省略——省略时使用默认值。

### 命令行参数

```bash
# 指定端口（覆盖 config.toml 中的配置）
cargo run -- -p 5001

# 连接指定节点（清空 bootstrap_nodes，只连接此地址）
cargo run -- -c /ip4/192.168.1.100/tcp/5000/p2p/12D3KooW...

# 启动后查询指定服务
cargo run -- -q ocr
```

## 节点行为

### 启动阶段

1. **配置加载** — 读取 `config.toml`，文件不存在/空/格式错误时自动创建新配置，更新本机 IP 地址后写回
2. **密钥加载** — 读取或生成 `keypair.bin`（ED25519），作为节点的永久身份
3. **初始化** — 创建 Kademlia MemoryStore、ServiceDiscovery（从配置加载待宣告服务列表）、ServiceDispatcher（健康检查所有后端）、Ping/Identify/RequestResponse/AddrWatcher 各 behaviour
4. **监听** — 按配置在 IPv4/IPv6 地址上绑定 TCP 端口
5. **连接 bootstrap** — 遍历 `bootstrap_nodes`，逐条 dial
6. **出站代理** — 如果配置启用，在 `127.0.0.1:5200` 启动 TCP 命令监听（tokio::spawn 独立任务），通过 `mpsc` 通道向 event loop 发送命令

### 事件循环

event loop 使用 `tokio::select!` 同时处理：

- **Swarm 事件** — 来自 libp2p 网络层的各种事件
- **Proxy 命令** — 来自出站代理 TCP 控制端口的请求

### Swarm 事件处理

| 事件 | 处理逻辑 |
|------|----------|
| **NewListenAddr** | 标记 listeners 就绪。如果服务已宣告完毕，更新地址后重新 `put_record` 宣告 |
| **IncomingConnectionError** | 记录错误到日志 |
| **ConnectionEstablished** | 在 `net_peer_list` 中记录或更新节点为 Connected 状态 |
| **ConnectionClosed** | 同版本节点标记为 Disconnected（保留）；不同版本节点从列表中移除 |
| **Identify::Received** | **核心路由逻辑**（见下方详细说明） |
| **Identify::Error** | 记录警告 |
| **Kademlia::Bootstrap Ok** | 向 DHT 宣告本地服务（`put_record`），如有 `--query` 则发起 `get_record` 查询 |
| **Kademlia::GetRecord Ok** | 解码 `ServiceInfo` 存入 ServiceDiscovery 缓存，记录发现的提供者 |
| **Kademlia::PutRecord Ok** | 记录确认 |
| **Kademlia::GetClosestPeers** | 将发现的节点加入 `net_peer_list`，对未连接的节点发起 dial |
| **Kademlia::UnroutablePeer** | 记录警告 |
| **RequestResponse::Message** | 收到请求 → `dispatcher.forward()` 转发到本地业务进程；收到响应 → 记录到日志 |
| **AddrWatcher::Changed** | IP 变化后：重新宣告服务，更新 `announce_addresses` 到配置，重连所有 bootstrap 节点 |

#### Identify::Received 详细说明

收到对端身份信息后按以下步骤处理：

```
收到 Identify::Received
  │
  ├─ 在 net_peer_list 中查找对应 peer_id
  │    └─ 找到 → 填充 agent_version、observed_addresses、public_key、supported_protocols
  │
  ├─ appropriate_address_filter()
  │    └─ 按本地 IP 协议栈优先级过滤对端 listen_addrs
  │         ├─ IPv4 v6 都启用 → IPv4 优先，再 IPv6
  │         ├─ 仅 IPv4 → 只保留 IPv4 地址
  │         ├─ 仅 IPv6 → 只保留 IPv6 地址
  │         └─ 都禁用 → None
  │
  ├─ 有兼容地址？
  │    ├─ Yes → 填充 entry.addresses（追加 /p2p/<peer_id>）
  │    └─ No  → 记录 "地址不兼容" 警告，跳过
  │
  ├─ 判断 is_my_node：
  │    ├─ Kademlia 已启用
  │    ├─ agent_version == /OAHD/<当前版本>
  │    └─ entry.addresses 包含 P2P 协议
  │
  ├─ is_my_node = true？
  │    ├─ Yes → add_address 到 Kademlia 路由表
  │    │         insert_bootstrap_nodes（写入 config.toml）
  │    │         如果 bootstrap_done == false，执行 bootstrap()
  │    └─ No  → 记录 "未知节点" 信息（仅地址兼容的情况下）
```

关键行为：
- **同版本 OAHD 节点**自动加入 Kademlia 路由表并被记录为 bootstrap 节点
- **地址不兼容的对端**（如纯 IPv4 节点接入了仅 IPv6 的节点）不会加入路由表，但节点信息仍被记录在 `net_peer_list` 中
- **不同版本的 OAHD 节点**保持基础连接（Ping 保活），但不加入路由表

### 服务宣告的生命周期

```
节点启动
  │
  ├─ 加载配置中 [services.service_discovery.services] 列表
  │    └─ 每个服务名对应创建一个 ServiceInfo（地址为空）
  │
  └─ Bootstrap 成功
       │
       ├─ set_addresses(listeners) 填充真实地址
       │
       ├─ get_announce_records() 序列化为 Kademlia Record
       │    ├─ key = /oahd/sd/<service_type>
       │    └─ value = JSON(ServiceInfo{ service_type, provider, addrs, version, metadata, timestamp, ttl })
       │
       └─ 对每个 record 执行 kademlia.put_record(Quorum::One)
            │
            ├─ 地址变化时 → reannounce() 重新 put_record
            └─ 记录 TTL 过期后自然从 DHT 消失
```

服务记录键前缀：`/oahd/sd/`（定义在 `SD_KEY_PREFIX`）。

### 服务查询

两种查询方式：

**1. 命令行 `--query`（启动时执行）**

```
cargo run -- -q ocr
```

启动后在 Bootstrap 成功时发起 `kademlia.get_record`，结果通过 `GetRecord` 事件处理——解码 `ServiceInfo` 后存入 ServiceDiscovery 缓存。

**2. OutboundProxy 控制端口（运行时查询）**

```bash
echo '{"type": "discover", "service": "ocr"}' | nc 127.0.0.1 5200
```

响应：

```json
{"success":true,"message":"找到 2 个提供者","data":{"providers":[{...}]}}
```

### 地址变化处理（AddrWatcher）

`addr_watcher::Behaviour` 是一个自定义 libp2p `NetworkBehaviour`，实现方式为：

- 每 `check_interval_secs`（默认 60s）触发一次
- 调用 `get_network_addresses()` 获取当前公共网络地址
- 与内部保存的 `last_addrs` 比较
- 变化时通过 `ToSwarm::GenerateEvent(AddrWatcherEvent::Changed(...))` 上报

收到 `Changed` 事件后，event loop：

1. 重新宣告所有已宣告的服务（`reannounce()` → `put_record`）
2. 更新 `config.announce_addresses`
3. 对已知 bootstrap 节点全部发起 dial 重连

### 出站代理（OutboundProxy）

通过本地 TCP 端口（默认 `127.0.0.1:5200`）提供了一个 JSON 行协议接口。连接打开后可发送以下命令：

```json
{"type": "discover", "service": "ocr"}
{"type": "request", "peer": "12D3KooW...", "service": "ocr", "payload": "<base64>"}
{"type": "ping"}
```

命令通过 `mpsc` 通道发送到 event loop，在 `tokio::select!` 中与 Swarm 事件并行处理。`discover` 会先查本地缓存再发起 DHT 查询；`request` 使用 libp2p RequestResponse 协议发送 `ServiceRequest`。

## 模块清单

| 模块 | 文件 | 职责 |
|------|------|------|
| **main** | `src/main.rs` | 入口，cli 解析，event loop，所有事件路由 |
| **config** | `src/config.rs` | TOML 配置定义、加载、写入、节点管理（insert_bootstrap_nodes） |
| **net** | `src/net.rs` | libp2p behaviour 组合（NetBehaviour）、IP 地址获取、地址兼容性过滤、密钥管理 |
| **log** | `src/log.rs` | 彩色终端输出 + 文件日志，10MB 自动轮转压缩 |
| **addr_watcher** | `src/addr_watcher.rs` | 自定义 NetworkBehaviour：周期性检测本机 IP 变化并上报事件 |
| **service_discovery** | `src/service_discovery.rs` | DHT 服务注册、查询、缓存管理（ServiceInfo 定义） |
| **service_dispatcher** | `src/service_dispatcher.rs` | 边车模式 TCP 转发：收到远程请求后连接本地业务进程 |
| **request_handler** | `src/request_handler.rs` | ServiceRequest/ServiceResponse 消息定义、发送、接收处理 |
| **outbound_proxy** | `src/outbound_proxy.rs` | 本地 TCP JSON 命令监听，通过 mpsc 通道操作 event loop |

## 协议与版本

- **协议标识**: `/OAHD`
- **Agent 版本**: `/OAHD/<CARGO_PKG_VERSION>`（编译时注入）
- **Kademlia**: `/ipfs/kad/1.0.0`
- **Request-Response**: `/oahd/req/1.0.0`
- 同版本节点通过 Identify 识别后自动加入 Kademlia 路由表；不同版本保持连接但不加入路由

## 网络地址发现

`get_network_addresses()` 通过 `get_if_addrs` 库扫描所有网络接口：

- IPv4: 过滤 loopback（127.x）、private（10.x/192.168.x/172.16-31.x）、link-local（169.254.x）
- IPv6: 过滤 loopback（::1）、unspecified（::）、multicast、link-local（fe80::/10）、ULA（fc00::/7）

取第一个符合条件的非空地址。

## 日志系统

- 所有日志同时输出到终端（带颜色）和 `log` 文件（纯文本）
- 日志格式：`[LEVEL] 时间\n    主题\n    内容`
- 错误和警告输出到 stderr，其余到 stdout
- 日志文件达到 10MB 时自动轮转压缩：后台线程将 `log` 重命名为 `log.tmp`，用 gzip 压缩为 `MMDD_HHMM-MMDD_HHMM.gz`，删除临时文件
- 文件写入使用 `Mutex` 保证单线程写入安全

## 节点身份

- 使用 ED25519 密钥对，序列化为 protobuf 格式存储在 `keypair.bin`
- 首次运行自动生成，已存在则读取
- **keypair.bin 不可丢失**——丢失后节点身份改变，网络中的路由关系丢失

## 网络拓扑维护

| 机制 | 实现 | 效果 |
|------|------|------|
| **Identify 自动发现** | 连接建立后 libp2p 自动触发 | 同版本节点自动入路由表 |
| **Kademlia GetClosestPeers** | Bootstrap 后周期性扩散 | 发现网络中的其他节点并 dial |
| **Ping 保活** | 每 15s 检测 | 心跳超时自动标记为 Disconnected |
| **ConnectionClosed 处理** | 同版本保留、不同版本移除 | 避免残留节点影响路由 |
| **AddrWatcher 重连** | 60s 检测 IP 变化 | 地址变化后自动重连所有已知 bootstrap 节点 |
| **insert_bootstrap_nodes** | Identify 发现同版本节点后自动写入 config.toml | 重启后自动连接已知节点 |

## 预备知识

当前版本：**0.1.2**

- [x] P2P 基础网络（Ping / Identify / Kademlia）
- [x] 服务注册与发现（DHT 宣告/查询）
- [x] 本地服务调度（边车代理 TCP 转发）
- [x] IP 地址变化自动检测与重连
- [x] 出站代理（本地 TCP 控制端口）
- [ ] 冷存储服务接口
- [ ] OCR 服务接口
- [ ] Web API 管理界面

## 许可

Apache-2.0
