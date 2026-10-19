# NexusNet 0.5.1-1526 热修说明

- 版本：`0.5.1-1526`
- 类型：0.5.1 热修

## 修复

- 修复 ServiceRegistry actor 空闲时的忙等：`JoinSet::join_next()` 在集合为空时立即返回 `None`，旧代码的 `Result`/`Option` 分支会持续自旋并占用一个 Tokio worker。
- 改为 `Some(result) = join_next()`，无任务时真正等待命令或 shutdown。
- 新增 `idle_actor_does_not_spin` 回归测试，确保空闲 actor 不会持续循环。

## 影响

- 空闲 CPU 从约 90%（单 worker）恢复到接近 0。
- 避免 controller、dispatcher、backend_task 被忙等任务饿住，修复边车 `whoami` 等控制命令超时。
- 服务注册、撤销、类型索引行为不变。

## 验证

- `cargo fmt --check`
- `cargo check --all-targets`
- `cargo test`
- `cargo clippy --all-targets`
- `cargo build --release`
- `deploy/build-deb.sh`

## 说明

Cargo 版本号使用 `0.5.1-1526`（对应请求的 `0.5.1_1526`；下划线不是合法 semver 字符）。
