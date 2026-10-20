use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── 消息类型 ─────────────────────────────────────────────

/// 客户端→服务器的 JSON 文本消息
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        #[serde(default)]
        version: u32,
        #[serde(default = "default_transport")]
        transport: String,
        audio_params: AudioParams,
        #[serde(default)]
        features: Features,
    },
    Listen {
        state: ListenState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Abort,
}

fn default_transport() -> String {
    "websocket".to_string()
}

/// 服务器→客户端的 JSON 文本消息
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        version: u32,
        transport: String,
        session_id: String,
        audio_params: AudioParams,
    },
    Tts {
        session_id: String,
        state: TtsState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Stt {
        session_id: String,
        text: String,
    },
    Error {
        code: String,
        message: String,
    },
}

// ─── 枚举 ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenState {
    Detect,
    Start,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsState {
    Start,
    #[serde(rename = "sentence_start")]
    SentenceStart,
    Stop,
}

// ─── 会话状态 ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionState {
    /// 等待 HELLO 握手
    AwaitingHello,
    /// 已握手，等待 listen 指令
    Ready,
    /// 录音中，接收音频帧
    Recording,
    /// 回放中，发送音频帧
    Playing,
}

// ─── 音频参数 ──────────────────────────────────────────────

/// 音频参数（双向协商）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioParams {
    #[serde(default = "default_audio_format")]
    pub format: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_frame_duration")]
    pub frame_duration: u32,
}

impl Default for AudioParams {
    fn default() -> Self {
        Self {
            format: default_audio_format(),
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            frame_duration: default_frame_duration(),
        }
    }
}

fn default_audio_format() -> String {
    "opus".to_string()
}
const fn default_sample_rate() -> u32 {
    24000
}
const fn default_channels() -> u8 {
    1
}
const fn default_frame_duration() -> u32 {
    60
}

// ─── 设备功能 ──────────────────────────────────────────────

/// 设备功能特性（参考 xiaozhi-client 和 xiaozhi-esp32-server）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    #[serde(default)]
    pub aec: bool,
    #[serde(default)]
    pub mcp: bool,
    #[serde(default = "default_emoji")]
    pub emoji: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            aec: false,
            mcp: false,
            emoji: default_emoji(),
            extra: HashMap::new(),
        }
    }
}

const fn default_emoji() -> bool {
    true
}

// ─── OTA 类型 ──────────────────────────────────────────────

/// OTA 请求
#[derive(Debug, Deserialize)]
pub struct OtaRequest {
    pub version: u32,
    /// 设备 MAC。缺省为空时，OTA 再尝试 `Device-Id` 请求头。
    #[serde(default)]
    pub mac_address: String,
    pub uuid: String,
    #[serde(default)]
    pub chip_model_name: Option<String>,
    #[serde(default)]
    pub application: Option<ApplicationInfo>,
    #[serde(default)]
    pub board: Option<BoardInfo>,
    #[serde(default)]
    pub flash_size: Option<u64>,
    #[serde(default)]
    pub minimum_free_heap_size: Option<String>,
    #[serde(default)]
    pub ota: Option<OtaInfo>,
}

/// OTA 响应
#[derive(Debug, Serialize)]
pub struct OtaResponse {
    /// 只有 protocol = websocket 时下发
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket: Option<WebsocketInfo>,
    /// 只有 protocol = mqtt 时下发。
    /// 固件看到 mqtt 对象就选 MqttProtocol，没有回退到 websocket。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttInfo>,
    pub server_time: ServerTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<FirmwareInfo>,
    pub audio_params: AudioParams,
}

/// 下发给设备的 MQTT 接入参数。
#[derive(Debug, Serialize, Clone)]
pub struct MqttInfo {
    /// 必须带端口，如 "192.168.110.35:1883"。
    /// 不带端口时固件默认 8883，会强制 TLS，明文 broker 会连不上。
    pub endpoint: String,
    /// `GID_<型号>@@@<mac下划线>@@@<mac下划线>`
    pub client_id: String,
    pub username: String,
    pub password: String,
    /// 心跳秒数，默认 240。
    pub keepalive: u32,
    /// 设备上行主题。
    pub publish_topic: String,
    /// 下行主题模板。设备忽略 topic，仅供约定。
    pub subscribe_topic: String,
}

/// 小智设备走哪条传输。默认 websocket，避免 M2' 把现网设备切到 MQTT。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum XiaozhiProtocol {
    #[default]
    Websocket,
    Mqtt,
}

/// 小智传输配置。`protocol = "mqtt"` 时 OTA 才下发 `mqtt` 段。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct XiaozhiConfig {
    /// `"websocket"`（默认）或 `"mqtt"`。非法取值反序列化失败，不回退默认。
    #[serde(default)]
    pub protocol: XiaozhiProtocol,
    #[serde(default)]
    pub mqtt: XiaozhiMqttConfig,
    #[serde(default)]
    pub udp: XiaozhiUdpConfig,
    /// OTA 端点是否只接受私有网络（局域网/回环）来源，默认 true。
    ///
    /// 为什么必须限制：`/xiaozhi/ota/` 无法真正鉴权 —— 固件没有预共享密钥，
    /// `Device-Id` 只是自报的 MAC。而 MQTT 模式下这个端点会直接返回
    /// `HMAC(auth_key, client_id)` 派生的连接密码。
    /// 也就是说：谁能访问这个 HTTP 端口，谁就能给任意 MAC 铸出合法密码、
    /// 顶掉真设备、拿到 UDP hello 的 key/nonce。HMAC 的安全性实际取决于
    /// 这个端点的可达范围。
    #[serde(default = "default_ota_allow_private_only")]
    pub ota_allow_private_only: bool,
    /// OTA 共享令牌（可选，但强烈建议设置）。
    ///
    /// **这是 OTA 端点唯一无法被伪造的关卡。** 基于 IP 的判断在
    /// Docker 端口映射 / 反向代理下都会失效 —— 容器里看到的对端永远是
    /// 网桥地址，X-Forwarded-For 又可以由客户端伪造。而 ota_url 存在设备
    /// NVS 里、由我们在 `http->Open()` 时原样发出，所以把令牌放进
    /// query 就能形成真正的共享密钥：
    ///
    /// ```text
    /// 设备 NVS ota_url = http://<host>:9527/xiaozhi/ota/?token=<令牌>
    /// haimen settings  [xiaozhi] ota_token = "<同一个令牌>"
    /// ```
    ///
    /// 为空表示不校验（向后兼容，但 OTA 仍能被任意可达客户端利用）。
    #[serde(default)]
    pub ota_token: String,
    /// 可信反向代理地址列表。
    ///
    /// 只有当 TCP 对端在这个列表里时，才会去读 `X-Forwarded-For`，
    /// 并从右往左跳过同样是可信代理的条目，取第一个非代理地址作为
    /// 真实客户端。对端不在列表里时 **完全忽略** 该头（它可被伪造）。
    #[serde(default)]
    pub ota_trusted_proxies: Vec<std::net::IpAddr>,
    /// `POST /api/v1/xiaozhi/say` 的 Bearer 令牌。空串 = 不校验（内网自用默认），
    /// 非空时请求必须带 `Authorization: Bearer <say_token>`，不匹配返回 401。
    #[serde(default)]
    pub say_token: String,
}

impl Default for XiaozhiConfig {
    fn default() -> Self {
        Self {
            protocol: XiaozhiProtocol::Websocket,
            mqtt: XiaozhiMqttConfig::default(),
            udp: XiaozhiUdpConfig::default(),
            ota_allow_private_only: true,
            ota_token: String::new(),
            ota_trusted_proxies: Vec::new(),
            say_token: String::new(),
        }
    }
}

/// MQTT broker 与主题模板。`{mac}` 会换成下划线形式的 MAC。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct XiaozhiMqttConfig {
    /// 形如 `192.168.110.35:1883`。缺端口时 OTA 会补 `:1883`。
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// MQTT 连接鉴权密钥。留空 = 不校验（向后兼容）。
    ///
    /// 设置后：设备密码由 `HMAC-SHA256(auth_key, client_id)` 派生，
    /// broker 在 CONNECT 时重算比较；控制面客户端用 client_id =
    /// `haimen-admin`。不设的话局域网内任何人都能注入播报或顶掉设备。
    #[serde(default)]
    pub auth_key: String,
    /// 迁移期是否继续接受配置里的静态 `password`。
    ///
    /// 设备只在开机时拉 OTA，直接开鉴权会让在线设备失联。
    /// 开启鉴权的步骤：auth_key + auth_grace=true → 重启设备 → auth_grace=false。
    #[serde(default)]
    pub auth_grace: bool,
    #[serde(default = "default_keepalive")]
    pub keepalive: u32,
    #[serde(default = "default_publish_topic")]
    pub publish_topic_template: String,
    #[serde(default = "default_downlink_topic")]
    pub downlink_topic_template: String,
}

const fn default_ota_allow_private_only() -> bool {
    true
}

const fn default_keepalive() -> u32 {
    240
}

fn default_publish_topic() -> String {
    "haimen/xz/{mac}/up".to_string()
}

fn default_downlink_topic() -> String {
    "haimen/xz/{mac}/down".to_string()
}

impl Default for XiaozhiMqttConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            username: String::new(),
            password: String::new(),
            auth_key: String::new(),
            auth_grace: false,
            keepalive: default_keepalive(),
            publish_topic_template: default_publish_topic(),
            downlink_topic_template: default_downlink_topic(),
        }
    }
}

/// UDP 音频监听地址。M2' 只入库，不在启动流程里 bind。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct XiaozhiUdpConfig {
    #[serde(default = "default_udp_bind")]
    pub bind_addr: String,
}

fn default_udp_bind() -> String {
    "0.0.0.0:8888".to_string()
}

impl Default for XiaozhiUdpConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_udp_bind(),
        }
    }
}

/// WebSocket 连接信息
#[derive(Debug, Serialize)]
pub struct WebsocketInfo {
    pub url: String,
    #[serde(default)]
    pub token: String,
    pub version: u32,
}

/// 服务器时间
#[derive(Debug, Serialize)]
pub struct ServerTime {
    pub timestamp: i64,
    pub timezone_offset: i32,
}

/// 固件信息
#[derive(Debug, Serialize)]
pub struct FirmwareInfo {
    pub version: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub force: bool,
}

/// 应用信息
#[derive(Debug, Deserialize)]
pub struct ApplicationInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub compile_time: Option<String>,
    #[serde(default)]
    pub board: Option<HashMap<String, String>>,
}

/// 板子信息
#[derive(Debug, Deserialize)]
pub struct BoardInfo {
    pub r#type: String,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub ssid: Option<String>,
    #[serde(default)]
    pub rssi: Option<i32>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub channel: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
}

/// OTA 信息
#[derive(Debug, Deserialize)]
pub struct OtaInfo {
    #[serde(default)]
    pub label: Option<String>,
}

// ─── 音频帧 ────────────────────────────────────────────────

/// 缓冲的音频帧
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub timestamp: u32,
    pub data: Vec<u8>,
}

/// 回放通道事件：策略层通过该事件流既发送音频帧，也发送文本消息。
///
/// 单个通道天然保持事件顺序，策略侧「先 Stt、后句子、穿插音频」的发送顺序
/// 会原样到达 ws.rs，无需额外的顺序协调。
#[derive(Debug, Clone)]
pub enum PlaybackEvent {
    /// 用户语音识别文本（ASR 结果）→ 设备 `stt` 消息
    Stt(String),
    /// LLM 回复文本（句级）→ 设备 `tts/sentence_start` 消息
    LlmSentence(String),
    /// TTS 音频帧 → Opus 二进制帧
    Audio(AudioFrame),
}
