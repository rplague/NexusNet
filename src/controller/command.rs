// NexusNet - OAHD 计划的核心网络层
//
// Copyright (C) 2026 OAHD
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::time::{Duration, Instant};

use libp2p::{PeerId, kad};

use crate::auth;
use crate::controller::service_registry::ServiceCommand;
use crate::log::{LogLevel, LogStruct};
use crate::network::NetworkHandle;
use crate::service_dispatcher::{ControlCommand, ToController};
use crate::service_protocol;
use crate::sidecar_protocol::Message;

use super::NodeController;
use super::peer_tracker::PeerTracker;
use super::relay_manager::RELAY_TARGET;

/// `whoami` result payload.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct WhoamiResult {
    peer_id: String,
    name: String,
    description: String,
}

/// `query_key` result payload.
#[derive(serde::Serialize)]
struct QueryKeyResult {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    providers: Vec<String>,
}

/// Serialize a result to CBOR.
fn to_cbor<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).expect("CBOR 编码失败");
    buf
}

fn response_payload(resp: service_protocol::Response) -> Result<Vec<u8>, String> {
    if resp.success {
        Ok(resp.data)
    } else if resp.data.is_empty() {
        Err("remote service failed".to_string())
    } else {
        Err(String::from_utf8_lossy(&resp.data).into_owned())
    }
}

fn is_writable_key(key: &str) -> bool {
    if key == "/oahd/service/types" || key == "/oahd/bootstrap" {
        return false;
    }
    key.starts_with("/oahd/auth/") || key.starts_with("/oahd/service/")
}

fn command_name(msg: &Message) -> &'static str {
    match msg {
        Message::Hello { .. } => "hello",
        Message::Request { .. } => "request",
        Message::Reply { .. } => "reply",
        Message::ListServices { .. } => "list_services",
        Message::DiscoverProviders { .. } => "discover_providers",
        Message::QueryPublicIp { .. } => "query_public_ip",
        Message::Whoami { .. } => "whoami",
        Message::ReconnectBootstrap { .. } => "reconnect_bootstrap",
        Message::ReannounceServices { .. } => "reannounce_services",
        Message::ReloadConfig { .. } => "reload_config",
        Message::RefreshCache { .. } => "refresh_cache",
        Message::RelayStatus { .. } => "relay_status",
        Message::PqStatus { .. } => "pq_status",
        Message::AuthStatus { .. } => "auth_status",
        Message::QueryKey { .. } => "query_key",
        Message::AddKey { .. } => "add_key",
        Message::RemoveKey { .. } => "remove_key",
        Message::ServiceRequest { .. } => "service_request",
        Message::ServiceRequestTo { .. } => "service_request_to",
    }
}

fn can_run_in_dht_worker(msg: &Message) -> bool {
    match msg {
        Message::QueryKey { .. }
        | Message::DiscoverProviders { .. }
        | Message::ServiceRequest { .. }
        | Message::ServiceRequestTo { .. } => true,
        Message::AddKey { providing, .. } => !*providing,
        _ => false,
    }
}

fn log_command(kind: &str, elapsed: Duration) {
    let level = if elapsed >= Duration::from_secs(1) {
        LogLevel::Warning
    } else {
        LogLevel::Debug
    };
    LogStruct::new(
        level,
        "控制命令",
        format!("{kind} {}ms", elapsed.as_millis()),
    )
    .emit();
}

fn provider_strings(providers: Vec<PeerId>) -> Vec<String> {
    providers.into_iter().map(|peer| peer.to_string()).collect()
}

async fn run_dht_command(
    swarm: &NetworkHandle,
    peers: &PeerTracker,
    msg: Message,
) -> Result<Vec<u8>, String> {
    match msg {
        Message::DiscoverProviders { service, .. } => {
            let key = kad::RecordKey::new(&format!("/oahd/service/{service}"));
            match swarm.get_providers(key).await {
                Ok(providers) => Ok(to_cbor(&provider_strings(providers))),
                Err(e) => Err(format!("{e:?}")),
            }
        }
        Message::QueryKey { key, .. } => {
            if key.is_empty() {
                return Err("missing key".to_string());
            }
            if let Ok(kad::GetRecordOk::FoundRecord(peer_record)) =
                swarm.get_record(kad::RecordKey::new(&key)).await
            {
                let value = peer_record.record.value.clone();
                let providers = swarm
                    .get_providers(kad::RecordKey::new(&key))
                    .await
                    .unwrap_or_default();
                return Ok(to_cbor(&QueryKeyResult {
                    key,
                    value: Some(value),
                    providers: provider_strings(providers),
                }));
            }

            let providers = swarm
                .get_providers(kad::RecordKey::new(&key))
                .await
                .unwrap_or_default();
            if providers.is_empty() {
                Err("key not found".to_string())
            } else {
                Ok(to_cbor(&QueryKeyResult {
                    key,
                    value: None,
                    providers: provider_strings(providers),
                }))
            }
        }
        Message::ServiceRequest {
            service, payload, ..
        } => {
            let key = kad::RecordKey::new(&format!("/oahd/service/{service}"));
            let providers = swarm
                .get_providers(key)
                .await
                .map_err(|e| format!("{e:?}"))?;
            if providers.is_empty() {
                return Err("No provider found".to_string());
            }
            let best = peers
                .best_peer(&providers)
                .or_else(|| providers.first().copied())
                .expect("providers is non-empty");
            let response = swarm
                .send_request(&best, service_protocol::Request { service, payload })
                .await
                .map_err(|e| e.to_string())?;
            response_payload(response)
        }
        Message::ServiceRequestTo {
            service,
            peer,
            payload,
            ..
        } => {
            let peer_id = peer
                .parse::<PeerId>()
                .map_err(|e| format!("invalid peer id: {e}"))?;
            let response = swarm
                .send_request(&peer_id, service_protocol::Request { service, payload })
                .await
                .map_err(|e| e.to_string())?;
            response_payload(response)
        }
        Message::AddKey {
            key,
            value,
            providing,
            ..
        } => {
            if providing {
                return Err("async add_key does not support providing".to_string());
            }
            if key.is_empty() {
                return Err("missing 'key' field".to_string());
            }
            if !is_writable_key(&key) {
                return Err("key namespace not allowed".to_string());
            }
            let Some(bytes) = value else {
                return Err("at least one of 'value' or 'providing' is required".to_string());
            };
            swarm
                .put_record(kad::RecordKey::new(&key), bytes)
                .await
                .map_err(|e| format!("put_record failed: {e}"))?;
            Ok(to_cbor(&serde_json::json!({"success": true, "key": key})))
        }
        _ => Err("command not supported by DHT worker".to_string()),
    }
}

impl NodeController {
    /// Handle a message arriving from the ServiceDispatcher.
    pub(super) async fn handle_controller_message(&mut self, msg: ToController) {
        match msg {
            ToController::Control(cmd) => self.dispatch_command(cmd).await,
        }
    }

    async fn dispatch_command(&mut self, req: ControlCommand) {
        let ControlCommand { msg, resp_tx } = req;
        let kind = command_name(&msg);
        let started = Instant::now();

        if can_run_in_dht_worker(&msg) {
            let permit = match self.dht_permits.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    LogStruct::new(LogLevel::Warning, "控制命令繁忙", kind.to_string()).emit();
                    let _ = resp_tx.send(Err("node busy: too many DHT commands".to_string()));
                    return;
                }
            };
            let swarm = self.swarm.clone();
            let peers = self.peers.clone();
            tokio::spawn(async move {
                let result = run_dht_command(&swarm, &peers, msg).await;
                log_command(kind, started.elapsed());
                let _ = resp_tx.send(result);
                drop(permit);
            });
        } else {
            let result = self.run_command(msg).await;
            log_command(kind, started.elapsed());
            let _ = resp_tx
                .send(result.unwrap_or_else(|| Err("command produced no result".to_string())));
        }
    }

    /// Route one backend control command to its handler.
    async fn run_command(&mut self, msg: Message) -> Option<Result<Vec<u8>, String>> {
        match msg {
            Message::ListServices { .. } => Some(self.cmd_list_services().await),
            Message::DiscoverProviders { service, .. } => {
                Some(self.cmd_discover_providers(&service).await)
            }
            Message::QueryPublicIp { .. } => Some(self.cmd_query_public_ip()),
            Message::Whoami { .. } => Some(self.cmd_whoami()),
            Message::ReconnectBootstrap { .. } => Some(self.cmd_reconnect_bootstrap().await),
            Message::ReannounceServices { .. } => Some(self.cmd_reannounce_services().await),
            Message::ReloadConfig { .. } => Some(self.cmd_reload_config().await),
            Message::RefreshCache { .. } => Some(self.cmd_refresh_cache().await),
            Message::RelayStatus { .. } => Some(self.cmd_relay_status()),
            Message::PqStatus { .. } => Some(self.cmd_pq_status()),
            Message::AuthStatus { .. } => Some(self.cmd_auth_status()),
            Message::AddKey {
                key,
                value,
                providing,
                ..
            } => Some(self.cmd_add_key(key, value, providing).await),
            Message::RemoveKey {
                key,
                providing,
                record,
                ..
            } => Some(self.cmd_remove_key(key, providing, record).await),
            Message::QueryKey { key, .. } => Some(self.cmd_query_key(key).await),
            Message::ServiceRequest {
                service, payload, ..
            } => Some(self.cmd_service_request(service, payload).await),
            Message::ServiceRequestTo {
                service,
                peer,
                payload,
                ..
            } => Some(self.cmd_service_request_to(service, peer, payload).await),
            _ => Some(Err("command not supported".to_string())),
        }
    }

    async fn cmd_list_services(&mut self) -> Result<Vec<u8>, String> {
        let types = self.services_snapshot.borrow().types.clone();
        let _ = self.services_tx.try_send(ServiceCommand::RefreshTypes);
        Ok(to_cbor(&types))
    }

    async fn cmd_discover_providers(&self, service: &str) -> Result<Vec<u8>, String> {
        let key = kad::RecordKey::new(&format!("/oahd/service/{}", service));
        match self.swarm.get_providers(key).await {
            Ok(providers) => {
                let providers: Vec<String> = providers.into_iter().map(|p| p.to_string()).collect();
                Ok(to_cbor(&providers))
            }
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn cmd_query_public_ip(&self) -> Result<Vec<u8>, String> {
        let network = &self.config.read().network;
        let ip_info = serde_json::json!({
            "ipv4": network.ipv4_address.map(|a| a.to_string()),
            "ipv6": network.ipv6_address.map(|a| a.to_string()),
        });
        Ok(to_cbor(&ip_info))
    }

    fn cmd_whoami(&self) -> Result<Vec<u8>, String> {
        let node = self.config.read().node.clone();
        Ok(to_cbor(&WhoamiResult {
            peer_id: self.my_peer_id.to_string(),
            name: node.name,
            description: node.description,
        }))
    }

    async fn cmd_reconnect_bootstrap(&self) -> Result<Vec<u8>, String> {
        let mut any_success = false;
        for addr in self.config.bootstrap_nodes() {
            if self.swarm.dial(addr).await.is_ok() {
                any_success = true;
            }
        }
        Ok(to_cbor(&serde_json::json!({ "success": any_success })))
    }

    async fn cmd_reannounce_services(&mut self) -> Result<Vec<u8>, String> {
        self.services_tx
            .try_send(ServiceCommand::Reannounce)
            .map_err(|_| "registry busy".to_string())?;
        Ok(to_cbor(&serde_json::json!({"success": true})))
    }

    async fn cmd_reload_config(&mut self) -> Result<Vec<u8>, String> {
        // Rebuild the Swarm from the current config.toml. Note: it drops
        // connections and clears the DHT local store.
        let result = match self.swarm.reload().await {
            Ok(_) => {
                self.relay.reset();
                self.bootstrap.reset();
                self.send_service(ServiceCommand::Reset).await;
                self.auth.reload();
                serde_json::json!({"success": true})
            }
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}),
        };
        Ok(to_cbor(&result))
    }

    async fn cmd_refresh_cache(&self) -> Result<Vec<u8>, String> {
        let result = match self.swarm.refresh_record_cache().await {
            Ok(cleared) => serde_json::json!({"success": true, "cleared": cleared}),
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}),
        };
        Ok(to_cbor(&result))
    }

    fn cmd_relay_status(&self) -> Result<Vec<u8>, String> {
        let result = serde_json::json!({
            "need_relay": self.relay.need_relay(),
            "target": RELAY_TARGET,
            "active": self.relay.active(),
            "pending": self.relay.pending(),
        });
        Ok(to_cbor(&result))
    }

    fn cmd_pq_status(&self) -> Result<Vec<u8>, String> {
        let result = serde_json::json!({
            "enabled": self.config.pq_enabled(),
            "transport": self.config.pq_transport_enabled(),
            "identity": self.config.pq_identity_enabled(),
            "required": self.config.pq_required(),
        });
        Ok(to_cbor(&result))
    }

    fn cmd_auth_status(&self) -> Result<Vec<u8>, String> {
        let network = self.config.auth_network();
        let (state, detail) = self.auth.state_label();
        let local_required: Vec<String> = self
            .config
            .auth_required_services()
            .into_iter()
            .filter(|n| auth::is_valid_service_name(n))
            .collect();

        let mut report = serde_json::json!({
            "network": network,
            "state": state,
            "local_required": local_required.clone(),
        });
        if let Some(d) = detail {
            report["detail"] = serde_json::json!(d);
        }
        if let Some(v) = self.auth.index_version() {
            report["index_version"] = serde_json::json!(v);
        }
        let mut whitelist_versions = serde_json::Map::new();
        for name in &local_required {
            if let Some(v) = self.auth.whitelist_version(name) {
                whitelist_versions.insert(name.clone(), serde_json::json!(v));
            }
        }
        report["whitelist_versions"] = serde_json::Value::Object(whitelist_versions);

        Ok(to_cbor(&report))
    }

    async fn cmd_add_key(
        &mut self,
        key: String,
        value: Option<Vec<u8>>,
        providing: bool,
    ) -> Result<Vec<u8>, String> {
        if key.is_empty() {
            return Err("missing 'key' field".to_string());
        }
        if !is_writable_key(&key) {
            return Err("key namespace not allowed".to_string());
        }
        let ack = || to_cbor(&serde_json::json!({"success": true, "key": key.clone()}));
        match value {
            Some(bytes) => {
                self.swarm
                    .put_record(kad::RecordKey::new(&key), bytes)
                    .await
                    .map_err(|e| format!("put_record failed: {e}"))?;
                if providing {
                    self.start_providing_record(&key).await?;
                }
                Ok(ack())
            }
            None if providing => {
                self.start_providing_record(&key).await?;
                Ok(ack())
            }
            None => Err("at least one of 'value' or 'providing' is required".to_string()),
        }
    }

    /// Revoke a provider announcement and/or remove a locally stored record.
    ///
    /// `providing` stops this node announcing as a provider for `key`
    /// (`stop_providing`); `record` removes the locally stored value record (only
    /// if this node is its publisher). Both are local operations: remote provider
    /// records and value copies expire on their own TTLs.
    async fn cmd_remove_key(
        &mut self,
        key: String,
        providing: bool,
        record: bool,
    ) -> Result<Vec<u8>, String> {
        if key.is_empty() {
            return Err("missing 'key' field".to_string());
        }
        if !is_writable_key(&key) {
            return Err("key namespace not allowed".to_string());
        }
        if !providing && !record {
            return Err("at least one of 'providing' or 'record' is required".to_string());
        }
        if providing {
            self.swarm
                .stop_providing(kad::RecordKey::new(&key))
                .await
                .map_err(|e| format!("stop_providing failed: {e}"))?;
        }
        if record {
            self.swarm
                .remove_record(kad::RecordKey::new(&key))
                .await
                .map_err(|e| format!("remove_record failed: {e}"))?;
        }
        Ok(to_cbor(&serde_json::json!({
            "success": true,
            "key": key,
            "providing": providing,
            "record": record,
        })))
    }

    async fn cmd_query_key(&self, key: String) -> Result<Vec<u8>, String> {
        if key.is_empty() {
            return Err("missing key".to_string());
        }
        if let Ok(kad::GetRecordOk::FoundRecord(peer_record)) =
            self.swarm.get_record(kad::RecordKey::new(&key)).await
        {
            let value = peer_record.record.value.clone();
            let providers = self
                .swarm
                .get_providers(kad::RecordKey::new(&key))
                .await
                .unwrap_or_default();
            return Ok(to_cbor(&QueryKeyResult {
                key,
                value: Some(value),
                providers: providers.into_iter().map(|p| p.to_string()).collect(),
            }));
        }

        let providers = self
            .swarm
            .get_providers(kad::RecordKey::new(&key))
            .await
            .unwrap_or_default();
        if providers.is_empty() {
            Err("key not found".to_string())
        } else {
            Ok(to_cbor(&QueryKeyResult {
                key,
                value: None,
                providers: providers.into_iter().map(|p| p.to_string()).collect(),
            }))
        }
    }

    async fn cmd_service_request(
        &mut self,
        service: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        let key = kad::RecordKey::new(&format!("/oahd/service/{}", service));
        let providers = self
            .swarm
            .get_providers(key)
            .await
            .map_err(|e| format!("{e:?}"))?;
        if providers.is_empty() {
            return Err("No provider found".to_string());
        }
        let best = self
            .peers
            .best_peer(&providers)
            .or_else(|| providers.first().copied())
            .expect("providers is non-empty");
        let request = service_protocol::Request { service, payload };
        let response = self
            .swarm
            .send_request(&best, request)
            .await
            .map_err(|e| e.to_string())?;
        response_payload(response)
    }

    async fn cmd_service_request_to(
        &mut self,
        service: String,
        peer: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        let peer_id = peer
            .parse::<PeerId>()
            .map_err(|e| format!("invalid peer id: {e}"))?;
        let request = service_protocol::Request { service, payload };
        let response = self
            .swarm
            .send_request(&peer_id, request)
            .await
            .map_err(|e| e.to_string())?;
        response_payload(response)
    }

    /// Announce providing a key; returns an error string.
    async fn start_providing_record(&self, key_str: &str) -> Result<(), String> {
        self.swarm
            .start_providing(kad::RecordKey::new(&key_str))
            .await
            .map(|_| ())
            .map_err(|e| format!("start_providing failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whoami_result_round_trip() {
        let result = WhoamiResult {
            peer_id: "12D3KooW".to_string(),
            name: "节点A".to_string(),
            description: "测试节点".to_string(),
        };
        let bytes = to_cbor(&result);
        let decoded: WhoamiResult = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn response_payload_accepts_success() {
        let resp = service_protocol::Response {
            success: true,
            data: b"ok".to_vec(),
        };
        assert_eq!(response_payload(resp), Ok(b"ok".to_vec()));
    }

    #[test]
    fn response_payload_maps_remote_failure_to_err() {
        let resp = service_protocol::Response {
            success: false,
            data: b"unauthorized: nope".to_vec(),
        };
        assert_eq!(
            response_payload(resp),
            Err("unauthorized: nope".to_string())
        );
    }

    #[test]
    fn response_payload_handles_empty_remote_failure() {
        let resp = service_protocol::Response {
            success: false,
            data: Vec::new(),
        };
        assert_eq!(
            response_payload(resp),
            Err("remote service failed".to_string())
        );
    }

    #[test]
    fn dht_worker_classification() {
        let id = uuid::Uuid::new_v4();
        assert!(can_run_in_dht_worker(&Message::QueryKey {
            id,
            key: "/k".into()
        }));
        assert!(can_run_in_dht_worker(&Message::DiscoverProviders {
            id,
            service: "cmd".into()
        }));
        assert!(can_run_in_dht_worker(&Message::ServiceRequest {
            id,
            service: "cmd".into(),
            payload: vec![],
        }));
        assert!(can_run_in_dht_worker(&Message::ServiceRequestTo {
            id,
            service: "cmd".into(),
            peer: "12D3KooW".into(),
            payload: vec![],
        }));
        assert!(can_run_in_dht_worker(&Message::AddKey {
            id,
            key: "/oahd/auth/x".into(),
            value: Some(vec![]),
            providing: false,
        }));
        assert!(!can_run_in_dht_worker(&Message::AddKey {
            id,
            key: "/oahd/auth/x".into(),
            value: Some(vec![]),
            providing: true,
        }));
        assert!(!can_run_in_dht_worker(&Message::Whoami { id }));
        assert!(!can_run_in_dht_worker(&Message::ListServices { id }));
    }

    #[test]
    fn writable_key_allows_service_and_auth_namespaces() {
        assert!(is_writable_key("/oahd/service/cmd"));
        assert!(is_writable_key("/oahd/auth/abc/service"));
    }

    #[test]
    fn writable_key_rejects_reserved_and_foreign_keys() {
        assert!(!is_writable_key("/oahd/service/types"));
        assert!(!is_writable_key("/oahd/bootstrap"));
        assert!(!is_writable_key("/oahd/other"));
        assert!(!is_writable_key("/custom/key"));
    }
}
