# NexusNet 0.5.1-1644 发布说明

- 版本：`0.5.1-1644`
- 类型：0.5.1 DHT 命令异步化热修

## 变更

- DHT 控制命令移入独立有界 worker：
  - `query_key`
  - `discover_providers`
  - `service_request`
  - `service_request_to`
  - `providing=false` 的 `add_key`
- 新增配置：

```toml
[services.dispatcher]
max_inflight_dht = 4
```

默认 4，范围 1–16；reload 后按新值重建信号量。
- 新增控制命令耗时日志：Debug 级；超过 1 秒升级为 Warning。
- DHT worker 满时立即返回 `node busy: too many DHT commands`。
- `add_key` 的 `providing=true` 仍由 NodeController 处理，以保持 ServiceRegistry 宣告状态一致。
- `whoami`、`relay_status`、`pq_status`、`auth_status`、`list_services` 不再等待 DHT 命令完成。

## 验证

- `cargo fmt --check`
- `cargo check --all-targets`
- `cargo test`：74 passed
- `cargo clippy --all-targets`
- `cargo build --release`
- `deploy/build-deb.sh`
- `deploy/build-win.sh`

## 说明

Cargo 版本号使用 `0.5.1-1644`（对应请求的 `0.5.1_1644`；下划线不是合法 semver 字符）。
