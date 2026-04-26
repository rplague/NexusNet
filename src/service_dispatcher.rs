// 公共 API 方法：供外部调用，当前未使用
#![allow(dead_code)]

use crate::config::DispatcherConfig;
use crate::{LogLevel, LogStruct};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

/// 本地服务调度器
///
/// 架构：微内核 + 边车模式
/// 通信层（NexusNet P2P）对外接收请求，Dispatcher 根据 service=xxx 字段
/// 将请求转发到本地回环地址上的对应业务进程（如 OCR、冷存储）。
///
/// 业务进程是独立的进程，监听 localhost:port，语言无关。
pub struct ServiceDispatcher {
    config: DispatcherConfig,
    /// 服务名 → 后端地址的表
    routes: HashMap<String, String>,
}

impl ServiceDispatcher {
    pub fn new(config: &DispatcherConfig) -> Self {
        let mut routes = HashMap::new();
        for entry in &config.local_services {
            let addr = format!("{}:{}", entry.host, entry.port);
            routes.insert(entry.name.clone(), addr);
        }

        if config.enabled {
            let count = routes.len();
            log_info(
                "服务调度器".to_string(),
                format!("已加载 {} 个本地服务路由", count),
            );
            for (name, addr) in &routes {
                log_info("服务调度器".to_string(), format!("  {} -> {}", name, addr));
            }
        }

        ServiceDispatcher {
            config: config.clone(),
            routes,
        }
    }

    /// 根据服务名查询后端地址
    pub fn get_backend(&self, service_name: &str) -> Option<&str> {
        self.routes.get(service_name).map(|s| s.as_str())
    }

    /// 转发请求到后端服务并返回响应
    ///
    /// `request_body` 是已序列化的请求载荷（如 JSON）。
    /// 返回后端服务的响应字节。
    pub fn forward(&self, service_name: &str, request_body: &[u8]) -> Result<Vec<u8>, String> {
        let backend = self
            .get_backend(service_name)
            .ok_or_else(|| format!("未知服务: {}", service_name))?;

        log_info(
            "服务调度器".to_string(),
            format!("转发请求到 {} ({})", service_name, backend),
        );

        let mut stream =
            TcpStream::connect(backend).map_err(|e| format!("连接后端 {} 失败: {}", backend, e))?;

        stream
            .write_all(request_body)
            .map_err(|e| format!("写入请求到 {} 失败: {}", backend, e))?;
        stream
            .flush()
            .map_err(|e| format!("刷新写入到 {} 失败: {}", backend, e))?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|e| format!("读取 {} 响应失败: {}", backend, e))?;

        log_info(
            "服务调度器".to_string(),
            format!("收到 {} 响应 ({} 字节)", service_name, response.len()),
        );

        Ok(response)
    }

    /// 健康检查：尝试连接所有本地后端，返回不可达的服务列表
    pub fn health_check(&self) -> Vec<String> {
        let mut unhealthy = Vec::new();
        for (name, addr) in &self.routes {
            match TcpStream::connect_timeout(
                &addr.parse().unwrap(),
                std::time::Duration::from_secs(3),
            ) {
                Ok(_stream) => {
                    // TCP 连通即健康
                }
                Err(_) => {
                    unhealthy.push(format!("{} ({})", name, addr));
                }
            }
        }
        unhealthy
    }

    /// 获取所有已注册的服务名
    pub fn list_services(&self) -> Vec<&str> {
        self.routes.keys().map(|s| s.as_str()).collect()
    }

    /// 调度器是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
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
