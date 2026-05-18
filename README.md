# NexusNet

**OAHD 计划的核心网络层** — 基于 libp2p 的去中心化 P2P 节点网络。

NexusNet 将一台机器接入 P2P 覆盖网络，通过 Kademlia DHT 实现节点自动发现与服务注册查询；利用边车（sidecar）模式，将远程服务请求经 TCP 转发到本地业务进程，业务端语言无关。

## 架构总览

```
                   ┌──────────────────────────────────────────┐
                   │             NexusNet 节点                  │
                   │                                          │
                   │  ┌─ 网络层（Swarm）─────────────────────┐ │
                   │  │  Ping（保活检测，可配间隔/超时/重试）│ │
                   │  │  Identify（协议/版本握手）            │ │
                   │  │  Kademlia（DHT 路由与存储）           │ │
                   │  │    ├ 路由表（kbuckets）               │ │
                   │  │    ├ 服务宣告（start_providing）      │ │
                   │  │    └ Bootstrap / 周期性扩散           │ │
                   │  │  Request-Response（CBOR 服务调用）     │ │
                   │  └───────────────────────────────────┘ │
                   │                                          │
                   │  ┌─ NodeController（主事件循环）───────┐ │
                   │  │  tokio::select! 驱动：               │ │
                   │  │    ├ 网络事件 → 路由/发现/响应       │ │
                   │  │    └ 后端命令 → 内部命令/远程调用     │ │
                   │  └──────────┬───────────────────────┘ │
                   │             │ cmd_tx (后端→节点命令)     │
                   │             │ inbound_req_tx (节点→后端) │
                   │  ┌──────────▼───────────────────────┐ │
                   │  │ ServiceDispatcher（后台任务）      │ │
                   │  │  持久 TCP 连接后端进程             │ │
                   │  │  UUID 帧协议：[uuid][payload]      │ │
                   │  │  控制命令：uuid_len=0 触发内部命令  │ │
                   │  │  超时处理：30s，自动清理 pending    │ │
                   │  └──────────────────────────────────┘ │
                   └──────┬──────────────────────────────┘
                          │ 持久 TCP（UUID 帧协议）
             ┌────────────┼────────────────────────────┐
             ▼            ▼                             ▼
      ┌───────────┐ ┌───────────┐               ┌───────────┐
      │ CLI 边车   │ │ OCR 服务  │      ...      │ 其他进程   │
      │ 端口 5014  │ │ 端口 5013 │               │ 任意语言   │
      └───────────┘ └───────────┘               └───────────┘
```

### 微内核 + 边车

- **NodeController** — 单一异步事件循环，集成所有 libp2p 事件处理（Ping/Identify/Kademlia/Request-Response）和后端命令路由。
- **ServiceDispatcher** — 独立后台任务。节点启动时主动连接所有配置的本地后端（重试 3 次），维持持久 TCP 连接。P2P 入站请求经 `inbound_req_tx` 转发到此处，由 `handle_request_with_backend()` 通过 UUID 帧协议发送给后端进程，等待响应后返回。
- **后端透明** — 后端只需要理解 UUID 帧协议即可接入，语言/框架无关。后端也可主动发起控制指令。
- **CBOR 协议** — P2P 层使用 libp2p CBOR 协议，`Request { service, payload }` / `Response { success, data }`。

## 快速开始

### 编译与运行

```bash
cargo run
```

首次运行自动生成 `config.toml` 和 `keypair.bin`。

### 命令行参数

```bash
# 指定监听端口
cargo run -- -p 5001

# 添加 bootstrap 节点
cargo run -- -c /ip4/192.168.1.100/tcp/5000/p2p/12D3KooW...

# 覆盖 bootstrap 列表（清空已有，仅连此节点）
cargo run -- --connect-overwrite /ip4/192.168.1.100/tcp/5000/p2p/12D3KooW...
```

所有 CLI 变更自动写回 `config.toml`。

## 配置（config.toml）

```toml
[node]
name = "未设置的p2p节点"
description = "无详细描述"

[network]
ipv4_enabled = false
ipv4_address = "x.x.x.x"       # 可选，不设则自动检测
ipv6_enabled = false
ipv6_address = "x:x::x"        # 可选
port = 5000
announce_addresses = []         # 手动指定宣告地址

[services.ping]
enabled = true
interval_secs = 15
with_timeout = 10
max_failures = 2

[services.kademlia]
enabled = true
record_ttl_seconds = 3600
replication_factor = 20
query_timeout_seconds = 60
bootstrap_nodes = []

[services.dispatcher]
enabled = true
query_timeout_secs = 60
record_ttl_secs = 3600

[[services.dispatcher.local_services]]
name = "cmd"
host = "127.0.0.1"
port = 5014
```

所有字段均有 `#[serde(default)]`，省略即默认值。

## 启动流程

```
boot::init()
  ├─ 读取 config.toml（损坏/不存在 → 创建默认）
  ├─ CLI 参数覆盖 & 写回
  ├─ 更新公网 IP 到配置
  ├─ 加载/生成 keypair.bin（ED25519）
  ├─ NetHandle::start() → 绑定端口，组建 Swarm
  ├─ 拨号所有 bootstrap 节点
  ├─ 启动 ServiceDispatcher（后台 tokio::spawn）
  └─ NodeController::run()（主协程）
       tokio::select! {
           event_rx → 网络事件
           cmd_rx  → 后端命令
       }
```

## 模块清单

| 模块 | 行数 | 职责 |
|------|------|------|
| **boot** | 71 | 初始化：加载配置、CLI 解析、持-久化 |
| **main** | 60 | 入口：`#[tokio::main]`，编排全流程 |
| **node_controller** | 421 | 事件循环：Ping/Identify/Kademlia/ReqResp 统一处理；服务自动宣告（start_providing），远程查询，内部命令 |
| **service_dispatcher** | 292 | 后端连接管理：连接/重试/帧读写；UUID 请求跟踪；控制指令路由 |
| **net** | 369 | KeyManager（原子写入）、Swarm 构建、地址检测 |
| **config** | 407 | ConfigHandle（RwLock）、TOML 持久化（.tmp → rename） |
| **service_protocol** | 34 | P2P CBOR 协议：`Request { service, payload }` / `Response { success, data }` |
| **log** | 265 | 彩色终端 + 文件输出，10MB 自动轮转 gzip |
| **合计** | 1919 | |

## 后端帧协议（TCP）

NexusNet 与后端进程之间使用**持久 TCP 连接**，由节点主动发起连接。

### 普通响应帧

```text
[4B uuid_len: u32 BE][uuid_str: N bytes][4B payload_len: u32 BE][payload: N bytes]
```

- `uuid_len = 0` 时表示控制指令（见下节）
- `uuid_len > 0` 时：读取 UUID 字符串，在 pending_map 中匹配并唤醒等待者
- 响应以 UUID 关联回原始请求

### 控制指令帧（uuid_len = 0）

```text
[4B 0x00000000][4B payload_len: u32 BE][payload: "prefix|content|rest"]
```

节点收到后解析为 `Command { prefix, content, payload }`，经 `cmd_tx` 发送给 NodeController，结果写回后端。

### 节点→后端请求帧

```text
[4B uuid_len: u32 BE][uuid_str: N bytes][4B payload_len: u32 BE][payload: N bytes]
```

- UUID 由 `Uuid::new_v4()` 生成
- 发送前将 `(uuid, oneshot::Sender)` 存入 `pending_map`
- 默认超时 30 秒，超时自动清理 pending 条目

## P2P 服务调用流程

```text
后端进程 → 控制指令(uuid_len=0, "prefix|content|payload")
  → ServiceDispatcher.backend_read_loop
  → Command(cmd_tx) → NodeController.handle_command()
  → discover_providers(service) → DHT get_providers
  → RTT 排序选最优点
  → send_request_to_peer(peer, service, payload)
  → CBOR Request-Response (libp2p)
  → 远程 NodeController → InboundServiceRequest(inbound_req_tx)
  → 远程 ServiceDispatcher.handle_request_with_backend()
  → UUID 帧 → 远程后端进程
  → 响应沿原路返回
```

## 服务注册与发现

- 本地服务列表由 `config.services.dispatcher.local_services` 定义
- Bootstrap 成功（首次 DHT 查询完成）后自动调用 `start_providing`，key 为 `/oahd/service/<name>`
- 同步 `/oahd/service/types` 全局服务类型记录（put_record/get_record）
- `@list_services` → 查询全局服务类型
- `@discover_providers` → DHT get_providers 获取提供者列表
- `call_service()` → 查询提供者，RTT 排序选优，P2P 调用

## 日志

- 双输出：终端（彩色）+ 纯文本文件
- 格式：`[LEVEL] 时间\n    主题\n    内容`
- 等级：`Critical | Error | Warning | Important | Preset | Debug | Log`
- 文件 10MB 自动轮转：gzip 压缩为 `MMDD_HHMM-MMDD_HHMM.gz`

## 节点身份

- ED25519 密钥对，Protobuf 编码，`keypair.bin`
- 首次运行自动生成，已有则加载
- **keypair.bin 不可丢失** — 丢失后节点身份变更

## 开发状态

当前版本：**0.2.0** — 完成度 **5/10**

## 许可

Apache-2.0
