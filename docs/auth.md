# NexusNet 鉴权契约

本文档定义 NexusNet 鉴权网络的记录格式、发布流程与验证算法，供边车（权限控制程序）与任意语言的实现者参考。

## 1. 信任模型

- DHT 是**不可信、可覆盖、可重放**的存储：任何节点都能 `put_record`。
- 信任来自**权威 ed25519 签名**：记录由持权威私钥者签发，接收方用配置中固定的权威公钥验签。
- 采用 TUF 的三类防护：
  - **回滚（rollback）**：`version` 单调，拒绝更低版本。
  - **冻结（freeze）**：索引带 `expires_at`，过期即拒。
  - **mix-and-match**：索引记录每个服务白名单的 `hash`+`length`，取回后校验。
- **fail-closed**：索引/白名单不可用、过期、验签失败一律拒绝；唯一例外见 §6 第 3 步。

## 2. 命名与 key

```
net_hash    = base64url_nopad(SHA-256(net_name))
index_key   = /oahd/auth/<net_hash>/service
service_key = /oahd/auth/<net_hash>/<service>
```

- `net_name`：配置中的明文网络名；`net_hash` 用于 key 路径（不直白暴露网络名，但不防枚举）。
- 网络名/服务名：非空、≤64、仅 `[A-Za-z0-9_-]`。
- 服务名**不得**为保留名 `service`（与索引 key 冲突）。

## 3. 记录格式（字节级）

DHT 中存储的 value 是一段 **COSE_Sign1**（RFC 9052，CBOR 编码）：

```
COSE_Sign1 = [
  protected    : bstr,     # CBOR 头 { 1: -8 }  (alg = EdDSA / Ed25519)
  unprotected  : {},       # 空 map
  payload      : bstr,     # CBOR 编码的 doc（见 §4）
  signature    : bstr      # Ed25519 签名
]
```

签名覆盖的 `Sig_structure`（COSE 定义）：

```
Sig_structure = [ "Signature1", protected_bstr, external_aad, payload_bstr ]
external_aad  = key 路径的 UTF-8 字节（index_key 或 service_key）
```

`external_aad` 把记录绑定到它所在的 DHT key，防止记录被用于其它 DHT key；跨网络另由各网络权威公钥区分。

## 4. payload schema

CBOR 编码（字段名与类型）：

```
IndexDoc = {
  "version":    uint,
  "expires_at": uint,                 # Unix 秒
  "services": [ { "name": tstr, "hash": bstr(32), "length": uint } ]
}

WhitelistDoc = {
  "version": uint,
  "members": [ tstr ]                 # PeerId 字符串（如 12D3KooW...）
}
```

- `hash` = **SHA-256(该服务白名单记录的完整 value 字节)**，`length` = 该字节长度，用于 mix-and-match 防护。
- 索引的 `expires_at` 决定索引新鲜度；白名单无自身过期时间，其新鲜度由「被新鲜索引引用 + SWR 缓存」保证。

## 5. 发布流程（边车）

1. 为每个服务构造 `WhitelistDoc`，签名得到 value 字节（`external_aad = service_key`）。
2. 计算每个 value 的 `SHA-256` 与长度，构造 `IndexDoc`（`external_aad = index_key`），签名。
3. 经本地后端控制通道发布（CBOR `add_key`，`value` 为 `bstr`，二进制安全）：

```
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/<service>", value:<bstr> }
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/service", value:<bstr> }
```

   **顺序**：先发各服务白名单，再发索引（索引引用其 hash）。
   控制层仅允许写 `/oahd/auth/...` 与 `/oahd/service/<name>`；`/oahd/service/authtypes/`、
   `/oahd/service/types` 和 `/oahd/bootstrap` 由节点维护，后端不可写。
4. 在 `expires_at` 之前**重发续期**；否则记录过期后节点 fail-closed，拒绝相关服务。

> 本地后端协议见 README「后端协议（TCP，v2）」；规范见 `docs/sidecar.cddl`。

## 6. 客户端验证算法（TUF-lite）

节点按以下顺序处理入站服务请求（`peer` = 传输层已认证的发起方 PeerId）：

1. 若节点 `auth.network == "none"` → 放行。
2. 若该服务本地 `require_auth == false` → 放行。
3. 通过 SWR 缓存读取索引：
   - 2 秒内直接命中；2–5 秒返回旧值并后台刷新；超过 5 秒等待刷新。
   - 缺失/过期 → **拒绝**。
   - 服务不在索引 → 本次**放行**并记 Warning；不修改本地 `require_auth`，索引恢复后自动重新生效。
4. 通过 SWR 缓存读取该服务白名单：
   - 缺失/过期/超过 stale 窗口 → **拒绝**。
   - `peer ∈ members` → 放行，否则 **拒绝**。

缓存从 DHT 一次取回多条记录，验签后选择最高 `version`；同版本不同内容视为冲突并拒绝。
刷新失败且已超过 5 秒 stale 窗口时 fail-closed。索引过期后即使仍在缓存中也会被拒绝。

## 7. 配置

```toml
[auth]
network = "none"                 # "none" = 不鉴权

[auth.networks.myorg]            # 每个网络的信任锚
authority = "<base64 ed25519 公钥>"

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
require_auth = true
```

节点固定连接 `127.0.0.1:<port>`；鉴权缓存的策略固定为 2 秒 fresh / 5 秒 stale，没有独立配置项。鉴权记录写入使用 `Quorum::One`，并受 `compat.max_record_bytes`（默认 14 KiB）限制，确保旧版节点仍能存储与验证。

- `authority`：标准 base64 编码的 ed25519 公钥（32 字节）。
- 若 `network != "none"` 但该网络未配置/非法 `authority`，节点进入 **fail-closed**：拒绝所有 `require_auth` 服务。

### `auth_status`

返回 JSON：

```json
{ "network": "myorg", "state": "active",
  "local_required": ["cmd"],
  "index_version": 3,
  "whitelist_versions": { "cmd": 7 } }
```

`state = "misconfigured"` 时额外返回 `detail`。版本号只在当前进程内跟踪，重载后清空。

`state` 取值：`disabled` / `active` / `misconfigured`（后者附 `detail`）。

## 8. 安全说明

- **撤销**依赖 `expires_at` 与 SWR 缓存：权威不续发即过期失效；已缓存的正记录最多再存活约 5 秒，随后必须刷新成功或拒绝。
- **混合版本**：旧节点仍按 60 秒缓存/单记录读取；新节点的 5 秒 SWR 与多版本选择只影响本地，不改变 wire 格式。
- **不防枚举**：`net_hash` 仅隐藏网络名明文，知道名字即可算出哈希；白名单成员 PeerId 公开可读。
- **无密钥轮换**：当前版本权威公钥固定在配置，轮换需改配置重启（后续可能扩展 root 委派）。
- **保留名** `service` 不得用作服务名。
- 常量上限：文档 1 MiB、成员 100000、服务 10000。

## 9. 常量速查

| 项 | 值 |
|---|---|
| COSE 算法 | `EdDSA`（-8，Ed25519） |
| payload 编码 | CBOR |
| 摘要 | SHA-256 |
| 编码（key 哈希） | base64url 无填充 |
| 编码（发布/权威公钥） | 标准 base64 |
| `MAX_DOC_BYTES` | 1 MiB（Kademlia store/packet 上限已同步调大） |
| `MAX_MEMBERS` | 100000 |
| `MAX_SERVICES` | 10000 |
| 保留服务名 | `service` |
