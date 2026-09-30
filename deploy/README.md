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
| `build-win.sh` | 交叉编译产物 + NSIS 组装 Windows 安装器与便携 zip，自动派生命名 |
| `win/service.nsi` | NSIS 安装脚本（含 `@PKG@`/`@BIN@`/`@VERSION@` 等占位符） |
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

## Windows 安装

Windows 侧在 Linux 上交叉编译（`x86_64-pc-windows-gnu`）+ NSIS 打包，命名同样自动从 `Cargo.toml` 派生。

### 构建

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
./deploy/build-win.sh
```

依赖：`mingw-w64`（`x86_64-w64-mingw32-gcc`）、NSIS（`makensis`）、`curl`/`unzip`/`zip`。
NSSM 由脚本按固定版本（2.24）+ SHA256 下载缓存到 `target/nssm-cache/`，不随仓库分发。

产物（`target/packaging/`）：

| 文件 | 说明 |
|---|---|
| `<pkg>_<version>_windows_amd64_setup.exe` | NSIS 安装器 |
| `<pkg>_<version>_windows_amd64.zip` | 便携包（仅裸 exe） |
| `<name>-windows-amd64.exe` | 裸二进制 |

### 目录布局

| 项 | 路径 | 说明 |
|---|---|---|
| 二进制 | `%ProgramFiles%\<pkg>\<bin>.exe` | 主程序 |
| 包装器 | `%ProgramFiles%\<pkg>\nssm.exe` | NSSM，服务 ImagePath |
| 配置 | `%ProgramData%\<pkg>\config.toml` | 首启自动生成 |
| 数据/日志 | `%ProgramData%\<pkg>` | 身份/日志文件 `log` 与轮转 `.gz` 同目录 |

路径经 `NEXUSNET_HOME` / `NEXUSNET_CONFIG` / `NEXUSNET_LOG_PATH` 锚定，由安装器以
NSSM `AppEnvironmentExtra` 注入，与 systemd 单元同一契约。

### 安装 / 升级 / 卸载

以管理员运行 `setup.exe`。安装过程：停删旧服务（升级场景）→ 释放文件 → 建数据目录并对
`NT SERVICE\<pkg>` 授予写权限 → `nssm install` 并配置（自动启动、失败重启、Ctrl+C 优雅停止 15s）
→ `sc.exe config obj= "NT SERVICE\<pkg>"` → 启动服务并写卸载项。

- 服务账号：虚拟账号 `NT SERVICE\<pkg>`（无需密码），配置/日志目录已 ACL 放行。
- 升级：运行新版本 `setup.exe`，`%ProgramData%\<pkg>` 保留。
- 卸载：应用和功能或 `uninstall.exe`；停删服务、删除 `%ProgramFiles%\<pkg>`，**保留** `%ProgramData%\<pkg>`。

```powershell
Get-Service <pkg>
sc.exe query <pkg>
sc.exe stop <pkg>; sc.exe start <pkg>
& "$env:ProgramFiles\<pkg>\nssm.exe" edit <pkg>
```

### 服务管理（等价 systemctl）

服务名即规范化包名 `<pkg>`，由 NSSM 托管；NSSM 位于 `%ProgramFiles%\<pkg>\nssm.exe`。

| systemctl | Windows 等价 |
|---|---|
| `systemctl status <pkg>` | `sc.exe query <pkg>` / `Get-Service <pkg>` / `nssm.exe status <pkg>` |
| `systemctl start/stop/restart` | `sc.exe start\|stop <pkg>` / `Restart-Service <pkg>` / `nssm.exe start\|stop\|restart <pkg>` |
| `systemctl enable/disable` | `sc.exe config <pkg> start= auto\|demand` |
| `systemctl cat` | `nssm.exe get <pkg> <param>`（逐项）/ `sc.exe qc <pkg>` |
| `systemctl edit` | `nssm.exe edit <pkg>`（GUI） |
| `systemctl reload`（SIGHUP） | 无 SIGHUP；用 `nssm.exe restart <pkg>` |

> 若服务卡在 `PAUSED`（`sc query` STATE 7）：`nssm.exe continue <pkg>` 恢复；
> 无效则 `nssm.exe stop <pkg>` → `nssm.exe remove <pkg> confirm` → `sc.exe delete <pkg>` 后重新运行 `setup.exe`。

### 日志

Windows 下无 journald，程序自动切换为**文件模式**，写入 `%ProgramData%\<pkg>\`：

| 项 | 路径 |
|---|---|
| 当前日志（追加，**无扩展名**） | `%ProgramData%\<pkg>\log` |
| 轮转归档（>10MB 触发，gzip） | `%ProgramData%\<pkg>\<MMDD_HHMM>-<MMDD_HHMM>-<nanos>.gz` |
| NSSM 捕获的 stdout/stderr（兜底） | `%ProgramData%\<pkg>\service.out.log` / `service.err.log` |
| NSSM 服务级事件（启停/重启） | Windows「应用程序」事件日志，来源 `nssm` |

```powershell
$log = "$env:ProgramData\<pkg>\log"
Get-Content $log -Tail 50                 # 最近 50 行
Get-Content $log -Wait -Tail 50           # 实时跟踪（等价 journalctl -f）
Get-Content $log | Select-String '\[!\]|\[CRITICAL\]'   # 只看错误/严重
Get-WinEvent -ProviderName nssm -MaxEvents 50            # NSSM 服务级事件
```

日志级别前缀：`[IMPORTANT] [+] [-] [*] [!] [CRITICAL]`。

前台直跑（快速排障，直接看终端输出）：
```powershell
$env:NEXUSNET_HOME="$env:ProgramData\<pkg>"
$env:NEXUSNET_CONFIG="$env:ProgramData\<pkg>\config.toml"
$env:NEXUSNET_LOG_PATH="$env:ProgramData\<pkg>"
& "$env:ProgramFiles\<pkg>\<bin>.exe"
```

## 说明

- 服务以 `nexusnet` 专用用户运行（`NoNewPrivileges`、`ProtectSystem=strict` 等加固），
  通过 `ReadWritePaths` 放行配置与数据目录的写权限；日志经 stdout/stderr 交给 journald。
- 收到 `SIGTERM` 时程序优雅关闭（`NodeController`/`ServiceDispatcher` 收到共享 shutdown 信号），
  `TimeoutStopSec=15` 防止卡死被强杀。Windows 下由 NSSM 以 Ctrl+C 触发同一优雅退出路径。
- 后端边车（CLI / OCR 等）仍由外部独立管理，本服务不管理其生命周期；
  如后续需要，可在 `nexusnet.service` 的 `[Unit]` 中追加 `After=`/`Wants=` 关联对应的边车服务。
