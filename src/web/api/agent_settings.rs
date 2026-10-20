//! Agent 配置 REST API
//!
//! 提供对 `settings.toml` 中 `[gateway]` 的 Agent 配置的读写接口。
//!
//! 与 ASR/TTS 一致，Agent 共享句柄经 axum State 注入：`PUT` 保存配置后
//! 重建 Agent 并换入共享状态，实现运行时热切换（无需重启）。
//!
//! # 端点
//!
//! - `GET /api/v1/settings/agent` — 获取 Agent 配置（active_provider + providers + 当前生效 agent）
//! - `PUT /api/v1/settings/agent` — 更新 Agent 配置并热切换
//! - `GET /api/v1/agent/providers` — 列出注册表中所有 Agent 提供商
//! - `POST /api/v1/settings/agent/verify` — 验证指定 Agent CLI 是否可用

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::{Json, http::StatusCode};

use crate::config::settings::AppConfig;
use crate::web::api::common::{
    SettingsUpdateError, error_response, load_strict, parse_providers, update_settings,
};

// ═══════════════════════════════════════════════════════════════════════════════
// 工具函数
// ═══════════════════════════════════════════════════════════════════════════════

/// 加载当前配置（文件不存在时返回默认值）
fn load_config() -> AppConfig {
    crate::config::settings::load_settings()
        .ok()
        .flatten()
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Agent 端点
// ═══════════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/settings/agent`
///
/// 返回完整的 Agent 配置：所有已配置的提供商参数 + 当前激活的提供商 +
/// 运行时实际生效的 Agent（`applied_agent` + `generation`）。
pub async fn get_agent_settings(
    State(shared_agent): State<crate::gateway::agent_handle::SharedAgent>,
) -> Json<serde_json::Value> {
    let cfg = load_config();
    Json(serde_json::json!({
        "success": true,
        "data": {
            "active_provider": cfg.gateway.active_provider,
            "providers": cfg.gateway.providers,
            "resolved": {
                "agent": cfg.gateway.resolved_agent(),
            },
            "applied_agent": crate::gateway::agent_handle::current_name(&shared_agent),
            "generation": crate::gateway::agent_handle::current_generation(&shared_agent),
        }
    }))
}

/// `PUT /api/v1/settings/agent`
///
/// 替换完整的 Agent 配置并热切换。支持以下字段：
/// - `active_provider` — 切换当前激活的 Agent 提供商
/// - `providers` — 所有提供商的完整参数映射
///
/// 原子切换顺序：**构建候选 → check_available → 写盘 → 换入内存**。
/// 任一步失败返回 4xx/5xx，磁盘与内存都停留在旧配置（自动回滚）。
/// 成功后下一次消息/事件即使用新 Agent；会话因换代自动重置。
pub async fn update_agent_settings(
    State(shared_agent): State<crate::gateway::agent_handle::SharedAgent>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 现有 settings.toml 非法时拒绝写盘，避免用默认值覆盖用户配置。
    let mut cfg = load_strict().map_err(|e| error_response(StatusCode::CONFLICT, e))?;

    // 更新 active_provider（如果提供）
    if let Some(active) = body.get("active_provider").and_then(|v| v.as_str()) {
        if !active.is_empty() {
            cfg.gateway.active_provider = active.to_string();
        }
    }

    // 更新 providers（如果提供）
    if let Some(providers) = parse_providers(&body) {
        cfg.gateway.providers = providers;
    }

    // 本次请求要写入的 Agent 字段。候选构建用快照，落盘时再与磁盘最新配置合并。
    let active_provider = cfg.gateway.active_provider.clone();
    let providers = cfg.gateway.providers.clone();
    let name = cfg.gateway.resolved_agent();

    // 1. 构建候选（未注册的 provider → 400，零改动）
    let candidate = match crate::agents::registry::registry().build(&name, &cfg.gateway) {
        Ok(a) => a,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": e.clone(),
                    "message": e,
                })),
            ));
        }
    };

    // 2. check_available（CLI 不可用 → 400，零改动，旧 Agent 继续工作）
    if let Err(e) = candidate.check_available().await {
        let msg = format!("{} 不可用，配置未保存: {}", name, e);
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": msg.clone(),
                "message": msg,
            })),
        ));
    }

    // 3. 写盘：锁内重新加载最新配置，只合并 Agent 字段。
    //
    // 候选构建和 check_available 都在锁外（可能耗时数秒）；这里如果直接写
    // 开头加载的旧快照，会把期间其他页面保存的 section 覆盖掉。
    let saved_cfg = save_agent_settings(active_provider, providers)
        .map_err(|e| error_response(e.status(), e.message()))?;

    // 4. 换入内存（此刻才换代，单条赋值原子生效）
    let (applied_agent, generation) = {
        let mut guard = shared_agent.write().expect("agent handle 锁中毒");
        guard.swap(Arc::from(candidate));
        (guard.name.clone(), guard.generation)
    };

    Ok(Json(serde_json::json!({
        "success": true,
        "data": {
            "active_provider": saved_cfg.gateway.active_provider,
            "providers": saved_cfg.gateway.providers,
            "applied": true,
            "applied_agent": applied_agent,
            "generation": generation,
        }
    })))
}

/// `GET /api/v1/agent/providers`
///
/// 返回注册表中所有可用的 Agent 提供商（id + display_name）。
/// 前端据此动态渲染提供商 Tab，新增 Agent 后无需改动前端。
pub async fn list_agent_providers() -> Json<serde_json::Value> {
    let providers: Vec<serde_json::Value> = crate::agents::registry::registry()
        .list()
        .into_iter()
        .map(|info| {
            serde_json::json!({
                "id": info.id,
                "display_name": info.display_name,
            })
        })
        .collect();

    Json(serde_json::json!({
        "success": true,
        "data": { "providers": providers }
    }))
}

/// 在设置写锁内保存 Agent 配置。
///
/// 只覆盖 `gateway.active_provider` 与 `gateway.providers`，其余 section 从
/// 磁盘最新配置重新加载后保留。这样 `check_available()` 等待期间其他页面
/// 保存的 Xiaozhi / ASR / TTS 配置不会被本请求的旧快照覆盖。
fn save_agent_settings(
    active_provider: String,
    providers: HashMap<String, HashMap<String, String>>,
) -> Result<AppConfig, SettingsUpdateError> {
    update_settings(|latest| {
        latest.gateway.active_provider = active_provider;
        latest.gateway.providers = providers;
        Ok(false)
    })
    .map(|(cfg, _)| cfg)
}

/// 把请求体里的草稿字段合并进指定 provider 的临时配置。
///
/// 请求体除 `provider` 外的字符串字段都视为该 provider 的草稿参数：
/// - 非空值写入临时配置（如 openai 的 `base_url`/`model`/`api_key`、CLI 的 `cli_path`）；
/// - 空串删除对应 key，使「清空后验证」与保存后的环境变量回退语义一致。
///
/// 只改内存中的克隆配置，不写盘、不影响正在运行的 Agent。
fn apply_draft_provider_fields(
    cfg: &mut crate::config::settings::GatewayConfig,
    provider: &str,
    body: &serde_json::Value,
) {
    let Some(fields) = body.as_object() else {
        return;
    };
    let entry = cfg.providers.entry(provider.to_string()).or_default();
    for (key, value) in fields {
        if key == "provider" {
            continue;
        }
        let Some(raw) = value.as_str() else {
            continue;
        };
        let value = raw.trim();
        if value.is_empty() {
            entry.remove(key);
        } else {
            entry.insert(key.clone(), value.to_string());
        }
    }
}

/// `POST /api/v1/settings/agent/verify`
///
/// 验证指定 Agent 提供商是否可用。
///
/// 请求体包含 `provider` 字段，以及该 provider 的**草稿**字段（尚未保存到配置），
/// 例如 openai 的 `base_url` / `model` / `api_key`，或 CLI Agent 的 `cli_path`。
/// 统一通过注册表构造 Agent 并调用其 `check_available()`
/// （如 `claude --version` / `codex --version` / openai 的 `/models`），
/// 新增 Agent 无需改动此处。
pub async fn verify_agent_credentials(
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-code");

    let mut cfg = load_config();

    // 草稿字段只注入克隆配置，用于未保存前的探活。
    apply_draft_provider_fields(&mut cfg.gateway, provider, &body);

    let agent = match crate::agents::registry::registry().build(provider, &cfg.gateway) {
        Ok(agent) => agent,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "success": true,
                "data": { "valid": false, "message": e }
            })));
        }
    };

    match agent.check_available().await {
        Ok(()) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": true, "message": format!("{} CLI 可用", agent.name()) }
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": e }
        }))),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_providers_valid() {
        let json = serde_json::json!({
            "providers": {
                "claude-code": {
                    "note": "CLI 工具无需额外凭证"
                },
                "codex": {
                    "note": "CLI 工具无需额外凭证"
                }
            }
        });
        let providers = parse_providers(&json).unwrap();
        assert_eq!(providers.len(), 2);
        assert!(providers.contains_key("claude-code"));
        assert!(providers.contains_key("codex"));
    }

    #[test]
    fn test_parse_providers_skips_empty() {
        let json = serde_json::json!({
            "providers": {
                "claude-code": {
                    "note": ""
                }
            }
        });
        let providers = parse_providers(&json).unwrap();
        let cc = providers.get("claude-code").unwrap();
        assert!(cc.get("note").is_none());
    }

    #[test]
    fn test_parse_providers_missing() {
        let json = serde_json::json!({"other": "value"});
        assert!(parse_providers(&json).is_none());
    }

    #[test]
    fn test_apply_draft_provider_fields_overrides_and_clears() {
        let mut cfg = crate::config::settings::GatewayConfig::default();
        cfg.providers.insert(
            "openai".to_string(),
            std::collections::HashMap::from([
                ("base_url".to_string(), "http://old/v1".to_string()),
                ("model".to_string(), "old-model".to_string()),
                ("api_key".to_string(), "old-key".to_string()),
            ]),
        );

        apply_draft_provider_fields(
            &mut cfg,
            "openai",
            &serde_json::json!({
                "provider": "openai",
                "base_url": "  http://draft/v1  ",
                "model": "",
                "api_key": "draft-key",
                "ignored_number": 42
            }),
        );

        let openai = cfg.providers.get("openai").expect("provider 应存在");
        assert_eq!(
            openai.get("base_url").map(String::as_str),
            Some("http://draft/v1")
        );
        assert_eq!(openai.get("api_key").map(String::as_str), Some("draft-key"));
        // 空串表示清空草稿字段，让验证走环境变量/默认值回退
        assert!(openai.get("model").is_none());
        // 非字符串字段被忽略
        assert!(openai.get("ignored_number").is_none());
        // provider 自身不会被写成配置 key
        assert!(openai.get("provider").is_none());
    }

    #[test]
    fn test_save_agent_settings_preserves_other_sections() {
        use crate::test_util::run_with_temp_home;

        run_with_temp_home(|_| {
            let mut initial = AppConfig::default();
            initial.xiaozhi.say_token = "keep-say".to_string();
            initial.asr.active_provider = "qwen".to_string();
            initial.tts.active_provider = "openai".to_string();
            crate::config::settings::save_settings(&initial).expect("预置配置应写入");

            let providers = HashMap::from([(
                "openai".to_string(),
                HashMap::from([
                    ("base_url".to_string(), "http://example/v1".to_string()),
                    ("model".to_string(), "example-model".to_string()),
                ]),
            )]);

            let saved =
                save_agent_settings("openai".to_string(), providers).expect("Agent 配置应保存成功");
            assert_eq!(saved.gateway.active_provider, "openai");
            // 其他 section 必须原样保留，不能被 Agent 的旧快照覆盖。
            assert_eq!(saved.xiaozhi.say_token, "keep-say");
            assert_eq!(saved.asr.active_provider, "qwen");
            assert_eq!(saved.tts.active_provider, "openai");

            let loaded = crate::config::settings::load_settings()
                .unwrap()
                .expect("settings.toml 应存在");
            assert_eq!(loaded.gateway.active_provider, "openai");
            assert_eq!(loaded.gateway.providers["openai"]["model"], "example-model");
            assert_eq!(loaded.xiaozhi.say_token, "keep-say");
            assert_eq!(loaded.asr.active_provider, "qwen");
            assert_eq!(loaded.tts.active_provider, "openai");
        });
    }

    // ─── 热切换原子性（回滚）测试 ─────────────────────────

    use crate::gateway::provider::{AgentOutput, AgentProvider};
    use crate::test_util::run_with_temp_home_async;
    use async_trait::async_trait;

    struct MockAgent;

    #[async_trait]
    impl AgentProvider for MockAgent {
        fn name(&self) -> &str {
            "mock-agent"
        }

        async fn check_available(&self) -> Result<(), String> {
            Ok(())
        }

        async fn process(
            &self,
            _msg: &str,
            _session_id: Option<&str>,
            _work_dir: &str,
        ) -> Result<(AgentOutput, String), String> {
            Ok((AgentOutput::default(), "mock-session".to_string()))
        }
    }

    #[tokio::test]
    async fn test_update_unknown_provider_rolls_back() {
        run_with_temp_home_async(move |_home| async move {
            let shared = crate::gateway::agent_handle::into_shared(Box::new(MockAgent));
            let body = Json(serde_json::json!({ "active_provider": "does-not-exist" }));

            let result = update_agent_settings(axum::extract::State(shared.clone()), body).await;
            let (status, payload) = result.expect_err("未知 provider 应返回 400");
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let msg = payload.0["message"].as_str().unwrap_or_default();
            assert!(
                msg.contains("does-not-exist"),
                "错误信息应包含 provider 名，实际: {}",
                msg
            );

            // 回滚：共享状态保持原样（未换代）
            let snap = crate::gateway::agent_handle::snapshot(&shared);
            assert_eq!(snap.name, "mock-agent");
            assert_eq!(snap.generation, 0);
        })
        .await;
    }
}
