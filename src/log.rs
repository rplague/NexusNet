use std::fs;
use std::io::{Read, Write};
use std::thread::spawn;
use std::sync::Mutex;
use chrono::{DateTime, Local};
use flate2::Compression;
use flate2::write::GzEncoder;
use colored::*;

static LOG_FILE_MUTEX: Mutex<()> = Mutex::new(());

pub enum LogLevel {
    Important,
    Debug,
    Preset,
    Warning,
    Error,
    Critical,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Important => "[IMPORTANT]",
            LogLevel::Debug => "[+]",
            LogLevel::Preset => "[-]",
            LogLevel::Warning => "[*]",
            LogLevel::Error => "[!]",
            LogLevel::Critical => "[CRITICAL]",
        }
    }
    fn color(&self) -> ColoredString {
        match self {
            LogLevel::Important => "[IMPORTANT]".on_green().bold(),
            LogLevel::Debug => "[+]".cyan(),
            LogLevel::Preset => "[-]".into(),
            LogLevel::Warning => "[*]".yellow(),
            LogLevel::Error => "[!]".red(),
            LogLevel::Critical => "[CRITICAL]".on_red().bold().blink(),
        }
    }
    
}

pub struct LogStruct {
    pub level: LogLevel,
    pub topic: String,
    pub content: String,
}

impl LogStruct {
    pub fn logout(&self){
        rolling_check();
        log(self)
    }
}

fn rolling_check() {
    const LOG_FILE: &str = "log";
    const MAX_SIZE: u64 = 10 * 1024 * 1024; // 10MB
    // 检查文件是否存在并获取大小
    let metadata = match fs::metadata(LOG_FILE) {
        Ok(metadata) => metadata,
        Err(_) => return, // 文件不存在，直接返回
    };
    // 检查文件大小
    if metadata.len() <= MAX_SIZE {
        return;
    }
    spawn(|| {
        const LOG_FILE: &str = "log";
        const TMP_FILE: &str = "log.tmp";
        // 检查文件是否存在并获取元数据
        let metadata = match fs::metadata(LOG_FILE) {
            Ok(metadata) => metadata,
            Err(_) => return, // 文件不存在，直接返回
        };
        // 重命名文件
        if let Err(e) = fs::rename(LOG_FILE, TMP_FILE) {
            panic!("[CRITICAL] 无法重命名log文件: {}", e);
        }

        // 生成时间戳
        let timestamp = metadata.created()
            .map(|created_time| {
                let datetime: DateTime<Local> = DateTime::from(created_time);
                datetime.format("%m%d_%H%M").to_string()
            })
            .unwrap_or_else(|_| "XXXX_XXXX".to_string());

        let current_time = Local::now().format("%m%d_%H%M").to_string();
        let output_filename = format!("{}-{}.gz", timestamp, current_time);

        // 读取文件内容
        let mut buffer = Vec::new();
        match fs::File::open(TMP_FILE) {
            Ok(mut input_file) => {
                if let Err(e) = input_file.read_to_end(&mut buffer) {
                    let _log = LogStruct {
                        level: LogLevel::Error,
                        topic: "无法读取log.tmp文件".to_string(),
                        content: e.to_string(),
                    };
                    log(&_log);
                    return;
                }
            }
            Err(e) => {
                let _log = LogStruct {
                    level: LogLevel::Error,
                    topic: "无法读取log.tmp文件".to_string(),
                    content: e.to_string(),
                };
                log(&_log);
                return;
            },
        }

        // 压缩文件
        match fs::File::create(&output_filename) {
            Ok(output_file) => {
                let mut encoder = GzEncoder::new(output_file, Compression::best());
                if let Err(e) = encoder.write_all(&buffer) {
                    let _log = LogStruct {
                        level: LogLevel::Error,
                        topic: "无法压缩log.tmp数据".to_string(),
                        content: e.to_string(),
                    };
                    log(&_log);
                    return;
                }
                if let Err(e) = encoder.finish() {
                    let _log = LogStruct {
                        level: LogLevel::Error,
                        topic: "无法压缩log.tmp数据".to_string(),
                        content: e.to_string(),
                    };
                    log(&_log);
                    return;
                }
                
                // 压缩成功后删除临时文件
                if let Err(e) = fs::remove_file(TMP_FILE) {
                    let _log = LogStruct {
                        level: LogLevel::Error,
                        topic: "无法删除log.tmp文件".to_string(),
                        content: e.to_string(),
                    };
                    log(&_log);
                }
            }
            Err(e) => {
                let _log = LogStruct {
                    level: LogLevel::Error,
                    topic: "无法创建日志轮转文件".to_string(),
                    content: e.to_string(),
                };
                log(&_log);
            },
        };
    });
}

fn log(info : &LogStruct) {
    let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    

    let prefix = info.level.as_str();
    let color_prefix = info.level.color();
    let text = format!("{} {}\n    {}\n    {}", prefix, time, info.topic, info.content);
    let cli_text = format!("{} {}\n    {}\n    {}", color_prefix, time, info.topic, info.content);

    // 输出到CLI
    println!("{}", cli_text);

    // 写入内容
    let _guard = LOG_FILE_MUTEX.lock().unwrap();
    let log_open_result = fs::OpenOptions::new()
        .append(true)       // 追加模式
        .create(true)       // 如果文件不存在则创建
        .open("log");
    let mut log = match log_open_result {
        Ok(file) => file,
        Err(_error) => {
            let log = LogStruct {
                level: LogLevel::Error,
                topic: "无法录入日志".to_string(),
                content: "log文件无法被追加写入".to_string(),
            };
            log_onlycli(&log);
            return;
        },
    };
    let write_result = writeln!(log, "{}", text);
    match write_result {
        Ok(_) => return,
        Err(_error) => {
            let log = LogStruct {
                level: LogLevel::Error,
                topic: "无法录入日志".to_string(),
                content: "log文件无法被追加写入".to_string(),
            };
            log_onlycli(&log);
            return;
        },
    };
}

fn log_onlycli(info : &LogStruct) {
    let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let color_prefix = info.level.color();
    let text = format!("{} {}\n    {}\n    {}", color_prefix, time, info.topic, info.content);

    // 输出到CLI
    println!("{}", text);

}