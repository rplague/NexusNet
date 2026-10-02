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

use crate::paths::{self, FailType};
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

/// Rotate the File-mode log once it grows past this size.
const MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
/// Minimum interval between rotation size checks.
const ROLL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// File-mode log path, resolved once via
/// `paths::fail_path_gener(FailType::Log, "log")`.
///
/// Only used in [`LogMode::File`].
fn log_filepath() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        paths::fail_path_gener(FailType::Log, "log")
            .display()
            .to_string()
    })
}

/// Rotation staging path: the current log renamed to `<log>.tmp`.
fn logtmp_filepath() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| format!("{}.tmp", log_filepath()))
}

/// Directory holding the current log file and rotated archives.
fn log_dir() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        std::path::Path::new(log_filepath())
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string())
    })
}

/// Whether ANSI color may be written to the terminal.
///
/// Disabled by `NO_COLOR` or when stdout is not a TTY, so redirected
/// output is never polluted with escape sequences.
fn use_color() -> bool {
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Log output mode.
///
/// Chosen entirely from the `NEXUSNET_LOG_MODE` environment variable, set by
/// the service manager at install time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogMode {
    Journald,
    Nssm,
    File,
}

impl LogMode {
    /// Parse `NEXUSNET_LOG_MODE`; unset or unknown values fall back to
    /// [`LogMode::File`].
    fn from_env() -> Self {
        match std::env::var("NEXUSNET_LOG_MODE").as_deref() {
            Ok(v) if v.eq_ignore_ascii_case("journald") => Self::Journald,
            Ok(v) if v.eq_ignore_ascii_case("nssm") => Self::Nssm,
            _ => Self::File,
        }
    }
}

/// Cached process-wide log mode, resolved on first use.
fn log_mode() -> LogMode {
    static MODE: OnceLock<LogMode> = OnceLock::new();
    *MODE.get_or_init(LogMode::from_env)
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

    /// Map to a syslog priority (parsed and stripped by journald).
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
    /// Emit the entry through the configured mode.
    pub fn emit(&self) {
        if log_mode() == LogMode::File {
            maybe_check_and_roll();
        }
        log(self)
    }
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

/// Render a stream entry, colored when the terminal supports it.
fn render_stream(info: &LogStruct, time: Option<&str>) -> String {
    if use_color() {
        format_entry(info.level.color(), time, &info.topic, &info.content)
    } else {
        format_entry(info.level.as_str(), time, &info.topic, &info.content)
    }
}

/// Collapse all whitespace runs (including newlines) into single spaces.
fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Journald mode: single line with a `<N>` syslog priority prefix.
fn render_journald(info: &LogStruct) -> String {
    let content = oneline(&info.content);
    let body = if content.is_empty() {
        format!("{} {}", info.level.as_str(), info.topic)
    } else {
        format!("{} {}: {}", info.level.as_str(), info.topic, content)
    };
    format!("<{}>{}", info.level.syslog_priority(), body)
}

/// NSSM mode: single plain line prefixed with a local timestamp.
fn render_nssm(info: &LogStruct) -> String {
    let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let content = oneline(&info.content);
    if content.is_empty() {
        format!("{} {} {}", time, info.level.as_str(), info.topic)
    } else {
        format!(
            "{} {} {}: {}",
            time,
            info.level.as_str(),
            info.topic,
            content
        )
    }
}

/// Write one entry to a stream (warnings and errors go to stderr).
fn write_stream(level: &LogLevel, text: &str) {
    match level {
        LogLevel::Error | LogLevel::Critical | LogLevel::Warning => eprintln!("{}", text),
        _ => println!("{}", text),
    }
}

/// Dispatch one entry according to the configured mode.
fn log(info: &LogStruct) {
    match log_mode() {
        LogMode::Journald => {
            write_stream(&info.level, &render_journald(info));
        }
        LogMode::Nssm => {
            write_stream(&info.level, &render_nssm(info));
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

/// Append one entry to the File-mode log file.
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

/// Stream-only fallback used when file output is impossible.
fn log_onlycli(info: &LogStruct) {
    match log_mode() {
        LogMode::Journald => write_stream(&info.level, &render_journald(info)),
        LogMode::Nssm => write_stream(&info.level, &render_nssm(info)),
        LogMode::File => {
            let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            write_stream(&info.level, &render_stream(info, Some(&time)));
        }
    }
}

/// Throttled rotation check: stat the log file at most every
/// [`ROLL_CHECK_INTERVAL`].
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

/// Rotate the log in a background thread once it exceeds [`MAX_SIZE_BYTES`].
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

/// Rename the current log to `<log>.tmp`, then gzip it into an archive named
/// from its creation time, the rotation time and current nanoseconds.
fn perform_roll(file_metadata: fs::Metadata) {
    {
        let _guard = LOG_MUTEX.lock().unwrap();
        match fs::rename(log_filepath(), logtmp_filepath()) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(_guard);
                let warning = LogStruct::new(LogLevel::Warning, "修复错误tmp文件", "");
                log_onlycli(&warning);
                repair_tmp_file();
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
        log_dir(),
        timestamp,
        current_time,
        fine_now
    );

    archive_and_cleanup(logtmp_filepath().to_owned(), output_filepath);
}

/// Recover a leftover `.tmp` from an interrupted rotation by archiving it.
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
    let output_filepath = format!("{}/REPAIR-{}-{}.gz", log_dir(), tmp_time, fine_now);
    archive_and_cleanup(logtmp_filepath().to_owned(), output_filepath);
}

/// Gzip `tmp_file` into `output_file_name` and remove the source on success.
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

/// Stream-only error entry used by the rotation internals.
fn error_entry(topic: &str, content: &str) {
    let entry = LogStruct::new(LogLevel::Error, topic, content);
    log_onlycli(&entry);
}
