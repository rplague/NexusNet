# 无公网 IP 接入（Circuit Relay v2）

NexusNet 通过 libp2p Circuit Relay v2 让没有公网地址的节点加入网络并提供服务。
NAT 节点不需要公网 IP，也不需要手工公告地址；只需要至少一台双栈公网
bootstrap/relay。

## 拓扑

```text
        [B：双栈公网 bootstrap + relay]
          ^                 ^
          |                 |
     [E：NAT 节点]      [A：调用方]
```

- B 必须双栈。双栈节点会自动启用 relay server。
- E 把 B 的 WebSocket 地址写入 `services.kademlia.bootstrap_nodes`，
  例如 `/ip6/.../tcp/5000/ws/p2p/<PeerId>`。
- E 可以单栈，也可以双栈但在 NAT 后面；它不需要公网地址。

## E 获得 relay 前不宣告服务

E 启动后：

1. 连接 B，完成 DHT bootstrap。
2. 向 B 预约 `p2p-circuit`。
3. 收到 circuit listener 后，ServiceRegistry 才允许 `start_providing`。
4. 如果 `active_relays == 0`，服务保持未宣告状态。

这样调用方不会发现一个还无法拨通的 provider。

## 地址如何被其他节点知道

relay 预约成功后，E 会产生一个 circuit listener：

```text
<R>/p2p-circuit
```

Identify 会把 E 的 listen addresses（包括该 circuit listener）推给已连接的对端 B。
B 的 `on_identify_peer` 把这些地址写入 Kademlia 路由表。

因此 E 不需要：

- 修改 `announce_addresses`；
- 把 relay 地址写进 provider 记录；
- 额外发布地址记录。

## 调用方如何解析地址

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

## relay 变化时的行为

| 事件 | 行为 |
|---|---|
| 第一个 relay 就绪 | `start_providing` |
| relay 增加（1 → 2/3） | 服务保持宣告 |
| 单个 relay 失效（2 → 1） | 服务保持宣告；调用方尝试剩余地址 |
| 最后一个 relay 失效（1 → 0） | `stop_providing` 所有本地服务 |
| relay 重新出现 | 再次 `start_providing` |

Kademlia 的 provider 记录是每个服务一条，不是每个 relay 一条；relay 地址由
Identify/peer lookup 提供。

## 传输层

本版本只使用 WebSocket over HTTP：

```text
TCP → HTTP WebSocket → multistream-select → TLS/Noise → Yamux
```

- `network.port` 是 WebSocket 的 TCP 端口，默认 `5000`；
- bootstrap 必须使用 `/ws` 地址，例如
  `/ip6/.../tcp/5000/ws/p2p/<PeerId>`；
- relay 预约和调用都经 WebSocket；
- 内层仍优先 TLS，Noise 作为兼容回退；
- 旧版 raw TCP 节点不兼容，所有节点必须升级。

如果网络连明文 WebSocket 也阻断，需要 WSS（443 反代 + 证书），当前版本不包含。

## relay 容量

relay server 的 `max_circuit_bytes` 自动设置为：

```text
max_request_bytes + max_response_bytes
```

bootstrap 的这两个上限应不小于客户端；否则大报文会在中继处被拒绝。
`max_circuits` 仍使用 libp2p 默认值。

## 状态

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
- `relay`：没有公网地址，但至少一个 relay 已就绪；
- `none`：两者都没有。
