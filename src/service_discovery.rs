// 公共 API 方法：供外部调用，当前未使用
#![allow(dead_code)]

use libp2p::{
    PeerId,
    kad::{self, Record, RecordKey},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{LogLevel, LogStruct, config::ServiceDiscoveryConfig};

/// 服务类型
///
/// 用字符串标识，支持自注册。
/// 预定义常量示例。
pub const SERVICE_COLD_STORAGE: &str = "cold-storage";
pub const SERVICE_OCR: &str = "ocr";
pub const SERVICE_RELAY: &str = "relay";

/// DHT 中服务宣告记录的数据键前缀
pub const SD_KEY_PREFIX: &[u8] = b"/oahd/sd/";

/// 服务宣告信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// 服务类型标识
    pub service_type: String,
    /// 提供节点的 PeerId 字符串
    pub provider: String,
    /// 服务访问地址（Multiaddr 字符串），可选
    pub addrs: Vec<String>,
    /// 协议版本
    pub version: String,
    /// 附加元数据
    pub metadata: HashMap<String, String>,
    /// 宣告时间戳（Unix 秒）
    pub timestamp: u64,
    /// 存活时间（秒）
    pub ttl: u64,
}

impl ServiceInfo {
    /// 创建一个新的服务宣告
    pub fn new(
        service_type: &str,
        provider: PeerId,
        addrs: Vec<String>,
        metadata: HashMap<String, String>,
        ttl: u64,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            service_type: service_type.to_string(),
            provider: provider.to_string(),
            addrs,
            version: env!("CARGO_PKG_VERSION").to_string(),
            metadata,
            timestamp: now,
            ttl,
        }
    }

    /// 检查宣告是否已过期
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now > self.timestamp + self.ttl
    }

    /// 生成 DHT 记录键
    pub fn record_key(service_type: &str) -> RecordKey {
        let mut key = SD_KEY_PREFIX.to_vec();
        key.extend_from_slice(service_type.as_bytes());
        RecordKey::new(&key)
    }
}

/// 服务发现管理器
pub struct ServiceDiscovery {
    /// 配置
    config: ServiceDiscoveryConfig,
    /// 本地节点 PeerId
    local_peer_id: PeerId,
    /// 本地宣告的服务（service_type -> ServiceInfo）
    local_services: HashMap<String, ServiceInfo>,
    /// 从网络中缓存的服务（service_type -> Vec<ServiceInfo>）
    cached_services: HashMap<String, Vec<ServiceInfo>>,
}

impl ServiceDiscovery {
    /// 创建服务发现管理器
    pub fn new(config: &ServiceDiscoveryConfig, local_peer_id: PeerId) -> Self {
        let config = config.clone();
        let mut local_services = HashMap::new();

        // 为本地每个服务创建宣告
        for svc in &config.services {
            let info = ServiceInfo {
                service_type: svc.clone(),
                provider: local_peer_id.to_string(),
                addrs: Vec::new(), // 调用者后续填充
                version: env!("CARGO_PKG_VERSION").to_string(),
                metadata: HashMap::new(),
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                ttl: config.record_ttl_secs,
            };
            local_services.insert(svc.clone(), info);
        }

        ServiceDiscovery {
            local_peer_id,
            config,
            local_services,
            cached_services: HashMap::new(),
        }
    }

    /// 设置本地服务的访问地址（启动时调用）
    pub fn set_addresses(&mut self, addrs: &[String]) {
        for info in self.local_services.values_mut() {
            info.addrs = addrs.to_vec();
        }
    }

    /// 获取需要宣告到 DHT 的记录列表
    pub fn get_announce_records(&self) -> Vec<Record> {
        let mut records = Vec::new();
        for info in self.local_services.values() {
            if let Ok(bytes) = serde_json::to_vec(info) {
                let key = ServiceInfo::record_key(&info.service_type);
                records.push(Record {
                    key,
                    value: bytes,
                    publisher: None,
                    expires: None,
                });
            }
        }
        records
    }

    /// 请求查询网络中的特定服务
    ///
    /// 返回 `(RecordKey, query_id)`，调用者需要用 `kademlia.get_record(key)` 来触发实际查询。
    /// 返回的 key 可以用于后续匹配结果。
    pub fn query_service(&self, service_type: &str) -> Option<RecordKey> {
        if !self.config.enabled {
            return None;
        }
        Some(ServiceInfo::record_key(service_type))
    }

    /// 处理 Kademlia GetRecord 成功结果，尝试解码服务宣告
    pub fn handle_get_record_ok(&mut self, ok: &kad::GetRecordOk) {
        match ok {
            kad::GetRecordOk::FoundRecord(peer_record) => {
                let key_str = String::from_utf8_lossy(peer_record.record.key.as_ref());
                if !key_str.starts_with(std::str::from_utf8(SD_KEY_PREFIX).unwrap_or("")) {
                    return;
                }
                let svc_type = key_str
                    .strip_prefix(std::str::from_utf8(SD_KEY_PREFIX).unwrap_or(""))
                    .unwrap_or("")
                    .to_string();
                if let Ok(info) = serde_json::from_slice::<ServiceInfo>(&peer_record.record.value) {
                    if info.is_expired() {
                        return;
                    }
                    self.cached_services.entry(svc_type).or_default().push(info);
                }
            }
            kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. } => {}
        }
    }

    /// 获取缓存的某个服务的所有提供者
    pub fn get_cached_providers(&self, service_type: &str) -> Vec<&ServiceInfo> {
        self.cached_services
            .get(service_type)
            .map(|v| v.iter().filter(|info| !info.is_expired()).collect())
            .unwrap_or_default()
    }

    /// 获取所有缓存的非过期服务宣告
    pub fn get_all_cached(&self) -> Vec<&ServiceInfo> {
        let mut result = Vec::new();
        for infos in self.cached_services.values() {
            for info in infos {
                if !info.is_expired() {
                    result.push(info);
                }
            }
        }
        result
    }

    /// 本地是否有某服务
    pub fn has_local_service(&self, service_type: &str) -> bool {
        self.local_services.contains_key(service_type)
    }

    /// 列出本地宣告的所有服务
    pub fn list_local_services(&self) -> Vec<&ServiceInfo> {
        self.local_services.values().collect()
    }

    /// 是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// 移除过期缓存并返回移除数量
    pub fn purge_expired_cache(&mut self) -> usize {
        let mut total = 0;
        for infos in self.cached_services.values_mut() {
            let before = infos.len();
            infos.retain(|info| !info.is_expired());
            total += before - infos.len();
        }
        self.cached_services.retain(|_, v| !v.is_empty());
        total
    }

    /// 重新宣告所有本地服务（地址或配置变更后调用）
    /// 返回新的宣告记录列表
    pub fn reannounce(&mut self, addrs: &[String]) -> Vec<Record> {
        self.set_addresses(addrs);
        self.get_announce_records()
    }

    /// 新增本地服务（运行时动态注册）
    pub fn add_local_service(
        &mut self,
        service_type: &str,
        addrs: Vec<String>,
        metadata: HashMap<String, String>,
    ) -> Option<Record> {
        let info = ServiceInfo::new(
            service_type,
            self.local_peer_id,
            addrs,
            metadata,
            self.config.record_ttl_secs,
        );
        let key = ServiceInfo::record_key(service_type);
        log_info(
            "服务注册".to_string(),
            format!("注册服务: {}", service_type),
        );
        self.local_services
            .insert(service_type.to_string(), info.clone());
        if let Ok(bytes) = serde_json::to_vec(&info) {
            return Some(Record {
                key,
                value: bytes,
                publisher: None,
                expires: None,
            });
        }
        None
    }
}

fn log_info(topic: String, content: String) {
    let log = LogStruct {
        level: LogLevel::Debug,
        topic,
        content,
    };
    log.logout();
}
