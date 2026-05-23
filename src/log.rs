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

use chrono::{DateTime, Local};
use colored::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use scopeguard::defer;
use std::fs;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::spawn;

static LOG_MUTEX: Mutex<()> = Mutex::new(());
static ROLLING: AtomicBool = AtomicBool::new(false);

const LOG_PATH: &str = "log";
const TMP_LOG_PATH: &str = "log.tmp";
const MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10MB

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
            LogLevel::Preset => "[-]".normal(),
            LogLevel::Warning => "[*]".yellow(),
            LogLevel::Error => "[!]".red(),
            LogLevel::Critical => "[CRITICAL]".on_red().bold().blink(),
        }
    }
}

pub struct LogStruct {
    level: LogLevel,
    topic: String,
    content: String,
}

impl LogStruct {
    pub fn new(level: LogLevel, topic: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            level,
            topic: topic.into(),
            content: content.into(),
        }
    }
}

impl LogStruct {
    pub fn emit(&self) {
        if !ROLLING.swap(true, Ordering::AcqRel) {
            check_and_roll();
        }
        log(self)
    }
}

// 内部错误日志助手，直接输出到终端，不绕路文件
fn error_entry(topic: &str, content: &str) {
    let entry = LogStruct::new(LogLevel::Error, topic, content);
    log_onlycli(&entry);
}

fn archive_and_cleanup(tmp_file: String, output_file_name: String) {
    let mut buffer = Vec::new();
    match fs::File::open(&tmp_file) {
        Ok(mut input_file) => {
            if let Err(e) = input_file.read_to_end(&mut buffer) {
                error_entry("无法读取log.tmp文件", &e.to_string());
                return;
            }
        }
        Err(e) => {
            error_entry("无法读取log.tmp文件", &e.to_string());
            return;
        }
    }

    match fs::File::create(&output_file_name) {
        Ok(output_file) => {
            let mut encoder = GzEncoder::new(output_file, Compression::best());
            if let Err(e) = encoder.write_all(&buffer) {
                error_entry("无法压缩log.tmp数据", &e.to_string());
                return;
            }
            if let Err(e) = encoder.finish() {
                error_entry("无法压缩log.tmp数据", &e.to_string());
                return;
            }

            if let Err(e) = fs::remove_file(&tmp_file) {
                error_entry("无法删除log.tmp文件", &e.to_string());
            }
        }
        Err(e) => {
            error_entry("无法创建日志轮转文件", &e.to_string());
        }
    }
}

fn repair_tmp_file() {
    let tmp_metadata = match fs::metadata(TMP_LOG_PATH) {
        Ok(meta) => meta,
        Err(_) => return,
    };

    let tmp_time = tmp_metadata
        .created()
        .map(|t| {
            let dt: DateTime<Local> = DateTime::from(t);
            dt.format("%m%d_%H%M").to_string()
        })
        .unwrap_or_else(|_| "XXXX_XXXX".to_string());

    let fine_now = Local::now().timestamp_nanos_opt().unwrap_or(0);
    let output_filename = format!("REPAIR-{}-{}.gz", tmp_time, fine_now);
    archive_and_cleanup(TMP_LOG_PATH.to_owned(), output_filename);
}

fn perform_roll(file_metadata: fs::Metadata) {
    {
        let _guard = LOG_MUTEX.lock().unwrap();
        // 重命名
        match fs::rename(LOG_PATH, TMP_LOG_PATH) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(_guard);
                let warning = LogStruct::new(LogLevel::Warning, "修复错误tmp文件", "");
                log_onlycli(&warning);
                repair_tmp_file();
                // 清理后重试
                let _guard = LOG_MUTEX.lock().unwrap();
                if let Err(e) = fs::rename(LOG_PATH, TMP_LOG_PATH) {
                    let critical =
                        LogStruct::new(LogLevel::Critical, "无法重命名log文件", e.to_string());
                    log_onlycli(&critical);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                let critical =
                    LogStruct::new(LogLevel::Critical, "无法重命名log文件", e.to_string());
                log_onlycli(&critical);
                return;
            }
            _ => {}
        }
    }
    let timestamp = file_metadata
        .created()
        .map(|t| {
            let dt: DateTime<Local> = DateTime::from(t);
            dt.format("%m%d_%H%M").to_string()
        })
        .unwrap_or_else(|_| "XXXX_XXXX".to_string());
    let current_time = Local::now().format("%m%d_%H%M").to_string();
    let fine_now = Local::now().timestamp_nanos_opt().unwrap_or(0);
    let output_filename = format!("{}-{}-{}.gz", timestamp, current_time, fine_now);

    archive_and_cleanup(TMP_LOG_PATH.to_owned(), output_filename);
}

fn check_and_roll() {
    let metadata = match fs::metadata(LOG_PATH) {
        Ok(m) => m,
        Err(_) => {
            ROLLING.store(false, Ordering::Release);
            return;
        }
    };

    if metadata.len() <= MAX_SIZE_BYTES {
        ROLLING.store(false, Ordering::Release);
        return;
    }

    spawn(|| {
        defer! { ROLLING.store(false, Ordering::Release); }
        let metadata = match fs::metadata(LOG_PATH) {
            Ok(m) => m,
            Err(_) => return,
        };
        perform_roll(metadata);
    });
}

fn format_colored(level: &LogLevel, time: &str, topic: &str, content: &str) -> String {
    let prefix = level.color();
    if content.is_empty() {
        format!("{} {}\n    {}", prefix, time, topic)
    } else {
        format!("{} {}\n    {}\n    {}", prefix, time, topic, content)
    }
}

fn format_plain(level: &LogLevel, time: &str, topic: &str, content: &str) -> String {
    let prefix = level.as_str();
    if content.is_empty() {
        format!("{} {}\n    {}", prefix, time, topic)
    } else {
        format!("{} {}\n    {}\n    {}", prefix, time, topic, content)
    }
}

fn log(info: &LogStruct) {
    let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let cli_text = format_colored(&info.level, &time, &info.topic, &info.content);
    let file_text = format_plain(&info.level, &time, &info.topic, &info.content);

    match info.level {
        LogLevel::Error | LogLevel::Critical | LogLevel::Warning => {
            eprintln!("{}", cli_text);
        }
        _ => {
            println!("{}", cli_text);
        }
    }

    let _guard = LOG_MUTEX.lock().unwrap();
    let mut log_file = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(LOG_PATH)
    {
        Ok(file) => file,
        Err(_) => {
            let err = LogStruct::new(LogLevel::Error, "无法录入日志", "log文件无法被追加写入");
            log_onlycli(&err);
            return;
        }
    };

    if writeln!(log_file, "{}", file_text).is_err() {
        let err = LogStruct::new(LogLevel::Error, "无法录入日志", "log文件无法被追加写入");
        log_onlycli(&err);
    }
}

fn log_onlycli(info: &LogStruct) {
    let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let text = format_colored(&info.level, &time, &info.topic, &info.content);
    match info.level {
        LogLevel::Error | LogLevel::Critical | LogLevel::Warning => {
            eprintln!("{}", text);
        }
        _ => {
            println!("{}", text);
        }
    }
}
