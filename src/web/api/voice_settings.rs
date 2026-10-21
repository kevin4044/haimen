//! ASR / TTS 语音配置 REST API
//!
//! 提供对 `settings.toml` 中 `[asr]` 和 `[tts]` 配置的读写接口。
//! ASR 和 TTS 端点均使用 axum State 持有共享 `Arc<RwLock<...>>`，
//! 在保存到磁盘后同步更新内存中的配置，实现运行时热加载。
//!
//! # 端点
//!
//! - `GET /api/v1/settings/asr` — 获取 ASR 配置（所有提供商 + 当前激活）
//! - `PUT /api/v1/settings/asr` — 更新 ASR 全部配置（providers + active_provider）
//! - `POST /api/v1/settings/asr/verify` — 验证指定提供商的凭证有效性
//! - `GET /api/v1/settings/tts` — 获取 TTS 配置（所有提供商 + 当前激活）
//! - `PUT /api/v1/settings/tts` — 更新 TTS 全部配置（providers + active_provider），
//!   同时更新内存中的共享配置，使策略热加载
//! - `GET /api/v1/settings/tts/voices` — 获取可用音色列表（支持 `?provider=` 参数）
//! - `POST /api/v1/settings/tts/verify` — 验证指定 TTS 提供商的凭证有效性

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::{Json, extract::Query, extract::State, http::StatusCode};

use crate::config::settings::AppConfig;
use crate::config::settings::AsrConfig;
use crate::config::settings::TtsConfig;
use crate::web::api::common::{SettingsUpdateError, parse_providers, update_settings};

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

/// 把共享设置写操作的错误映射成 HTTP 响应。
fn settings_update_error(e: SettingsUpdateError) -> (StatusCode, Json<serde_json::Value>) {
    let status = e.status();
    let message = e.message();
    (
        status,
        Json(serde_json::json!({
            "success": false,
            "error": message,
            "message": message,
        })),
    )
}

/// ASR 配置的 JSON 形态（GET 与 PUT 共用）。
fn asr_settings_json(cfg: &AsrConfig) -> serde_json::Value {
    serde_json::json!({
        "active_provider": cfg.active_provider,
        "providers": cfg.providers,
        "resolved": {
            "api_key": cfg.resolved_api_key().ok(),
        }
    })
}

/// TTS 配置的 JSON 形态（GET 与 PUT 共用）。
///
/// PUT 之前只回 active_provider/providers，导致前端保存后刷新前无法拿到
/// fixed_text/greeting/thinking 的最新值；这里统一返回完整配置。
fn tts_settings_json(cfg: &TtsConfig) -> serde_json::Value {
    serde_json::json!({
        "active_provider": cfg.active_provider,
        "providers": cfg.providers,
        "fixed_text_enabled": cfg.fixed_text_enabled,
        "fixed_text": cfg.fixed_text,
        "wake_greeting_enabled": cfg.wake_greeting_enabled,
        "wake_greeting": cfg.wake_greeting,
        "no_speech_goodbye": cfg.no_speech_goodbye,
        "no_speech_timeout_ms": cfg.no_speech_timeout_ms,
        "thinking_feedback_enabled": cfg.thinking_feedback_enabled,
        "thinking_feedback_interval_ms": cfg.thinking_feedback_interval_ms,
        "thinking_feedback_text": cfg.thinking_feedback_text,
        "thinking_feedback_timeout_text": cfg.thinking_feedback_timeout_text,
        "resolved": {
            "api_key": cfg.resolved_api_key().ok(),
            "voice": Some(cfg.resolved_voice()),
        }
    })
}

/// 把 PUT 请求体应用到 TTS 配置（纯内存修改，不写盘）。
fn apply_tts_update(cfg: &mut TtsConfig, body: &serde_json::Value) {
    if let Some(active) = body.get("active_provider").and_then(|v| v.as_str()) {
        if !active.is_empty() {
            cfg.active_provider = active.to_string();
        }
    }

    if let Some(providers) = parse_providers(body) {
        cfg.providers = providers;
    }

    if let Some(enabled) = body.get("fixed_text_enabled").and_then(|v| v.as_bool()) {
        cfg.fixed_text_enabled = enabled;
    }
    if body.get("fixed_text").is_some() {
        let text = body
            .get("fixed_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cfg.fixed_text = (!text.is_empty()).then(|| text.to_string());
    }

    if let Some(enabled) = body.get("wake_greeting_enabled").and_then(|v| v.as_bool()) {
        cfg.wake_greeting_enabled = enabled;
    }
    if body.get("wake_greeting").is_some() {
        let text = body
            .get("wake_greeting")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cfg.wake_greeting = (!text.is_empty()).then(|| text.to_string());
    }

    if body.get("no_speech_goodbye").is_some() {
        let text = body
            .get("no_speech_goodbye")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cfg.no_speech_goodbye = (!text.is_empty()).then(|| text.to_string());
    }
    if let Some(ms) = body.get("no_speech_timeout_ms").and_then(|v| v.as_u64()) {
        cfg.no_speech_timeout_ms = ms;
    }

    if let Some(enabled) = body
        .get("thinking_feedback_enabled")
        .and_then(|v| v.as_bool())
    {
        cfg.thinking_feedback_enabled = enabled;
    }
    if let Some(ms) = body
        .get("thinking_feedback_interval_ms")
        .and_then(|v| v.as_u64())
    {
        cfg.thinking_feedback_interval_ms = ms;
    }
    if body.get("thinking_feedback_text").is_some() {
        let text = body
            .get("thinking_feedback_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cfg.thinking_feedback_text = (!text.is_empty()).then(|| text.to_string());
    }
    if body.get("thinking_feedback_timeout_text").is_some() {
        let text = body
            .get("thinking_feedback_timeout_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        cfg.thinking_feedback_timeout_text = (!text.is_empty()).then(|| text.to_string());
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 豆包 TTS 模型 / 音色分类
// ═══════════════════════════════════════════════════════════════════════════════

/// 依据豆包音色 ID 后缀判断所属模型（火山引擎命名约定）
///
/// 2.0 线音色后缀为 `_uranus_bigtts` / `_jupiter_bigtts`，以及 `saturn_*_tob`；
/// 其余（`_moon_bigtts` / `_mars_bigtts` / `_emo_*` / `_conversation_*` 等）为 1.0 线。
fn doubao_voice_model(id: &str) -> &'static str {
    if id.ends_with("_uranus_bigtts")
        || id.ends_with("_jupiter_bigtts")
        || id.starts_with("saturn_")
    {
        "seed-tts-2.0"
    } else {
        "seed-tts-1.0"
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ASR 端点
// ═══════════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/settings/asr`
///
/// 返回完整的 ASR 配置：所有已配置的提供商凭证 + 当前激活的提供商。
/// 凭证字段返回明文，前端通过 `type="password"` 控制展示/隐藏。
///
/// 同时返回当前激活提供商的 resolved 值（配置 → 环境变量回退）。
pub async fn get_asr_settings() -> Json<serde_json::Value> {
    let cfg = load_config();
    Json(serde_json::json!({
        "success": true,
        "data": asr_settings_json(&cfg.asr),
    }))
}

/// `PUT /api/v1/settings/asr`
///
/// 替换完整的 ASR 配置。支持以下字段：
/// - `active_provider` — 切换当前激活的服务商
/// - `providers` — 所有提供商的完整凭证映射
///
/// 保存到磁盘后同步更新共享内存中的 ASR 配置，实现运行时热加载。
pub async fn update_asr_settings(
    State(asr_config): State<Arc<RwLock<AsrConfig>>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (cfg, _) = update_settings(|cfg| {
        if let Some(active) = body.get("active_provider").and_then(|v| v.as_str()) {
            if !active.is_empty() {
                cfg.asr.active_provider = active.to_string();
            }
        }

        // 空串会删除对应 key，运行时回退环境变量/默认值。
        if let Some(providers) = parse_providers(&body) {
            cfg.asr.providers = providers;
        }

        Ok(false)
    })
    .map_err(settings_update_error)?;

    // 热加载：更新共享内存中的 ASR 配置，下次 ASR 调用立即生效
    *asr_config.write().unwrap() = cfg.asr.clone();

    Ok(Json(serde_json::json!({
        "success": true,
        "data": asr_settings_json(&cfg.asr),
        "restart_required": false,
        "message": "已保存",
    })))
}

/// `POST /api/v1/settings/asr/verify`
///
/// 验证指定提供商的凭证是否有效。
///
/// 请求体包含 `provider` 字段和各提供商对应的凭证字段：
/// - doubao: `api_key`
/// - qwen / glm / mimo: `api_key`
/// - xfyun: `app_id` + `api_key` + `api_secret`
pub async fn verify_asr_credentials(
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("doubao");

    match provider {
        "doubao" => verify_doubao(&body).await,
        "qwen" => {
            verify_http_key(
                "qwen",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://dashscope.aliyuncs.com/api/v1/models",
            )
            .await
        }
        "glm" => {
            verify_http_key(
                "glm",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://open.bigmodel.cn/api/paas/v4/audio/transcriptions",
            )
            .await
        }
        "mimo" => {
            verify_http_key(
                "mimo",
                &body,
                "api_key",
                "api-key",
                "{key}",
                "https://api.xiaomimimo.com/v1/chat/completions",
            )
            .await
        }
        "xfyun" => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": "讯飞凭证验证需 WebSocket HMAC 鉴权，请在 Web UI 保存后直接测试语音识别功能".to_string() }
        }))),
        _ => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": format!("暂不支持验证 {} 提供商", provider) }
        }))),
    }
}

/// 通用的 HTTP API Key 验证：向指定 URL 发送带认证头的 GET 请求，
/// 非 401 响应即视为凭证有效。
async fn verify_http_key(
    provider: &str,
    body: &serde_json::Value,
    key_field: &str,
    header_name: &str,
    header_template: &str,
    url: &str,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body
        .get(key_field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("缺少 {key_field}")
                })),
            )
        })?;

    let header_value = header_template.replace("{key}", api_key);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "success": false, "error": e.to_string() })),
            )
        })?;

    let response = client
        .get(url)
        .header(header_name, &header_value)
        .send()
        .await;

    match response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Ok(Json(serde_json::json!({
                    "success": true,
                    "data": { "valid": false, "message": format!("{} API Key 无效 (HTTP {})", provider, status) }
                })))
            } else {
                // 200、4xx（非401）、5xx 都认为凭证格式有效（服务可达）
                Ok(Json(serde_json::json!({
                    "success": true,
                    "data": { "valid": true, "message": format!("{} 凭证格式正确，服务可达", provider) }
                })))
            }
        }
        Err(e) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": format!("无法连接到 {} 服务: {}", provider, e) }
        }))),
    }
}

/// 验证 Doubao 凭证（通过 TTS 合成测试）
async fn verify_doubao(
    body: &serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body
        .get("api_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "缺少 api_key"
                })),
            )
        })?;

    // ASR 侧仅验证 token 有效性，使用默认 TTS 资源与音色
    match verify_doubao_token(api_key, "seed-tts-2.0", "zh_female_xiaohe_uranus_bigtts").await {
        Ok(()) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": true, "message": "凭证验证成功" }
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": format!("凭证验证失败: {}", e) }
        }))),
    }
}

/// 用提供的凭证调用 Doubao TTS 合成测试音频验证有效性
async fn verify_doubao_token(api_key: &str, resource_id: &str, voice: &str) -> Result<(), String> {
    use univoice::tts::provider::{DoubaoTts, DoubaoTtsOption};
    use univoice::tts::{BaseTtsOption, TtsProvider, TtsRequest, VoiceId};

    let tts = DoubaoTts::new(DoubaoTtsOption {
        base: BaseTtsOption {
            format: Some("pcm".into()),
            voice: Some(VoiceId::from(voice)),
            ..Default::default()
        },
        api_key: Some(api_key.to_string()),
        resource_id: Some(resource_id.to_string()),
        ..Default::default()
    });

    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tts.synthesize(TtsRequest {
            text: "测".to_string(),
            options: None,
        }),
    )
    .await
    {
        Ok(Ok(response)) if !response.audio.is_empty() => Ok(()),
        Ok(Ok(_)) => Err("TTS 返回空音频，请检查凭证".to_string()),
        Ok(Err(e)) => Err(format!("TTS 请求失败: {}", e)),
        Err(_) => Err("TTS 请求超时 (15s)".to_string()),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TTS 端点
// ═══════════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/settings/tts`
///
/// 返回完整的 TTS 配置：所有已配置的提供商凭证 + 当前激活的提供商。
/// 凭证字段返回明文，前端通过 `type="password"` 控制展示/隐藏。
///
/// 同时返回当前激活提供商的 resolved 值（配置 → 环境变量回退）。
pub async fn get_tts_settings() -> Json<serde_json::Value> {
    let cfg = load_config();
    Json(serde_json::json!({
        "success": true,
        "data": tts_settings_json(&cfg.tts),
    }))
}

/// `PUT /api/v1/settings/tts`
///
/// 替换完整的 TTS 配置。支持以下字段：
/// - `active_provider` — 切换当前激活的服务商
/// - `providers` — 所有提供商的完整凭证映射
/// - `fixed_text_enabled` — 是否启用固定文本模式
/// - `fixed_text` — 固定文本内容
/// - `wake_greeting_enabled` — 是否在设备唤醒时主动播报问候
/// - `wake_greeting` — 唤醒问候文案（空串视为未设置，回退「你好」）
/// - `no_speech_goodbye` — 无语音超时后的告别文案（空串视为未设置，回退「拜拜」）
/// - `no_speech_timeout_ms` — 录音开始后累计无有效语音达到该毫秒数时播报告别并关闭连接（0 禁用）
/// - `thinking_feedback_enabled` — 是否在等待 Agent 首个可播文本期间播报处理进度提示
/// - `thinking_feedback_interval_ms` — 进度提示播报间隔毫秒（0 表示仅保留超时兜底、不做周期提示）
/// - `thinking_feedback_text` — 周期进度提示文案（空串视为未设置，回退默认）
/// - `thinking_feedback_timeout_text` — 等待超时的兜底文案（空串视为未设置，回退默认）
pub async fn update_tts_settings(
    State(tts_config): State<Arc<RwLock<TtsConfig>>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (cfg, _) = update_settings(|cfg| {
        apply_tts_update(&mut cfg.tts, &body);
        Ok(false)
    })
    .map_err(settings_update_error)?;

    // 更新内存中的共享 TTS 配置，使运行时策略即时感知变更
    *tts_config.write().unwrap() = cfg.tts.clone();

    Ok(Json(serde_json::json!({
        "success": true,
        "data": tts_settings_json(&cfg.tts),
        "restart_required": false,
        "message": "已保存",
    })))
}

/// `GET /api/v1/settings/tts/voices`
///
/// 获取指定提供商的可用音色列表。通过 `?provider=doubao` 查询参数指定。
/// 默认返回当前激活提供商的音色。
///
/// 支持 `?provider=` 和 `?model=` 查询参数。豆包每个音色响应附带 `model` 字段；
/// 提供 `model` 参数时按模型过滤，缺省时返回全部音色（由前端按音色所属模型分组，
/// 选择音色即自动确定模型）；其他提供商的 `model` 参数被忽略。
pub async fn list_tts_voices(params: Query<HashMap<String, String>>) -> Json<serde_json::Value> {
    let cfg = load_config();
    let provider = params
        .get("provider")
        .map(String::as_str)
        .unwrap_or_else(|| cfg.tts.active_provider.as_str());

    let model = params.get("model").map(String::as_str);

    let (voices, resp_model, is_doubao) = match provider {
        "doubao" => {
            let list = univoice::tts::voices::doubao::list_voices();
            let list = match model {
                Some(m) => list
                    .into_iter()
                    .filter(|v| doubao_voice_model(&v.id) == m)
                    .collect::<Vec<_>>(),
                None => list,
            };
            (list, model, true)
        }
        "qwen" => (univoice::tts::voices::qwen3_tts::list_voices(), None, false),
        "glm" => (univoice::tts::voices::glm::list_voices(), None, false),
        "minimax" => (univoice::tts::voices::minimax::list_voices(), None, false),
        "qwen_realtime" => (univoice::tts::voices::qwen3_tts::list_voices(), None, false),
        _ => (Vec::new(), None, false),
    };

    Json(serde_json::json!({
        "success": true,
        "data": {
            "provider": provider,
            "model": resp_model,
            "voices": voices.iter().map(|v| {
                let mut obj = serde_json::json!({
                    "id": v.id,
                    "name": v.name,
                    "language": v.language,
                });
                if is_doubao {
                    obj["model"] = serde_json::json!(doubao_voice_model(&v.id));
                }
                obj
            }).collect::<Vec<_>>(),
        }
    }))
}

/// `POST /api/v1/settings/tts/verify`
///
/// 验证指定 TTS 提供商的凭证是否有效。
///
/// 请求体包含 `provider` 字段和各提供商对应的凭证字段：
/// - doubao: `api_key`
/// - qwen / glm / openai / minimax / gemini: `api_key`
/// - xfyun: `app_id` + `api_key` + `api_secret`
pub async fn verify_tts_credentials(
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("doubao");

    match provider {
        "doubao" => verify_tts_doubao(&body).await,
        "qwen" => {
            verify_http_key(
                "qwen",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://dashscope.aliyuncs.com/api/v1/models",
            )
            .await
        }
        "glm" => {
            verify_http_key(
                "glm",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://open.bigmodel.cn/api/paas/v4/audio/transcriptions",
            )
            .await
        }
        "openai" => {
            verify_http_key(
                "openai",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://api.openai.com/v1/models",
            )
            .await
        }
        "minimax" => {
            verify_http_key(
                "minimax",
                &body,
                "api_key",
                "Authorization",
                "Bearer {key}",
                "https://api.minimax.chat/v1/text/chatcompletion_v2",
            )
            .await
        }
        "gemini" => {
            verify_http_key(
                "gemini",
                &body,
                "api_key",
                "x-goog-api-key",
                "{key}",
                "https://generativelanguage.googleapis.com/v1/models",
            )
            .await
        }
        "xfyun" => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": "讯飞凭证验证需 WebSocket HMAC 鉴权，请在 Web UI 保存后直接测试语音合成功能".to_string() }
        }))),
        _ => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": format!("暂不支持验证 {} 提供商", provider) }
        }))),
    }
}

/// 用提供的凭证调用 Doubao TTS 合成测试音频验证有效性
///
/// 复用 ASR 端的 verify_doubao 逻辑（TTS 合成测试）
async fn verify_tts_doubao(
    body: &serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let api_key = body
        .get("api_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "缺少 api_key"
                })),
            )
        })?;
    // 用表单里配置的模型（resource_id）与音色做真实合成验证，
    // 避免硬编码 2.0 导致「验证通过但实际合成 55000000」的假通过。
    let resource_id = body
        .get("resource_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("seed-tts-2.0");
    let voice = body
        .get("voice")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("zh_female_xiaohe_uranus_bigtts");

    match verify_doubao_token(api_key, resource_id, voice).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": true, "message": "凭证验证成功" }
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": true,
            "data": { "valid": false, "message": format!("凭证验证失败: {}", e) }
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
                "doubao": {
                    "api_key": "key1"
                },
                "qwen": {
                    "api_key": "qwen-key"
                }
            }
        });
        let providers = parse_providers(&json).unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(
            providers.get("doubao").unwrap().get("api_key").unwrap(),
            "key1"
        );
        assert_eq!(
            providers.get("qwen").unwrap().get("api_key").unwrap(),
            "qwen-key"
        );
    }

    #[test]
    fn test_parse_providers_skips_empty() {
        let json = serde_json::json!({
            "providers": {
                "doubao": {
                    "api_key": "key1",
                    "resource_id": ""
                }
            }
        });
        let providers = parse_providers(&json).unwrap();
        let doubao = providers.get("doubao").unwrap();
        assert_eq!(doubao.get("api_key").unwrap(), "key1");
        assert!(doubao.get("resource_id").is_none());
    }

    #[test]
    fn test_parse_providers_missing() {
        let json = serde_json::json!({"other": "value"});
        assert!(parse_providers(&json).is_none());
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    #[test]
    fn update_tts_clears_empty_provider_key_and_returns_full_settings() {
        use crate::test_util::run_with_temp_home;

        run_with_temp_home(|_| {
            let shared = Arc::new(RwLock::new(TtsConfig::default()));

            // 第一次写入完整 map（UI 每次提交的是当前 Tab 的完整字段）。
            let first = serde_json::json!({
                "active_provider": "doubao",
                "providers": {
                    "doubao": {
                        "api_key": "secret",
                        "voice": "zh_female_vv_uranus_bigtts"
                    }
                },
                "fixed_text_enabled": true,
                "fixed_text": "固定文本",
                "thinking_feedback_enabled": false,
                "thinking_feedback_interval_ms": 5000,
                "thinking_feedback_text": "请稍等",
                "thinking_feedback_timeout_text": "还在处理"
            });
            let res = block_on(update_tts_settings(State(shared.clone()), Json(first)))
                .expect("首次保存应成功");
            assert_eq!(res.0["restart_required"], false);
            assert_eq!(res.0["data"]["fixed_text_enabled"], true);
            assert_eq!(res.0["data"]["thinking_feedback_interval_ms"], 5000);
            assert_eq!(res.0["data"]["thinking_feedback_text"], "请稍等");

            // 清空 api_key：key 应被剪掉，voice 保留；运行时回退环境变量。
            let second = serde_json::json!({
                "providers": {
                    "doubao": {
                        "api_key": "",
                        "voice": "zh_female_vv_uranus_bigtts"
                    }
                }
            });
            let res = block_on(update_tts_settings(State(shared), Json(second)))
                .expect("清空 key 应成功");
            let doubao = &res.0["data"]["providers"]["doubao"];
            assert!(doubao.get("api_key").is_none(), "空串应删除 api_key");
            assert_eq!(doubao["voice"], "zh_female_vv_uranus_bigtts");
            // 未提供的顶层字段保持不变
            assert_eq!(res.0["data"]["fixed_text_enabled"], true);
            assert_eq!(res.0["data"]["thinking_feedback_enabled"], false);
        });
    }

    #[test]
    fn update_asr_clears_empty_provider_key() {
        use crate::test_util::run_with_temp_home;

        run_with_temp_home(|_| {
            let shared = Arc::new(RwLock::new(AsrConfig::default()));
            let first = serde_json::json!({
                "active_provider": "doubao",
                "providers": { "doubao": { "api_key": "secret", "resource_id": "volc.seedasr.sauc.duration" } }
            });
            let _ = block_on(update_asr_settings(State(shared.clone()), Json(first)))
                .expect("保存成功");

            let second = serde_json::json!({
                "providers": { "doubao": { "api_key": "", "resource_id": "volc.seedasr.sauc.duration" } }
            });
            let res = block_on(update_asr_settings(State(shared), Json(second))).expect("清空成功");
            let doubao = &res.0["data"]["providers"]["doubao"];
            assert!(doubao.get("api_key").is_none());
            assert_eq!(doubao["resource_id"], "volc.seedasr.sauc.duration");
        });
    }

    // ─── doubao_voice_model 分类器 ──────────────────────────

    #[test]
    fn test_doubao_voice_model_2_0_suffixes() {
        assert_eq!(
            doubao_voice_model("zh_female_xiaohe_uranus_bigtts"),
            "seed-tts-2.0"
        );
        assert_eq!(
            doubao_voice_model("zh_female_vv_jupiter_bigtts"),
            "seed-tts-2.0"
        );
        assert_eq!(
            doubao_voice_model("saturn_zh_female_keainvsheng_tob"),
            "seed-tts-2.0"
        );
    }

    #[test]
    fn test_doubao_voice_model_1_0_suffixes() {
        assert_eq!(
            doubao_voice_model("zh_female_wanwanxiaohe_moon_bigtts"),
            "seed-tts-1.0"
        );
        assert_eq!(
            doubao_voice_model("zh_male_zhubajie_mars_bigtts"),
            "seed-tts-1.0"
        );
        assert_eq!(
            doubao_voice_model("zh_male_lengkugege_emo_v2_mars_bigtts"),
            "seed-tts-1.0"
        );
        assert_eq!(
            doubao_voice_model("zh_male_xudong_conversation_wvae_bigtts"),
            "seed-tts-1.0"
        );
    }
}
