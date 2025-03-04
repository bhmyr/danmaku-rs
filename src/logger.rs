use crate::config;
use log::LevelFilter;
use log4rs::{
    append::{
        console::{ConsoleAppender, Target},
        rolling_file::{
            RollingFileAppender,
            policy::compound::{
                CompoundPolicy, roll::fixed_window::FixedWindowRoller, trigger::size::SizeTrigger,
            },
        },
    },
    config::{Appender, Config, Root},
    encode::pattern::PatternEncoder,
    filter::threshold::ThresholdFilter,
};
use std::fs;

const TRIGGER_FILE_SIZE: u64 = 1024 * 1024 * 10; // 10MB
const LOG_FILE_COUNT: u32 = 5;
const FILE_PATH: &str = "log/app.log";
const ARCHIVE_PATTERN: &str = "log/archive/app.log.{}";

pub fn init() {
    // 创建日志目录
    fs::create_dir_all("log/archive").unwrap();

    let level = match config::init().log_level.to_lowercase().as_str() {
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };

    // 控制台输出
    let stdout = ConsoleAppender::builder()
        .target(Target::Stdout)
        .encoder(Box::new(PatternEncoder::new(
            "{d(%Y%m%d %H:%M:%S)} {l} - {m}{n}",
        )))
        .build();

    // 文件输出策略配置
    let trigger = SizeTrigger::new(TRIGGER_FILE_SIZE);
    let roller = FixedWindowRoller::builder()
        .base(0)
        .build(ARCHIVE_PATTERN, LOG_FILE_COUNT)
        .unwrap();
    let policy = CompoundPolicy::new(Box::new(trigger), Box::new(roller));

    // 文件输出appender
    let file = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "{d(%Y-%m-%d %H:%M:%S)} {l} {M} - {m}{n}",
        )))
        .build(FILE_PATH, Box::new(policy))
        .unwrap();

    // 构建日志配置
    let config = Config::builder()
        // 控制台appender
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(LevelFilter::Debug)))
                .build("stdout", Box::new(stdout)),
        )
        // 文件appender
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(level)))
                .build("file", Box::new(file)),
        )
        // 根日志配置
        .build(
            Root::builder()
                .appenders(vec!["stdout", "file"])
                .build(LevelFilter::Debug), // 全局最低级别设为DEBUG
        )
        .unwrap();

    // 初始化日志系统
    log4rs::init_config(config).unwrap();
}
