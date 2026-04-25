# NexusNet

**OAHD 计划的核心网络层** — 基于 libp2p 的 P2P 节点网络，实现节点自动发现、地址动态更新留存、服务注册与发现。

## 概述

NexusNet 是 OAHD（Open Autonomous Hosting & Decentralization）计划的网络基础设施。它通过 P2P 网络将各节点连接起来，解决以下核心问题：

- **动态 IP 地址更新** — 节点 IP 变化后自动更新网络拓扑，确保其他节点仍能访问
- **服务注册与发现** — 节点可声明自身提供的服务（如 OCR、冷存储），其他节点可查询可用服务
- **去中心化路由** — 不依赖中心服务器，通过 Kademlia DHT 实现分布式节点发现和寻址

## 项目状态

当前版本：**0.1.1** — 核心 P2P 功能可用，服务注册/发现功能开发中。

## 架构

```
┌─────────────┐
│  NexusNet   │
│  P2P 节点    │
├─────────────┤
│ Ping        │ ← 节点健康检测、RTT 测量
├─────────────┤
│ Identify    │ ← 节点身份交换、版本协商
├─────────────┤
│ Kademlia    │ ← DHT 路由表、节点发现、记录存储
├─────────────┤
│ Service Reg │ ← [开发中] 服务注册/发现
└─────────────┘
```

### 模块说明

| 模块 | 文件 | 说明 |
|------|------|------|
| **main** | `src/main.rs` | 入口：加载配置、初始化节点、事件循环 |
| **config** | `src/config.rs` | JSON 配置管理：节点信息、网络参数、服务开关 |
| **net** | `src/net.rs` | 网络核心：libp2p 行为组合、节点密钥、IP 发现 |
| **log** | `src/log.rs` | 日志系统：彩色终端输出、文件日志、10MB 自动轮转 |

## 快速开始

### 前提条件

- Rust 1.70+（推荐使用 rustup 安装）

### 安装与运行

```bash
# 克隆仓库
git clone https://git.oahd.cn/rplague/NexusNet.git
cd NexusNet

# 首次运行会自动生成配置文件
cargo run
```

首次启动后会在当前目录生成：

- `config.json` — 节点配置文件
- `keypair.bin` — 节点身份密钥（请妥善保管）

### 配置说明

```json
{
  "node": {
    "name": "节点名称",
    "description": "节点描述"
  },
  "network": {
    "ipv4_enabled": true,
    "ipv4_address": "自动检测",
    "ipv6_enabled": false,
    "ipv6_address": "不可用",
    "port": 5000,
    "announce_addresses": []
  },
  "services": {
    "ping": {
      "enabled": true,
      "interval_secs": 15,
      "with_timeout": 10
    },
    "kademlia": {
      "enabled": true,
      "record_ttl_seconds": 3600,
      "replication_factor": 20,
      "query_timeout_seconds": 60,
      "bootstrap_nodes": []
    }
  }
}
```

### 命令行参数

```bash
# 指定端口
cargo run -- -p 5001

# 连接指定节点
cargo run -- -c /ip4/192.168.1.100/tcp/5000/p2p/12D3KooW...
```

## 协议标识

节点使用 `/OAHD` 协议标识。同版本节点间自动建立路由关系并加入 Kademlia DHT 路由表；外部节点仅保持基础连接，不加入核心路由。

## 开发计划

- [x] 核心 P2P 网络（Ping / Identify / Kademlia）
- [ ] 服务注册与发现协议
- [ ] 节点地址动态更新机制
- [ ] 冷存储服务接口
- [ ] OCR 服务接口
- [ ] Web API 管理界面

## 许可

Apache-2.0
