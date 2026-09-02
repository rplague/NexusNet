# NexusNet deb 安装指南

NexusNet 以原生的 Debian 打包（`.deb`）分发，由 `dpkg`/`apt` 管理安装、升级与卸载生命周期。
服务以专用用户 `nexusnet` 运行，路径与 systemd 单元由包内声明。

## 目录布局（方案 A）

| 文件 | 路径 | 属主/权限 | 说明 |
|---|---|---|---|
| 配置 | `/etc/nexusnet/config.toml` | `nexusnet:nexusnet 0640` | **首启由程序自动生成**（不随包分发） |
| 数据 | `/var/lib/nexusnet/keypair.bin` | `nexusnet:nexusnet 0600` | 节点身份，不可丢失 |
| 日志 | `/var/log/nexusnet/nexusnet.log` | `nexusnet:nexusnet 0640` | 追加写 + 自动 gz 轮转（归档保留） |
| 二进制 | `/usr/bin/NexusNet` | `root:root 0755` | 主程序 |
| 单元 | `/lib/systemd/system/nexusnet.service` | `root:root 0644` | systemd 单元 |

路径由 `NEXUSNET_HOME` / `NEXUSNET_LOG_FILE` 环境变量锚定，见 `src/paths.rs`。
本地 `cargo run`（不设任何环境变量）仍回退当前目录 `./config.toml / ./keypair.bin / ./log`。

## 安装

在 Debian / Ubuntu 上以 root 执行：

```bash
apt install -y ./nexusnet_<version>_amd64.deb
```

安装过程（postinst）自动：创建专用用户 `nexusnet`、创建数据/日志目录、`daemon-reload`、`enable + start`。

```bash
systemctl status nexusnet        # active (running)
journalctl -u nexusnet -f        # 查看运行日志（已去除 ANSI 彩色）
```

日志同时在 `/var/log/nexusnet/nexusnet.log` 持久记录。

## 升级

```bash
apt install -y ./nexusnet_<新版本>.deb
```

升级时 postinst 执行 `try-restart`（服务在运行则重启应用新二进制），**保留** `keypair.bin` 身份与日志归档。

## 卸载

```bash
apt remove nexusnet             # 移除包、停服务
apt purge nexusnet              # 彻底清除（移除专用用户）
```

无论 `remove` 还是 `purge`，**都不会删除** `/var/lib/nexusnet/keypair.bin` 与 `/var/log/nexusnet` 归档，
以免丢失 PeerId 与历史日志。（如需清理请手动处理。）

## 目录内容

| 文件 | 作用 |
|---|---|
| `build-deb.sh` | 用 `dpkg-deb` 组装 `.deb`（无需 cargo-deb） |
| `nexusnet.service` | systemd 单元（打包素材） |
| `nexusnet.tmpfiles.conf` | 预建目录与属主（打包素材） |
| `nexusnet.sysusers` | 专用用户声明（打包素材） |
| `../deb/{postinst,prerm,postrm}` | Debian 维护脚本（打包素材） |

## 手动构建 deb

```bash
cargo build --release
./deploy/build-deb.sh
# 产物: target/packaging/nexusnet_<version>_amd64.deb
```

如已安装 `cargo-deb`，也可运行 `cargo deb`（利用 `Cargo.toml` 的 `[package.metadata.deb]`）。

## 常用命令

```bash
systemctl restart nexusnet         # 重启
systemctl stop nexusnet            # 停止（SIGTERM 优雅退出）
systemctl cat nexusnet             # 查看当前单元定义
systemctl show nexusnet            # 查看运行状态详情
```

## 说明

- 服务以 `nexusnet` 专用用户运行（`NoNewPrivileges`、`ProtectSystem=strict` 等加固），
  通过 `ReadWritePaths` 放行三个目录的写权限。
- 收到 `SIGTERM` 时程序优雅关闭（`NodeController`/`ServiceDispatcher` 收到共享 shutdown 信号），
  `TimeoutStopSec=15` 防止卡死被强杀。
- 后端边车（CLI / OCR 等）仍由外部独立管理，本服务不管理其生命周期；
  如后续需要，可在 `nexusnet.service` 的 `[Unit]` 中追加 `After=`/`Wants=` 关联对应的边车服务。
