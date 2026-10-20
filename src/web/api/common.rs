//! 设置接口共用工具
//!
//! 多个 settings PUT 都需要「加载 → 修改 → 保存」，并且必须避免两类事故：
//! 1. settings.toml 本身 TOML 非法时，静默用默认值覆盖掉用户配置；
//! 2. 两个页面并发保存时互相覆盖。
//!
//! 这里提供一个进程级写锁和严格加载函数。只覆盖本轮接入的接口，
//! 后续可以逐步把其他 settings handler 也迁过来。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::{Json, http::StatusCode};
use serde_json::Value;

use crate::config::settings::{AppConfig, load_settings, save_settings};

/// 串行化所有「读-改-写 settings.toml」操作。
///
/// 用 std::sync::Mutex 而不是 tokio Mutex：临界区只有同步文件 IO，
/// 不跨 await 持有，持有时间在毫秒级。
static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 严格加载配置。
///
/// - 文件不存在：返回默认配置（首次启动/首次保存）
/// - 文件存在但 TOML 非法/读取失败：返回 Err，调用方必须拒绝写盘
///
/// 绝不能在这里 `unwrap_or_default()`：那会在用户配置写错一个字符时，
/// 把整份配置替换成默认值。
pub fn load_strict() -> Result<AppConfig, String> {
    match load_settings() {
        Ok(Some(cfg)) => Ok(cfg),
        Ok(None) => Ok(AppConfig::default()),
        Err(e) => Err(format!("settings.toml 解析失败: {e}")),
    }
}

/// 设置写操作的错误分类。
///
/// 让 handler 能区分「客户端参数错误 / 现有配置文件冲突 / 服务端写盘失败」，
/// 而不是把所有错误都回 500。
#[derive(Debug)]
pub enum SettingsUpdateError {
    /// 现有 settings.toml 无法解析，必须先人工修复
    Conflict(String),
    /// 请求参数校验失败
    BadRequest(String),
    /// 写盘失败
    Internal(String),
}

impl SettingsUpdateError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Conflict(message) | Self::BadRequest(message) | Self::Internal(message) => {
                message.clone()
            }
        }
    }
}

/// 在写锁内执行一次「加载 → 修改 → 保存」。
///
/// 闭包返回 `restart_required`，由 handler 原样带给前端。
pub fn update_settings<F>(update: F) -> Result<(AppConfig, bool), SettingsUpdateError>
where
    F: FnOnce(&mut AppConfig) -> Result<bool, String>,
{
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut cfg = load_strict().map_err(SettingsUpdateError::Conflict)?;
    let restart_required = update(&mut cfg).map_err(SettingsUpdateError::BadRequest)?;
    save_settings(&cfg).map_err(|e| SettingsUpdateError::Internal(format!("保存配置失败: {e}")))?;
    Ok((cfg, restart_required))
}

/// 解析请求体里的 `providers` 映射。
///
/// 与旧实现的差异：空串不再被丢弃，而是保留下来，随后由调用方写回配置。
/// 运行时 `get_credential` 会过滤空值并回退环境变量，因此「清空输入框」
/// 能真正生效。值为空串的 key 会被剪掉，避免在 TOML 里留下 `api_key = ""`
/// 这种噪音。
///
/// 返回 `None` 表示请求体没有 `providers` 字段（本次不更新 provider 表）；
/// 返回 `Some(空 map)` 表示显式清空所有 provider 配置。
pub fn parse_providers(value: &Value) -> Option<HashMap<String, HashMap<String, String>>> {
    let obj = value.get("providers")?.as_object()?;
    let mut providers = HashMap::new();
    for (name, fields) in obj {
        let Some(fields_obj) = fields.as_object() else {
            continue;
        };
        let mut creds = HashMap::new();
        for (k, v) in fields_obj {
            let Some(s) = v.as_str() else {
                continue;
            };
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                creds.insert(k.clone(), trimmed.to_string());
            }
        }
        providers.insert(name.clone(), creds);
    }
    Some(providers)
}

/// 统一成功响应：`data` 之外带 `restart_required`，前端据此显示重启提示。
pub fn ok_response(data: Value, restart_required: bool, message: &str) -> Json<Value> {
    Json(serde_json::json!({
        "success": true,
        "data": data,
        "restart_required": restart_required,
        "message": message,
    }))
}

/// 统一失败响应。
pub fn error_response(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    let message = message.into();
    (
        status,
        Json(serde_json::json!({
            "success": false,
            "error": message,
            "message": message,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_providers_keeps_non_empty_and_drops_empty_keys() {
        let json = serde_json::json!({
            "providers": {
                "doubao": {
                    "api_key": " key ",
                    "resource_id": "",
                    "end_window_size": "800"
                }
            }
        });
        let providers = parse_providers(&json).unwrap();
        let doubao = providers.get("doubao").unwrap();
        assert_eq!(doubao.get("api_key").map(String::as_str), Some("key"));
        assert_eq!(
            doubao.get("end_window_size").map(String::as_str),
            Some("800")
        );
        assert!(doubao.get("resource_id").is_none());
    }

    #[test]
    fn parse_providers_empty_value_clears_all() {
        let json = serde_json::json!({ "providers": {} });
        let providers = parse_providers(&json).unwrap();
        assert!(providers.is_empty());
    }

    #[test]
    fn parse_providers_missing_returns_none() {
        assert!(parse_providers(&serde_json::json!({ "active_provider": "doubao" })).is_none());
    }

    #[test]
    fn load_strict_rejects_invalid_toml() {
        use crate::test_util::run_with_temp_home;

        run_with_temp_home(|home| {
            let dir = home.join(".haimen");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.toml"), "{invalid").unwrap();

            let err = load_strict().expect_err("非法 TOML 必须返回 Err");
            assert!(err.contains("settings.toml"), "{err}");
        });
    }

    #[test]
    fn parse_providers_ignores_non_object_provider() {
        let json = serde_json::json!({
            "providers": {
                "broken": "not-an-object",
                "qwen": { "api_key": "k" }
            }
        });
        let providers = parse_providers(&json).unwrap();
        assert!(!providers.contains_key("broken"));
        assert!(providers.contains_key("qwen"));
    }
}
