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

use crate::paths;
use chrono::{DateTime, Local};
use colored::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use scopeguard::defer;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::spawn;
use std::time::{Duration, Instant};

static LOG_MUTEX: Mutex<()> = Mutex::new(());
static ROLLING: AtomicBool = AtomicBool::new(false);
static LAST_ROLL_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

const MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10MB
const ROLL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 系统日志文件路径。
///
/// 由 `paths::log_path()` 解析
/// 惰性初始化一次。轮转所用临时路径为在其上追加 `.tmp`。
fn log_filepath() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| format!("{}/log", paths::log_path().to_string_lossy().into_owned()))
}

/// 轮转临时路径
fn logtmp_filepath() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| format!("{}.tmp", log_filepath()))
}

fn gz_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| paths::log_path().to_string_lossy().into_owned())
}

/// 是否在终端输出 ANSI 彩色。
///
/// 非 TTY 或设置了 `NO_COLOR` 时输出纯文本
/// 避免日志被转义序列污染。
fn use_color() -> bool {
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// 日志输出模式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogMode {
    Journald,
    File,
}

/// 判定日志模式
///
/// `JOURNAL_STREAM` 存在表示 stdout/stderr 被 journald 捕获，此时完全交由 journald 管理。
fn log_mode() -> LogMode {
    static MODE: OnceLock<LogMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        if std::env::var_os("JOURNAL_STREAM").is_some_and(|v| !v.is_empty()) {
            LogMode::Journald
        } else {
            LogMode::File
        }
    })
}

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

    /// 映射到 syslog priority（journald 会解析并剥离行首 `<N>`）。
    fn syslog_priority(&self) -> u8 {
        match self {
            LogLevel::Critical => 2,
            LogLevel::Error => 3,
            LogLevel::Warning => 4,
            LogLevel::Important => 5,
            LogLevel::Preset => 6,
            LogLevel::Debug => 7,
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
    pub fn emit(&self) {
        if log_mode() == LogMode::File {
            maybe_check_and_roll();
        }
        log(self)
    }
}

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
    let tmp_metadata = match fs::metadata(logtmp_filepath()) {
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
    let output_filepath = format!("{}/REPAIR-{}-{}.gz", gz_path(), tmp_time, fine_now);
    archive_and_cleanup(logtmp_filepath().to_owned(), output_filepath);
}

fn perform_roll(file_metadata: fs::Metadata) {
    {
        let _guard = LOG_MUTEX.lock().unwrap();
        // 重命名
        match fs::rename(log_filepath(), logtmp_filepath()) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(_guard);
                let warning = LogStruct::new(LogLevel::Warning, "修复错误tmp文件", "");
                log_onlycli(&warning);
                repair_tmp_file();
                // 清理后重试
                let _guard = LOG_MUTEX.lock().unwrap();
                if let Err(e) = fs::rename(log_filepath(), logtmp_filepath()) {
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
    let output_filepath = format!(
        "{}/{}-{}-{}.gz",
        gz_path(),
        timestamp,
        current_time,
        fine_now
    );

    archive_and_cleanup(logtmp_filepath().to_owned(), output_filepath);
}

/// 节流后的轮转检查：File 模式下每 `ROLL_CHECK_INTERVAL` 最多 stat 一次。
fn maybe_check_and_roll() {
    {
        let mut last = LAST_ROLL_CHECK.lock().unwrap();
        let now = Instant::now();
        if last.is_some_and(|prev| now.duration_since(prev) < ROLL_CHECK_INTERVAL) {
            return;
        }
        *last = Some(now);
    }
    if !ROLLING.swap(true, Ordering::AcqRel) {
        check_and_roll();
    }
}

fn check_and_roll() {
    let metadata = match fs::metadata(log_filepath()) {
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
        let metadata = match fs::metadata(log_filepath()) {
            Ok(m) => m,
            Err(_) => return,
        };
        perform_roll(metadata);
    });
}

fn format_entry(
    prefix: impl std::fmt::Display,
    time: Option<&str>,
    topic: &str,
    content: &str,
) -> String {
    match (time, content.is_empty()) {
        (Some(t), true) => format!("{} {}\n    {}", prefix, t, topic),
        (Some(t), false) => format!("{} {}\n    {}\n    {}", prefix, t, topic, content),
        (None, true) => format!("{} {}", prefix, topic),
        (None, false) => format!("{} {}\n    {}", prefix, topic, content),
    }
}

/// 按是否彩色渲染流输出（File 模式，多行）。
fn render_stream(info: &LogStruct, time: Option<&str>) -> String {
    if use_color() {
        format_entry(info.level.color(), time, &info.topic, &info.content)
    } else {
        format_entry(info.level.as_str(), time, &info.topic, &info.content)
    }
}

/// journald 模式：单行 + `<N>` 优先级前缀（journald 会解析并剥离该前缀）。
fn render_journald(info: &LogStruct) -> String {
    let content = info
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let body = if content.is_empty() {
        format!("{} {}", info.level.as_str(), info.topic)
    } else {
        format!("{} {}: {}", info.level.as_str(), info.topic, content)
    };
    format!("<{}>{}", info.level.syslog_priority(), body)
}

/// 写一行到流（错误类走 stderr，其余 stdout）。
fn write_stream(level: &LogLevel, text: &str) {
    match level {
        LogLevel::Error | LogLevel::Critical | LogLevel::Warning => eprintln!("{}", text),
        _ => println!("{}", text),
    }
}

fn log(info: &LogStruct) {
    match log_mode() {
        LogMode::Journald => {
            write_stream(&info.level, &render_journald(info));
        }
        LogMode::File => {
            let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            write_stream(&info.level, &render_stream(info, Some(&time)));

            let file_text =
                format_entry(info.level.as_str(), Some(&time), &info.topic, &info.content);
            append_file(&file_text);
        }
    }
}

/// 追加写入日志文件（File 模式）。
fn append_file(text: &str) {
    let _guard = LOG_MUTEX.lock().unwrap();
    let mut log_file = match fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_filepath())
    {
        Ok(file) => file,
        Err(_) => {
            let err = LogStruct::new(LogLevel::Error, "无法录入日志", "log文件无法被追加写入");
            log_onlycli(&err);
            return;
        }
    };

    if writeln!(log_file, "{}", text).is_err() {
        let err = LogStruct::new(LogLevel::Error, "无法录入日志", "log文件无法被追加写入");
        log_onlycli(&err);
    }
}

fn log_onlycli(info: &LogStruct) {
    match log_mode() {
        LogMode::Journald => {
            write_stream(&info.level, &render_journald(info));
        }
        LogMode::File => {
            let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            write_stream(&info.level, &render_stream(info, Some(&time)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syslog_priority_mapping() {
        assert_eq!(LogLevel::Critical.syslog_priority(), 2);
        assert_eq!(LogLevel::Error.syslog_priority(), 3);
        assert_eq!(LogLevel::Warning.syslog_priority(), 4);
        assert_eq!(LogLevel::Important.syslog_priority(), 5);
        assert_eq!(LogLevel::Preset.syslog_priority(), 6);
        assert_eq!(LogLevel::Debug.syslog_priority(), 7);
    }

    #[test]
    fn journald_single_line_with_prefix_and_collapsed_newlines() {
        let entry = LogStruct::new(LogLevel::Error, "topic", "line1\nline2");
        let line = render_journald(&entry);
        assert_eq!(line, "<3>[!] topic: line1 line2");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn journald_empty_content() {
        let entry = LogStruct::new(LogLevel::Debug, "topic", "");
        assert_eq!(render_journald(&entry), "<7>[+] topic");
    }
}
