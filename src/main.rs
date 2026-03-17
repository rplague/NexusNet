mod log;
use log::LogLevel;
use log::LogStruct;

fn main() {
    let log = LogStruct{
        level: LogLevel::Preset,
        topic: "测试log功能".to_string(),
        content: "".to_string(),
    };
    log.logout();
    let log = LogStruct{
        level: LogLevel::Preset,
        topic: "测试log功能".to_string(),
        content: "".to_string(),
    };
    log.logout();
}
