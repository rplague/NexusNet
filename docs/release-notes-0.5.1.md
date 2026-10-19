# NexusNet 0.5.1 发布说明

- 发布日期：2026-10-19
- Tag：`v0.5.1`

## 重点变更

- ServiceRegistry 改为独立异步 actor：服务注册、撤销、类型索引 reconcile 不再阻塞 NodeController。
- DHT job 使用 `JoinSet` 执行，新增 `services.registry.max_inflight_dht` 配置（默认 4，范围 1–16）。
- `list_services` 立即返回本地快照，并在后台触发类型索引刷新。
- `reannounce_services` 投递后台任务后立即返回 accepted。
- announce 失败使用指数退避重试（5s → 60s）；withdraw 失败也会重试。
- 类型索引 reconcile 单飞，并用 dirty 标记在当前链结束后重跑。
- reload 使用 generation 隔离旧 job 结果；shutdown 时排空 `JoinSet`。
- 修复“后端刚启动时串行 `start_providing` 长时间占用控制器”的问题。

## 配置

```toml
[services.registry]
max_inflight_dht = 4
```

## 行为说明

- `list_services` 为最终一致：返回当前快照，后台刷新；新类型可能在后续调用中可见。
- 服务注册/撤销在后台完成，边车控制命令不再等待 DHT。
- F-001 的 Identify/Bootstrap/Relay 内联 DHT 仍未处理；F-008 的 Swarm actor 无界通道仍未处理。

## 验证

- `cargo fmt --check`
- `cargo check --all-targets`
- `cargo test`：72 passed
- `cargo clippy --all-targets`：通过（保留原有 8 个 dead-code/derivable 警告）
- `cargo build --release`
- `cargo build --release --target x86_64-pc-windows-gnu`
- `deploy/build-deb.sh`
- `deploy/build-win.sh`

## 产物

产物位于 `target/packaging/release-0.5.1/`。
