use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Json},
    http::{HeaderMap, StatusCode},
};
use chrono::Local;

use crate::types::*;

/// 处理 POST /xiaozhi/ota/ 请求
///
/// 从请求头中提取 Device-Id，按当前小智配置下发 WebSocket 或 MQTT 段，
/// 并带上服务器时间、固件信息和音频参数。
pub async fn handle_ota(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<OtaRequest>,
) -> Result<Json<OtaResponse>, (StatusCode, Json<serde_json::Value>)> {
    // 来源校验必须在派生密码之前：MQTT 模式下这个响应里带着设备的
    // 连接密码，谁能访问这个端点，谁就能冒充任意设备。
    if !ota_peer_allowed(peer.ip(), current_xiaozhi_config().ota_allow_private_only) {
        tracing::warn!(
          %peer,
          "OTA 请求来自非私有网络地址，拒绝（可配置 [xiaozhi] ota_allow_private_only 关闭该限制）"
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "OTA 端点仅允许私有网络访问"
            })),
        ));
    }
    // 记录设备自报的固件与板子信息。
    // 用于排查「某个功能设备没反应」——比如 notify 需要较新固件，
    // 而设备自报的 version 能直接看出它跑的是哪一版。
    // 用 warn 级：本项目的 INFO 会被日志过滤吞掉。
    tracing::warn!(
        app_name = body
            .application
            .as_ref()
            .map(|a| a.name.as_str())
            .unwrap_or("-"),
        app_version = body
            .application
            .as_ref()
            .map(|a| a.version.as_str())
            .unwrap_or("-"),
        board = body
            .board
            .as_ref()
            .map(|b| b.r#type.as_str())
            .unwrap_or("-"),
        board_name = body
            .board
            .as_ref()
            .and_then(|b| b.name.as_deref())
            .unwrap_or("-"),
        mac = body.mac_address.as_str(),
        "收到 OTA 请求"
    );

    // 验证 Device-Id。现有调用方仍依赖缺头时返回 400。
    let _device_id = headers
        .get("device-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            let err = serde_json::json!({"error": "缺少 Device-Id 请求头"});
            (StatusCode::BAD_REQUEST, Json(err))
        })?;

    Ok(Json(compose_ota(
        &headers,
        &body,
        &current_xiaozhi_config(),
    )))
}

/// 安装 OTA 使用的配置。默认是 websocket，不会把设备切到 MQTT。
///
/// 必须在 HTTP 服务启动**之前**调用，否则第一台设备的 OTA 会拿到默认
/// （websocket）配置。根 crate 在 `cli.rs` 里用 `AppConfig.xiaozhi` 调用它。
pub fn set_xiaozhi_ota_config(cfg: XiaozhiConfig) {
    match ota_config_slot().lock() {
        Ok(mut guard) => *guard = cfg,
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            *guard = cfg;
        }
    }
}

fn ota_config_slot() -> &'static Mutex<XiaozhiConfig> {
    static SLOT: OnceLock<Mutex<XiaozhiConfig>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(XiaozhiConfig::default()))
}

/// OTA 端点是否接受来自 `peer` 的请求。
///
/// `private_only` 为 true 时只放行回环、私有网段与链路本地地址。
/// 这样即使 9527 端口被误暴露到公网，也无法用它铸造 MQTT 密码。
fn ota_peer_allowed(peer: IpAddr, private_only: bool) -> bool {
    if !private_only {
        return true;
    }
    match peer {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()      // fd00::/8
                || v6.is_unicast_link_local() // fe80::/10
                // 双栈监听会把 IPv4 连接呈现成 IPv4-mapped 的 IPv6（::ffff:a.b.c.d），
                // 不还原成 v4 判一次的话，局域网设备会被误拒、拿不到 OTA。
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_v4(v4))
        }
    }
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_private() || v4.is_loopback() || v4.is_link_local()
}

fn current_xiaozhi_config() -> XiaozhiConfig {
    match ota_config_slot().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 按协议组装 OTA 响应。websocket 与 mqtt 互斥，避免固件看到 mqtt 就切走。
fn compose_ota(headers: &HeaderMap, body: &OtaRequest, cfg: &XiaozhiConfig) -> OtaResponse {
    let mac = extract_mac(body, headers);
    let (websocket, mqtt) = match cfg.protocol {
        XiaozhiProtocol::Websocket => (Some(build_ws_info(headers)), None),
        XiaozhiProtocol::Mqtt => (None, Some(build_mqtt_info(&cfg.mqtt, &mac))),
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timezone_offset = Local::now().offset().local_minus_utc() / 60;

    OtaResponse {
        websocket,
        mqtt,
        server_time: ServerTime {
            timestamp: now.as_millis() as i64,
            timezone_offset,
        },
        firmware: Some(FirmwareInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            url: String::new(),
            force: false,
        }),
        audio_params: AudioParams::default(),
    }
}

/// 设备 MAC：优先请求体 `mac_address`，其次 `Device-Id`。都没有时用 `unknown` 并告警。
fn extract_mac(body: &OtaRequest, headers: &HeaderMap) -> String {
    let from_body = body.mac_address.trim();
    if !from_body.is_empty() {
        return from_body.to_string();
    }
    if let Some(from_header) = headers.get("device-id").and_then(|v| v.to_str().ok()) {
        let from_header = from_header.trim();
        if !from_header.is_empty() {
            return from_header.to_string();
        }
    }
    tracing::warn!(
        "OTA 请求缺少 MAC（body.mac_address 与 Device-Id 都为空），client_id 使用退化值 unknown"
    );
    "unknown".to_string()
}

fn build_ws_info(headers: &HeaderMap) -> WebsocketInfo {
    WebsocketInfo {
        url: build_ws_url(headers),
        token: String::new(),
        version: 2,
    }
}

/// 拼 MQTT 下发段。`endpoint` 缺端口时补 `:1883`，否则固件会走 8883 TLS。
fn build_mqtt_info(mqtt: &XiaozhiMqttConfig, mac: &str) -> MqttInfo {
    let safe = mac_safe(mac);
    let client_id = format!("GID_haimen@@@{safe}@@@{safe}");
    // 配了 auth_key 就用每设备派生的密码，否则沿用配置文件里的静态值。
    // 派生密码 = HMAC-SHA256(auth_key, client_id)，broker 侧重算比较。
    let password = if mqtt.auth_key.is_empty() {
        mqtt.password.clone()
    } else {
        crate::mqtt_broker::derive_mqtt_password(&mqtt.auth_key, &client_id)
    };
    MqttInfo {
        endpoint: ensure_mqtt_endpoint(&mqtt.endpoint),
        client_id,
        username: mqtt.username.clone(),
        password,
        keepalive: mqtt.keepalive,
        publish_topic: apply_mac_template(&mqtt.publish_topic_template, &safe),
        subscribe_topic: apply_mac_template(&mqtt.downlink_topic_template, &safe),
    }
}

/// 冒号换成下划线，并转成小写，与固件 client_id / 主题约定一致。
fn mac_safe(mac: &str) -> String {
    mac.trim().replace(':', "_").to_ascii_lowercase()
}

fn apply_mac_template(template: &str, mac_safe: &str) -> String {
    template.replace("{mac}", mac_safe)
}

/// 已有十进制端口则原样返回，否则补 `:1883` 并打日志。
fn ensure_mqtt_endpoint(endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    if endpoint_has_port(endpoint) {
        return endpoint.to_string();
    }
    tracing::warn!(
        endpoint,
        "xiaozhi.mqtt.endpoint 缺少端口，已补 :1883（固件缺端口会默认 8883 并强制 TLS）"
    );
    format!("{endpoint}:1883")
}

fn endpoint_has_port(endpoint: &str) -> bool {
    match endpoint.rsplit_once(':') {
        Some((_, port)) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// 从请求头构建 WebSocket URL
///
/// 优先级：
/// 1. `X-Forwarded-Proto` + `X-Forwarded-Host`（反向代理场景）
/// 2. `Host` 请求头（直接访问）
/// 3. 回退到 `ws://localhost:9527/xiaozhi/ws`
fn build_ws_url(headers: &HeaderMap) -> String {
    // 判断是否使用安全协议
    let is_secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s == "https" || s == "wss")
        .unwrap_or(false);

    let scheme = if is_secure { "wss" } else { "ws" };

    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))
        .unwrap_or("localhost:9527");

    format!("{scheme}://{host}/xiaozhi/ws")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ConnectInfo;
    use axum::http::HeaderValue;
    use std::net::SocketAddr;

    /// 串行化会改全局 OTA 配置的测试，避免和默认 websocket 用例交错。
    fn ota_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        match LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    struct RestoreDefaultConfig;
    impl Drop for RestoreDefaultConfig {
        fn drop(&mut self) {
            set_xiaozhi_ota_config(XiaozhiConfig::default());
        }
    }

    /// 构建一个包含 Device-Id 头的最小请求
    /// OTA handler 现在要求 ConnectInfo；测试里统一用回环地址。
    /// OTA 来源策略：默认只放行私有/回环地址。
    #[test]
    fn ota_peer_policy_allows_private_only() {
        use std::net::IpAddr;
        let private: IpAddr = "192.168.1.10".parse().expect("ip");
        let loopback: IpAddr = "127.0.0.1".parse().expect("ip");
        let public: IpAddr = "8.8.8.8".parse().expect("ip");
        let public_v6: IpAddr = "2001:4860:4860::8888".parse().expect("ip");

        assert!(ota_peer_allowed(private, true));
        assert!(ota_peer_allowed(loopback, true));
        assert!(!ota_peer_allowed(public, true));
        assert!(!ota_peer_allowed(public_v6, true));

        // 关闭限制时一律放行
        assert!(ota_peer_allowed(public, false));
        assert!(ota_peer_allowed(public_v6, false));
    }

    /// IPv6 的内网地址也必须放行：双栈监听的 IPv4-mapped 地址、
    /// 唯一本地地址（fd00::/8）、链路本地地址（fe80::/10）。
    #[test]
    fn ota_peer_policy_allows_private_ipv6() {
        use std::net::IpAddr;
        let cases = [
            ("::1", true),
            ("fd00::1", true),
            ("fe80::1", true),
            ("::ffff:192.168.1.10", true), // IPv4-mapped 的局域网地址
            ("::ffff:127.0.0.1", true),    // IPv4-mapped 的回环
            ("::ffff:8.8.8.8", false),     // IPv4-mapped 的公网地址
            ("2001:4860:4860::8888", false),
        ];
        for (raw, expect) in cases {
            let ip: IpAddr = raw.parse().expect("解析 IP");
            assert_eq!(
                ota_peer_allowed(ip, true),
                expect,
                "{raw} 的判定应为 {expect}"
            );
        }
    }

    /// 公网来源的 OTA 请求必须被拒 —— 否则它能拿到派生 MQTT 密码。
    #[tokio::test]
    async fn ota_rejects_public_peer() {
        let _lock = ota_test_lock();
        let peer = ConnectInfo(SocketAddr::from(([8, 8, 8, 8], 1234)));
        let result = handle_ota(
            peer,
            make_headers(),
            Json(sample_request("84:c7:bb:59:93:64")),
        )
        .await;
        let (status, _) = result.expect_err("公网来源应被拒绝");
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    fn test_peer() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    fn make_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("device-id", HeaderValue::from_static("AA:BB:CC:DD:EE:FF"));
        headers
    }

    fn sample_request(mac: &str) -> OtaRequest {
        OtaRequest {
            version: 2,
            mac_address: mac.to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        }
    }

    fn mqtt_config(endpoint: &str) -> XiaozhiConfig {
        XiaozhiConfig {
            protocol: XiaozhiProtocol::Mqtt,
            mqtt: XiaozhiMqttConfig {
                endpoint: endpoint.to_string(),
                username: "haimen".to_string(),
                password: "haimen".to_string(),
                ..XiaozhiMqttConfig::default()
            },
            ..XiaozhiConfig::default()
        }
    }

    #[tokio::test]
    async fn test_missing_device_id() {
        let _lock = ota_test_lock();
        let headers = HeaderMap::new();
        let body = OtaRequest {
            version: 2,
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        };

        let result = handle_ota(test_peer(), headers, Json(body)).await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_successful_ota() {
        let _lock = ota_test_lock();
        let headers = make_headers();
        let body = OtaRequest {
            version: 2,
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        };

        let result = handle_ota(test_peer(), headers, Json(body)).await;
        assert!(result.is_ok());

        let response = result.unwrap().0;
        let websocket = response.websocket.as_ref().expect("websocket");
        assert!(websocket.url.contains("ws://"));
        assert!(websocket.url.contains("/xiaozhi/ws"));
        assert_eq!(websocket.version, 2);
        assert_eq!(response.audio_params.format, "opus");
        assert_eq!(response.audio_params.sample_rate, 24000);
    }

    #[tokio::test]
    async fn test_ota_with_host_header() {
        let _lock = ota_test_lock();
        let mut headers = make_headers();
        headers.insert("host", HeaderValue::from_static("example.com:8080"));

        let body = OtaRequest {
            version: 2,
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        };

        let result = handle_ota(test_peer(), headers, Json(body)).await;
        assert!(result.is_ok());

        let response = result.unwrap().0;
        let websocket = response.websocket.as_ref().expect("websocket");
        assert!(websocket.url.contains("example.com:8080"));
    }

    #[tokio::test]
    async fn test_ota_with_forwarded_headers() {
        let _lock = ota_test_lock();
        let mut headers = make_headers();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("public.example.com"),
        );

        let body = OtaRequest {
            version: 2,
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        };

        let result = handle_ota(test_peer(), headers, Json(body)).await;
        assert!(result.is_ok());

        let response = result.unwrap().0;
        let websocket = response.websocket.as_ref().expect("websocket");
        // X-Forwarded 优先于 Host
        assert!(websocket.url.contains("wss://"));
        assert!(websocket.url.contains("public.example.com"));
    }

    #[tokio::test]
    async fn test_server_time_fields() {
        let _lock = ota_test_lock();
        let headers = make_headers();
        let body = OtaRequest {
            version: 2,
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            uuid: "test-uuid".to_string(),
            chip_model_name: None,
            application: None,
            board: None,
            flash_size: None,
            minimum_free_heap_size: None,
            ota: None,
        };

        let result = handle_ota(test_peer(), headers, Json(body)).await;
        assert!(result.is_ok());

        let response = result.unwrap().0;
        // timestamp 应该是正数（Unix 毫秒时间戳）
        assert!(response.server_time.timestamp > 1_700_000_000_000i64);
        // timezone_offset 应该在 -720 到 840 之间（合理范围）
        assert!(response.server_time.timezone_offset >= -720);
        assert!(response.server_time.timezone_offset <= 840);
    }

    #[tokio::test]
    async fn test_websocket_protocol_omits_mqtt() {
        let _lock = ota_test_lock();
        let _restore = RestoreDefaultConfig;
        set_xiaozhi_ota_config(XiaozhiConfig::default());

        let response = handle_ota(
            test_peer(),
            make_headers(),
            Json(sample_request("84:c7:bb:59:93:64")),
        )
        .await
        .expect("ota")
        .0;
        assert!(response.websocket.is_some());
        assert!(response.mqtt.is_none());
    }

    #[tokio::test]
    async fn test_mqtt_protocol_omits_websocket() {
        let _lock = ota_test_lock();
        let _restore = RestoreDefaultConfig;
        set_xiaozhi_ota_config(mqtt_config("192.168.110.35:1883"));

        let response = handle_ota(
            test_peer(),
            make_headers(),
            Json(sample_request("84:c7:bb:59:93:64")),
        )
        .await
        .expect("ota")
        .0;
        let mqtt = response.mqtt.expect("mqtt");
        assert!(response.websocket.is_none());
        assert_eq!(mqtt.endpoint, "192.168.110.35:1883");
        assert_eq!(mqtt.username, "haimen");
        assert_eq!(mqtt.password, "haimen");
        assert_eq!(mqtt.keepalive, 240);
    }

    #[test]
    fn test_mqtt_endpoint_fills_missing_port() {
        let info = build_mqtt_info(&mqtt_config("192.168.110.35").mqtt, "84:c7:bb:59:93:64");
        assert_eq!(info.endpoint, "192.168.110.35:1883");

        let kept = build_mqtt_info(
            &mqtt_config("192.168.110.35:1883").mqtt,
            "84:c7:bb:59:93:64",
        );
        assert_eq!(kept.endpoint, "192.168.110.35:1883");
    }

    #[test]
    fn test_mqtt_client_id_uses_underscored_mac() {
        let info = build_mqtt_info(
            &mqtt_config("192.168.110.35:1883").mqtt,
            "84:C7:BB:59:93:64",
        );
        assert_eq!(
            info.client_id,
            "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64"
        );
    }

    #[test]
    fn test_mqtt_topics_replace_mac_placeholder() {
        let info = build_mqtt_info(
            &mqtt_config("192.168.110.35:1883").mqtt,
            "84:c7:bb:59:93:64",
        );
        assert_eq!(info.publish_topic, "haimen/xz/84_c7_bb_59_93_64/up");
        assert_eq!(info.subscribe_topic, "haimen/xz/84_c7_bb_59_93_64/down");
    }

    #[test]
    fn test_mac_falls_back_to_device_id_header() {
        let body = sample_request("");
        let headers = make_headers_with_device("84:c7:bb:59:93:64");
        let info = build_mqtt_info(
            &mqtt_config("192.168.110.35:1883").mqtt,
            &extract_mac(&body, &headers),
        );
        assert_eq!(
            info.client_id,
            "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64"
        );
        assert_eq!(info.publish_topic, "haimen/xz/84_c7_bb_59_93_64/up");
    }

    #[test]
    fn test_missing_mac_uses_unknown_without_panic() {
        let body = sample_request("   ");
        let headers = HeaderMap::new();
        let mac = extract_mac(&body, &headers);
        assert_eq!(mac, "unknown");
        let info = build_mqtt_info(&mqtt_config("192.168.110.35:1883").mqtt, &mac);
        assert_eq!(info.client_id, "GID_haimen@@@unknown@@@unknown");
    }

    fn make_headers_with_device(device_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "device-id",
            HeaderValue::from_str(device_id).expect("device-id"),
        );
        headers
    }
}
