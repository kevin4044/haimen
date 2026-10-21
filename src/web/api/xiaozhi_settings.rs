//! 小智传输 / OTA / 播报鉴权配置 REST API
//!
//! 只管理 `settings.toml` 中 `[xiaozhi]` 的**传输与安全**字段：
//! `protocol`、`mqtt`（broker / 主题模板 / 鉴权）、`udp.bind_addr`、
//! `ota_allow_private_only`、`ota_token`、`ota_trusted_proxies`、`say_token`。
//!
//! 设备身份、名字、房间仍由 [`crate::web::api::xiaozhi_devices`] 管理
//! （`/api/v1/settings/xiaozhi/devices`），本模块**不碰** `devices`，
//! 避免两个端点互相覆盖对方的字段。
//!
//! # 端点
//!
//! - `GET /api/v1/settings/xiaozhi` — 读取当前配置（密钥按项目惯例明文回显）
//! - `PUT /api/v1/settings/xiaozhi` — 部分更新；只更新请求体里出现的字段
//!
//! # 为什么传输配置不能热生效
//!
//! MQTT broker / UDP 监听是在进程启动时 bind 的，改 `protocol`、`mqtt` 或
//! `udp` 后必须重启 haimen 才会真正监听新地址。此时如果立刻把新配置写进
//! OTA 全局槽，设备会拿到「指向一个还没起来的 broker」的配置而彻底失联。
//! 所以传输配置变化时只写盘、不热更新 OTA，并在响应里要求重启；
//! 只改 say/ota 时则立即热更新，无需重启。

use std::net::{IpAddr, SocketAddr};

use axum::{Json, http::StatusCode};

use haimen_xiaozhi::{XiaozhiConfig, XiaozhiProtocol};

use crate::web::api::common::{SettingsUpdateError, load_strict, update_settings};

type ApiResult = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

fn error_response(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
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

/// 把 settings 读写错误映射成 HTTP 响应。
///
/// `settings.toml` 无法解析属于客户端需要先修复的冲突，用 409；
/// 写盘失败才是 500。
fn settings_error(e: String) -> (StatusCode, Json<serde_json::Value>) {
    error_response(StatusCode::CONFLICT, e)
}

/// 共享写操作的错误已带分类，直接按状态码返回。
fn settings_update_error(e: SettingsUpdateError) -> (StatusCode, Json<serde_json::Value>) {
    error_response(e.status(), e.message())
}

// ═══════════════════════════════════════════════════════════════════════════════
// JSON 序列化
// ═══════════════════════════════════════════════════════════════════════════════

fn protocol_str(protocol: XiaozhiProtocol) -> &'static str {
    match protocol {
        XiaozhiProtocol::Websocket => "websocket",
        XiaozhiProtocol::Mqtt => "mqtt",
    }
}

/// 配置的 JSON 视图（不含 `devices`）。
///
/// 密钥字段按项目现有约定明文返回，前端用 `type="password"` 控制展示。
fn settings_json(cfg: &XiaozhiConfig) -> serde_json::Value {
    serde_json::json!({
        "protocol": protocol_str(cfg.protocol),
        "mqtt": {
            "endpoint": cfg.mqtt.endpoint,
            "username": cfg.mqtt.username,
            "password": cfg.mqtt.password,
            "auth_key": cfg.mqtt.auth_key,
            "auth_grace": cfg.mqtt.auth_grace,
            "keepalive": cfg.mqtt.keepalive,
            "publish_topic_template": cfg.mqtt.publish_topic_template,
            "downlink_topic_template": cfg.mqtt.downlink_topic_template,
        },
        "udp": {
            "bind_addr": cfg.udp.bind_addr,
            "tts_start_settle_ms": cfg.udp.tts_start_settle_ms,
        },
        "ota_allow_private_only": cfg.ota_allow_private_only,
        "ota_token": cfg.ota_token,
        "ota_trusted_proxies": cfg
            .ota_trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "say_token": cfg.say_token,
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// 请求体合并与校验
// ═══════════════════════════════════════════════════════════════════════════════

fn expect_str<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{field} 必须是字符串"))
}

fn expect_bool(value: &serde_json::Value, field: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{field} 必须是布尔值"))
}

fn parse_protocol(value: &serde_json::Value) -> Result<XiaozhiProtocol, String> {
    match value.as_str().map(str::trim) {
        Some("websocket") => Ok(XiaozhiProtocol::Websocket),
        Some("mqtt") => Ok(XiaozhiProtocol::Mqtt),
        _ => Err("protocol 只能是 websocket 或 mqtt".to_string()),
    }
}

/// 把请求体里出现的字段合并进 `cfg`。`mqtt` / `udp` 子对象也按字段部分合并。
///
/// 只做「类型 + 单字段解析」，跨字段规则由 [`validate`] 统一检查，
/// 这样校验逻辑可以脱离 HTTP 单独测试。
fn apply_patch(cfg: &mut XiaozhiConfig, body: &serde_json::Value) -> Result<(), String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;

    if let Some(value) = obj.get("protocol") {
        cfg.protocol = parse_protocol(value)?;
    }

    if let Some(value) = obj.get("mqtt") {
        let mqtt = value
            .as_object()
            .ok_or_else(|| "mqtt 必须是 JSON 对象".to_string())?;

        if let Some(value) = mqtt.get("endpoint") {
            cfg.mqtt.endpoint = expect_str(value, "mqtt.endpoint")?.to_string();
        }
        if let Some(value) = mqtt.get("username") {
            cfg.mqtt.username = expect_str(value, "mqtt.username")?.to_string();
        }
        if let Some(value) = mqtt.get("password") {
            cfg.mqtt.password = expect_str(value, "mqtt.password")?.to_string();
        }
        if let Some(value) = mqtt.get("auth_key") {
            cfg.mqtt.auth_key = expect_str(value, "mqtt.auth_key")?.to_string();
        }
        if let Some(value) = mqtt.get("auth_grace") {
            cfg.mqtt.auth_grace = expect_bool(value, "mqtt.auth_grace")?;
        }
        if let Some(value) = mqtt.get("keepalive") {
            let keepalive = value
                .as_u64()
                .ok_or_else(|| "mqtt.keepalive 必须是非负整数".to_string())?;
            cfg.mqtt.keepalive =
                u32::try_from(keepalive).map_err(|_| "mqtt.keepalive 超出范围".to_string())?;
        }
        if let Some(value) = mqtt.get("publish_topic_template") {
            cfg.mqtt.publish_topic_template =
                expect_str(value, "mqtt.publish_topic_template")?.to_string();
        }
        if let Some(value) = mqtt.get("downlink_topic_template") {
            cfg.mqtt.downlink_topic_template =
                expect_str(value, "mqtt.downlink_topic_template")?.to_string();
        }
    }

    if let Some(value) = obj.get("udp") {
        let udp = value
            .as_object()
            .ok_or_else(|| "udp 必须是 JSON 对象".to_string())?;
        if let Some(value) = udp.get("bind_addr") {
            cfg.udp.bind_addr = expect_str(value, "udp.bind_addr")?.to_string();
        }
        if let Some(value) = udp.get("tts_start_settle_ms") {
            cfg.udp.tts_start_settle_ms = value
                .as_u64()
                .ok_or_else(|| "udp.tts_start_settle_ms 必须是非负整数".to_string())?;
        }
    }

    if let Some(value) = obj.get("ota_allow_private_only") {
        cfg.ota_allow_private_only = expect_bool(value, "ota_allow_private_only")?;
    }
    if let Some(value) = obj.get("ota_token") {
        cfg.ota_token = expect_str(value, "ota_token")?.to_string();
    }
    if let Some(value) = obj.get("say_token") {
        cfg.say_token = expect_str(value, "say_token")?.to_string();
    }
    if let Some(value) = obj.get("ota_trusted_proxies") {
        let list = value
            .as_array()
            .ok_or_else(|| "ota_trusted_proxies 必须是数组".to_string())?;
        let mut proxies = Vec::with_capacity(list.len());
        for (index, item) in list.iter().enumerate() {
            let text = item
                .as_str()
                .ok_or_else(|| format!("ota_trusted_proxies[{index}] 必须是字符串"))?;
            let ip = text
                .trim()
                .parse::<IpAddr>()
                .map_err(|_| format!("ota_trusted_proxies[{index}] 不是合法 IP：{text}"))?;
            proxies.push(ip);
        }
        cfg.ota_trusted_proxies = proxies;
    }

    Ok(())
}

/// 跨字段校验。返回 `Err` 时调用方必须以 400 响应且不得写盘。
fn validate(cfg: &XiaozhiConfig) -> Result<(), String> {
    // protocol 反序列化/parse_protocol 已保证取值合法，这里只做跨字段约束。
    if cfg.protocol == XiaozhiProtocol::Mqtt && cfg.mqtt.endpoint.trim().is_empty() {
        return Err("protocol=mqtt 时 mqtt.endpoint 不能为空".to_string());
    }
    if cfg.mqtt.keepalive < 1 {
        return Err("mqtt.keepalive 必须 >= 1".to_string());
    }
    for (field, template) in [
        (
            "mqtt.publish_topic_template",
            &cfg.mqtt.publish_topic_template,
        ),
        (
            "mqtt.downlink_topic_template",
            &cfg.mqtt.downlink_topic_template,
        ),
    ] {
        if !template.contains("{mac}") {
            return Err(format!("{field} 必须包含 {{mac}} 占位符"));
        }
    }
    cfg.udp
        .bind_addr
        .trim()
        .parse::<SocketAddr>()
        .map_err(|_| format!("udp.bind_addr 不是合法的 SocketAddr：{}", cfg.udp.bind_addr))?;
    // 1000ms 已远超「等固件切 Speaking」的物理需要，再大只会平白增加首帧延迟。
    if cfg.udp.tts_start_settle_ms > 1000 {
        return Err("udp.tts_start_settle_ms 不能超过 1000（推荐 0–300）".to_string());
    }
    Ok(())
}

/// 传输层配置是否变化（决定是否需要重启 + 是否可以热更新 OTA）。
fn transport_changed(old: &XiaozhiConfig, new: &XiaozhiConfig) -> bool {
    old.protocol != new.protocol || old.mqtt != new.mqtt || old.udp != new.udp
}

// ═══════════════════════════════════════════════════════════════════════════════
// 路由处理函数
// ═══════════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/settings/xiaozhi`
///
/// 返回传输 / OTA / say 配置；`restart_required` 固定为 false（读取不改变运行态）。
pub async fn get_xiaozhi_settings() -> ApiResult {
    let cfg = load_strict().map_err(settings_error)?;
    Ok(Json(serde_json::json!({
        "success": true,
        "data": settings_json(&cfg.xiaozhi),
        "restart_required": false,
    })))
}

/// `PUT /api/v1/settings/xiaozhi`
///
/// 部分更新：只有请求体里出现的字段才会被修改，`mqtt` / `udp` 子对象同样
/// 按字段合并。校验失败返回 400 且不写盘；`settings.toml` 本身非法时返回
/// 409 且保留原文件。
///
/// 保存成功后的热更新规则：
/// - 总是更新 say_token（立即生效）；
/// - protocol/mqtt/udp 未变时热更新 OTA 配置（say/ota 类字段立即生效）；
/// - 传输配置变化时不热更新 OTA，返回 `restart_required=true`，避免把设备
///   指向尚未监听的 broker。
pub async fn update_xiaozhi_settings(Json(body): Json<serde_json::Value>) -> ApiResult {
    let (cfg, restart_required) = update_settings(|cfg| {
        let old = cfg.xiaozhi.clone();
        apply_patch(&mut cfg.xiaozhi, &body)?;
        validate(&cfg.xiaozhi)?;
        Ok(transport_changed(&old, &cfg.xiaozhi))
    })
    .map_err(settings_update_error)?;

    // say_token 与传输层无关，随时可以热更新。
    haimen_xiaozhi::notify::set_say_token(cfg.xiaozhi.say_token.clone());

    // 传输配置变了：broker/UDP 还没起来，绝不能把新配置下发给设备。
    if !restart_required {
        haimen_xiaozhi::set_xiaozhi_ota_config(cfg.xiaozhi.clone());
    }

    let mut payload = serde_json::json!({
        "success": true,
        "data": settings_json(&cfg.xiaozhi),
        "restart_required": restart_required,
    });
    if restart_required {
        payload["message"] =
            serde_json::json!("已保存；MQTT/UDP 传输配置需重启 haimen，设备需重新拉取 OTA");
    }
    Ok(Json(payload))
}

// ═══════════════════════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::run_with_temp_home;

    /// 在同步闭包中执行异步接口（run_with_temp_home 的闭包是同步的）
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    /// say_token / OTA 全局槽是进程级的：每个用例前后都复位，避免污染其它测试。
    fn reset_runtime_globals() {
        haimen_xiaozhi::notify::set_say_token(String::new());
        haimen_xiaozhi::set_xiaozhi_ota_config(XiaozhiConfig::default());
    }

    fn put(body: serde_json::Value) -> ApiResult {
        block_on(update_xiaozhi_settings(Json(body)))
    }

    #[test]
    fn get_returns_defaults_when_settings_file_missing() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let res =
                block_on(get_xiaozhi_settings()).expect("缺少 settings.toml 时应回退默认配置");
            assert_eq!(res.0["success"], true);
            assert_eq!(res.0["restart_required"], false);

            let data = &res.0["data"];
            assert_eq!(data["protocol"], "websocket");
            assert_eq!(data["mqtt"]["endpoint"], "");
            assert_eq!(data["mqtt"]["username"], "");
            assert_eq!(data["mqtt"]["password"], "");
            assert_eq!(data["mqtt"]["auth_key"], "");
            assert_eq!(data["mqtt"]["auth_grace"], false);
            assert_eq!(data["mqtt"]["keepalive"], 240);
            assert_eq!(data["mqtt"]["publish_topic_template"], "haimen/xz/{mac}/up");
            assert_eq!(
                data["mqtt"]["downlink_topic_template"],
                "haimen/xz/{mac}/down"
            );
            assert_eq!(data["udp"]["bind_addr"], "0.0.0.0:8888");
            assert_eq!(
                data["udp"]["tts_start_settle_ms"],
                haimen_xiaozhi::DEFAULT_TTS_START_SETTLE_MS
            );
            assert_eq!(data["ota_allow_private_only"], true);
            assert_eq!(data["ota_token"], "");
            assert_eq!(data["say_token"], "");
            assert_eq!(data["ota_trusted_proxies"], serde_json::json!([]));
            // 设备表由 /settings/xiaozhi/devices 管理，这里不应出现。
            assert!(data.get("devices").is_none());

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_partial_update_keeps_other_fields() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let first = put(serde_json::json!({
                "protocol": "mqtt",
                "mqtt": {
                    "endpoint": "192.168.110.35:1883",
                    "username": "haimen",
                    "password": "mqtt-secret",
                    "auth_key": "auth-secret",
                    "auth_grace": true,
                    "keepalive": 120,
                },
                "udp": { "bind_addr": "0.0.0.0:9999", "tts_start_settle_ms": 150 },
                "ota_allow_private_only": true,
                "ota_token": "ota-secret",
                "ota_trusted_proxies": ["127.0.0.1", "::1"],
                "say_token": "say-secret",
            }))
            .expect("完整配置应保存成功");
            assert_eq!(first.0["restart_required"], true);
            assert!(
                first.0["message"].as_str().unwrap_or("").contains("MQTT"),
                "{}",
                first.0["message"]
            );

            // 只改 say_token：mqtt / udp / ota 字段必须原样保留。
            let second = put(serde_json::json!({ "say_token": "new-say-token" }))
                .expect("部分更新应保存成功");
            assert_eq!(second.0["success"], true);
            assert_eq!(second.0["restart_required"], false);
            assert!(second.0.get("message").is_none());

            let data = &second.0["data"];
            assert_eq!(data["say_token"], "new-say-token");
            assert_eq!(data["protocol"], "mqtt");
            assert_eq!(data["mqtt"]["endpoint"], "192.168.110.35:1883");
            assert_eq!(data["mqtt"]["username"], "haimen");
            assert_eq!(data["mqtt"]["password"], "mqtt-secret");
            assert_eq!(data["mqtt"]["auth_key"], "auth-secret");
            assert_eq!(data["mqtt"]["auth_grace"], true);
            assert_eq!(data["mqtt"]["keepalive"], 120);
            assert_eq!(data["udp"]["bind_addr"], "0.0.0.0:9999");
            assert_eq!(data["udp"]["tts_start_settle_ms"], 150);
            assert_eq!(data["ota_token"], "ota-secret");
            assert_eq!(
                data["ota_trusted_proxies"],
                serde_json::json!(["127.0.0.1", "::1"])
            );

            // 落盘内容与响应一致（部分更新没有把其它字段写回默认值）。
            let saved = crate::config::settings::load_settings()
                .unwrap()
                .expect("settings.toml 应已写入");
            assert_eq!(saved.xiaozhi.protocol, XiaozhiProtocol::Mqtt);
            assert_eq!(saved.xiaozhi.mqtt.endpoint, "192.168.110.35:1883");
            assert_eq!(saved.xiaozhi.mqtt.keepalive, 120);
            assert_eq!(saved.xiaozhi.udp.bind_addr, "0.0.0.0:9999");
            assert_eq!(saved.xiaozhi.udp.tts_start_settle_ms, 150);
            assert_eq!(saved.xiaozhi.say_token, "new-say-token");
            assert_eq!(saved.xiaozhi.ota_token, "ota-secret");

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_mqtt_without_endpoint_returns_400_and_does_not_write() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let (status, body) = put(serde_json::json!({ "protocol": "mqtt" })).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body.0["success"], false);
            assert!(
                body.0["error"].as_str().unwrap_or("").contains("endpoint"),
                "{}",
                body.0["error"]
            );
            // 校验失败不能写盘（文件应仍不存在）。
            assert!(
                crate::config::settings::load_settings().unwrap().is_none(),
                "校验失败的请求不应写入 settings.toml"
            );

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_udp_settle_over_limit_returns_400() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let (status, body) = put(serde_json::json!({
                "protocol": "mqtt",
                "mqtt": { "endpoint": "192.168.110.35:1883" },
                "udp": { "tts_start_settle_ms": 5000 },
            }))
            .unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                body.0["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("tts_start_settle_ms"),
                "{}",
                body.0["error"]
            );

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_invalid_trusted_proxy_returns_400() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let (status, body) = put(serde_json::json!({
                "ota_trusted_proxies": ["127.0.0.1", "not-an-ip"],
            }))
            .unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                body.0["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("ota_trusted_proxies"),
                "{}",
                body.0["error"]
            );
            assert!(
                crate::config::settings::load_settings().unwrap().is_none(),
                "校验失败的请求不应写入 settings.toml"
            );

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_valid_save_persists_and_reports_restart() {
        run_with_temp_home(|_| {
            reset_runtime_globals();

            let res = put(serde_json::json!({
                "protocol": "mqtt",
                "mqtt": { "endpoint": "192.168.110.35:1883" },
                "say_token": "say-secret",
            }))
            .expect("合法配置应保存成功");

            assert_eq!(res.0["success"], true);
            assert_eq!(res.0["restart_required"], true);
            assert!(
                res.0["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("重启 haimen"),
                "{}",
                res.0["message"]
            );

            let saved = crate::config::settings::load_settings()
                .unwrap()
                .expect("settings.toml 应已写入");
            assert_eq!(saved.xiaozhi.protocol, XiaozhiProtocol::Mqtt);
            assert_eq!(saved.xiaozhi.mqtt.endpoint, "192.168.110.35:1883");
            assert_eq!(saved.xiaozhi.say_token, "say-secret");
            // 主题模板等未提交字段保持默认值。
            assert_eq!(
                saved.xiaozhi.mqtt.publish_topic_template,
                "haimen/xz/{mac}/up"
            );

            // GET 按项目惯例明文回显密钥。
            let get = block_on(get_xiaozhi_settings()).unwrap();
            assert_eq!(get.0["data"]["say_token"], "say-secret");
            assert_eq!(get.0["data"]["mqtt"]["endpoint"], "192.168.110.35:1883");

            reset_runtime_globals();
        });
    }

    #[test]
    fn put_invalid_settings_toml_returns_409_without_overwriting() {
        run_with_temp_home(|home| {
            reset_runtime_globals();

            let dir = home.join(".haimen");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("settings.toml");
            let original = "protocol = [\n";
            std::fs::write(&path, original).unwrap();

            let (status, body) =
                put(serde_json::json!({ "say_token": "should-not-save" })).unwrap_err();
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body.0["success"], false);
            assert!(
                body.0["error"].as_str().unwrap_or("").contains("解析失败"),
                "{}",
                body.0["error"]
            );
            // 非法 TOML 必须原样保留，不能被默认配置覆盖。
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

            reset_runtime_globals();
        });
    }

    /// 纯函数覆盖各条校验规则，不触碰全局槽 / 文件系统。
    #[test]
    fn patch_and_validate_reject_invalid_values() {
        let mut cfg = XiaozhiConfig::default();
        assert!(apply_patch(&mut cfg, &serde_json::json!({ "protocol": "coap" })).is_err());
        // 非法 protocol 不应改动原值。
        assert_eq!(cfg.protocol, XiaozhiProtocol::Websocket);

        let mut cfg = XiaozhiConfig::default();
        apply_patch(&mut cfg, &serde_json::json!({ "protocol": "mqtt" })).unwrap();
        assert!(validate(&cfg).is_err(), "mqtt 缺 endpoint 应校验失败");

        let mut cfg = XiaozhiConfig::default();
        apply_patch(&mut cfg, &serde_json::json!({ "mqtt": { "keepalive": 0 } })).unwrap();
        assert!(validate(&cfg).is_err(), "keepalive < 1 应校验失败");

        let mut cfg = XiaozhiConfig::default();
        apply_patch(
            &mut cfg,
            &serde_json::json!({ "mqtt": { "publish_topic_template": "no-placeholder" } }),
        )
        .unwrap();
        assert!(validate(&cfg).is_err(), "主题模板缺 {{mac}} 应校验失败");

        let mut cfg = XiaozhiConfig::default();
        apply_patch(
            &mut cfg,
            &serde_json::json!({ "mqtt": { "downlink_topic_template": "no-placeholder" } }),
        )
        .unwrap();
        assert!(validate(&cfg).is_err(), "下行主题模板缺 {{mac}} 应校验失败");

        let mut cfg = XiaozhiConfig::default();
        apply_patch(
            &mut cfg,
            &serde_json::json!({ "udp": { "bind_addr": "not-an-addr" } }),
        )
        .unwrap();
        assert!(validate(&cfg).is_err(), "非法 bind_addr 应校验失败");

        let mut cfg = XiaozhiConfig::default();
        assert!(
            apply_patch(
                &mut cfg,
                &serde_json::json!({ "ota_trusted_proxies": ["::gg"] })
            )
            .is_err(),
            "非法代理 IP 应在合并阶段被拒绝"
        );

        // 合法值可以通过。
        let mut cfg = XiaozhiConfig::default();
        apply_patch(
            &mut cfg,
            &serde_json::json!({
                "protocol": "mqtt",
                "mqtt": { "endpoint": "127.0.0.1:1883" },
                "udp": { "bind_addr": "127.0.0.1:8888" },
                "ota_trusted_proxies": ["10.0.0.1"],
            }),
        )
        .unwrap();
        assert!(validate(&cfg).is_ok());
    }

    /// 传输配置不变时 OTA 可以热更新，变了必须要求重启。
    #[test]
    fn transport_changed_only_tracks_protocol_mqtt_udp() {
        let base = XiaozhiConfig::default();
        let mut say_only = base.clone();
        say_only.say_token = "new".to_string();
        say_only.ota_token = "ota".to_string();
        assert!(!transport_changed(&base, &say_only));

        let mut mqtt = base.clone();
        mqtt.mqtt.endpoint = "127.0.0.1:1883".to_string();
        assert!(transport_changed(&base, &mqtt));

        let mut udp = base.clone();
        udp.udp.bind_addr = "127.0.0.1:9000".to_string();
        assert!(transport_changed(&base, &udp));

        let mut protocol = base.clone();
        protocol.protocol = XiaozhiProtocol::Mqtt;
        assert!(transport_changed(&base, &protocol));
    }
}
