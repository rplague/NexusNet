# NexusNet 鉴权契约 / Authorization contract

**语言 / Language:** [中文](#中文) · [English](#english)

<a id="中文"></a>
## 中文

本文档定义 NexusNet 鉴权（authorization）网络的记录格式、发布流程与验证算法，供后端（权限控制程序）与任意语言的实现者参考。

### 1. 信任模型

- DHT 是**不可信、可覆盖、可重放**的存储：任何节点都能 `put_record`。
- 信任来自**权威方 ed25519 签名**：记录由持权威私钥者签发，接收方用配置中固定的权威公钥验签。
- 采用 TUF 的三类防护：
  - **回滚（rollback）**：`version` 单调，拒绝更低版本。
  - **冻结（freeze）**：索引带 `expires_at`，过期即拒。
  - **mix-and-match**：索引记录每个服务白名单的 `hash`+`length`，取回后校验。
- **fail-closed**：索引/白名单不可用、过期、验签失败一律拒绝；唯一例外见 §6 第 3 步。

### 2. 命名与 key

```
net_hash    = base64url_nopad(SHA-256(net_name))
index_key   = /oahd/auth/<net_hash>/service
service_key = /oahd/auth/<net_hash>/<service>
```

- `net_name`：配置中的明文网络名；`net_hash` 用于 key 路径（不直白暴露网络名，但不防枚举）。
- 网络名/服务名：非空、≤64、仅 `[A-Za-z0-9_-]`。
- 服务名**不得**为保留名 `service`（与索引 key 冲突）。

### 3. 记录格式（字节级）

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

### 4. payload schema

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
- 索引的 `expires_at` 决定索引新鲜度；白名单无自身过期时间，其新鲜度由「被新鲜索引引用 + 本地缓存」保证。

### 5. 发布流程（后端）

1. 为每个服务构造 `WhitelistDoc`，签名得到 value 字节（`external_aad = service_key`）。
2. 计算每个 value 的 `SHA-256` 与长度，构造 `IndexDoc`（`external_aad = index_key`），签名。
3. 经本地后端控制通道发布（CBOR `add_key`，`value` 为 `bstr`，二进制安全）：

```
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/<service>", value:<bstr> }
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/service", value:<bstr> }
```

   **顺序**：先发各服务白名单，再发索引（索引引用其 hash）。
   控制层仅允许写 `/oahd/auth/...` 与 `/oahd/service/<name>`；`/oahd/service/types`
   和 `/oahd/bootstrap` 由节点维护，后端不可写。
4. 在 `expires_at` 之前**重发续期**；否则记录过期后节点 fail-closed，拒绝相关服务。

> 本地边车协议见 README「边车协议（TCP，v2）」；规范见 `docs/sidecar.cddl`。

### 6. 客户端验证算法（TUF-lite）

节点按以下顺序处理入站服务请求（`peer` = 传输层已认证（authentication）的发起方 PeerId）：

1. 若节点 `auth.network == "none"` → 放行。
2. 若该服务本地 `require_auth == false` → 放行。
3. 读取索引：
   - 有上一次结果时立即返回，同时后台刷新；冷启动等待第一次查询。
   - 缺失 → **拒绝**。
   - `expires_at` 已过期 → **拒绝**。
   - 服务不在索引 → 本次**放行**并记 Warning；不修改本地 `require_auth`，索引恢复后自动重新生效。
4. 读取该服务白名单：
   - 缺失 → **拒绝**。
   - `peer ∈ members` → 放行，否则 **拒绝**。

缓存从 DHT 一次取回多条记录，验签后选择最高 `version`；同版本不同内容视为冲突并拒绝。
后台刷新失败时继续使用旧结果，但 `expires_at` 过期后仍然拒绝。

### 7. 配置

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

节点固定连接 `127.0.0.1:<port>`；鉴权缓存没有独立的 TTL 配置：有旧结果立即返回，后台单飞刷新。鉴权记录写入使用 `Quorum::One`，并受 `compat.max_record_bytes`（默认 14 KiB）限制，确保旧版节点仍能存储与验证。

- `authority`：标准 base64 编码的 ed25519 公钥（32 字节）。
- 若 `network != "none"` 但该网络未配置/非法 `authority`，节点进入 **fail-closed**：拒绝所有 `require_auth` 服务。

#### `auth_status`

返回 JSON：

```json
{ "network": "myorg", "state": "active",
  "local_required": ["cmd"],
  "index_version": 3,
  "whitelist_versions": { "cmd": 7 } }
```

`state = "misconfigured"` 时额外返回 `detail`。版本号只在当前进程内跟踪，重载后清空。

`state` 取值：`disabled` / `active` / `misconfigured`（后者附 `detail`）。

### 8. 安全说明

- **吊销（revoke）**依赖 `expires_at` 与后台刷新：权威方不续发即过期失效；已缓存的正记录会继续用于判定，直到下一次刷新成功或索引过期。建议权威方使用较短的 `expires_at`。
- **混合版本**：旧节点仍按 60 秒缓存/单记录读取；新节点使用“立即返回旧值 + 后台刷新”和多版本选择，只影响本地，不改变 wire 格式。
- **不防枚举**：`net_hash` 仅隐藏网络名明文，知道名字即可算出哈希；白名单成员 PeerId 公开可读。
- **无密钥轮换**：当前版本权威公钥固定在配置，轮换需改配置重启（后续可能扩展 root 委派）。
- **保留名** `service` 不得用作服务名。
- 常量上限：文档 1 MiB、成员 100000、服务 10000。

### 9. 常量速查

| 项 | 值 |
|---|---|
| COSE 算法 | `EdDSA`（-8，Ed25519） |
| payload 编码 | CBOR |
| 摘要 | SHA-256 |
| 编码（key 哈希） | base64url 无填充 |
| 编码（发布/权威公钥） | 标准 base64 |
| `MAX_DOC_BYTES` | 1 MiB（Kademlia 存储/包上限已同步调大） |
| `MAX_MEMBERS` | 100000 |
| `MAX_SERVICES` | 10000 |
| 保留服务名 | `service` |

---

<a id="english"></a>
## English

This document defines the record format, publishing flow and verification algorithm of a NexusNet authorization network. It is intended for backend programs (policy programs) and implementers in any language.

### 1. Trust model

- The DHT is **untrusted, overwritable and replayable**: any node can call `put_record`.
- Trust comes from a **signed record**: records are issued by the holder of the authority ed25519 private key, and receivers verify them with the authority public key pinned in configuration.
- Three TUF protections are used:
  - **Rollback**: `version` is monotonic; lower versions are rejected.
  - **Freeze**: the index carries `expires_at`; expired indexes are rejected.
  - **Mix-and-match**: the index records each service whitelist's `hash` and `length`, which are checked after retrieval.
- **Fail-closed**: a missing, expired or invalid index or whitelist is rejected. The only exception is described in §6 step 3.

### 2. Naming and keys

```
net_hash    = base64url_nopad(SHA-256(net_name))
index_key   = /oahd/auth/<net_hash>/service
service_key = /oahd/auth/<net_hash>/<service>
```

- `net_name` is the plaintext network name from configuration; `net_hash` is used in key paths (it hides the name from casual inspection but does not prevent enumeration).
- Network and service names must be non-empty, at most 64 characters, and match `[A-Za-z0-9_-]`.
- The service name **must not** be the reserved name `service` (it collides with the index key).

### 3. Record format (byte level)

The value stored in the DHT is a **COSE_Sign1** structure (RFC 9052, CBOR-encoded):

```
COSE_Sign1 = [
  protected    : bstr,     # CBOR header { 1: -8 }  (alg = EdDSA / Ed25519)
  unprotected  : {},       # empty map
  payload      : bstr,     # CBOR-encoded doc (see §4)
  signature    : bstr      # Ed25519 signature
]
```

The signed `Sig_structure` (as defined by COSE):

```
Sig_structure = [ "Signature1", protected_bstr, external_aad, payload_bstr ]
external_aad  = UTF-8 bytes of the key path (index_key or service_key)
```

`external_aad` binds the record to its DHT key so it cannot be replayed under another key. Different networks are separated by their authority public keys.

### 4. Payload schema

CBOR encoding (field names and types):

```
IndexDoc = {
  "version":    uint,
  "expires_at": uint,                 # Unix seconds
  "services": [ { "name": tstr, "hash": bstr(32), "length": uint } ]
}

WhitelistDoc = {
  "version": uint,
  "members": [ tstr ]                 # PeerId strings (e.g. 12D3KooW...)
}
```

- `hash` is **SHA-256 of the complete value bytes of that service's whitelist record**, and `length` is its byte length. Together they provide mix-and-match protection.
- The index's `expires_at` determines index freshness. Whitelists have no expiry of their own; their freshness is guaranteed by being referenced from a fresh index and by the local cache.

### 5. Publishing flow (backend)

1. Build a `WhitelistDoc` for each service and sign it to obtain the value bytes (`external_aad = service_key`).
2. Compute `SHA-256` and the length of each value, build the `IndexDoc` (`external_aad = index_key`), and sign it.
3. Publish through the local backend control channel (CBOR `add_key`, with `value` as a `bstr` so it is binary-safe):

```
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/<service>", value:<bstr> }
add_key { t:"add_key", id:<uuid>, key:"/oahd/auth/<net_hash>/service", value:<bstr> }
```

   **Order**: publish every service whitelist first, then the index (the index references their hashes).
   The control layer only permits writes to `/oahd/auth/...` and `/oahd/service/<name>`;
   `/oahd/service/types` and `/oahd/bootstrap` are maintained by the node and are not writable by backends.
4. **Republish before `expires_at`** to renew. Otherwise the record expires and nodes fail-closed for the affected services.

> See the README section "Sidecar protocol (TCP, v2)" for the local sidecar protocol and `docs/sidecar.cddl` for the contract.

### 6. Client verification algorithm (TUF-lite)

For each inbound service request (`peer` is the transport-authenticated initiator PeerId), the node checks in this order:

1. If `auth.network == "none"` → allow.
2. If the service's local `require_auth == false` → allow.
3. Read the index:
   - If a previous result exists, return it immediately while refreshing in the background; a cold start waits for the first query.
   - Missing → **deny**.
   - `expires_at` in the past → **deny**.
   - Service absent from the index → **allow this request** and log a warning; do not change the local `require_auth`, so protection resumes automatically when the index recovers.
4. Read the service whitelist:
   - Missing → **deny**.
   - `peer ∈ members` → allow; otherwise **deny**.

The cache fetches multiple records from the DHT at once, verifies signatures, and selects the highest `version`; the same version with different content is a conflict and is rejected.
A failed background refresh keeps using the previous result, but an expired `expires_at` is still rejected.

### 7. Configuration

```toml
[auth]
network = "none"                 # "none" = no authorization

[auth.networks.myorg]            # trust anchor for each network
authority = "<base64 ed25519 public key>"

[[services.dispatcher.local_services]]
name = "cmd"
port = 5014
require_auth = true
```

The node always connects to `127.0.0.1:<port>`. The auth cache has no separate TTL setting: a previous result is returned immediately and refreshed in the background with single-flight. Auth records are written with `Quorum::One` and are limited by `compat.max_record_bytes` (default 14 KiB), so older nodes can still store and verify them.

- `authority` is a standard-base64 ed25519 public key (32 bytes).
- If `network != "none"` but that network has no valid `authority`, the node becomes **fail-closed**: every `require_auth` service is denied.

#### `auth_status`

Returns JSON:

```json
{ "network": "myorg", "state": "active",
  "local_required": ["cmd"],
  "index_version": 3,
  "whitelist_versions": { "cmd": 7 } }
```

When `state = "misconfigured"`, an extra `detail` field is included. Versions are tracked only within the current process and are cleared on reload.

`state` values: `disabled` / `active` / `misconfigured` (the last one includes `detail`).

### 8. Security notes

- **Revocation** relies on `expires_at` and background refresh: if the authority stops republishing, records expire. A cached positive record remains in use until the next successful refresh or until the index expires. Prefer short `expires_at` values.
- **Mixed versions**: older nodes still cache for 60 seconds and read one record at a time; newer nodes use "return previous value immediately + background refresh" and multi-version selection. This only changes local behavior, not the wire format.
- **Not enumeration-resistant**: `net_hash` only hides the plaintext name; anyone who knows the name can compute the hash. Whitelist member PeerIds are publicly readable.
- **No key rotation**: the authority public key is pinned in configuration; rotating it requires a configuration change and restart (root delegation may be added later).
- **Reserved name** `service` cannot be used as a service name.
- Constant limits: 1 MiB per document, 100000 members, 10000 services.

### 9. Constants

| Item | Value |
|---|---|
| COSE algorithm | `EdDSA` (-8, Ed25519) |
| Payload encoding | CBOR |
| Digest | SHA-256 |
| Encoding (key hash) | base64url without padding |
| Encoding (publishing / authority key) | standard base64 |
| `MAX_DOC_BYTES` | 1 MiB (Kademlia store/packet limits were raised accordingly) |
| `MAX_MEMBERS` | 100000 |
| `MAX_SERVICES` | 10000 |
| Reserved service name | `service` |
