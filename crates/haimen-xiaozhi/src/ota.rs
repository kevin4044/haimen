use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Json},
    http::{HeaderMap, StatusCode},
};
use chrono::Local;

use crate::firmware::{self, DeviceFirmwareReport};
use crate::types::*;

/// 处理 POST /xiaozhi/ota/ 请求
///
/// 从请求头中提取 Device-Id，按当前小智配置下发 WebSocket 或 MQTT 段，
/// 并带上服务器时间、固件信息和音频参数。
pub async fn handle_ota(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Json(body): Json<OtaRequest>,
) -> Result<Json<OtaResponse>, (StatusCode, Json<serde_json::Value>)> {
    // 来源校验必须在派生密码之前：MQTT 模式下这个响应里带着设备的
    // 连接密码，谁能访问这个端点，谁就能冒充任意设备。
    if let Err(reason) = ota_source_allowed(peer.ip(), &uri, &headers, &current_xiaozhi_config()) {
        tracing::warn!(%peer, reason, "OTA 来源校验未通过，拒绝");
        return Err(forbidden_ota_peer());
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

    // 记录设备上报（固件版本/板型/芯片/OTA 分区大小）。存储未安装时是 no-op，
    // 不影响本次 OTA 响应。
    let mac = extract_mac(&body, &headers);
    firmware::record_device_report(build_device_report(&mac, &body)).await;

    Ok(Json(
        compose_ota(&mac, &headers, &current_xiaozhi_config()).await,
    ))
}

/// 把 OTA 请求里的设备信息整理成存储层结构。
fn build_device_report(mac: &str, body: &OtaRequest) -> DeviceFirmwareReport {
    let app = body.application.as_ref();
    let board = body.board.as_ref();
    let ota_label = body
        .ota
        .as_ref()
        .and_then(|ota| ota.label.as_deref())
        .map(str::trim)
        .filter(|label| !label.is_empty());
    // OTA 分区：优先 `ota.label` 指向的 app 分区，其次 ota_0、factory。
    // 设备没上报 partition_table 时为 None，服务端跳过体积预检。
    let ota_partition_size = ota_label
        .and_then(|label| {
            body.partition_table
                .iter()
                .find(|part| part.r#type == 0 && part.label.eq_ignore_ascii_case(label))
                .map(|part| part.size)
        })
        .or_else(|| {
            body.partition_table
                .iter()
                .find(|part| part.r#type == 0 && part.subtype == 0x11)
                .map(|part| part.size)
        })
        .or_else(|| {
            body.partition_table
                .iter()
                .find(|part| part.r#type == 0 && part.subtype == 0x10)
                .map(|part| part.size)
        })
        // 真实 ESP-IDF 的 factory 子类型是 0x00；没有 label 时兜底取第一个 app 分区。
        .or_else(|| {
            body.partition_table
                .iter()
                .find(|part| part.r#type == 0)
                .map(|part| part.size)
        })
        .filter(|size| *size > 0);

    DeviceFirmwareReport {
        device_id: crate::devices::normalize_mac(mac),
        app_name: app.map(|info| info.name.clone()).unwrap_or_default(),
        app_version: app.map(|info| info.version.clone()).unwrap_or_default(),
        board_type: board.map(|info| info.r#type.clone()).unwrap_or_default(),
        board_name: board.and_then(|info| info.name.clone()).unwrap_or_default(),
        chip_model: body.chip_model_name.clone().unwrap_or_default(),
        mac: mac.trim().to_string(),
        ota_partition_size,
        reported_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or_default(),
    }
}

/// 安装 OTA 使用的配置。默认是 websocket，不会把设备切到 MQTT。
///
/// 必须在 HTTP 服务启动**之前**调用，否则第一台设备的 OTA 会拿到默认
/// （websocket）配置。根 crate 在 `cli.rs` 里用 `AppConfig.xiaozhi` 调用它。
pub fn set_xiaozhi_ota_config(cfg: XiaozhiConfig) {
    // 启动即说明 OTA 的真实防护强度，避免「配了 IP 限制就安全了」的误解。
    if cfg.ota_token.trim().is_empty() {
        tracing::warn!(
            private_only = cfg.ota_allow_private_only,
            "OTA 端点未配置 ota_token：MQTT 模式下 /xiaozhi/ota/ 会向任何可达客户端           下发派生的连接密码。基于 IP 的限制在 Docker 端口映射 / 反向代理下           会被削弱（容器内看到的对端恒为网桥地址），只有令牌是可靠的。\
           建议：设备 ota_url 带上 ?token=<随机串>，并在 [xiaozhi] 配同名 ota_token。"
        );
    }
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
                || v6.to_ipv4_mapped().is_some_and(is_private_v4)
        }
    }
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_private() || v4.is_loopback() || v4.is_link_local()
}

/// 统一的 OTA 来源拒绝响应。
fn forbidden_ota_peer() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": "OTA 端点仅允许私有网络访问" })),
    )
}

/// OTA 来源校验。返回 Err(原因) 表示拒绝。
///
/// 两道闸，任一不过就拒：
///
/// 1. **令牌**（若配置了 `ota_token`）：OTA 端点唯一无法伪造的凭据。
///    IP 判断在 Docker 端口映射与反向代理下都会失效，令牌不会。
/// 2. **来源地址**（若 `ota_allow_private_only`）：见 [`resolve_client_ip`]。
///
/// 刻意做成 fail-closed：只要无法确认来源是内网，就拒绝。
fn ota_source_allowed(
    peer: IpAddr,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    cfg: &XiaozhiConfig,
) -> Result<(), &'static str> {
    let expected = cfg.ota_token.trim();
    if !expected.is_empty() {
        let supplied = query_token(uri)
            .or_else(|| {
                headers
                    .get("x-haimen-ota-token")
                    .and_then(|v| v.to_str().ok())
            })
            .unwrap_or("");
        if !constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
            return Err("OTA 令牌缺失或不匹配");
        }
        // 令牌对上了就不再依赖 IP 判定 —— 令牌本身就是强凭据。
        return Ok(());
    }
    if !cfg.ota_allow_private_only {
        return Ok(());
    }
    let client_ip = resolve_client_ip(peer, headers, &cfg.ota_trusted_proxies)
        .ok_or("无法确定真实客户端地址（对端是代理但缺少可用的 X-Forwarded-For）")?;
    if !ota_peer_allowed(client_ip, true) {
        return Err("客户端不是私有网络地址");
    }
    Ok(())
}

/// 解析真实客户端地址。
///
/// - 对端**不在** `trusted_proxies` 里：客户端就是对端，`X-Forwarded-For`
///   一律忽略 —— 任何人都能伪造这个头，读了只会被绕过。
/// - 对端**在** `trusted_proxies` 里：说明请求经过了我们认可的代理，
///   真实客户端只能从 `X-Forwarded-For` 得到。此时**从右往左**跳过同样
///   可信的代理条目，取第一个非代理地址。
///   （从右往左而不是从左往右：左侧条目完全由客户端控制，攻击者可以
///   前置一个 `10.0.0.1` 冒充内网。）
/// - 该头缺失、为空或无法解析：返回 None，由调用方按失败关闭处理。
fn resolve_client_ip(
    peer: IpAddr,
    headers: &HeaderMap,
    trusted_proxies: &[IpAddr],
) -> Option<IpAddr> {
    if !trusted_proxies.contains(&peer) {
        return Some(peer);
    }
    let raw = headers.get("x-forwarded-for")?.to_str().ok()?;
    for hop in raw.split(',').map(str::trim).rev() {
        let Ok(ip) = hop.parse::<IpAddr>() else {
            // 解析不了就当作不可信，直接失败关闭。
            return None;
        };
        if !trusted_proxies.contains(&ip) {
            return Some(ip);
        }
    }
    None
}

/// 从 `?token=` 取 OTA 令牌。
fn query_token(uri: &axum::http::Uri) -> Option<&str> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then_some(value)
    })
}

/// 常量时间比较，避免按字节提前返回泄露令牌前缀。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn current_xiaozhi_config() -> XiaozhiConfig {
    match ota_config_slot().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 按协议组装 OTA 响应。websocket 与 mqtt 互斥，避免固件看到 mqtt 就切走。
async fn compose_ota(mac: &str, headers: &HeaderMap, cfg: &XiaozhiConfig) -> OtaResponse {
    let (websocket, mqtt) = match cfg.protocol {
        XiaozhiProtocol::Websocket => (Some(build_ws_info(headers)), None),
        XiaozhiProtocol::Mqtt => (None, Some(build_mqtt_info(&cfg.mqtt, mac))),
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timezone_offset = Local::now().offset().local_minus_utc() / 60;

    // 只有用户显式指定过目标（且文件存在）才下发 firmware。
    // 默认不自动升级，避免设备重启即被刷成上游版本。
    let firmware_info = firmware::firmware_info_for(
        &crate::devices::normalize_mac(mac),
        &device_http_base(headers),
    )
    .await;

    OtaResponse {
        websocket,
        mqtt,
        server_time: ServerTime {
            timestamp: now.as_millis() as i64,
            timezone_offset,
        },
        firmware: firmware_info,
        audio_params: AudioParams::default(),
    }
}

/// 设备能访问的 haimen HTTP 基址。
///
/// 与 [`build_ws_url`] 的取值顺序一致：反向代理头优先，其次 Host，最后本地回退。
/// 固件 URL 必须让设备可达，因此不能写死局域网 IP。
fn device_http_base(headers: &HeaderMap) -> String {
    let is_secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|proto| {
            proto.eq_ignore_ascii_case("https") || proto.eq_ignore_ascii_case("wss")
        });
    let scheme = if is_secure { "https" } else { "http" };
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))
        .unwrap_or("localhost:9527");
    format!("{scheme}://{host}")
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
    fn cfg_with(token: &str, private_only: bool, proxies: &[&str]) -> XiaozhiConfig {
        let mut cfg = XiaozhiConfig::default();
        cfg.ota_token = token.to_string();
        cfg.ota_allow_private_only = private_only;
        cfg.ota_trusted_proxies = proxies
            .iter()
            .map(|raw| raw.parse().expect("代理地址"))
            .collect();
        cfg
    }

    fn uri_with_token(token: &str) -> axum::http::Uri {
        format!("/xiaozhi/ota/?token={token}").parse().expect("URI")
    }

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(value).expect("头"));
        headers
    }

    /// 对端不是可信代理时，`X-Forwarded-For` 必须被完全忽略 ——
    /// 否则公网客户端只要自报一个内网地址就能绕过来源校验。
    #[test]
    fn forwarded_for_is_ignored_for_untrusted_peer() {
        let peer: IpAddr = "8.8.8.8".parse().expect("ip");
        let got = resolve_client_ip(peer, &headers_with_xff("10.0.0.1"), &[]);
        assert_eq!(got, Some(peer), "不可信对端时 XFF 应被忽略");

        // 即使伪造的 XFF 是内网地址，也照样判公网 → 拒绝
        let cfg = cfg_with("", true, &[]);
        assert!(
            ota_source_allowed(peer, &test_uri(), &headers_with_xff("10.0.0.1"), &cfg).is_err(),
            "伪造 XFF 不能绕过"
        );
    }

    /// 对端是可信代理时，从**右往左**取第一个非代理地址。
    ///
    /// 从右往左是关键：左侧条目由客户端控制，攻击者可以前置
    /// `10.0.0.1` 冒充内网；右侧的才是代理追加的真实来源。
    #[test]
    fn forwarded_for_uses_rightmost_untrusted_hop() {
        let peer: IpAddr = "127.0.0.1".parse().expect("ip");
        let rules: Vec<IpAddr> = vec!["127.0.0.1".parse().expect("ip")];

        // 攻击者前置内网地址：真实客户端在右侧
        let headers = headers_with_xff("10.0.0.1, 203.0.113.5");
        assert_eq!(
            resolve_client_ip(peer, &headers, &rules),
            Some("203.0.113.5".parse().expect("ip")),
            "应取右侧的真实客户端地址"
        );

        // 中间还有一跳可信代理：继续往左跳过
        let rules2: Vec<IpAddr> = vec![
            "127.0.0.1".parse().expect("ip"),
            "10.0.0.2".parse().expect("ip"),
        ];
        let headers2 = headers_with_xff("1.2.3.4, 10.0.0.2");
        assert_eq!(
            resolve_client_ip(peer, &headers2, &rules2),
            Some("1.2.3.4".parse().expect("ip"))
        );
    }

    /// 对端是可信代理但缺少 XFF → 失败关闭（不能默认放行）。
    #[test]
    fn trusted_proxy_without_forwarded_header_fails_closed() {
        let peer: IpAddr = "127.0.0.1".parse().expect("ip");
        let rules: Vec<IpAddr> = vec!["127.0.0.1".parse().expect("ip")];
        assert_eq!(resolve_client_ip(peer, &HeaderMap::new(), &rules), None);

        let cfg = cfg_with("", true, &["127.0.0.1"]);
        assert!(
            ota_source_allowed(peer, &test_uri(), &HeaderMap::new(), &cfg).is_err(),
            "缺少 XFF 时应拒绝"
        );
    }

    /// 配置了令牌时：缺失/错误都拒，正确才放行（且不再依赖 IP）。
    #[test]
    fn token_is_required_when_configured() {
        let cfg = cfg_with("s3cret", true, &[]);
        let public: IpAddr = "8.8.8.8".parse().expect("ip");

        assert!(
            ota_source_allowed(public, &test_uri(), &HeaderMap::new(), &cfg).is_err(),
            "缺少令牌应拒绝"
        );
        assert!(
            ota_source_allowed(public, &uri_with_token("wrong"), &HeaderMap::new(), &cfg).is_err(),
            "令牌错误应拒绝"
        );
        assert!(
            ota_source_allowed(public, &uri_with_token("s3cret"), &HeaderMap::new(), &cfg).is_ok(),
            "令牌正确应放行（令牌本身就是强凭据）"
        );

        // 也支持头部传递
        let mut headers = HeaderMap::new();
        headers.insert("x-haimen-ota-token", HeaderValue::from_static("s3cret"));
        assert!(ota_source_allowed(public, &test_uri(), &headers, &cfg).is_ok());
    }

    /// 查询参数解析与常量时间比较的基础行为。
    #[test]
    fn token_query_and_compare() {
        assert_eq!(query_token(&uri_with_token("abc")), Some("abc"));
        assert_eq!(query_token(&test_uri()), None);
        let with_others: axum::http::Uri = "/xiaozhi/ota/?k=v&token=xyz".parse().expect("URI");
        assert_eq!(query_token(&with_others), Some("xyz"));

        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

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
            test_uri(),
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

    /// 默认不带 query 的请求 URI。
    fn test_uri() -> axum::http::Uri {
        "/xiaozhi/ota/".parse().expect("URI")
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
            partition_table: Vec::new(),
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
            partition_table: Vec::new(),
        };

        let result = handle_ota(test_peer(), test_uri(), headers, Json(body)).await;
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
            partition_table: Vec::new(),
        };

        let result = handle_ota(test_peer(), test_uri(), headers, Json(body)).await;
        assert!(result.is_ok());

        let response = result.unwrap().0;
        let websocket = response.websocket.as_ref().expect("websocket");
        assert!(websocket.url.contains("ws://"));
        assert!(websocket.url.contains("/xiaozhi/ws"));
        assert_eq!(websocket.version, 2);
        assert_eq!(response.audio_params.format, "opus");
        assert_eq!(response.audio_params.sample_rate, 24000);
        assert!(
            response.firmware.is_none(),
            "没有用户指定的升级目标时不应下发 firmware"
        );
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
            partition_table: Vec::new(),
        };

        let result = handle_ota(test_peer(), test_uri(), headers, Json(body)).await;
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
            partition_table: Vec::new(),
        };

        let result = handle_ota(test_peer(), test_uri(), headers, Json(body)).await;
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
            partition_table: Vec::new(),
        };

        let result = handle_ota(test_peer(), test_uri(), headers, Json(body)).await;
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
            test_uri(),
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
            test_uri(),
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

    /// 安装一份临时文件存储，测试结束随 TempDir 删除。
    async fn install_temp_firmware_store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::firmware::install_firmware_store(crate::firmware::FirmwareStoreConfig {
            metadata: std::sync::Arc::new(crate::firmware::JsonFileMetadataStore::new(
                dir.path().join("state.json"),
            )),
            blobs: std::sync::Arc::new(crate::firmware::LocalFileBlobStore::new(
                dir.path().join("bin"),
            )),
            gc_on_start: false,
        })
        .await
        .expect("安装固件存储");
        dir
    }

    fn report_body(mac: &str, version: &str) -> OtaRequest {
        let mut body = sample_request(mac);
        body.chip_model_name = Some("esp32s3".to_string());
        body.application = Some(ApplicationInfo {
            name: "xiaozhi".to_string(),
            version: version.to_string(),
            compile_time: None,
            board: None,
        });
        body.board = Some(BoardInfo {
            r#type: "bread-compact-wifi".to_string(),
            mac: None,
            ssid: None,
            rssi: None,
            ip: None,
            channel: None,
            name: Some("bread-compact-wifi".to_string()),
        });
        body.ota = Some(OtaInfo {
            label: Some("ota_0".to_string()),
        });
        body.partition_table = vec![PartitionInfo {
            label: "ota_0".to_string(),
            r#type: 0,
            subtype: 0x11,
            address: 0x20000,
            size: 0x3f0000,
        }];
        body
    }

    #[tokio::test]
    async fn test_ota_records_device_report() {
        let _config_lock = ota_test_lock();
        let _firmware_lock = crate::firmware::test_lock().lock().await;
        let _store = install_temp_firmware_store().await;

        let mac = "12:34:56:78:9a:bc";
        let response = handle_ota(
            test_peer(),
            test_uri(),
            make_headers_with_device(mac),
            Json(report_body(mac, "2.5.1")),
        )
        .await
        .expect("ota")
        .0;
        assert!(response.firmware.is_none());

        let report = crate::firmware::report_for(mac).expect("应记录设备上报");
        assert_eq!(report.app_version, "2.5.1");
        assert_eq!(report.app_name, "xiaozhi");
        assert_eq!(report.board_type, "bread-compact-wifi");
        assert_eq!(report.board_name, "bread-compact-wifi");
        assert_eq!(report.chip_model, "esp32s3");
        assert_eq!(report.ota_partition_size, Some(0x3f0000));
    }

    #[tokio::test]
    async fn test_ota_serves_firmware_target_with_numeric_force() {
        let _config_lock = ota_test_lock();
        let _firmware_lock = crate::firmware::test_lock().lock().await;
        let _store = install_temp_firmware_store().await;

        let mac = "12:34:56:78:9a:bd";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        crate::firmware::store_firmware_file(file, vec![0xE9, 1, 2, 3])
            .await
            .expect("写固件");
        crate::firmware::set_firmware_target(
            mac,
            crate::firmware::FirmwareTarget {
                version: "2.5.2".to_string(),
                from_version: "2.5.1".to_string(),
                file: file.to_string(),
                asset: "v2.5.2_bread-compact-wifi.zip".to_string(),
                sha256: "deadbeef".to_string(),
                board_type: "bread-compact-wifi".to_string(),
                board_name: "bread-compact-wifi".to_string(),
                installed_at_ms: 1,
                deliveries: 0,
                delivered: false,
            },
        )
        .await
        .expect("设置目标");

        let mut headers = make_headers_with_device(mac);
        headers.insert("host", HeaderValue::from_static("example.com:9527"));
        let response = handle_ota(
            test_peer(),
            test_uri(),
            headers,
            Json(report_body(mac, "2.5.1")),
        )
        .await
        .expect("ota")
        .0;

        let firmware = response.firmware.as_ref().expect("应下发 firmware");
        assert_eq!(firmware.version, "2.5.2");
        assert_eq!(firmware.force, 1);
        assert_eq!(
            firmware.url,
            "http://example.com:9527/xiaozhi/firmware/2.5.2-bread-compact-wifi-abcdef123456.bin"
        );

        let json = serde_json::to_value(&response).expect("序列化响应");
        assert_eq!(
            json["firmware"]["force"],
            serde_json::json!(1),
            "上游固件只认数字 1，传 true 会被忽略"
        );
        assert_eq!(json["firmware"]["version"], "2.5.2");
    }
}
