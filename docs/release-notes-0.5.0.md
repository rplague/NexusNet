# NexusNet 0.5.0 发布说明

- 发布日期：2026-10-18
- Tag：`v0.5.0`
- 合并提交：`e7aa824`

## 重点变更

### 鉴权与并发

- 入站鉴权移入独立 worker 池（4 worker × 32 队列），不再阻塞 NodeController。
- 鉴权队列满时立即返回 `authentication overloaded`，避免无限排队。
- reload 在同一鉴权网络/权威公钥下保留回滚版本历史；信任域变化时才清空。
- 服务不在索引中时只放行当前请求，不再持久化把本地 `require_auth` 降级。

### PQ

- ML-DSA 签名身份绑定传输层 PeerId，修复身份与连接不匹配的问题。
- `pq_identity_enabled` 要求入站请求携带签名；部分身份字段会被拒绝。
- 仅在确认对端不支持 PQ 时回退明文；能力未知或身份未就绪时返回可重试错误，不再静默降级。
- 清理失败的 PQ 身份请求；可解码的失败请求会返回错误响应。

### 服务与协议

- `add_key` / `remove_key` 仅允许 `/oahd/service/<name>` 与 `/oahd/auth/...`。
- `/oahd/service/types` 名称校验并限制为 4096 条；索引解析失败会记录日志。
- `/oahd/bootstrap` 的自动发现改为 opt-in（`accept_bootstrap_providers = false` 默认）。
- 服务类型索引的并发覆盖收敛逻辑保持单调并集。

### 可靠性与配置

- 服务请求失败状态会正确回传给本地后端，不再把远端拒绝当作成功。
- DHT 传输错误不再写入 60 秒负缓存。
- dispatcher 控制命令超时预算会覆盖 DHT 查询和 P2P 请求时间。
- 后端请求、控制、事件通道改为有界；过载时返回明确错误。
- 配置校验覆盖端口范围、重复服务、零超时、PQ 组合等。
- 文档与实际配置字段、环境变量、启动流程对齐。

## 已回退

- **F-011**：provider failover 已回退，恢复单 provider 选择。
- **F-012**：公网 IP 自动探测的覆盖行为已回退，恢复原有逻辑。

对应回退提交：`beac162`、`f21b8d6`；回退后的超时预算清理：`62d4d03`。

## 已知限制

- **F-001**：Bootstrap、周期性 reconcile 和部分命令仍在 NodeController 循环内联执行，可能造成秒级阻塞；本轮只修复了入站鉴权热路径。
- **F-008**：Swarm actor 的 `cmd` / `event` 通道仍为无界；本轮覆盖了鉴权、dispatcher 和后端链路。

## 产物

产物位于 `target/packaging/release-0.5.0/`：

- `NexusNet-linux-amd64`
- `nexusnet_0.5.0_amd64.deb`
- `nexusnet_0.5.0_windows_amd64_setup.exe`
- `nexusnet_0.5.0_windows_amd64.zip`
- `NexusNet-windows-amd64.exe`
- `SHA256SUMS`

## 验证

- `cargo fmt --check`
- `cargo check --all-targets`
- `cargo test`：62 passed
- `cargo clippy --all-targets`：通过（保留原有 8 个 dead-code/derivable 警告）
- `cargo build --release`
- `cargo build --release --target x86_64-pc-windows-gnu`
- `deploy/build-deb.sh`
- `deploy/build-win.sh`
