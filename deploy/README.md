# 部署与 deb 安装指南

NexusNet 以原生的 Debian 打包（`.deb`）分发，由 `dpkg`/`apt` 管理安装、升级与卸载生命周期。
服务以专用用户 `nexusnet` 运行，路径与 systemd 单元由包内声明。

包名 / 二进制名在打包时**自动从 `Cargo.toml` 的 `[package]` 派生**：

| 来源 | 默认值 | 用途 |
|---|---|---|
| `Cargo.toml` `name` | `NexusNet` | 二进制名 |
| 规范化后（小写、`_`→`-`） | `nexusnet` | Debian 包名 / systemd 单元名 |

> 下文以 `<pkg>` 代指规范化后的包名，`<bin>` 代指原始二进制名。修改 `Cargo.toml` 的 `name` 后，
> `build-deb.sh` 会自动跟随，无需改动脚本。

## 目录布局

| 文件 | 路径 | 属主/权限 | 说明 |
|---|---|---|---|
| 配置 | `/etc/<pkg>/config.toml` | `nexusnet:nexusnet 0755` | **首启由程序自动生成**（不随包分发） |
| 数据 | `/var/lib/<pkg>/keypair.bin` | `nexusnet:nexusnet 0700` | 节点身份，不可丢失 |
| 日志 | journald | — | 由 systemd 采集/轮转/压缩/保留 |
| 二进制 | `/usr/bin/<bin>` | `root:root 0755` | 主程序 |
| 单元 | `/lib/systemd/system/<pkg>.service` | `root:root 0644` | systemd 单元 |

路径由 `NEXUSNET_HOME` / `NEXUSNET_CONFIG` / `NEXUSNET_KEYPAIR` / `NEXUSNET_LOG_PATH` 环境变量锚定，
见 `src/paths.rs`。本地 `cargo run`（不设任何环境变量）仍回退当前目录 `./config.toml` / `./keypair.bin` / `./log`。

## 安装

在 Debian / Ubuntu 上以 root 执行：

```bash
apt install -y ./<pkg>_<version>_amd64.deb
```

安装过程（postinst）自动：确保专用用户 `nexusnet` 存在、创建数据目录、确保 journald 持久化
（创建 `/var/log/journal`）、`daemon-reload`、`enable` 并启动。

```bash
systemctl status <pkg>        # active (running)
journalctl -u <pkg> -f        # 查看运行日志（单行、带日志级别，已去除 ANSI 彩色）
```

日志按 syslog 级别写入 journald，可用 `journalctl -u <pkg> -p err` 过滤错误及以上。

## 升级

```bash
apt install -y ./<pkg>_<新版本>.deb
```

升级时 prerm 停止服务、postinst 重新启动，**保留** `keypair.bin` 身份。

## 卸载

```bash
apt remove <pkg>             # 移除包、停服务
apt purge <pkg>              # 彻底清除包配置
```

无论 `remove` 还是 `purge`，**都不会删除** `/var/lib/<pkg>/keypair.bin`，也**不会删除 `nexusnet` 用户**
（该账号由 NexusNet 后端服务共享）。

## 环境变量

| 变量 | 作用 | 默认（systemd） |
|---|---|---|
| `NEXUSNET_HOME` | 数据根目录 | `/var/lib/<pkg>` |
| `NEXUSNET_CONFIG` | 配置文件路径 | `/etc/<pkg>/config.toml` |
| `NEXUSNET_KEYPAIR` | 节点身份密钥路径 | `<NEXUSNET_HOME>/keypair.bin` |
| `NEXUSNET_LOG_PATH` | 日志路径 | `/var/lib/<pkg>`（journald 模式下不写文件） |

`JOURNAL_STREAM` 存在时自动切换为 journald 模式（由 systemd 设置）；`NO_COLOR` 或非 TTY 时去色。

## 目录内容

| 文件 | 作用 |
|---|---|
| `build-deb.sh` | 用 `dpkg-deb` 组装 `.deb`（无需 cargo-deb），自动派生命名 |
| `service.service` | systemd 单元（打包素材，含 `@PKG@`/`@BIN@`/`@USER@` 占位符） |
| `service.tmpfiles.conf` | 预建目录与属主（打包素材） |
| `../deb/{postinst,prerm,postrm}` | Debian 维护脚本（打包素材） |
| `publish-release.sh` | 幂等发布：把指定目录内的文件上传到 Gitea Release（供 CI 复用） |

## 手动构建 deb

```bash
cargo build --release
./deploy/build-deb.sh
# 产物: target/packaging/<pkg>_<version>_amd64.deb
```

## 常用命令

```bash
systemctl restart <pkg>         # 重启
systemctl stop <pkg>            # 停止（SIGTERM 优雅退出）
systemctl reload <pkg>          # 重载（SIGHUP，重建网络）
systemctl cat <pkg>             # 查看当前单元定义
systemctl show <pkg>            # 查看运行状态详情
```

## 说明

- 服务以 `nexusnet` 专用用户运行（`NoNewPrivileges`、`ProtectSystem=strict` 等加固），
  通过 `ReadWritePaths` 放行配置与数据目录的写权限；日志经 stdout/stderr 交给 journald。
- 收到 `SIGTERM` 时程序优雅关闭（`NodeController`/`ServiceDispatcher` 收到共享 shutdown 信号），
  `TimeoutStopSec=15` 防止卡死被强杀。
- 后端边车（CLI / OCR 等）仍由外部独立管理，本服务不管理其生命周期；
  如后续需要，可在 `nexusnet.service` 的 `[Unit]` 中追加 `After=`/`Wants=` 关联对应的边车服务。
