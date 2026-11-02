use crate::auth::{AuthNetwork, is_valid_service_name};
use crate::config::NodeConfig;
use libp2p::multiaddr::Protocol;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const EXIT_USAGE: i32 = 2;
const EXIT_LEGACY: i32 = 3;
const EXIT_IO: i32 = 4;

#[derive(Debug)]
struct SetupError {
    code: i32,
    message: String,
}

impl SetupError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }

    fn legacy(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_LEGACY,
            message: message.into(),
        }
    }

    fn io(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_IO,
            message: message.into(),
        }
    }
}

type SetupResult<T> = Result<T, SetupError>;

pub fn run_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first()?.as_str();
    let rest = &args[1..];
    let result = match mode {
        "--write-config" => run_write_config(rest),
        "--defaults" => run_defaults(rest),
        "--check-config" => run_check(rest),
        _ => return None,
    };
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            println!("setup: {}", e.message);
            e.code
        }
    })
}

fn run_write_config(args: &[String]) -> SetupResult<()> {
    let (answers, config_dir, backup) = parse_write_config_args(args)?;
    let value = read_json(&answers)?;
    let config: NodeConfig = serde_json::from_value(value)
        .map_err(|e| SetupError::usage(format!("invalid answers JSON: {e}")))?;
    validate(&config, true)?;
    let path = write_config(&config_dir, &config, backup)?;
    println!("setup: wrote {}", path.display());
    Ok(())
}

fn run_defaults(args: &[String]) -> SetupResult<()> {
    let config_dir = parse_config_dir(args)?;
    let path = config_dir.join("config.toml");
    if path.exists() {
        println!("setup: keeping existing {}", path.display());
        return Ok(());
    }
    let config = NodeConfig::default();
    validate(&config, false)?;
    let path = write_config(&config_dir, &config, false)?;
    println!("setup: wrote defaults to {}", path.display());
    Ok(())
}

fn run_check(args: &[String]) -> SetupResult<()> {
    let config_dir = parse_config_dir(args)?;
    let path = config_dir.join("config.toml");
    if !path.exists() {
        println!("setup: no config at {}", path.display());
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| SetupError::io(format!("read {}: {e}", path.display())))?;
    let config: NodeConfig = toml::from_str(&content)
        .map_err(|e| SetupError::usage(format!("parse {}: {e}", path.display())))?;
    validate(&config, false)?;
    println!("setup: config ok");
    Ok(())
}

fn parse_write_config_args(args: &[String]) -> SetupResult<(PathBuf, PathBuf, bool)> {
    let answers = args
        .first()
        .filter(|s| !s.starts_with("--"))
        .map(PathBuf::from)
        .ok_or_else(|| SetupError::usage("--write-config requires a JSON file"))?;
    let mut config_dir = None;
    let mut backup = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config-dir" => {
                i += 1;
                let path = args
                    .get(i)
                    .ok_or_else(|| SetupError::usage("--config-dir requires a path"))?;
                config_dir = Some(PathBuf::from(path));
            }
            "--backup" => backup = true,
            other => return Err(SetupError::usage(format!("unknown argument: {other}"))),
        }
        i += 1;
    }
    Ok((
        answers,
        config_dir.unwrap_or_else(default_config_dir),
        backup,
    ))
}

fn parse_config_dir(args: &[String]) -> SetupResult<PathBuf> {
    let mut config_dir = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config-dir" => {
                i += 1;
                let path = args
                    .get(i)
                    .ok_or_else(|| SetupError::usage("--config-dir requires a path"))?;
                config_dir = Some(PathBuf::from(path));
            }
            other => return Err(SetupError::usage(format!("unknown argument: {other}"))),
        }
        i += 1;
    }
    Ok(config_dir.unwrap_or_else(default_config_dir))
}

fn default_config_dir() -> PathBuf {
    std::env::var_os("NEXUSNET_CONFIG_PATH")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_json(path: &Path) -> SetupResult<Value> {
    let text = read_text(path)?;
    serde_json::from_str(&text)
        .map_err(|e| SetupError::usage(format!("parse {}: {e}", path.display())))
}

fn read_text(path: &Path) -> SetupResult<String> {
    let bytes =
        fs::read(path).map_err(|e| SetupError::io(format!("read {}: {e}", path.display())))?;
    if bytes.starts_with(&[0xff, 0xfe]) {
        if (bytes.len() - 2) % 2 != 0 {
            return Err(SetupError::usage(format!(
                "invalid UTF-16LE file: {}",
                path.display()
            )));
        }
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16(&units)
            .map_err(|e| SetupError::usage(format!("decode {}: {e}", path.display())))
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        String::from_utf8(bytes[3..].to_vec())
            .map_err(|e| SetupError::usage(format!("decode {}: {e}", path.display())))
    } else {
        String::from_utf8(bytes)
            .map_err(|e| SetupError::usage(format!("decode {}: {e}", path.display())))
    }
}

fn validate(config: &NodeConfig, setup_mode: bool) -> SetupResult<()> {
    config
        .validate()
        .map_err(|e| SetupError::usage(format!("invalid config: {e}")))?;

    if config.network.port == 0 {
        return Err(SetupError::usage("network.port must not be zero"));
    }

    for addr in &config.services.kademlia.bootstrap_nodes {
        let has_ws = addr.iter().any(|p| matches!(p, Protocol::Ws(_)));
        let has_peer = addr.iter().any(|p| matches!(p, Protocol::P2p(_)));
        if !has_ws {
            let message = format!("bootstrap address must include /ws: {addr}");
            return if setup_mode {
                Err(SetupError::usage(message))
            } else {
                Err(SetupError::legacy(message))
            };
        }
        if !has_peer {
            return Err(SetupError::usage(format!(
                "bootstrap address must include /p2p/<PeerId>: {addr}"
            )));
        }
    }

    if setup_mode && config.auth.networks.len() > 1 {
        return Err(SetupError::usage("setup supports at most one auth network"));
    }
    if setup_mode && config.auth.network == "none" && !config.auth.networks.is_empty() {
        return Err(SetupError::usage(
            "auth.network is none but auth.networks is not empty",
        ));
    }

    for (name, network) in &config.auth.networks {
        AuthNetwork::new(name, &network.authority)
            .map_err(|e| SetupError::usage(format!("invalid auth network {name}: {e}")))?;
    }

    let auth_enabled = config.auth.network != "none";
    if auth_enabled && !config.auth.networks.contains_key(&config.auth.network) {
        return Err(SetupError::usage(format!(
            "missing authority for auth network {}",
            config.auth.network
        )));
    }

    for service in &config.services.dispatcher.local_services {
        if service.name.is_empty() {
            return Err(SetupError::usage("local service name must not be empty"));
        }
        if service.require_auth {
            if !auth_enabled {
                return Err(SetupError::usage(format!(
                    "service {} requires auth but auth.network is none",
                    service.name
                )));
            }
            if !is_valid_service_name(&service.name) {
                return Err(SetupError::usage(format!(
                    "invalid auth service name: {}",
                    service.name
                )));
            }
        }
    }

    Ok(())
}

fn write_config(config_dir: &Path, config: &NodeConfig, backup: bool) -> SetupResult<PathBuf> {
    fs::create_dir_all(config_dir)
        .map_err(|e| SetupError::io(format!("create {}: {e}", config_dir.display())))?;
    let path = config_dir.join("config.toml");
    if backup && path.exists() {
        let backup_path = backup_path(&path);
        fs::copy(&path, &backup_path).map_err(|e| {
            SetupError::io(format!(
                "backup {} to {}: {e}",
                path.display(),
                backup_path.display()
            ))
        })?;
        println!("setup: backup {}", backup_path.display());
    }
    let text = toml::to_string_pretty(config)
        .map_err(|e| SetupError::usage(format!("serialize config: {e}")))?;
    let tmp = config_dir.join("config.toml.tmp");
    fs::write(&tmp, text).map_err(|e| SetupError::io(format!("write {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
    }
    if let Err(first) = fs::rename(&tmp, &path) {
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| SetupError::io(format!("remove {}: {e}", path.display())))?;
            fs::rename(&tmp, &path)
                .map_err(|e| SetupError::io(format!("rename to {}: {e}", path.display())))?;
        } else {
            return Err(SetupError::io(format!(
                "rename to {}: {first}",
                path.display()
            )));
        }
    }
    Ok(path)
}

fn backup_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    for counter in 0..1000 {
        let candidate = if counter == 0 {
            path.with_file_name(format!("{name}.bak.{stamp}"))
        } else {
            path.with_file_name(format!("{name}.bak.{stamp}.{counter}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    path.with_file_name(format!("{name}.bak.{stamp}.overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nexusnet-setup-{}-{}-{}",
            std::process::id(),
            counter,
            name
        ))
    }

    fn peer() -> String {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_string()
    }

    fn write(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
    }

    #[test]
    fn defaults_writes_valid_config() {
        let dir = temp_dir("defaults");
        let _ = fs::remove_dir_all(&dir);
        run_defaults(&["--config-dir".into(), dir.display().to_string()]).unwrap();
        let text = fs::read_to_string(dir.join("config.toml")).unwrap();
        let config: NodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.network.port, 5000);
        assert!(!config.network.ipv4_enabled);
        assert!(!config.network.ipv6_enabled);
        assert!(config.crypto.pq_transport_enabled);
        assert!(config.crypto.pq_identity_enabled);
        assert_eq!(config.node.name, "未设置的p2p节点");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_config_uses_defaults_for_missing_fields() {
        let dir = temp_dir("setup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let answers = dir.join("answers.json");
        let out = dir.join("config");
        let bootstrap = format!("/ip4/127.0.0.1/tcp/5000/ws/p2p/{}", peer());
        write(
            &answers,
            &format!(
                r#"{{"node":{{"name":"win-node"}},"network":{{"port":6000}},"services":{{"ping":{{"enabled":false}},"kademlia":{{"bootstrap_nodes":["{bootstrap}"]}}}}}}"#
            ),
        );
        run_write_config(&[
            answers.display().to_string(),
            "--config-dir".into(),
            out.display().to_string(),
        ])
        .unwrap();
        let config: NodeConfig =
            toml::from_str(&fs::read_to_string(out.join("config.toml")).unwrap()).unwrap();
        assert_eq!(config.node.name, "win-node");
        assert_eq!(config.network.port, 6000);
        assert!(!config.services.ping.enabled);
        assert_eq!(config.services.ping.interval_secs, 15);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_rejects_raw_bootstrap() {
        let dir = temp_dir("raw");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let basic = dir.join("basic.json");
        let out = dir.join("config");
        let bootstrap = format!("/ip4/127.0.0.1/tcp/5000/p2p/{}", peer());
        write(
            &basic,
            &format!(r#"{{"services":{{"kademlia":{{"bootstrap_nodes":["{bootstrap}"]}}}}}}"#),
        );
        let err = run_write_config(&[
            basic.display().to_string(),
            "--config-dir".into(),
            out.display().to_string(),
        ])
        .unwrap_err();
        assert_eq!(err.code, EXIT_USAGE);
        assert!(err.message.contains("/ws"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_detects_legacy_bootstrap() {
        let dir = temp_dir("check");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = NodeConfig::default();
        let addr: libp2p::Multiaddr = format!("/ip4/127.0.0.1/tcp/5000/p2p/{}", peer())
            .parse()
            .unwrap();
        config.services.kademlia.bootstrap_nodes = vec![addr];
        write(
            &dir.join("config.toml"),
            &toml::to_string_pretty(&config).unwrap(),
        );
        let err = run_check(&["--config-dir".into(), dir.display().to_string()]).unwrap_err();
        assert_eq!(err.code, EXIT_LEGACY);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_creates_unique_file() {
        let dir = temp_dir("backup");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write(&dir.join("config.toml"), "old");
        let answers = dir.join("answers.json");
        write(&answers, r#"{"node":{"name":"backup"}}"#);
        run_write_config(&[
            answers.display().to_string(),
            "--config-dir".into(),
            dir.display().to_string(),
            "--backup".into(),
        ])
        .unwrap();
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
            .collect();
        assert_eq!(backups.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn example_setup_json_is_valid() {
        let value: Value = serde_json::from_str(include_str!("../deploy/win/setup.example.json"))
            .expect("example setup JSON");
        let config: NodeConfig = serde_json::from_value(value).expect("example NodeConfig");
        validate(&config, true).unwrap();
    }

    #[test]
    fn reads_utf16le() {
        let dir = temp_dir("utf16");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("setup.json");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&[0xff, 0xfe]).unwrap();
        for unit in r#"{"node":{"name":"中文"}}"#.encode_utf16() {
            file.write_all(&unit.to_le_bytes()).unwrap();
        }
        let text = read_text(&path).unwrap();
        assert!(text.contains("中文"));
        let _ = fs::remove_dir_all(&dir);
    }
}
