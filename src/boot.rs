use std::env;

use crate::{
	LogLevel,
	LogStruct,
	config::{
		NodeConfig,
		read_config_file
	}
};

pub fn init() -> (NodeConfig, Vec<String>, u16) {
    let config = match read_config_file() {
        Ok(cfg) => cfg,
        Err(e) => {
            let log = LogStruct {
                level: LogLevel::Critical,
                topic: "无法读取设置文件".to_string(),
                content: format!("{}", e),
            };
            log.logout();
            std::process::exit(1);
        }
    };

    let mut connect_list: Vec<String> = if config.services.kademlia.enabled {
        config.services.kademlia.bootstrap_nodes.clone()
    } else {
        Vec::new()
    };

    let mut port = config.network.port;

    let args: Vec<String> = env::args().collect();
    parse_cli_args(&args, &mut port, &mut connect_list);

    (config, connect_list, port)
}

/// 解析命令行参数并修改 port 和 connect_list
fn parse_cli_args(args: &[String], port: &mut u16, connect_list: &mut Vec<String>) {
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" if i + 1 < args.len() => {
                *port = match args[i + 1].parse() {
                    Ok(arg) => arg,
                    Err(_) => {
                        let log = LogStruct {
                            level: LogLevel::Warning,
                            topic: "-p 参数输入错误".to_string(),
                            content: "使用默认值 5000".to_string(),
                        };
                        log.logout();
                        5000
                    }
                };
                i += 1; // 跳过参数值
            }
            "-c" if i + 1 < args.len() => {
                connect_list.push(args[i + 1].clone());
                i += 1;
            }
            "--connect-overwrite" if i + 1 < args.len() => {
                connect_list.clear();
                connect_list.push(args[i + 1].clone());
                i += 1;
            }
            unknown_arg => {
                let log = LogStruct {
                    level: LogLevel::Critical,
                    topic: "错误的参数输入".to_string(),
                    content: format!("{} 参数是无法识别的", unknown_arg),
                };
                log.logout();
                std::process::exit(1);
            }
        }
        i += 1;
    }
}