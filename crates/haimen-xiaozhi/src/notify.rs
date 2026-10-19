//! `POST /api/v1/xiaozhi/say`：文本 → Ogg Opus → 托管 → 下发 notify 给小智设备。
//!
//! 这是 T2 的主动播报 HTTP 入口。它自己不合成音频（那是根 crate 的事，
//! 因为要访问 TTS 配置与 Opus 编码器），只负责：鉴权 → 参数校验 → 设备解析
//! → 忙状态检查 → 调合成器 → 托管音频 → 拼 URL → 下发 notify。
//!
//! notify 没有 ACK：200 只表示「已下发」，不保证设备真的播了。

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::mqtt_udp::SayDeviceInfo;
use crate::say_audio::say_audio_store;

/// 一次播报允许的最长文本（字符数）。再长 TTS 合成慢、设备屏幕也显示不下。
const MAX_TEXT_CHARS: usize = 500;

/// 合成的整体超时。TTS 是外部网络调用，不封顶的话一个卡死的服务商
/// 会把这个 HTTP 请求挂到天荒地老。
const SYNTH_TIMEOUT: Duration = Duration::from_secs(15);

/// 托管音频的 TTL。设备收到 notify 后会立刻来拉；拉不到（不在 Idle 被
/// 静默丢弃）时由 [`crate::say_audio::SayAudioStore::sweep`] 兜底清理。
const AUDIO_TTL: Duration = Duration::from_secs(60);

/// 文本 → Ogg Opus 字节。音色由实现方决定（一律用当前生效音色）。
///
/// 为什么放 trait 而不是直接调 TTS：haimen-xiaozhi 不知道根 crate 的
/// TTS 配置长什么样；根 crate 在 web 启动时把实现注入全局槽。
#[async_trait::async_trait]
pub trait SaySynthesizer: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, String>;
}

static SYNTHESIZER: OnceLock<Arc<dyn SaySynthesizer>> = OnceLock::new();

/// 注入合成器。只能在启动时装一次；重复设置打 warn 并忽略
/// （OnceLock 只能 set 一次，测试里共用同一个假实现也靠这个语义）。
pub fn set_say_synthesizer(synthesizer: Arc<dyn SaySynthesizer>) {
    if SYNTHESIZER.set(synthesizer).is_err() {
        tracing::warn!("say 合成器已安装，忽略重复设置");
    }
}

fn synthesizer() -> Option<Arc<dyn SaySynthesizer>> {
    SYNTHESIZER.get().cloned()
}

/// say 接口的运行时设置。
///
/// 为什么拆三个部分 setter 而不是一次 set 整个 struct：
/// web 层知道 token 和 HTTP 端口，gateway 知道 advertise_ip，
/// 两处启动时机不同，分别安装自己知道的那部分。
#[derive(Default)]
struct SaySettings {
    say_token: String,
    advertise_ip: Option<String>,
    http_port: u16,
}

static SAY_SETTINGS: RwLock<SaySettings> = RwLock::new(SaySettings {
    say_token: String::new(),
    advertise_ip: None,
    http_port: 0,
});

fn lock_settings() -> std::sync::RwLockWriteGuard<'static, SaySettings> {
    SAY_SETTINGS.write().unwrap_or_else(|err| err.into_inner())
}

fn read_settings() -> std::sync::RwLockReadGuard<'static, SaySettings> {
    SAY_SETTINGS.read().unwrap_or_else(|err| err.into_inner())
}

/// 由 web 层在启动时安装。空串 = 不校验（内网自用默认）。
pub fn set_say_token(token: String) {
    lock_settings().say_token = token;
}

/// 由 gateway 在 MQTT 传输层启动成功后安装。`None` = 回退到自动探测局域网 IP。
pub fn set_say_advertise_ip(ip: Option<String>) {
    lock_settings().advertise_ip = ip;
}

/// 由 web 层在启动时安装：notify 音频 URL 的端口部分。
pub fn set_say_http_port(port: u16) {
    lock_settings().http_port = port;
}

#[derive(Deserialize)]
pub struct SayRequest {
    text: String,
    /// 目标设备：MAC（任意写法，内部归一化）或 MQTT client_id。
    /// 不给 = 恰好一台在线时用那台，多台报 409。
    #[serde(default)]
    device: Option<String>,
    // 不加 deny_unknown_fields：多传的字段（如 voice）按 serde 默认行为忽略。
}

fn err_body(error: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut body = serde_json::json!({ "ok": false, "error": error });
    if let (Some(map), serde_json::Value::Object(extra)) = (body.as_object_mut(), extra) {
        map.extend(extra);
    }
    body
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(err_body("unauthorized", serde_json::json!({}))),
    )
        .into_response()
}

fn bad_request(detail: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(err_body(
            "bad_request",
            serde_json::json!({ "detail": detail }),
        )),
    )
        .into_response()
}

fn device_offline() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(err_body("device_offline", serde_json::json!({}))),
    )
        .into_response()
}

fn tts_failed(detail: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(err_body(
            "tts_failed",
            serde_json::json!({ "detail": detail }),
        )),
    )
        .into_response()
}

/// 校验 Bearer 令牌。空 token = 不校验（内网自用），直接放行。
fn check_auth(headers: &HeaderMap) -> bool {
    let token = &read_settings().say_token;
    if token.is_empty() {
        return true;
    }
    let expect = format!("Bearer {token}");
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|got| got == expect)
}

/// 设备解析的失败原因。不直接返回 `Response`：clippy 的 result_large_err
/// 不允许把 128+ 字节的 Response 塞进 Result。
enum ResolveError {
    Offline,
    Multiple,
}

impl ResolveError {
    fn into_response(self) -> Response {
        match self {
            Self::Offline => device_offline(),
            Self::Multiple => (
                StatusCode::CONFLICT,
                Json(err_body("multiple_devices", serde_json::json!({}))),
            )
                .into_response(),
        }
    }
}

/// 在在线设备里解析目标：指定了按 client_id 精确匹配或按归一化 MAC 匹配；
/// 没指定则要求恰好一台在线。
fn resolve_device(
    devices: Vec<SayDeviceInfo>,
    device: Option<&str>,
) -> Result<SayDeviceInfo, ResolveError> {
    match device {
        Some(want) => {
            let want_mac = crate::mqtt_udp::colon_mac(want);
            devices
                .into_iter()
                .find(|info| info.client_id == want || info.device_id == want_mac)
                .ok_or(ResolveError::Offline)
        }
        None => {
            let mut iter = devices.into_iter();
            match (iter.next(), iter.next()) {
                (None, _) => Err(ResolveError::Offline),
                (Some(only), None) => Ok(only),
                (Some(_), Some(_)) => Err(ResolveError::Multiple),
            }
        }
    }
}

/// 从 Ogg Opus 字节里算时长（毫秒）。
///
/// Ogg 的 granule position 恒为 48 kHz 采样单位（与 Opus 内部时钟一致）。
/// ogg crate 只暴露 packet 所在页的 granule（`absgp_page`），但我们封装时
/// 末帧带 EndStream，最后一页的页 granule 就是全流总采样数。
/// 解析失败（比如测试用的假字节）返回 0，不报错 —— 时长只是响应里的
/// 参考信息，不值得为它 5xx。
fn ogg_duration_ms(bytes: &[u8]) -> u64 {
    let cursor = std::io::Cursor::new(bytes);
    let mut reader = ogg::reading::PacketReader::new(cursor);
    let mut last_granule = 0u64;
    loop {
        match reader.read_packet() {
            Ok(Some(packet)) => last_granule = packet.absgp_page(),
            Ok(None) => break,
            Err(_) => return 0,
        }
    }
    last_granule * 1000 / 48_000
}

pub async fn handle_say(headers: HeaderMap, Json(req): Json<SayRequest>) -> Response {
    if !check_auth(&headers) {
        return unauthorized();
    }

    let text = req.text.trim();
    if text.is_empty() {
        return bad_request("text 不能为空".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return bad_request(format!("text 超过 {MAX_TEXT_CHARS} 字上限"));
    }

    // say_hub 为 None = MQTT 传输层没跑（websocket 模式），
    // notify 是 MQTT 通路的能力，此时没有任何可下发的通道，按设备离线报。
    let Some(hub) = crate::mqtt_udp::say_hub() else {
        tracing::warn!("[say 诊断] say_hub() 为 None —— install_say_hub 没生效");
        return device_offline();
    };

    let target = match resolve_device(hub.online_devices().await, req.device.as_deref()) {
        Ok(info) => info,
        Err(err) => return err.into_response(),
    };

    if let Some(state) = hub.busy_state(&target.client_id).await {
        return (
            StatusCode::CONFLICT,
            Json(err_body(
                "device_busy",
                serde_json::json!({ "state": state }),
            )),
        )
            .into_response();
    }

    let Some(synthesizer) = synthesizer() else {
        return tts_failed("合成器未注入（web 层启动未完成）".to_string());
    };
    let ogg_bytes = match tokio::time::timeout(SYNTH_TIMEOUT, synthesizer.synthesize(text)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => return tts_failed(err),
        Err(_) => {
            return tts_failed(format!("合成超时（{}s）", SYNTH_TIMEOUT.as_secs()));
        }
    };

    let duration_ms = ogg_duration_ms(&ogg_bytes);
    let id = say_audio_store().insert(ogg_bytes, AUDIO_TTL);

    // URL 基址：优先 advertise_ip（MQTT endpoint host，设备能连 broker 就能连它），
    // 否则自动探测局域网 IP，都不行才回退回环地址并告警（设备基本拉不到，仅本地调试用）。
    let host = {
        let settings = read_settings();
        settings
            .advertise_ip
            .as_deref()
            .map(str::trim)
            .filter(|ip| !ip.is_empty())
            .map(str::to_string)
    }
    .or_else(crate::mqtt_udp::lan_ip)
    .unwrap_or_else(|| {
        tracing::warn!("没有可用的 advertise_ip / 局域网 IP，notify 音频 URL 回退到 127.0.0.1");
        "127.0.0.1".to_string()
    });
    let port = read_settings().http_port;
    let audio_url = format!("http://{host}:{port}/xiaozhi/say/{id}.ogg");

    let body = serde_json::json!({
        "type": "notify",
        "audio_url": audio_url,
        "subtitles": [{ "start_ms": 0, "text": text }],
    });
    // publish_json 返回 false = 下发时发现设备刚掉线（publish_body 内部已清理会话）
    if !hub.publish_json(&target.client_id, &body.to_string()).await {
        return device_offline();
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "device": target.device_id,
            "mode": "notify",
            "audio_url": audio_url,
            "duration_ms": duration_ms,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt_udp::{SayDeviceHub, install_say_hub};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FAKE_OGG: &[u8] = b"OggS-fake";

    struct FakeSynth;

    #[async_trait::async_trait]
    impl SaySynthesizer for FakeSynth {
        async fn synthesize(&self, _text: &str) -> Result<Vec<u8>, String> {
            Ok(FAKE_OGG.to_vec())
        }
    }

    struct FakeHub {
        devices: Mutex<Vec<SayDeviceInfo>>,
        busy: Mutex<HashMap<String, &'static str>>,
        published: Mutex<Vec<(String, String)>>,
    }

    impl FakeHub {
        fn device(client_id: &str) -> SayDeviceInfo {
            SayDeviceInfo {
                client_id: client_id.to_string(),
                device_id: crate::mqtt_udp::colon_mac(client_id),
            }
        }

        fn one() -> Arc<Self> {
            Arc::new(Self {
                devices: Mutex::new(vec![Self::device("84:c7:bb:59:93:64")]),
                busy: Mutex::new(HashMap::new()),
                published: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl SayDeviceHub for FakeHub {
        async fn online_devices(&self) -> Vec<SayDeviceInfo> {
            self.devices
                .lock()
                .unwrap()
                .iter()
                .map(|info| SayDeviceInfo {
                    client_id: info.client_id.clone(),
                    device_id: info.device_id.clone(),
                })
                .collect()
        }

        async fn busy_state(&self, client_id: &str) -> Option<&'static str> {
            self.busy.lock().unwrap().get(client_id).copied()
        }

        async fn publish_json(&self, client_id: &str, body: &str) -> bool {
            self.published
                .lock()
                .unwrap()
                .push((client_id.to_string(), body.to_string()));
            true
        }
    }

    struct Server {
        addr: std::net::SocketAddr,
    }

    /// 起一台真实 HTTP 服务器（仿 say_audio.rs 的测试写法）。
    ///
    /// 每个用例各自装假 hub（install_say_hub 是覆盖式的）、各自设置
    /// say 设置槽 —— CI 单线程跑测试，全局槽没有并行竞争。
    async fn boot(hub: Arc<FakeHub>, token: &str) -> Server {
        install_say_hub(hub);
        set_say_synthesizer(Arc::new(FakeSynth));
        set_say_token(token.to_string());
        set_say_advertise_ip(Some("127.0.0.1".to_string()));

        let app = crate::add_routes(axum::Router::new(), Arc::new(crate::EchoStrategy));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定测试端口失败");
        let addr = listener.local_addr().expect("获取测试端口失败");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("测试服务器启动失败");
        });
        // 音频 URL 的端口必须指向这台测试服务器，后面才能用同一地址回拉。
        set_say_http_port(addr.port());
        Server { addr }
    }

    struct HttpResp {
        status: u16,
        body: String,
    }

    async fn http_post(
        addr: std::net::SocketAddr,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> HttpResp {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            req.push_str(&format!("{name}: {value}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let raw = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = raw
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("状态行");
        let body = raw
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        HttpResp { status, body }
    }

    async fn http_get(addr: std::net::SocketAddr, path: &str) -> Vec<u8> {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        buf
    }

    async fn post_say(addr: std::net::SocketAddr, body: &str) -> HttpResp {
        http_post(addr, "/api/v1/xiaozhi/say", body, &[]).await
    }

    #[tokio::test]
    async fn busy_device_returns_409_with_state() {
        let hub = FakeHub::one();
        hub.busy
            .lock()
            .unwrap()
            .insert("84:c7:bb:59:93:64".to_string(), "listening");
        let server = boot(hub.clone(), "").await;

        let resp = post_say(server.addr, r#"{"text":"你好"}"#).await;
        assert_eq!(resp.status, 409, "设备忙应 409：{}", resp.body);
        assert!(resp.body.contains("\"device_busy\""), "{}", resp.body);
        assert!(
            resp.body.contains("\"state\":\"listening\""),
            "{}",
            resp.body
        );
        assert!(hub.published.lock().unwrap().is_empty(), "忙时不应下发");
    }

    #[tokio::test]
    async fn multiple_devices_without_selector_returns_409() {
        let hub = Arc::new(FakeHub {
            devices: Mutex::new(vec![
                FakeHub::device("aa:bb:cc:dd:ee:01"),
                FakeHub::device("aa:bb:cc:dd:ee:02"),
            ]),
            busy: Mutex::new(HashMap::new()),
            published: Mutex::new(Vec::new()),
        });
        let server = boot(hub, "").await;

        let resp = post_say(server.addr, r#"{"text":"你好"}"#).await;
        assert_eq!(resp.status, 409, "{}", resp.body);
        assert!(resp.body.contains("\"multiple_devices\""), "{}", resp.body);
    }

    #[tokio::test]
    async fn empty_token_allows_request_without_authorization() {
        let server = boot(FakeHub::one(), "").await;

        let resp = post_say(server.addr, r#"{"text":"你好"}"#).await;
        assert_eq!(resp.status, 200, "空 token 应放行：{}", resp.body);
        assert!(resp.body.contains("\"ok\":true"), "{}", resp.body);
    }

    #[tokio::test]
    async fn token_auth_required_missing_wrong_correct() {
        let server = boot(FakeHub::one(), "secret-token").await;

        let missing = post_say(server.addr, r#"{"text":"你好"}"#).await;
        assert_eq!(missing.status, 401, "{}", missing.body);
        assert!(
            missing.body.contains("\"unauthorized\""),
            "{}",
            missing.body
        );

        let wrong = http_post(
            server.addr,
            "/api/v1/xiaozhi/say",
            r#"{"text":"你好"}"#,
            &[("Authorization", "Bearer nope")],
        )
        .await;
        assert_eq!(wrong.status, 401, "{}", wrong.body);

        let right = http_post(
            server.addr,
            "/api/v1/xiaozhi/say",
            r#"{"text":"你好"}"#,
            &[("Authorization", "Bearer secret-token")],
        )
        .await;
        assert_eq!(right.status, 200, "{}", right.body);
    }

    #[tokio::test]
    async fn happy_path_publishes_notify_and_audio_is_fetchable() {
        let hub = FakeHub::one();
        let server = boot(hub.clone(), "").await;

        let resp = post_say(server.addr, r#"{"text":"取件提醒"}"#).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(resp.body.contains("\"ok\":true"), "{}", resp.body);
        assert!(resp.body.contains("\"mode\":\"notify\""), "{}", resp.body);
        assert!(
            resp.body.contains("\"device\":\"84:c7:bb:59:93:64\""),
            "{}",
            resp.body
        );

        // 下发的 notify 消息：目标设备正确、含 audio_url 和原文 subtitles
        let published = hub.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1, "应恰好下发一条");
        assert_eq!(published[0].0, "84:c7:bb:59:93:64");
        let notify: serde_json::Value = serde_json::from_str(&published[0].1).expect("notify JSON");
        assert_eq!(notify["type"], "notify");
        let audio_url = notify["audio_url"].as_str().expect("audio_url");
        assert_eq!(notify["subtitles"][0]["text"], "取件提醒");
        drop(published);

        // 用响应里的 URL 路径回拉同一台服务器，应拿到合成器的原始字节
        let path = audio_url
            .strip_prefix(&format!("http://127.0.0.1:{}", server.addr.port()))
            .expect("audio_url 应指向测试服务器");
        let raw = http_get(server.addr, path).await;
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(raw_text.starts_with("HTTP/1.1 200"), "{raw_text}");
        assert!(
            raw.ends_with(FAKE_OGG),
            "回拉 body 应为合成器字节：{raw_text}"
        );
    }

    #[tokio::test]
    async fn extra_fields_like_voice_are_ignored() {
        let server = boot(FakeHub::one(), "").await;

        let resp = post_say(server.addr, r#"{"text":"你好","voice":"xxx"}"#).await;
        assert_eq!(resp.status, 200, "多传字段应被忽略：{}", resp.body);
    }

    #[tokio::test]
    async fn empty_or_too_long_text_returns_400() {
        let server = boot(FakeHub::one(), "").await;

        let empty = post_say(server.addr, r#"{"text":"   "}"#).await;
        assert_eq!(empty.status, 400, "{}", empty.body);
        assert!(empty.body.contains("\"bad_request\""), "{}", empty.body);

        let long = format!(r#"{{"text":"{}"}}"#, "汉".repeat(501));
        let resp = post_say(server.addr, &long).await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert!(resp.body.contains("\"bad_request\""), "{}", resp.body);
    }
}
