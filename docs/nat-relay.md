# 无公网 IP 接入（Circuit Relay v2）/ Joining without a public IP

**语言 / Language:** [中文](#中文) · [English](#english)

<a id="中文"></a>
## 中文

NexusNet 通过 libp2p Circuit Relay v2 让没有公网地址的节点加入网络并提供服务。
NAT 节点不需要公网 IP，也不需要手工宣告地址；只需要至少一台双栈公网
bootstrap/中继。

### 拓扑

```text
        [B：双栈公网 bootstrap + 中继]
          ^                 ^
          |                 |
     [E：NAT 节点]      [A：调用方]
```

- B 必须双栈。双栈节点会自动启用中继服务器。
- E 把 B 的 WebSocket 地址写入 `services.kademlia.bootstrap_nodes`，
  例如 `/ip6/.../tcp/5000/ws/p2p/<PeerId>`。
- E 可以单栈，也可以双栈但在 NAT 后面；它不需要公网地址。

### E 获得中继前不宣告服务

E 启动后：

1. 连接 B，完成 DHT bootstrap。
2. 向 B 预约 `p2p-circuit`。
3. 收到电路监听器（circuit listener）后，ServiceRegistry 才允许 `start_providing`。
4. 如果 `active_relays == 0`，服务保持未宣告状态。

这样调用方不会发现一个还无法拨通的 provider。

### 地址如何被其他节点知道

中继预约成功后，E 会产生一个电路监听器：

```text
<R>/p2p-circuit
```

Identify 会把 E 的 listen addresses（包括该电路监听器）推给已连接的对端 B。
B 的 `on_identify_peer` 把这些地址写入 Kademlia 路由表。

因此 E 不需要：

- 修改 `announce_addresses`；
- 把中继地址写进 provider 记录；
- 额外发布地址记录。

### 调用方如何解析地址

```text
A: get_providers(service)
  -> E 的 PeerId

A: get_closest_peers(E)
  -> PeerInfo { peer_id: E, addrs: [<R>/p2p-circuit, ...] }

A: send_request_with_addresses(E, addrs, request)
  -> Swarm 拨 <R>/p2p-circuit/p2p/E
  -> R 转发到 E
```

关键点：`get_closest_peers` 返回的 `PeerInfo` 带地址；`get_providers` 只返回
PeerId。当前 `request_service` 在直连失败时会自动执行这一步。

### 中继变化时的行为

| 事件 | 行为 |
|---|---|
| 第一个中继就绪 | `start_providing` |
| 中继增加（1 → 2/3） | 服务保持宣告 |
| 单个中继失效（2 → 1） | 服务保持宣告；调用方尝试剩余地址 |
| 最后一个中继失效（1 → 0） | `stop_providing` 所有本地服务 |
| 中继重新出现 | 再次 `start_providing` |

Kademlia 的 provider 记录是每个服务一条，不是每个中继一条；中继地址由
Identify/peer lookup 提供。

### 传输层

本版本只使用 WebSocket over HTTP：

```text
TCP → HTTP WebSocket → multistream-select → TLS/Noise → Yamux
```

- `network.port` 是 WebSocket 的 TCP 端口，默认 `5000`；
- bootstrap 必须使用 `/ws` 地址，例如
  `/ip6/.../tcp/5000/ws/p2p/<PeerId>`；
- 中继预约和调用都经 WebSocket；
- 内层仍优先 TLS，Noise 作为兼容回退；
- 旧版 raw TCP 节点不兼容，所有节点必须升级。

如果网络连明文 WebSocket 也阻断，需要 WSS（443 反代 + 证书），当前版本不包含。

### 中继容量

中继服务器的 `max_circuit_bytes` 自动设置为：

```text
max_request_bytes + max_response_bytes
```

bootstrap 的这两个上限应不小于客户端；否则大报文会在中继处被拒绝。
`max_circuits` 仍使用 libp2p 默认值。

### 状态

`relay_status` 返回：

```json
{
  "need_relay": true,
  "target": 3,
  "reachability": "relay",
  "active_relays": ["12D3Koo..."],
  "pending_relays": []
}
```

`reachability`：

- `public`：本机探测到公网地址；
- `relay`：没有公网地址，但至少一个中继已就绪；
- `none`：两者都没有。

---

<a id="english"></a>
## English

NexusNet uses libp2p Circuit Relay v2 to let nodes without a public address join
the network and serve traffic. A NATed node needs no public IP and no manual
address announcement; it only needs at least one dual-stack public
bootstrap/relay.

### Topology

```text
        [B: dual-stack public bootstrap + relay]
          ^                 ^
          |                 |
     [E: NAT node]      [A: caller]
```

- B must be dual-stack. Dual-stack nodes enable the relay server automatically.
- E writes B's WebSocket address into `services.kademlia.bootstrap_nodes`,
  for example `/ip6/.../tcp/5000/ws/p2p/<PeerId>`.
- E can be single-stack, or dual-stack behind NAT. It does not need a public address.

### E does not announce services before it has a relay

After E starts:

1. It connects to B and completes DHT bootstrap.
2. It reserves a `p2p-circuit` on B.
3. Only after receiving a circuit listener does ServiceRegistry allow `start_providing`.
4. If `active_relays == 0`, services remain unannounced.

This prevents callers from discovering a provider they cannot reach.

### How other nodes learn the address

After a successful reservation, E gains a circuit listener:

```text
<R>/p2p-circuit
```

Identify pushes E's listen addresses (including that circuit listener) to its
connected peer B. B's `on_identify_peer` writes those addresses into the
Kademlia routing table.

Therefore E does not need to:

- change `announce_addresses`;
- put relay addresses into provider records;
- publish separate address records.

### How a caller resolves the address

```text
A: get_providers(service)
  -> E's PeerId

A: get_closest_peers(E)
  -> PeerInfo { peer_id: E, addrs: [<R>/p2p-circuit, ...] }

A: send_request_with_addresses(E, addrs, request)
  -> Swarm dials <R>/p2p-circuit/p2p/E
  -> R forwards to E
```

The key point: `get_closest_peers` returns a `PeerInfo` with addresses, while
`get_providers` returns only PeerIds. `request_service` performs this step
automatically when a direct dial fails.

### Behavior when relays change

| Event | Behavior |
|---|---|
| First relay becomes ready | `start_providing` |
| Relay count grows (1 → 2/3) | Services stay announced |
| One relay fails (2 → 1) | Services stay announced; callers try the remaining addresses |
| Last relay fails (1 → 0) | `stop_providing` for every local service |
| Relay returns | `start_providing` again |

Kademlia keeps one provider record per service, not per relay. Relay addresses
come from Identify and peer lookups.

### Transport

This version uses WebSocket over HTTP only:

```text
TCP → HTTP WebSocket → multistream-select → TLS/Noise → Yamux
```

- `network.port` is the WebSocket TCP port, default `5000`;
- bootstrap addresses must use `/ws`, for example
  `/ip6/.../tcp/5000/ws/p2p/<PeerId>`;
- relay reservations and calls all use WebSocket;
- TLS is still preferred inside, with Noise as a compatibility fallback;
- older raw-TCP nodes are incompatible; every node must be upgraded.

If the network blocks even plaintext WebSocket, WSS (a 443 reverse proxy with a
certificate) is required. This version does not include it.

### Relay capacity

The relay server sets `max_circuit_bytes` automatically to:

```text
max_request_bytes + max_response_bytes
```

The bootstrap node's limits should be at least as large as its clients';
otherwise large messages are rejected at the relay. `max_circuits` keeps the
libp2p default.

### Status

`relay_status` returns:

```json
{
  "need_relay": true,
  "target": 3,
  "reachability": "relay",
  "active_relays": ["12D3Koo..."],
  "pending_relays": []
}
```

`reachability`:

- `public`: a public address was detected locally;
- `relay`: no public address, but at least one relay is ready;
- `none`: neither is available.
