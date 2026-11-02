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
| 配置 | `/etc/<pkg>/config.toml` | `nexusnet:nexusnet 0644` | 向导生成；兼容升级时保留现有配置 |
| 配置 schema | `/etc/<pkg>/config.schema` | `nexusnet:nexusnet 0644` | 安装器升级决策；成功写入后更新 |
| 数据 | `/var/lib/<pkg>/keypair.bin` | `nexusnet:nexusnet 0700` | 节点身份，不可丢失 |
| 日志 | journald | — | 由 systemd 采集/轮转/压缩/保留 |
| 二进制 | `/usr/bin/<bin>` | `root:root 0755` | 主程序 |
| 单元 | `/lib/systemd/system/<pkg>.service` | `root:root 0644` | systemd 单元 |

路径由 `NEXUSNET_CONFIG_PATH` / `NEXUSNET_KEYPAIR_PATH` / `NEXUSNET_LOG_PATH`（均为目录）
与 `NEXUSNET_LOG_MODE` 环境变量锚定，见 `src/paths.rs`。本地 `cargo run`（不设任何环境变量）
仍回退当前目录 `./config.toml` / `./keypair.bin` / `./log`。

## 网络规划

- 至少一台机器需要**双栈公网地址**，并在各 NAT 节点的
  `services.kademlia.bootstrap_nodes` 中配置为 bootstrap。
- 双栈 bootstrap 会自动开启 Circuit Relay v2 server；NAT 节点无需公网 IP。
- NAT 节点在第一个 relay 就绪前不会宣告服务，最后一个 relay 失效后会撤销宣告。
- 中继电路上限自动取 `max_request_bytes + max_response_bytes`，bootstrap 的
  这两个值应不小于客户端。
- 调用方通过 `get_closest_peers` 解析 provider 的 relay 地址，详见
  [../docs/nat-relay.md](../docs/nat-relay.md)。
- 本版本只使用 WebSocket over HTTP；`network.port` 是 WebSocket TCP 端口。
- bootstrap 地址必须带 `/ws`，例如
  `/ip6/.../tcp/5000/ws/p2p/<PeerId>`。
- 旧版 raw TCP 节点不兼容，所有节点必须使用 0.6.x；当前发布版本 0.6.2-11020。

## 安装

在 Debian / Ubuntu 上以 root 执行：

```bash
apt install -y ./<pkg>_<version>_amd64.deb
```

安装过程（postinst）自动：确保专用用户 `nexusnet` 存在、创建数据目录、确保 journald 持久化
（创建 `/var/log/journal`），然后根据 `config.schema` 决定是否进入向导，并处理配置与服务状态。

### 配置向导（whiptail）

交互安装和 `dpkg-reconfigure <pkg>` 使用 whiptail 向导；debconf 只负责数据存储、
preseed 和非交互兼容。依赖 `whiptail` 与 `python3 (>= 3.11)`，目标为 Debian 12+。

向导页面：

- 配置方式：向导 / 跳过（仅首次安装或破坏性升级）
- 节点名称、描述（不再询问角色和 IPv4/IPv6 监听；启动时自动检测地址族）
- WebSocket 端口（1..65535）
- 本地服务动态编辑器（添加/编辑/删除/保存/取消；可关闭并清空本地服务）
- 鉴权网络名、Authority 公钥，以及逐个服务切换鉴权状态（回车切换，保存并返回）；可关闭鉴权网络
- Bootstrap 编辑器（添加/编辑/删除单条地址；列表中长地址中间截断，编辑时显示完整值；每项必须包含 `/ws` 和 `/p2p/<PeerId>`；可留空）
- 汇总确认后写入配置

PQ 传输和 PQ 身份使用程序默认值（新安装默认启用），不在页面中显示；`pq_required`
仍可通过 preseed 设置。已有 `/etc/<pkg>/config.toml` 时会预填节点、端口、bootstrap、
本地服务和鉴权信息；解析失败只告警并回退默认值。

升级决策：

- 没有配置：生成默认配置并进入向导。
- `config.schema` 低于 `BREAKING_BELOW`：备份旧配置后进入向导。
- `config.schema` 在兼容范围内：不进入向导，保留配置和服务原有状态。
- `config.schema` 高于当前程序：禁用服务并提示降级/升级。
- `dpkg-reconfigure <pkg>`：进入向导；旧配置会先备份，未显示字段会回到默认值。

“跳过”语义：

- 已有配置：保留原配置，不重新询问，保持服务原有 enabled/disabled 状态。
- 没有配置：生成默认配置，服务不自动启动，手动 `systemctl enable --now <pkg>`。
- 破坏性升级时取消向导：保留旧配置，禁用服务并提示。

DHT、Ping、Dispatcher、Registry、Relay、Compat 等高级项仍保存在 debconf 中，可用
preseed 设置；向导不再逐项询问。

重新配置会备份旧 `config.toml`，服务原本启用则重启。

非交互安装：

```bash
DEBIAN_FRONTEND=noninteractive apt install -y ./<pkg>_<version>_amd64.deb
```

无 preseed 时按“跳过向导”处理：生成默认配置并 `systemctl disable --now <pkg>`，
不会自动启动。需要无人值守配置时使用 preseed：

```text
<pkg> <pkg>/setup_mode select wizard
<pkg> <pkg>/setup_mode seen true
<pkg> <pkg>/node_name string node-01
<pkg> <pkg>/bootstrap string /ip4/1.2.3.4/tcp/5000/ws/p2p/12D3KooW...
<pkg> <pkg>/local_services_enabled boolean true
<pkg> <pkg>/local_services string ocr:5014:1
<pkg> <pkg>/auth_enabled boolean true
<pkg> <pkg>/auth_network string corp
<pkg> <pkg>/auth_authority string <base64-ed25519-public-key>
# 可选：覆盖 registry 租约；未设置时按 2 * types_sync + 1 自动派生
<pkg> <pkg>/registry_type_ttl string 1800
```

preseed 值优先。config 脚本仍会运行，但兼容的 `config.schema` 会自动跳过向导；
`setup_mode=wizard` 时未预置字段由 `postinst` 使用默认值补齐。

跳过向导后手动启用：

```bash
sudo nano /etc/<pkg>/config.toml
sudo systemctl enable --now <pkg>
```

### 服务状态

- 向导完成：`systemctl enable` 并启动。
- 跳过且已有配置：保持原有 enabled 状态；enabled 才启动。
- 跳过且无配置：生成默认配置，`systemctl disable --now`；用户配置后手动 `enable --now`。
- 旧版 raw TCP bootstrap：`--check-config` 检测到后停止并禁用服务，提示重新运行
  `dpkg-reconfigure <pkg>` 或手动修改配置。
- Debian 包不管理防火墙；直连节点需要自行放行 TCP 5000（或向导中配置的端口）。

```bash
systemctl status <pkg>        # active (running)
journalctl -u <pkg> -f        # 查看运行日志（单行、带日志级别，已去除 ANSI 彩色）
```

日志按 syslog 级别写入 journald，可用 `journalctl -u <pkg> -p err` 过滤错误及以上。

## 升级

```bash
apt install -y ./<pkg>_<新版本>.deb
```

升级时 prerm 停止服务；postinst 保留 `keypair.bin`。`config.schema` 兼容时保留现有
`config.toml`，并按服务原有 enabled 状态决定是否启动；低于 `BREAKING_BELOW` 时先备份再
进入向导。0.6.x 检测到旧版 raw TCP bootstrap 时会禁用服务并提示重新配置。

## 卸载

```bash
apt remove <pkg>             # 移除包、停服务
apt purge <pkg>              # 彻底清除包配置
```

`remove` 保留 `/etc/<pkg>/config.toml` 和 `config.schema`。`purge` 会清除 debconf 答案、
`config.toml`、`config.schema` 及 `config.toml.bak.*`。

无论 `remove` 还是 `purge`，**都不会删除** `/var/lib/<pkg>/keypair.bin`，也**不会删除 `nexusnet` 用户**
（该账号由 NexusNet 后端服务共享）。

## 环境变量

| 变量 | 作用 | 默认（systemd） |
|---|---|---|
| `NEXUSNET_CONFIG_PATH` | 配置目录 | `/etc/<pkg>/` |
| `NEXUSNET_KEYPAIR_PATH` | 身份密钥目录 | `/var/lib/<pkg>/` |
| `NEXUSNET_LOG_PATH` | 日志目录 | `/var/lib/<pkg>/`（journald 模式下不写文件） |
| `NEXUSNET_LOG_MODE` | 日志模式（`journald` / `nssm` / `file`） | `journald` |

`JOURNAL_STREAM` 存在时自动切换为 journald 模式（由 systemd 设置）；`NO_COLOR` 或非 TTY 时去色。

## 目录内容

| 文件 | 作用 |
|---|---|
| `build-deb.sh` | 用 `dpkg-deb` 组装 `.deb`（无需 cargo-deb），自动派生命名 |
| `service.service` | systemd 单元（打包素材，含 `@PKG@`/`@BIN@`/`@USER@` 占位符） |
| `service.tmpfiles.conf` | 预建目录与属主（打包素材） |
| `../deb/{postinst,prerm,postrm}` | Debian 维护脚本（打包素材） |
| `../deb/{config,templates}` | whiptail 配置向导 / debconf 数据模板（打包素材） |
| `../config-schema.toml` | `current` / `breaking_below` 单一来源；构建时注入 deb 和 NSIS |
| `../deb/tests` | 配置预填与非交互 preseed 测试 |
| `build-win.sh` | 交叉编译产物 + NSIS 组装 Windows 安装器与便携 zip，自动派生命名 |
| `win/service.nsi` | NSIS 安装脚本（含 `@PKG@`/`@BIN@`/`@VERSION@` 等占位符） |
| `win/common.nsh` / `win/wizard.nsh` | NSIS 向导页面、JSON 生成与校验 |
| `win/setup.example.json` | 静默安装 `/SETUPFILE` 示例 |
| `publish-release.sh` | 幂等发布：把指定目录内的文件上传到 Gitea Release（手动发布或旧 Gitea CI 使用；WebGit CI 仅构建，不上传产物） |

## 安装器内部配置模式

Windows 和 Debian 安装器共用同一个配置写入器：

```bash
NexusNet --write-config <answers.json> --config-dir <dir> [--backup]
NexusNet --defaults --config-dir <dir>
NexusNet --check-config --config-dir <dir>
```

- `--write-config`：从一个 answers JSON 生成完整配置；缺失字段使用 Rust 默认值；
  `--backup` 会把旧 `config.toml` 备份为 `config.toml.bak.<timestamp>`。
- `--defaults`：仅在配置不存在时生成默认配置；已存在时保持不动。
- `--check-config`：只读检查；退出码 `0` 有效、`2` 配置非法、`3` 旧版 raw TCP
  bootstrap、`4` IO 错误。
- 安装器在成功写入后同时写入 `config.schema`。
- Windows NSIS 写入 UTF-16LE JSON；Debian postinst 写入 UTF-8 JSON；两者都支持中文。

正常服务启动不会解析这些参数。

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
| 配置 | `%ProgramData%\<pkg>\config.toml` | 向导生成；兼容升级时保留现有配置 |
| 配置 schema | `%ProgramData%\<pkg>\config.schema` | 安装器升级决策；成功写入后更新 |
| 数据/日志 | `%ProgramData%\<pkg>` | 身份/日志文件 `log` 与轮转 `.gz` 同目录 |

路径经 `NEXUSNET_CONFIG_PATH` / `NEXUSNET_KEYPAIR_PATH` / `NEXUSNET_LOG_PATH` 与
`NEXUSNET_LOG_MODE=nssm` 锚定，由安装器以 NSSM `AppEnvironmentExtra` 注入，与 systemd
单元同一契约。

### 配置向导

安装器向导与 Linux 保持同一流程：

- 配置方式：使用配置向导 / 跳过（已有配置时跳过即保留）
- 节点名称、描述
- WebSocket 端口（1..65535）
- 本地服务编辑器（添加/编辑/删除；名称、端口校验）
- 单个鉴权网络与 Authority，以及每个服务的鉴权开关
- Bootstrap 地址（每行一个或逗号分隔，必须包含 `/ws` 和 `/p2p/<PeerId>`；可留空）
- 摘要确认

不再询问角色、IPv4/IPv6 监听和防火墙；程序启动时自动探测地址族。
PQ 传输和 PQ 身份使用程序默认值；`pq_required` 使用默认值。
Ping/Kademlia、Dispatcher/Registry、Relay/Compat 等高级项使用默认值，不在页面中逐项询问。
Windows 不预填旧 `config.toml`；重新配置从默认值开始，旧配置会备份为
`config.toml.bak.<timestamp>`。

兼容升级不会显示向导。需要强制重新配置时运行：

```bat
setup.exe /RECONFIGURE
```

跳过语义：

- 已有配置：保留原配置，保持原有自动/手动启动方式。
- 没有配置：生成默认 `config.toml`，服务设为 `SERVICE_DEMAND_START` 且不启动。
  完成提示会给出：

```powershell
notepad "$env:ProgramData\<pkg>\config.toml"
sc.exe start <pkg>
sc.exe config <pkg> start= auto
```

0.6.x 检测到旧版 raw TCP bootstrap 时会阻断安装，提示重新使用配置向导。

静默安装：

```bat
setup.exe /S /SETUPFILE=C:\deploy\nexusnet-setup.json
```

示例见 `deploy/win/setup.example.json`（其中的 PeerId 为占位，请替换）。没有 `/SETUPFILE` 时：已有配置保留，无配置则
使用默认配置并保持手动启动。

向导完成后安装器按端口自动添加 `NexusNet (<pkg>)` 的 TCP 入站规则；卸载时删除。
跳过/保留现有配置以及 `/SETUPFILE` 静默安装时不改动已有规则。
配置写入失败、端口非法或旧版 bootstrap 会在服务安装前中止。

### 安装 / 升级 / 卸载

以管理员运行 `setup.exe`。安装过程：停删旧服务（升级场景）→ 释放文件 → 建数据目录 →
安装服务并配置虚拟账号 → 对 `NT SERVICE\<pkg>` 授予数据目录写权限 → 按模式配置 NSSM
服务（向导：自动启动并启动；跳过且无配置：手动；跳过且已有配置：保持原状态）→ 写卸载项。
关键步骤（`nssm install`、`sc.exe config`、`icacls`、环境变量）失败会中止安装。

- 服务账号：虚拟账号 `NT SERVICE\<pkg>`（无需密码），配置/日志目录已 ACL 放行。
- 升级：运行新版本 `setup.exe`，`%ProgramData%\<pkg>` 保留；原有服务启动方式（自动/手动）保持。
- 重新配置：旧配置备份，写入新配置，服务恢复自动启动并启动。
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

Windows 下无 journald。安装器设置 `NEXUSNET_LOG_MODE=nssm`，程序把日志写到
stdout/stderr，由 NSSM 捕获并按大小轮转：

| 项 | 路径 |
|---|---|
| stdout | `%ProgramData%\<pkg>\service.out.log` |
| stderr（Warning/Error/Critical） | `%ProgramData%\<pkg>\service.err.log` |
| NSSM 服务级事件（启停/重启） | Windows「应用程序」事件日志，来源 `nssm` |
| 程序自身文件模式（仅 `NEXUSNET_LOG_MODE=file`） | `%ProgramData%\<pkg>\log` |

```powershell
$out = "$env:ProgramData\<pkg>\service.out.log"
$err = "$env:ProgramData\<pkg>\service.err.log"
Get-Content $out -Tail 50
Get-Content $err -Tail 50
Get-Content $out -Wait -Tail 50           # 实时跟踪（等价 journalctl -f）
Get-Content $err | Select-String '\[!\]|\[CRITICAL\]'
Get-WinEvent -ProviderName nssm -MaxEvents 50
```

日志级别前缀：`[IMPORTANT] [+] [-] [*] [!] [CRITICAL]`。

前台直跑（快速排障，直接看终端输出）：
```powershell
$env:NEXUSNET_CONFIG_PATH="$env:ProgramData\<pkg>"
$env:NEXUSNET_KEYPAIR_PATH="$env:ProgramData\<pkg>"
$env:NEXUSNET_LOG_PATH="$env:ProgramData\<pkg>"
$env:NEXUSNET_LOG_MODE="file"
& "$env:ProgramFiles\<pkg>\<bin>.exe"
```

## 说明

- 服务以 `nexusnet` 专用用户运行（`NoNewPrivileges`、`ProtectSystem=strict` 等加固），
  通过 `ReadWritePaths` 放行配置与数据目录的写权限；日志经 stdout/stderr 交给 journald。
- 收到 `SIGTERM` 时程序优雅关闭（`NodeController`/`ServiceDispatcher` 收到共享 shutdown 信号），
  `TimeoutStopSec=15` 防止卡死被强杀。Windows 下由 NSSM 以 Ctrl+C 触发同一优雅退出路径。
- 后端边车（CLI / OCR 等）仍由外部独立管理，本服务不管理其生命周期；
  如后续需要，可在 `nexusnet.service` 的 `[Unit]` 中追加 `After=`/`Wants=` 关联对应的边车服务。

## CI（WebGit）

CI 使用 WebGit 原生 runner 的 `.webgit/ci.yml`（version 1），不使用 GitHub/Gitea Actions：

- `test`：`cargo test --verbose`
- `fmt`：`cargo fmt -- --check`
- `build-linux`：`cargo build --release` + `./deploy/build-deb.sh`
- `build-windows`：存在 mingw-w64 与 NSIS 时交叉编译并调用 `./deploy/build-win.sh`，否则跳过

WebGit runner 不提供 artifact 上传或 Release 资产发布；构建产物保留在本次 run 的工作区，run 结束后清理。
