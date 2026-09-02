# NexusNet systemd 部署指南

本目录提供将 NexusNet 主节点作为 systemd 服务运行所需的全部文件。

## 目录布局（方案 A）

| 文件 | 路径 | 属主/权限 | 说明 |
|---|---|---|---|
| 配置 | `/etc/nexusnet/config.toml` | `nexusnet:nexusnet 0640` | 服务可回写（CLI 变更写回兼容） |
| 数据 | `/var/lib/nexusnet/keypair.bin` | `nexusnet:nexusnet 0600` | 节点身份，不可丢失 |
| 日志 | `/var/log/nexusnet/nexusnet.log` | `nexusnet:nexusnet 0640` | 追加写 + 自动 gz 轮转（归档保留） |

路径由 `NEXUSNET_HOME` / `NEXUSNET_LOG_FILE` 环境变量锚定，见 `src/paths.rs`。
本地 `cargo run`（不设任何环境变量）仍回退当前目录 `./config.toml / ./keypair.bin / ./log`。

## 快速安装

以 root 执行：

```bash
sudo ./install.sh
```

脚本是幂等的：重复执行不会覆盖已有身份/配置。完成后：

```bash
systemctl status nexusnet        # active (running)
journalctl -u nexusnet -f        # 查看运行日志（已去除 ANSI 彩色）
```

日志同时在 `/var/log/nexusnet/nexusnet.log` 持久记录。

## 卸载

```bash
sudo ./uninstall.sh
```

卸载**保留**节点身份与日志归档，避免 PeerId 变更与历史丢失。

## 目录内容

| 文件 | 作用 |
|---|---|
| `nexusnet.service` | systemd 单元（专用用户、崩溃重启、沙箱加固、日志走 journald） |
| `nexusnet.tmpfiles.conf` | 预建目录与属主（配合 `systemd-tmpfiles`） |
| `nexusnet.sysusers` | 声明专用系统用户 `nexusnet` |
| `install.sh` | 幂等安装（构建、建用户、建目录、装单元、启动） |
| `uninstall.sh` | 幂等卸载（保留数据） |

## 常用命令

```bash
systemctl restart nexusnet         # 重启
systemctl stop nexusnet            # 停止（SIGTERM 优雅退出）
systemctl reload nexusnet          # HUP（如需）
systemctl cat nexusnet             # 查看当前单元定义
```

## 说明

- 服务以 `nexusnet` 专用用户运行（`NoNewPrivileges`、`ProtectSystem=strict` 等加固），
  通过 `ReadWritePaths` 放行三个目录的写权限。
- 收到 `SIGTERM` 时程序优雅关闭（`Run/Shutdown` 分支），`TimeoutStopSec=15` 防止卡死被强杀。
- 后端边车（CLI / OCR 等）仍由外部独立管理，本服务不管理其生命周期；
  如后续需要，可在 `nexusnet.service` 的 `[Unit]` 中追加 `After=`/`Wants=` 关联对应的边车服务。
