use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use crate::error::AppError;

/// 核心运行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreMode {
    #[default]
    Xray,
    #[serde(rename = "sing-box")]
    SingBox,
    Both,
}

impl CoreMode {
    pub fn runs_sing_box(&self) -> bool {
        matches!(self, Self::SingBox | Self::Both)
    }

    pub fn runs_xray(&self) -> bool {
        matches!(self, Self::Xray | Self::Both)
    }
}

/// 核心配置。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Core {
    #[serde(default)]
    pub mode: CoreMode,
}

/// 应用配置, 对应 `settings.json`。
///
/// 所有字段均提供默认值, 配置文件缺失或解析失败时自动回退。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub log: Log,
    #[serde(default)]
    pub download: Download,
    #[serde(default)]
    pub core: Core,
}

/// 加载期间收集的警告信息, 供 init_logging 之后通过 tracing 输出。
static LOAD_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 核心下载配置。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CoreDownload {
    /// GitHub CDN 代理 URL 前缀, 空字符串表示不启用代理。
    #[serde(default)]
    pub gh_proxy: String,
}

/// 日志配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Log {
    /// 日志级别, 可选值: "debug", "info", "warn", "error"。
    #[serde(default = "default_log_level")]
    pub level: String,
    /// `app.log` 的大小上限 (MB) , 0 表示不限制。
    #[serde(default = "default_log_max_size_mb")]
    pub max_size_mb: u64,
}

/// 单个规则集配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RulesetEntry {
    pub dat: String,
    pub sha256sum: String,
    pub last_update: Option<u64>,
}

/// 规则集下载配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ruleset {
    /// 规则集条目。用 BTreeMap 而不是 HashMap: 保存配置时会把整个 Settings
    /// 序列化回 settings.json, HashMap 的迭代顺序每次都不同, 会让文件产生
    /// 无意义的 diff, 日志里的处理顺序也跟着乱。
    #[serde(flatten)]
    pub entries: BTreeMap<String, RulesetEntry>,
    #[serde(default = "default_interval_days")]
    pub interval_days: u64,
}

impl Default for Ruleset {
    fn default() -> Self {
        Ruleset {
            entries: BTreeMap::new(),
            interval_days: default_interval_days(),
        }
    }
}

/// 下载配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Download {
    #[serde(default)]
    pub core: CoreDownload,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default)]
    pub ruleset: Ruleset,
}

/// 下载重试配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Retry {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay")]
    pub delay_secs: u64,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            max_retries: default_max_retries(),
            delay_secs: default_retry_delay(),
        }
    }
}

fn default_log_level() -> String {
    "debug".to_string()
}

fn default_log_max_size_mb() -> u64 {
    10
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay() -> u64 {
    2
}

fn default_interval_days() -> u64 {
    3
}

impl Default for Log {
    fn default() -> Self {
        Log {
            level: default_log_level(),
            max_size_mb: default_log_max_size_mb(),
        }
    }
}

impl Default for Download {
    fn default() -> Self {
        Download {
            core: CoreDownload::default(),
            retry: Retry {
                max_retries: default_max_retries(),
                delay_secs: default_retry_delay(),
            },
            ruleset: Ruleset::default(),
        }
    }
}

const ALLOWED_LEVELS: &[&str] = &["debug", "info", "warn", "error"];

/// 规则集更新间隔上限 (天) 。
///
/// 除了避免无意义的巨大间隔, 也防止 `interval_days * 86400` 溢出 u64。
const MAX_INTERVAL_DAYS: u64 = 365;

/// 日志文件大小上限的可配置上界 (MB) 。
const MAX_LOG_SIZE_MB: u64 = 4096;

impl Settings {
    /// 从 `exe_dir/settings.json` 加载配置。
    /// 文件不存在或格式错误时打印警告并返回默认值。
    pub fn load(exe_dir: &Path) -> Self {
        let path = exe_dir.join("settings.json");

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                push_warning(format!("读取配置文件失败 ({}), 使用默认配置: {e}", path.display()));
                return Self::default();
            }
        };

        match parse_settings(&text) {
            Ok((settings, warnings)) => {
                for w in warnings {
                    push_warning(w);
                }
                settings
            }
            Err(e) => {
                push_warning(format!("解析配置文件失败 ({}), 使用默认配置: {e}", path.display()));
                Self::default()
            }
        }
    }

    /// 将当前配置写入 `exe_dir/settings.json`。
    pub fn save(&self, exe_dir: &Path) -> Result<(), AppError> {
        let path = exe_dir.join("settings.json");
        let json = serde_json::to_string_pretty(self).map_err(|e| AppError::Msg(format!("序列化配置失败: {e}")))?;
        std::fs::write(&path, json).map_err(|e| AppError::Msg(format!("写入配置文件失败: {e}")))
    }

    /// 取出加载期间收集的警告。调用后清空, 仅首次调用返回内容。
    pub fn take_warnings() -> Vec<String> {
        LOAD_WARNINGS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// 校验配置值合法性, 非法值回退到默认值。
    /// 返回校验过程中产生的警告信息, 由调用方决定如何输出。
    fn validate(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();

        let level = self.log.level.to_lowercase();
        if ALLOWED_LEVELS.contains(&level.as_str()) {
            self.log.level = level;
        } else {
            warnings.push(format!(
                "无效的日志级别 \"{}\", 可选值: {:?}, 回退到 \"debug\"",
                self.log.level, ALLOWED_LEVELS
            ));
            self.log.level = "debug".to_string();
        }

        if self.download.retry.max_retries == 0 {
            warnings.push("max_retries 不能为 0, 回退到默认值 3".to_string());
            self.download.retry.max_retries = 3;
        } else if self.download.retry.max_retries > 10 {
            warnings.push("max_retries 超出上限 10, 已自动限制".to_string());
            self.download.retry.max_retries = 10;
        }

        if self.download.retry.delay_secs == 0 {
            warnings.push("delay_secs 不能为 0, 回退到默认值 2".to_string());
            self.download.retry.delay_secs = 2;
        } else if self.download.retry.delay_secs > 30 {
            warnings.push("delay_secs 超出上限 30, 已自动限制".to_string());
            self.download.retry.delay_secs = 30;
        }

        // interval_days 允许为 0, 语义是每次启动都更新规则集。
        if self.download.ruleset.interval_days > MAX_INTERVAL_DAYS {
            warnings.push(format!("interval_days 超出上限 {MAX_INTERVAL_DAYS}, 已自动限制"));
            self.download.ruleset.interval_days = MAX_INTERVAL_DAYS;
        }

        // max_size_mb 允许为 0, 语义是不限制日志大小。
        if self.log.max_size_mb > MAX_LOG_SIZE_MB {
            warnings.push(format!("log.max_size_mb 超出上限 {MAX_LOG_SIZE_MB}, 已自动限制"));
            self.log.max_size_mb = MAX_LOG_SIZE_MB;
        }

        warnings
    }
}

/// 解析并校验 JSON 配置字符串, 返回配置与校验警告列表。
/// 解析失败时返回错误, 由调用方决定回退策略。
fn parse_settings(text: &str) -> Result<(Settings, Vec<String>), serde_json::Error> {
    let mut settings = serde_json::from_str::<Settings>(text)?;
    let warnings = settings.validate();
    Ok((settings, warnings))
}

fn push_warning(msg: String) {
    eprintln!("警告: {msg}");
    if let Ok(mut warnings) = LOAD_WARNINGS.lock() {
        warnings.push(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_settings_defaults() {
        let (s, warnings) = parse_settings("{}").unwrap();
        assert_eq!(s.log.level, "debug");
        assert_eq!(s.download.retry.max_retries, 3);
        assert_eq!(s.download.retry.delay_secs, 2);
        assert_eq!(s.download.ruleset.interval_days, 3);
        assert_eq!(s.core.mode, CoreMode::Xray);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_settings_invalid_json() {
        assert!(parse_settings("not json").is_err());
    }

    #[test]
    fn test_parse_settings_invalid_log_level() {
        let (s, warnings) = parse_settings(r#"{"log":{"level":"trace"}}"#).unwrap();
        assert_eq!(s.log.level, "debug");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("无效的日志级别"));
    }

    #[test]
    fn test_parse_settings_log_level_normalized() {
        let (s, warnings) = parse_settings(r#"{"log":{"level":"DEBUG"}}"#).unwrap();
        assert_eq!(s.log.level, "debug");
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_settings_max_retries_bounds() {
        let (s, warnings) = parse_settings(r#"{"download":{"retry":{"max_retries":0}}}"#).unwrap();
        assert_eq!(s.download.retry.max_retries, 3);
        assert_eq!(warnings.len(), 1);

        let (s, warnings) = parse_settings(r#"{"download":{"retry":{"max_retries":11}}}"#).unwrap();
        assert_eq!(s.download.retry.max_retries, 10);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn test_parse_settings_delay_secs_bounds() {
        let (s, warnings) = parse_settings(r#"{"download":{"retry":{"delay_secs":0}}}"#).unwrap();
        assert_eq!(s.download.retry.delay_secs, 2);
        assert_eq!(warnings.len(), 1);

        let (s, warnings) = parse_settings(r#"{"download":{"retry":{"delay_secs":31}}}"#).unwrap();
        assert_eq!(s.download.retry.delay_secs, 30);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn test_parse_settings_interval_days_bounds() {
        let (s, warnings) = parse_settings(r#"{"download":{"ruleset":{"interval_days":100000}}}"#).unwrap();
        assert_eq!(s.download.ruleset.interval_days, MAX_INTERVAL_DAYS);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("interval_days"));

        // 0 是合法值, 语义为每次启动都更新规则集
        let (s, warnings) = parse_settings(r#"{"download":{"ruleset":{"interval_days":0}}}"#).unwrap();
        assert_eq!(s.download.ruleset.interval_days, 0);
        assert!(warnings.is_empty());
    }

    /// Ruleset 用 serde flatten 收集规则集条目, 同时保留具名的 interval_days,
    /// 两者共存时不应互相吞掉。
    #[test]
    fn test_parse_settings_ruleset_entries_and_interval() {
        let json = r#"{
            "download": {
                "ruleset": {
                    "geoip": {
                        "dat": "https://example.com/geoip.dat",
                        "sha256sum": "https://example.com/geoip.dat.sha256sum",
                        "last_update": 1700000000
                    },
                    "interval_days": 7
                }
            }
        }"#;
        let (s, warnings) = parse_settings(json).unwrap();
        assert_eq!(s.download.ruleset.interval_days, 7);
        assert_eq!(s.download.ruleset.entries.len(), 1);
        let entry = &s.download.ruleset.entries["geoip"];
        assert_eq!(entry.dat, "https://example.com/geoip.dat");
        assert_eq!(entry.last_update, Some(1700000000));
        assert!(warnings.is_empty());
    }

    /// 规则集条目用 BTreeMap 存放, 序列化后键顺序稳定, 否则每次写回
    /// settings.json 都会产生无意义的 diff。
    #[test]
    fn test_ruleset_serialization_order_is_stable() {
        let json = r#"{
            "download": {
                "ruleset": {
                    "zeta": { "dat": "z", "sha256sum": "z", "last_update": null },
                    "alpha": { "dat": "a", "sha256sum": "a", "last_update": null }
                }
            }
        }"#;
        let (s, _) = parse_settings(json).unwrap();
        let out = serde_json::to_string(&s).unwrap();
        let alpha = out.find("alpha").expect("应含 alpha");
        let zeta = out.find("zeta").expect("应含 zeta");
        assert!(alpha < zeta, "键应按字典序输出: {out}");
    }

    #[test]
    fn test_parse_settings_full_valid() {
        let json = r#"{
            "log": {"level": "info"},
            "download": {
                "core": {"gh_proxy": "https://proxy.example.com/"},
                "retry": {"max_retries": 5, "delay_secs": 3}
            },
            "core": {"mode": "both"}
        }"#;
        let (s, warnings) = parse_settings(json).unwrap();
        assert_eq!(s.log.level, "info");
        assert_eq!(s.download.core.gh_proxy, "https://proxy.example.com/");
        assert_eq!(s.download.retry.max_retries, 5);
        assert_eq!(s.download.retry.delay_secs, 3);
        assert_eq!(s.core.mode, CoreMode::Both);
        assert!(warnings.is_empty());
    }
}
