//! MQTT + UDP 传输层。
//!
//! 设备连的是 haimen 自己托管的 broker，不是 `mqtt.rs` 里的 rumqttc 客户端。
//! 控制面走 [`MqttBrokerHandle::incoming`] / [`publish_to`]，音频走 [`UdpAudioHandle`]。
//! 帧节奏从 `ws.rs` 复制（见各函数注释里的行号），不抽公共层。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant as TokioInstant;

use crate::mqtt_broker::{
    BrokerIncoming, BrokerOutgoing, MqttBrokerConfig, MqttBrokerError, MqttBrokerHandle,
};
use crate::protocol_udp::{UdpCrypto, UdpProtoError};
use crate::strategy::ResponseStrategy;
use crate::types::{AudioFrame, AudioParams, PlaybackEvent, ServerMessage, SessionState, TtsState};
use crate::udp_audio::{UdpAudioConfig, UdpAudioError, UdpAudioHandle};

/// 16 字节头之外的载荷上限。再大整包就会超过 1500，设备会静默丢掉。
const MAX_DOWNLINK_PAYLOAD: usize = 1484;

/// 录音超过这个时间还没收到 listen stop，由服务器收口。设备自己不会回 Idle。
const RECORDING_LIMIT: Duration = Duration::from_secs(30);

/// 一轮对话结束后，这么久没有新的语音输入就发 goodbye 收口。
///
/// 不发的话设备会一直停在 Listening：持续把麦克风音频推上来（隐私问题），
/// 我们的 ASR 管道也会不停重启（白烧火山额度），屏幕也回不到「待命」。
/// 15 秒足够覆盖「说完一句再补一句」的自然停顿。
const IDLE_CLOSE_LIMIT: Duration = Duration::from_secs(15);

/// 单条 MQTT 下发的写队列超时。
///
/// 写队列容量 32，设备侧 TCP 卡住时 `publish_to` 会一直 await。
/// 不设上限的话，一个坏设备能把整个传输层 run_loop 拖死。
const MQTT_WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// 单帧喂入策略侧 ASR 的超时。下游积压时丢帧优于卡死全局循环。
const ASR_FEED_TIMEOUT: Duration = Duration::from_secs(1);

/// 上行 hello 没带采样率时用 16 kHz。这是设备麦克风的参数，不是下行 TTS 的 24 kHz。
const fn default_uplink_rate() -> u32 {
    16_000
}

fn default_opus() -> String {
    "opus".to_owned()
}

const fn default_channels() -> u8 {
    1
}

const fn default_frame_ms() -> u32 {
    60
}

/// 发 `tts stop` 前额外等待的帧数，默认 2。
/// 来历是参考实现的 `prebuffer + 2` 公式里那个 2，覆盖 UDP 抖动与固件调度延迟。
const fn default_tail_extra_frames() -> u32 {
    2
}

/// 传输层配置。`prebuffer_count` / `frame_duration_ms` 的默认值见 [`Default`]。
#[derive(Debug, Clone)]
pub struct MqttUdpConfig {
    pub broker: MqttBrokerConfig,
    pub udp: UdpAudioConfig,
    /// 设备上行主题模板，`{mac}` 替换为带冒号的 MAC（如 84:c7:bb:59:93:64）。
    pub publish_topic_template: String,
    /// 服务器下行主题模板。设备不看 topic，只用于日志和抓包。
    pub downlink_topic_template: String,
    /// 预缓冲帧数，默认 5（与 ws.rs 一致）。
    pub prebuffer_count: usize,
    /// 帧时长毫秒，默认 60。
    pub frame_duration_ms: u32,
    /// 发 `tts stop` 前额外等待的帧数（乘 frame_duration）。
    /// 默认 2（= 参考实现的 `prebuffer + 2` 里的那个 2）。
    pub tail_extra_frames: u32,
    /// 下发 hello 时告诉设备的 UDP 服务器地址。
    ///
    /// 为什么需要显式指定：容器里 `local_ip()` 拿到的是容器 IP（如 192.168.215.2），
    /// 局域网里的真机根本到不了。所以应填入**设备可达的主机地址** ——
    /// 最自然的值就是 MQTT endpoint 里的 host（设备既然能连上 broker，
    /// 就一定也能用同一地址发 UDP）。
    ///
    /// `None` 时回退到自动探测（裸机部署下可用）。
    pub advertise_ip: Option<String>,
}

impl Default for MqttUdpConfig {
    fn default() -> Self {
        Self {
            broker: MqttBrokerConfig::default(),
            udp: UdpAudioConfig {
                bind_addr: "0.0.0.0:0".to_owned(),
            },
            publish_topic_template: "device-server".to_owned(),
            downlink_topic_template: "devices/p2p/{mac}".to_owned(),
            prebuffer_count: 5,
            frame_duration_ms: 60,
            tail_extra_frames: default_tail_extra_frames(),
            advertise_ip: None,
        }
    }
}

/// 传输层启动失败。运行期的单包错误打日志后丢弃，不从这里返回。
#[derive(Debug)]
pub enum MqttUdpError {
    Broker(MqttBrokerError),
    Udp(UdpAudioError),
    Proto(UdpProtoError),
}

impl fmt::Display for MqttUdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Broker(err) => write!(f, "MQTT broker 错误: {err}"),
            Self::Udp(err) => write!(f, "UDP 音频通道错误: {err}"),
            Self::Proto(err) => write!(f, "UDP 协议错误: {err}"),
        }
    }
}

impl std::error::Error for MqttUdpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Broker(err) => Some(err),
            Self::Udp(err) => Some(err),
            Self::Proto(err) => Some(err),
        }
    }
}

impl From<MqttBrokerError> for MqttUdpError {
    fn from(err: MqttBrokerError) -> Self {
        Self::Broker(err)
    }
}

impl From<UdpAudioError> for MqttUdpError {
    fn from(err: UdpAudioError) -> Self {
        Self::Udp(err)
    }
}

impl From<UdpProtoError> for MqttUdpError {
    fn from(err: UdpProtoError) -> Self {
        Self::Proto(err)
    }
}

/// 一台设备的会话。按 MQTT `client_id` 区分，UDP 会话只是它的音频密钥。
struct DeviceSession {
    client_id: String,
    /// 从 `client_id` 的 `@@@` 分段里取出、并规范成冒号形式的 MAC。
    device_id: String,
    /// 下发给设备的协议会话号（UUID 文本），和 UDP 的 u32 会话号不是同一个。
    protocol_session_id: String,
    state: SessionState,
    /// UDP 会话 ID，等于下发 nonce 的 `[4..8)`。
    udp_session_id: Option<u32>,
    /// 录音阶段缓冲的音频帧，交给 strategy。
    audio_buffer: Vec<AudioFrame>,
    /// 发送侧帧节奏。`None` 表示还没离开预缓冲连发。
    next_send_at: Option<Instant>,
    /// 下行 timestamp。必须从 1 起跳，上下行共用 key 时 0 会和上行首包撞 IV。
    downlink_ts: u32,
    recording_deadline: Option<TokioInstant>,
    last_activity: Instant,
    /// 最近一次真正给设备下发音频的时刻。
    ///
    /// 收口超时以它为基准，而不是 last_activity 或 finish_ready：
    /// - 设备在 Listening 时会不停推音频，`last_activity` 永远在刷新
    /// - 环境噪音会让 VAD 反复触发「生成回复」，`finish_ready` 也一直在刷新
    ///   （实测 ASR 返回空文本时仍会走完一轮）
    ///
    /// 只有真正播放了音频，才说明这一轮对话有内容。
    last_speech_at: Instant,
    /// 每开一轮回放加一。旧回放任务靠它知道自己已经被替换。
    playback_gen: u64,
    cancel_tx: Option<watch::Sender<bool>>,
}

impl DeviceSession {
    fn new(client_id: &str) -> Self {
        Self {
            client_id: client_id.to_owned(),
            device_id: mac_from_client_id(client_id),
            protocol_session_id: String::new(),
            state: SessionState::AwaitingHello,
            udp_session_id: None,
            audio_buffer: Vec::new(),
            next_send_at: None,
            downlink_ts: 1,
            recording_deadline: None,
            last_activity: Instant::now(),
            last_speech_at: Instant::now(),
            playback_gen: 0,
            cancel_tx: None,
        }
    }

    /// 停掉当前回放并回到 Ready。UDP 密钥和对端地址留着。
    fn end_round(&mut self) {
        self.playback_gen = self.playback_gen.wrapping_add(1);
        if let Some(tx) = &self.cancel_tx {
            let _ = tx.send(true);
        }
        self.state = SessionState::Ready;
        self.audio_buffer.clear();
        self.recording_deadline = None;
        self.next_send_at = None;
        self.last_activity = Instant::now();
        // 收口计时也要重置。
        //
        // 打断是「这一轮结束、设备马上要接着说话」，不是「会话结束」。
        // 不刷新的话 prune_sessions 会拿 hello 时刻的 last_speech_at 去比，
        // 15 秒后把设备正要复用的通道 goodbye 掉。hello 有同样的处理。
        self.last_speech_at = Instant::now();
    }
}

struct Tables {
    sessions: HashMap<String, DeviceSession>,
    /// `udp session_id` → `client_id`。未知 id 的包在 UDP 层就会被丢掉，这里再挡一层。
    udp_index: HashMap<u32, String>,
}

struct Shared {
    broker: MqttBrokerHandle,
    udp: UdpAudioHandle,
    strategy: Arc<dyn ResponseStrategy>,
    cfg: MqttUdpConfig,
    tables: Mutex<Tables>,
    playbacks: Mutex<Vec<JoinHandle<()>>>,
    shutdown_tx: watch::Sender<bool>,
}

struct PlaybackJob {
    client_id: String,
    protocol_session_id: String,
    generation: u64,
    buffer: Vec<AudioFrame>,
    cancel: watch::Receiver<bool>,
}

/// 一轮下行播放的下发统计（T2 埋点）。
///
/// 用途是 T1 上线前后的 A/B 对比：`elapsed_ms` 明显小于 `expected_ms`
/// 说明发送侧被砍帧；`tail_wait_ms` 是发 stop 前实际等尾帧的时长。
#[derive(Default)]
struct PlaybackStats {
    /// 本轮真正发给设备的帧数。
    frames: u64,
    first_send_at: Option<Instant>,
    last_send_at: Option<Instant>,
}

impl PlaybackStats {
    /// 每成功下发一帧记一次。热路径里只做加法和时间戳，不做任何字符串操作。
    fn note_frame_sent(&mut self) {
        let now = Instant::now();
        self.frames += 1;
        if self.first_send_at.is_none() {
            self.first_send_at = Some(now);
        }
        self.last_send_at = Some(now);
    }
}

/// 生成任务跟回放任务绑在一起。回放提前结束时把还在跑的合成停掉。
struct AbortJoin(Option<JoinHandle<()>>);

impl Drop for AbortJoin {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// MQTT 控制面 + UDP 音频。`start` 之后主循环在后台跑。
pub struct MqttUdpTransport {
    broker_addr: String,
    udp_addr: std::net::SocketAddr,
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
    udp: UdpAudioHandle,
}

impl MqttUdpTransport {
    /// 绑定 broker 和 UDP，并 spawn 主循环。
    pub async fn start(
        cfg: MqttUdpConfig,
        strategy: Arc<dyn ResponseStrategy>,
    ) -> Result<Self, MqttUdpError> {
        let broker = MqttBrokerHandle::start(cfg.broker.clone()).await?;
        let udp = UdpAudioHandle::start(cfg.udp.clone()).await?;
        let broker_addr = broker.local_addr().to_string();
        let udp_addr = udp.local_addr();
        // incoming 只能取一次，必须在别的任务碰句柄之前拿走。
        let mqtt_rx = broker.incoming();
        let udp_rx = udp.incoming();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            broker,
            udp: udp.clone(),
            strategy,
            cfg,
            tables: Mutex::new(Tables {
                sessions: HashMap::new(),
                udp_index: HashMap::new(),
            }),
            playbacks: Mutex::new(Vec::new()),
            shutdown_tx: shutdown_tx.clone(),
        });
        let task_shared = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            run_loop(task_shared, mqtt_rx, udp_rx, shutdown_rx).await;
        });
        Ok(Self {
            broker_addr,
            udp_addr,
            shared,
            shutdown_tx,
            task,
            udp,
        })
    }

    /// broker 实际监听地址。端口写 0 时用它拿内核分配的端口。
    pub fn broker_addr(&self) -> String {
        self.broker_addr.clone()
    }

    /// UDP 实际监听地址。
    pub fn udp_addr(&self) -> std::net::SocketAddr {
        self.udp_addr
    }

    /// 当前还有会话记录的设备数（含刚连上、还在等 hello 的）。
    pub async fn online_device_count(&self) -> usize {
        self.shared.tables.lock().await.sessions.len()
    }

    /// 停主循环和 UDP 收包。broker 的 accept 任务没有公开的停止接口，进程退出时一起结束。
    pub async fn shutdown(self) {
        let Self {
            shutdown_tx,
            task,
            udp,
            ..
        } = self;
        let _ = shutdown_tx.send(true);
        if tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .is_err()
        {
            tracing::warn!("MQTT/UDP 传输主循环没有在 3 秒内退出");
        }
        udp.shutdown().await;
    }
}

async fn run_loop(
    shared: Arc<Shared>,
    mut mqtt_rx: mpsc::Receiver<BrokerIncoming>,
    mut udp_rx: mpsc::Receiver<crate::udp_audio::UdpIncomingAudio>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let next_deadline = {
            let tables = shared.tables.lock().await;
            tables
                .sessions
                .values()
                .filter(|session| session.state == SessionState::Recording)
                .filter_map(|session| session.recording_deadline)
                .min()
        };
        tokio::select! {
          biased;
          changed = shutdown_rx.changed() => {
            if changed.is_err() || *shutdown_rx.borrow() {
              break;
            }
          }
          msg = mqtt_rx.recv() => {
            match msg {
              Some(msg) => handle_mqtt(&shared, msg).await,
              None => break,
            }
          }
          audio = udp_rx.recv() => {
            match audio {
              Some(audio) => handle_udp(&shared, audio).await,
              None => break,
            }
          }
          _ = async {
            if let Some(deadline) = next_deadline {
              tokio::time::sleep_until(deadline).await;
            } else {
              std::future::pending::<()>().await;
            }
          } => {
            expire_recordings(&shared).await;
          }
          // ★ 策略侧的「说完了」信号。
          // AsrLlmTtsStrategy 在流式 ASR 里自己检测到语音结束（服务端 VAD 判停
          // 或本地静音）时会 notify 这个 Notifier。ws.rs 一直靠它及时收口，
          // 而 MQTT 传输层最初只等设备的 listen stop —— 自动模式下设备根本不会
          // 发 stop，于是 ASR 识别完却没人触发 LLM/TTS，表现为「一直在聆听中」。
          //
          // 每次循环都重新取一次 Notifier：策略在每次录音开始时可能换掉内部的
          // Arc，缓存旧 Arc 会永远收不到信号。
          // 多个设备可能同时在录音，所以这里轮询【所有】Recording 会话各自的
          // notifier，谁先完成就推进谁。用一个全局 notifier 会让 A 设备说完
          // 把 B 设备也一起收口（多设备串台）。
          done_key = async {
            let keys: Vec<String> = {
              let tables = shared.tables.lock().await;
              tables
                .sessions
                .iter()
                .filter(|(_, session)| session.state == SessionState::Recording)
                .map(|(client_id, _)| client_id.clone())
                .collect()
            };
            if keys.is_empty() {
              return std::future::pending::<String>().await;
            }
            type KeyFuture = std::pin::Pin<
              Box<dyn std::future::Future<Output = String> + Send>,
            >;
            let futures: Vec<KeyFuture> = keys
              .into_iter()
              .map(|key| {
                let strategy = shared.strategy.clone();
                let fut: KeyFuture = Box::pin(async move {
                  match strategy.vad_completion(&key) {
                    Some(notify) => notify.notified().await,
                    None => std::future::pending::<()>().await,
                  }
                  key
                });
                fut
              })
              .collect();
            futures_util::future::select_all(futures).await.0
          } => {
            finish_recording_by_vad(&shared, &done_key).await;
          }
          _ = tick.tick() => {
            prune_sessions(&shared).await;
          }
        }
    }
    stop_playbacks(&shared).await;
}

/// 控制面转发主题前缀。
///
/// 任何 MQTT 客户端把消息发到 `haimen/ctl/<目标 client_id>`，传输层就会把
/// payload **原样**作为下行推给该设备（topic 由设备忽略，用固定值即可）。
///
/// 用途：
/// - 联调/测试：外部脚本可以直接让设备播报，不必改 haimen 代码
/// - M4 主动播报的基础：HA 自动化或任何脚本都能触发
///
/// 之所以放在传输层而不是 broker：broker 的 `handle_packet` 只能看到当前连接，
/// 拿不到全局注册表；而传输层的 `incoming()` 天然能收到所有客户端的 PUBLISH。
const CONTROL_TOPIC_PREFIX: &str = "haimen/ctl/";

/// 解析 `__say_audio` 命令里的音频帧（hex 编码，避免引入 base64 依赖）。
///
/// 期望格式：
/// {"type":"__say_audio","frames_hex":["3c1a...", "..."]}
fn parse_say_frames(value: &serde_json::Value) -> Result<Vec<Vec<u8>>, String> {
    let arr = value
        .get("frames_hex")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "缺少 frames_hex 数组".to_string())?;
    if arr.is_empty() {
        return Err("frames_hex 为空".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let hex = item
            .as_str()
            .ok_or_else(|| format!("第 {i} 帧不是字符串"))?;
        let bytes = decode_hex(hex).ok_or_else(|| format!("第 {i} 帧 hex 非法"))?;
        out.push(bytes);
    }
    Ok(out)
}

/// 主动播报：用预编码的 Opus 帧走一遍 tts 会话。
///
/// 为什么不用 notify：设备固件是 2.2.6，`notify` 是较新固件才有的能力
/// （实测 stt / alert 都正常，说明下行链路没问题，唯独 notify 完全无反应）。
///
/// 走 tts 会话的约束：
/// - 设备必须先唤醒过、UDP socket 还没销毁（断网 / 重启 / 120 秒超时会销毁）
/// - `tts stop` 后，非手动模式下设备会自动进入 Listening —— 这就是「主动开启对话」
async fn say_raw_frames(shared: &Arc<Shared>, client_id: &str, frames: Vec<Vec<u8>>) {
    let udp_id = {
        let tables = shared.tables.lock().await;
        tables
            .sessions
            .get(client_id)
            .and_then(|s| s.udp_session_id)
    };
    let Some(udp_id) = udp_id else {
        tracing::warn!(
            %client_id,
            "主动播报失败：没有 UDP 会话（设备需先唤醒过一次）"
        );
        return;
    };

    let step = frame_step_ms(&shared.cfg) as u64;
    let prebuffer = shared.cfg.prebuffer_count.max(1);
    tracing::warn!(
        %client_id,
        frames = frames.len(),
        step_ms = step,
        prebuffer,
        "主动播报：开始"
    );

    if !publish_tts(shared, client_id, TtsState::Start, None).await {
        return;
    }

    // 前 prebuffer 帧连发填满设备 jitter buffer，之后按 step 实时发送。
    // 设备解码队列只有 20 包（1.2 秒），发太快会静默丢包。
    let mut ts: u32 = 1;
    for (i, frame) in frames.iter().enumerate() {
        if frame.len() > MAX_DOWNLINK_PAYLOAD {
            tracing::warn!(len = frame.len(), "帧超过 1500 字节，跳过");
            continue;
        }
        if let Err(err) = shared.udp.send_audio(udp_id, ts, frame).await {
            tracing::warn!(%err, "主动播报发送失败，中止");
            return;
        }
        ts = ts.saturating_add(step as u32);
        if i + 1 >= prebuffer {
            tokio::time::sleep(Duration::from_millis(step)).await;
        }
    }

    // 给最后几帧留播放时间再发 stop，避免掐掉尾巴。
    tokio::time::sleep(Duration::from_millis(step * 3)).await;
    let _ = publish_tts(shared, client_id, TtsState::Stop, None).await;
    tracing::warn!(
        %client_id,
        "主动播报：已发送 tts stop，非手动模式下设备会自动进入聆听"
    );

    // 主动播报结束后重置收口计时：设备进 Listening 等用户回答，
    // 超过 IDLE_CLOSE_LIMIT 没回应就由收口逻辑发 goodbye 回到待命。
    {
        let mut tables = shared.tables.lock().await;
        if let Some(session) = tables.sessions.get_mut(client_id) {
            session.last_speech_at = Instant::now();
            session.last_activity = Instant::now();
        }
    }
}

async fn handle_mqtt(shared: &Arc<Shared>, msg: BrokerIncoming) {
    // ── 控制面转发：haimen/ctl/<target_client_id> → 直接推给该设备 ──
    if let Some(target) = msg.topic.strip_prefix(CONTROL_TOPIC_PREFIX) {
        // 控制主题能让设备说任意话，必须只允许控制面客户端发布。
        // 不拦的话，一台被攻陷的设备（或局域网内任何人）就能操纵其它设备。
        // 未配置 auth_key 时不强制，保持向后兼容。
        if shared.cfg.broker.auth_key.is_some()
            && msg.client_id != crate::mqtt_broker::ADMIN_CLIENT_ID
        {
            tracing::warn!(
              client_id = %msg.client_id,
              topic = %msg.topic,
              "非控制面客户端尝试发布控制主题，拒绝"
            );
            return;
        }
        let target = target.trim();
        if target.is_empty() {
            tracing::warn!(topic = %msg.topic, "控制主题缺少目标 client_id，丢弃");
            return;
        }
        // 本地命令：主动播报（不走转发，由传输层自己按 tts 会话下发音频）
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            if value.get("type").and_then(|v| v.as_str()) == Some("__say_audio") {
                match parse_say_frames(&value) {
                    Ok(frames) => {
                        // 不能在这里直接 await：一次播报要按 60ms 的实时节奏
                        // sleep 好几秒，会把整个传输层的 run_loop 卡住 —— 期间
                        // 收不到设备的 listen / abort / 上行音频，多设备时全体停摆。
                        let shared = Arc::clone(shared);
                        let target = target.to_string();
                        tokio::spawn(async move {
                            say_raw_frames(&shared, &target, frames).await;
                        });
                    }
                    Err(err) => tracing::warn!(target, %err, "主动播报命令非法，丢弃"),
                }
                return;
            }
        }

        let payload_len = msg.payload.len();
        let result = shared
            .broker
            .publish_to(BrokerOutgoing {
                client_id: target.to_owned(),
                topic: format!("haimen/down/{target}"),
                payload: msg.payload,
            })
            .await;
        match result {
            Ok(true) => tracing::warn!(target, payload_len, "控制面消息已转发给设备"),
            Ok(false) => tracing::warn!(target, "控制面消息转发失败：设备不在线"),
            Err(err) => tracing::warn!(
                target,
                %err,
                "控制面消息转发失败"
            ),
        }
        return;
    }

    // ── 以下是设备上行消息的正常处理 ──

    {
        let mut tables = shared.tables.lock().await;
        let session = ensure_session(&mut tables, &msg.client_id);
        session.last_activity = Instant::now();
    }
    let text = match std::str::from_utf8(&msg.payload) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(client_id = %msg.client_id, %err, "MQTT 载荷不是 UTF-8，丢弃");
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(client_id = %msg.client_id, %err, "MQTT JSON 解析失败，丢弃");
            return;
        }
    };
    let kind = value
        .get("type")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    match kind {
        "hello" => handle_hello(shared, &msg.client_id, value).await,
        "listen" => handle_listen(shared, &msg.client_id, value).await,
        "abort" => handle_abort(shared, &msg.client_id).await,
        "goodbye" => {
            // 不断开 MQTT。设备会在同一条连接上再发 hello。
            let playing =
                session_state(shared, &msg.client_id).await == Some(SessionState::Playing);
            release_udp(shared, &msg.client_id, "goodbye").await;
            if playing {
                let _ = publish_tts(shared, &msg.client_id, TtsState::Stop, None).await;
            }
        }
        other => {
            tracing::warn!(client_id = %msg.client_id, kind = other, "忽略未知的 MQTT 消息类型");
        }
    }
}

async fn handle_hello(shared: &Arc<Shared>, client_id: &str, value: serde_json::Value) {
    let hello: HelloIn = match serde_json::from_value(value) {
        Ok(hello) => hello,
        Err(err) => {
            tracing::warn!(%client_id, %err, "hello 字段解析失败，丢弃");
            return;
        }
    };
    // 记录设备能力声明：排查「屏幕不显示文字」这类问题必须看这里 ——
    // text_font.charset=basic 表示只加载了固件内置小字库，中文渲染不出来。
    // 用 warn 级：本项目的 INFO 会被日志过滤。
    tracing::warn!(
        %client_id,
        features = %hello
            .features
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string()),
        text_font = %hello
            .text_font
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(缺失)".to_string()),
        "设备 hello 能力声明"
    );

    if let Some(transport) = hello.transport.as_deref() {
        if !transport.eq_ignore_ascii_case("udp") {
            tracing::warn!(%client_id, transport, "hello 的 transport 不是 udp，忽略");
            return;
        }
    }
    let client_params = hello
        .audio_params
        .map(UplinkAudio::into_params)
        .unwrap_or_else(uplink_defaults);
    // 下行参数由策略决定。AsrLlmTtsStrategy 会把采样率改成 24000。
    let downlink = shared.strategy.hello_audio_params(&client_params);
    // 优先用显式配置的地址（容器部署必须指定，否则会下发容器 IP）
    let server = match shared.cfg.advertise_ip.clone() {
        Some(ip) if !ip.trim().is_empty() => ip.trim().to_string(),
        _ => match lan_ip() {
            Some(ip) => ip,
            None => {
                tracing::warn!(%client_id, "没有可用的局域网 IP，不能下发 hello");
                return;
            }
        },
    };

    let key = random_bytes();
    let udp_id = {
        let tables = shared.tables.lock().await;
        random_session_id(&tables.udp_index)
    };
    let nonce = nonce_template(udp_id);
    let key_hex = encode_hex(&key);
    let nonce_hex = encode_hex(&nonce);
    let crypto = match UdpCrypto::from_hex(&key_hex, &nonce_hex) {
        Ok(crypto) => crypto,
        Err(err) => {
            tracing::warn!(%client_id, %err, "构造 UDP 密钥失败");
            return;
        }
    };
    let protocol_session_id = uuid::Uuid::new_v4().to_string();
    let port = shared.udp.local_addr().port();
    let body = json!({
      "type": "hello",
      "transport": "udp",
      "session_id": protocol_session_id,
      "audio_params": downlink,
      "udp": {
        "server": server,
        "port": port,
        "key": key_hex,
        "nonce": nonce_hex,
      }
    });
    let body = match serde_json::to_string(&body) {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(%client_id, %err, "hello 序列化失败");
            return;
        }
    };

    // 先改会话再 register，再 publish。设备要拿到 hello 才有密钥，
    // 在那之前不会发 UDP；主循环这期间也不会读下一条 MQTT。
    // 重复 hello 时 previous 是旧的 UDP 会话号，必须注销，否则旧 key 还能解开包。
    let previous = {
        let mut tables = shared.tables.lock().await;
        let previous = {
            let Some(session) = tables.sessions.get_mut(client_id) else {
                return;
            };
            let previous = session.udp_session_id.replace(udp_id);
            session.protocol_session_id = protocol_session_id.clone();
            session.state = SessionState::Ready;
            session.audio_buffer.clear();
            session.downlink_ts = 1;
            session.next_send_at = None;
            session.recording_deadline = None;
            session.playback_gen = session.playback_gen.wrapping_add(1);
            if let Some(tx) = session.cancel_tx.take() {
                let _ = tx.send(true);
            }
            session.last_activity = Instant::now();
            // 必须同时刷新收口计时：设备唤醒后往往要先看屏幕/想一下才开口。
            // 不刷新的话 last_speech_at 还停在会话创建时刻，15 秒后 prune_sessions
            // 会把刚打开、还没说过话的通道直接 goodbye 掉。
            session.last_speech_at = Instant::now();
            previous
        };
        if let Some(prev) = previous {
            tables.udp_index.remove(&prev);
        }
        tables.udp_index.insert(udp_id, client_id.to_owned());
        previous
    };
    if let Some(prev) = previous {
        shared.udp.unregister_session(prev).await;
    }
    shared.udp.register_session(udp_id, crypto).await;
    if !publish_body(shared, client_id, &body).await {
        return;
    }
    tracing::info!(%client_id, %protocol_session_id, %server, port, "UDP hello 已下发");
}

async fn handle_listen(shared: &Arc<Shared>, client_id: &str, value: serde_json::Value) {
    let listen: ListenIn = match serde_json::from_value(value) {
        Ok(listen) => listen,
        Err(err) => {
            tracing::warn!(%client_id, %err, "listen 字段解析失败，丢弃");
            return;
        }
    };
    match listen.state.as_str() {
        "start" => enter_recording(shared, client_id).await,
        "stop" => {
            let recording = session_state(shared, client_id).await == Some(SessionState::Recording);
            if recording {
                start_playback(shared, client_id).await;
            } else {
                tracing::debug!(%client_id, "不在录音中，忽略 listen stop");
            }
        }
        "detect" => {
            tracing::debug!(%client_id, "忽略 listen detect");
        }
        other => {
            tracing::warn!(%client_id, state = other, "忽略未知的 listen state");
        }
    }
}

async fn enter_recording(shared: &Arc<Shared>, client_id: &str) {
    let session_id = {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get_mut(client_id) else {
            return;
        };
        // 设备没发 hello 就先发 listen 时，状态还停在 AwaitingHello。
        // 这时回任何下行都会被设备清掉事件位，所以直接丢掉。
        if session.state == SessionState::AwaitingHello || session.protocol_session_id.is_empty() {
            tracing::warn!(%client_id, "还没收到 hello 就收到 listen，忽略");
            return;
        }
        if session.state == SessionState::Playing {
            session.playback_gen = session.playback_gen.wrapping_add(1);
            if let Some(tx) = &session.cancel_tx {
                let _ = tx.send(true);
            }
        }
        session.audio_buffer.clear();
        session.state = SessionState::Recording;
        session.recording_deadline = Some(TokioInstant::now() + RECORDING_LIMIT);
        session.next_send_at = None;
        session.last_activity = Instant::now();
        tracing::debug!(client_id = %session.client_id, "进入录音");
        session.protocol_session_id.clone()
    };
    if let Err(err) = shared
        .strategy
        .on_recording_start(client_id, &session_id)
        .await
    {
        tracing::warn!(%client_id, %err, "on_recording_start 失败，继续收录音");
    }
}

async fn handle_abort(shared: &Arc<Shared>, client_id: &str) {
    let state = session_state(shared, client_id).await;
    match state {
        Some(SessionState::Recording) => {
            // 录音中的 abort 和 listen stop 一样，表示这句说完了，要生成回复。
            // 这里不能注销 UDP：回复的音频还得用这把 key 发出去。
            start_playback(shared, client_id).await;
        }
        Some(SessionState::AwaitingHello) | None => {
            tracing::warn!(%client_id, "还没 hello 就收到 abort，忽略");
        }
        Some(SessionState::Playing) | Some(SessionState::Ready) => {
            // 打断只结束这一轮。小智接着发 listen start，并继续用同一条 UDP 通道。
            // 密钥和对端地址必须留着：注销之后上行会被当成未知会话丢掉，
            // 下一轮回复也发不出去，直到设备再发一次 hello。
            {
                let mut tables = shared.tables.lock().await;
                if let Some(session) = tables.sessions.get_mut(client_id) {
                    session.end_round();
                }
            }
            if state == Some(SessionState::Playing) {
                let _ = publish_tts(shared, client_id, TtsState::Stop, None).await;
            }
        }
    }
}

async fn handle_udp(shared: &Arc<Shared>, audio: crate::udp_audio::UdpIncomingAudio) {
    let frame = AudioFrame {
        timestamp: audio.timestamp,
        data: audio.payload,
    };
    let recording_client = {
        let mut tables = shared.tables.lock().await;
        let Some(client_id) = tables.udp_index.get(&audio.session_id).cloned() else {
            // 旧会话残留，或者 hello 已经轮换过 key。
            tracing::debug!(
                session_id = audio.session_id,
                "丢弃未知 session 的 UDP 音频"
            );
            return;
        };
        let Some(session) = tables.sessions.get_mut(&client_id) else {
            tracing::debug!(
                session_id = audio.session_id,
                "UDP 会话没有对应的设备，丢弃"
            );
            return;
        };
        if session.state != SessionState::Recording {
            tracing::debug!(
              %client_id,
              state = ?session.state,
              "不在录音中，丢弃 UDP 音频"
            );
            return;
        }
        session.last_activity = Instant::now();
        session.audio_buffer.push(frame.clone());
        Some(client_id)
    };
    // 用 client_id 作为策略层的会话键：它在一个设备的整个连接周期内稳定，
    // 不像 protocol_session_id 每次 hello 都会换。
    if let Some(client_id) = recording_client {
        // ASR 的 PCM 通道有界（32~64 帧 ≈ 2~4 秒）。下游积压时 send 会 await，
        // 拖住整个 run_loop。这里给硬超时：宁可丢这一帧，也不能卡住所有设备。
        match tokio::time::timeout(
            ASR_FEED_TIMEOUT,
            shared.strategy.on_audio_frame(&client_id, &frame),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(%err, "on_audio_frame 失败，帧已留在缓冲里"),
            Err(_) => tracing::warn!(
                %client_id,
                timeout_ms = ASR_FEED_TIMEOUT.as_millis() as u64,
                "喂入 ASR 超时（下游积压），本帧丢弃"
            ),
        }
    }
}

/// 策略检测到「用户说完了」：把所有正在录音的会话推进到「生成回复」。
///
/// 必须存在的原因：`AsrLlmTtsStrategy` 是在流式 ASR 内部判断语音结束的，
/// 它通过 `vad_completion()` 暴露一个 Notifier。传输层如果不监听，
/// 就只能等设备发 `listen stop` —— 而自动模式下设备永远不发，
/// 表现为「设备一直在聆听中，ASR 识别出了文字却没有任何回复」。
///
/// 注意：该 Notifier 是策略级而非会话级的，多设备同时录音时会一起收口。
/// M2' 是单设备场景可接受；多设备需要给策略加带 session 参数的版本。
async fn finish_recording_by_vad(shared: &Arc<Shared>, client_id: &str) {
    let still_recording = {
        let tables = shared.tables.lock().await;
        tables
            .sessions
            .get(client_id)
            .is_some_and(|session| session.state == SessionState::Recording)
    };
    if !still_recording {
        // 期间可能已被录音上限或 abort 收口，忽略这次信号。
        return;
    }
    tracing::warn!(%client_id, "策略侧 VAD 检测到语音结束，立即生成回复");
    start_playback(shared, client_id).await;
}

async fn expire_recordings(shared: &Arc<Shared>) {
    let now = TokioInstant::now();
    let due: Vec<String> = {
        let tables = shared.tables.lock().await;
        tables
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Recording
                    && session
                        .recording_deadline
                        .is_some_and(|deadline| deadline <= now)
            })
            .map(|(client_id, _)| client_id.clone())
            .collect()
    };
    for client_id in due {
        tracing::info!(%client_id, "录音超过 30 秒，强制生成回复");
        start_playback(shared, &client_id).await;
    }
}

async fn prune_sessions(shared: &Shared) {
    let online: HashSet<String> = shared
        .broker
        .connected_clients()
        .await
        .into_iter()
        .collect();
    let mut gone = Vec::new();
    {
        let tables = shared.tables.lock().await;
        for client_id in tables.sessions.keys() {
            if !online.contains(client_id) {
                gone.push(client_id.clone());
            }
        }
    }
    for client_id in gone {
        // 整份会话都删掉（含 UDP 密钥）—— 如果这行出现得莫名其妙，
        // 说明 broker 的 connected_clients 把还在线的设备误判成断开了。
        tracing::warn!(%client_id, "MQTT 连接已断开，清理会话与 UDP 密钥");
        drop_client(shared, &client_id).await;
    }

    // 对话收口：一轮结束后长时间没新语音，就发 goodbye 让设备回待命。
    let idle: Vec<String> = {
        let tables = shared.tables.lock().await;
        tables
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.state == SessionState::Ready
                    && s.udp_session_id.is_some()
                    && s.last_speech_at.elapsed() > IDLE_CLOSE_LIMIT
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    for client_id in idle {
        close_idle_conversation(shared, &client_id).await;
    }
}

/// 给设备发 goodbye 收口当前对话，让它关闭音频通道回到 Idle。
///
/// 固件的处理是：收到 goodbye 后只在 `session_id` 匹配时才
/// `CloseAudioChannel(false)`，所以这里必须带上协议会话号。
/// 不带或带错，设备会继续停在 Listening 一直推麦克风音频。
async fn close_idle_conversation(shared: &Shared, client_id: &str) {
    let session_id = {
        let tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get(client_id) else {
            return;
        };
        session.protocol_session_id.clone()
    };
    if session_id.is_empty() {
        return;
    }
    let body = format!("{{\"type\":\"goodbye\",\"session_id\":\"{session_id}\"}}");
    if publish_body(shared, client_id, &body).await {
        tracing::warn!(%client_id, "长时间无语音，发送 goodbye 收口对话，设备应回到待命");
        release_udp(shared, client_id, "idle-close").await;
    }
}

async fn start_playback(shared: &Arc<Shared>, client_id: &str) {
    let job = {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get_mut(client_id) else {
            return;
        };
        if session.state != SessionState::Recording {
            return;
        }
        session.state = SessionState::Playing;
        session.recording_deadline = None;
        session.playback_gen = session.playback_gen.wrapping_add(1);
        if let Some(old) = session.cancel_tx.take() {
            let _ = old.send(true);
        }
        let (tx, rx) = watch::channel(false);
        session.cancel_tx = Some(tx);
        session.last_activity = Instant::now();
        PlaybackJob {
            client_id: client_id.to_owned(),
            protocol_session_id: session.protocol_session_id.clone(),
            generation: session.playback_gen,
            buffer: std::mem::take(&mut session.audio_buffer),
            cancel: rx,
        }
    };
    let task_shared = Arc::clone(shared);
    let handle = tokio::spawn(async move {
        run_playback(task_shared, job).await;
    });
    let mut playbacks = shared.playbacks.lock().await;
    playbacks.retain(|item| !item.is_finished());
    playbacks.push(handle);
}

async fn run_playback(shared: Arc<Shared>, job: PlaybackJob) {
    let mut tts_started = false;
    // 正常跑完的一轮，尾帧等待在 drive_playback 里做 —— 埋点要记录实际等待时长，
    // 等待发生在函数外就量不到。返回的 tail_wait_done 表示它已经等过。
    let tail_wait_done = drive_playback(&shared, &job, &mut tts_started).await;
    // tail_wait_done 为真时即使等待期间被取消也要照常发 stop（设备不能停在 Speaking）；
    // 还没等过且已被取消的轮次不发，由 handle_abort 那边的 stop 收口。
    if tts_started && (tail_wait_done || !cancelled(&job.cancel, &shared)) {
        if !tail_wait_done {
            // 中途异常退出（设备掉线等）的轮次没等尾帧，在这里补上，
            // 行为与正常路径一致。
            let mut cancel = job.cancel.clone();
            let _ = wait_tail_playback(&shared, &mut cancel).await;
        }
        // 设备要求 start → 句子/音频 → stop。stop 放在所有帧之后。
        let _ = publish_tts(&shared, &job.client_id, TtsState::Stop, None).await;
        finish_ready(&shared, &job).await;
    }
}

/// 驱动一轮下行播放，返回是否已在函数内做完尾帧等待（T1）。
///
/// T2 埋点：统计本轮下发帧数与耗时，结束时打一条 warn!（INFO 会被过滤）。
/// 尾帧等待放在这里而不是 run_playback，是因为 `tail_wait_ms` 要记录
/// 实际等待时长 —— 等待发生在函数外就量不到。正常跑完的一轮在这里等；
/// 中途异常退出的轮次由 run_playback 补等。
async fn drive_playback(shared: &Arc<Shared>, job: &PlaybackJob, tts_started: &mut bool) -> bool {
    let mut stats = PlaybackStats::default();
    let completed = drive_playback_inner(shared, job, tts_started, &mut stats).await;

    // 正常跑完（流式循环自然结束）才在这里等尾帧；被取消时 wait_tail_playback
    // 会立刻返回，用户插话不会白等。
    let mut tail_wait_ms = 0u64;
    let mut tail_wait_done = false;
    if completed && !cancelled(&job.cancel, shared) {
        let mut cancel = job.cancel.clone();
        let wait_start = Instant::now();
        wait_tail_playback(shared, &mut cancel).await;
        tail_wait_ms = wait_start.elapsed().as_millis() as u64;
        tail_wait_done = true;
    }

    // frames == 0 的轮次（生成失败、被秒取消）不打，避免噪音。
    if stats.frames > 0 {
        let elapsed_ms = match (stats.first_send_at, stats.last_send_at) {
            (Some(first), Some(last)) => last.saturating_duration_since(first).as_millis() as u64,
            _ => 0,
        };
        let expected_ms = stats
            .frames
            .saturating_mul(u64::from(frame_step_ms(&shared.cfg)));
        tracing::warn!(
            client_id = %job.client_id,
            frames = stats.frames,
            elapsed_ms,
            expected_ms,
            tail_wait_ms,
            "下行播放统计"
        );
    }
    tail_wait_done
}

/// 发送主体。返回 true 表示流式循环自然结束（所有帧已下发）；
/// 提前退出（取消 / 发送失败 / 会话失效）返回 false。
async fn drive_playback_inner(
    shared: &Arc<Shared>,
    job: &PlaybackJob,
    tts_started: &mut bool,
    stats: &mut PlaybackStats,
) -> bool {
    let (frame_tx, mut frame_rx) = mpsc::channel::<PlaybackEvent>(32);
    let strategy = Arc::clone(&shared.strategy);
    // 策略层的会话 key 必须和钩子一致（钩子收的是 client_id），
    // 这里不能传 protocol_session_id —— 它每次 hello 都会变。
    let strategy_session_key = job.client_id.clone();
    let log_session_id = job.protocol_session_id.clone();
    let buffer = job.buffer.clone();
    // 任务单里的 is_streaming_capable 对应 trait 的 supports_streaming_playback。
    // 默认的 generate_response_stream 会再调用 generate_response，两条路径都从这里进。
    let generate = tokio::spawn(async move {
        if let Err(err) = strategy
            .generate_response_stream(buffer, &strategy_session_key, frame_tx)
            .await
        {
            tracing::warn!(%err, %log_session_id, "生成语音回复失败");
        }
    });
    let _abort_gen = AbortJoin(Some(generate));

    let frame = frame_duration(shared);
    let want = shared.cfg.prebuffer_count;
    let mut prebuffer = Vec::new();
    let mut sentences = Vec::new();

    // 源自 ws.rs:723。文本不占预缓冲名额：stt 马上发，句子先攒着，等 tts start 之后再发。
    while prebuffer.len() < want {
        if cancelled(&job.cancel, shared) {
            return false;
        }
        match frame_rx.recv().await {
            Some(PlaybackEvent::Stt(text)) => {
                if !publish_stt(shared, &job.client_id, &text).await {
                    return false;
                }
            }
            Some(PlaybackEvent::LlmSentence(text)) => sentences.push(text),
            Some(PlaybackEvent::Audio(audio)) => prebuffer.push(audio),
            None => break,
        }
    }
    if cancelled(&job.cancel, shared) {
        return false;
    }

    // 先 tts start，设备才肯播后面的 UDP。提前发的音频会被丢掉。
    if !publish_tts(shared, &job.client_id, TtsState::Start, None).await {
        return false;
    }
    *tts_started = true;
    for sentence in sentences {
        if !publish_tts(
            shared,
            &job.client_id,
            TtsState::SentenceStart,
            Some(sentence),
        )
        .await
        {
            return false;
        }
    }

    // 源自 ws.rs:831。前 N 帧连发，把设备 300ms 的 jitter buffer 填上。
    for audio in prebuffer {
        if cancelled(&job.cancel, shared) {
            return false;
        }
        if !send_downlink_frame(shared, job, &audio.data).await {
            return false;
        }
        stats.note_frame_sent();
    }

    // 源自 ws.rs:847。时钟从连发结束的这一刻起算，而不是从第一帧起算。
    {
        let mut tables = shared.tables.lock().await;
        if let Some(session) = tables.sessions.get_mut(&job.client_id) {
            if session.playback_gen == job.generation {
                session.next_send_at = Some(Instant::now() + frame);
            }
        }
    }

    while let Some(event) = frame_rx.recv().await {
        if cancelled(&job.cancel, shared) {
            return false;
        }
        match event {
            PlaybackEvent::Stt(text) => {
                if !publish_stt(shared, &job.client_id, &text).await {
                    return false;
                }
            }
            PlaybackEvent::LlmSentence(text) => {
                if !publish_tts(shared, &job.client_id, TtsState::SentenceStart, Some(text)).await {
                    return false;
                }
            }
            PlaybackEvent::Audio(audio) => {
                let mut next = {
                    let tables = shared.tables.lock().await;
                    let Some(session) = tables.sessions.get(&job.client_id) else {
                        return false;
                    };
                    if session.playback_gen != job.generation {
                        return false;
                    }
                    session.next_send_at.unwrap_or_else(Instant::now)
                };
                let mut cancel = job.cancel.clone();
                if !wait_until_send_slot(&mut next, frame, &mut cancel).await {
                    return false;
                }
                if !send_downlink_frame(shared, job, &audio.data).await {
                    return false;
                }
                stats.note_frame_sent();
                // 源自 ws.rs:903。累加帧长，而不是改成 now + 帧长，否则每帧都把发送耗时叠进去。
                next += frame;
                let mut tables = shared.tables.lock().await;
                if let Some(session) = tables.sessions.get_mut(&job.client_id) {
                    if session.playback_gen == job.generation {
                        session.next_send_at = Some(next);
                    }
                }
            }
        }
    }
    true
}

async fn send_downlink_frame(shared: &Shared, job: &PlaybackJob, payload: &[u8]) -> bool {
    if payload.len() > MAX_DOWNLINK_PAYLOAD {
        tracing::warn!(
          client_id = %job.client_id,
          len = payload.len(),
          "下行帧超过 1500 字节，丢弃该帧"
        );
        return true;
    }
    let step = frame_step_ms(&shared.cfg);
    let (udp_id, timestamp) = {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get_mut(&job.client_id) else {
            return false;
        };
        if session.playback_gen != job.generation || session.state != SessionState::Playing {
            return false;
        }
        let Some(udp_id) = session.udp_session_id else {
            tracing::warn!(client_id = %job.client_id, "没有 UDP 会话，无法下发音频");
            return false;
        };
        let timestamp = alloc_downlink_ts(session, step);
        // 真正往外发音频了，收口计时从这里重置。
        session.last_speech_at = Instant::now();
        (udp_id, timestamp)
    };
    match shared.udp.send_audio(udp_id, timestamp, payload).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(client_id = %job.client_id, %err, "下行音频发送失败");
            false
        }
    }
}

/// 下行 timestamp 从 1 开始，之后每次加一帧时长。0 会和上行第一包复用 CTR 的 IV。
fn alloc_downlink_ts(session: &mut DeviceSession, step: u32) -> u32 {
    let timestamp = if session.downlink_ts == 0 {
        1
    } else {
        session.downlink_ts
    };
    let next = timestamp.saturating_add(step.max(1));
    session.downlink_ts = if next == 0 { 1 } else { next };
    timestamp
}

async fn finish_ready(shared: &Shared, job: &PlaybackJob) {
    {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get_mut(&job.client_id) else {
            return;
        };
        if session.playback_gen != job.generation {
            return;
        }
        if session.state == SessionState::Playing {
            session.state = SessionState::Ready;
        }
        session.recording_deadline = None;
        session.last_activity = Instant::now();
    }

    // 用户说了告别语：回复已经播完，立刻收口让设备回待命。
    // 不能等 IDLE_CLOSE_LIMIT 超时 —— 那样设备会白听 15 秒并继续推麦克风音频。
    if shared.strategy.end_of_conversation(&job.client_id) {
        tracing::warn!(
            client_id = %job.client_id,
            "用户说了告别语，回复播完立即结束会话"
        );
        close_idle_conversation(shared, &job.client_id).await;
    }
}

/// 注销 UDP 密钥并回到 Ready，但不把 MQTT 会话删掉。
///
/// `reason` 只用于日志：排查「主动播报时报没有 UDP 会话」时，
/// 需要知道是谁把会话注销掉的（goodbye / 空闲清理）。
/// 打断不能走这里：设备会留着同一条音频通道继续说话。
/// 本项目的 INFO 会被日志过滤，所以这里必须用 warn。
async fn release_udp(shared: &Shared, client_id: &str, reason: &str) {
    tracing::warn!(%client_id, reason, "注销 UDP 会话");
    let udp_id = {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get_mut(client_id) else {
            return;
        };
        session.end_round();
        // 收回 Ready 之后必须再退回 AwaitingHello。
        //
        // 为什么：UDP 通道已经没了，设备此刻若还停在 Listening，会把之前
        // tts stop 触发的 listen start 发上来。enter_recording 只挡 AwaitingHello，
        // 于是这个杂散 listen start 会重新进录音 → 30 秒 RECORDING_LIMIT 到期
        // → 生成空回复 → tts stop → 设备又回 Listening → 又 listen start……
        // 实测形成每 30 秒一轮的死循环，用户看到的就是「设备自己不停进入聆听」。
        //
        // 退回 AwaitingHello 后，必须等设备重新 hello 才能再录音 —— 那时 UDP
        // 密钥也才存在，否则录上来的音频根本解不了。
        session.state = SessionState::AwaitingHello;
        let udp_id = session.udp_session_id.take();
        if let Some(id) = udp_id {
            tables.udp_index.remove(&id);
        }
        udp_id
    };
    if let Some(id) = udp_id {
        shared.udp.unregister_session(id).await;
    }
}

/// `publish_to` 返回 false 时设备已经不在 broker 上，整份会话都清掉。
async fn drop_client(shared: &Shared, client_id: &str) {
    let udp_id = {
        let mut tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.remove(client_id) else {
            return;
        };
        if let Some(tx) = session.cancel_tx {
            let _ = tx.send(true);
        }
        if let Some(id) = session.udp_session_id {
            tables.udp_index.remove(&id);
            Some(id)
        } else {
            None
        }
    };
    if let Some(id) = udp_id {
        shared.udp.unregister_session(id).await;
    }
}

async fn stop_playbacks(shared: &Shared) {
    {
        let tables = shared.tables.lock().await;
        for session in tables.sessions.values() {
            if let Some(tx) = &session.cancel_tx {
                let _ = tx.send(true);
            }
        }
    }
    let handles = {
        let mut playbacks = shared.playbacks.lock().await;
        std::mem::take(&mut *playbacks)
    };
    for mut handle in handles {
        if tokio::time::timeout(Duration::from_millis(200), &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn session_state(shared: &Shared, client_id: &str) -> Option<SessionState> {
    shared
        .tables
        .lock()
        .await
        .sessions
        .get(client_id)
        .map(|session| session.state)
}

fn ensure_session<'a>(tables: &'a mut Tables, client_id: &str) -> &'a mut DeviceSession {
    tables
        .sessions
        .entry(client_id.to_owned())
        .or_insert_with(|| DeviceSession::new(client_id))
}

fn cancelled(cancel: &watch::Receiver<bool>, shared: &Shared) -> bool {
    *cancel.borrow() || *shared.shutdown_tx.borrow()
}

async fn publish_stt(shared: &Shared, client_id: &str, text: &str) -> bool {
    let session_id = match protocol_session_id(shared, client_id).await {
        Some(session_id) => session_id,
        None => return false,
    };
    let body = match serde_json::to_string(&ServerMessage::Stt {
        session_id,
        text: text.to_owned(),
    }) {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(%err, "stt 消息序列化失败");
            return false;
        }
    };
    publish_body(shared, client_id, &body).await
}

async fn publish_tts(
    shared: &Shared,
    client_id: &str,
    state: TtsState,
    text: Option<String>,
) -> bool {
    let session_id = match protocol_session_id(shared, client_id).await {
        Some(session_id) => session_id,
        None => return false,
    };
    let body = match serde_json::to_string(&ServerMessage::Tts {
        session_id,
        state,
        text,
    }) {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(%err, "tts 消息序列化失败");
            return false;
        }
    };
    publish_body(shared, client_id, &body).await
}

async fn protocol_session_id(shared: &Shared, client_id: &str) -> Option<String> {
    shared
        .tables
        .lock()
        .await
        .sessions
        .get(client_id)
        .map(|session| session.protocol_session_id.clone())
}

async fn publish_body(shared: &Shared, client_id: &str, body: &str) -> bool {
    let topic = {
        let tables = shared.tables.lock().await;
        let Some(session) = tables.sessions.get(client_id) else {
            return false;
        };
        shared
            .cfg
            .downlink_topic_template
            .replace("{mac}", &session.device_id)
    };
    // 写队列（容量 32）满时 publish_to 会 await。设备侧 TCP 卡住时
    // 这会把整个 run_loop 拖死，所以这里加硬超时。
    let published = tokio::time::timeout(
        MQTT_WRITE_TIMEOUT,
        shared.broker.publish_to(BrokerOutgoing {
            client_id: client_id.to_owned(),
            topic,
            payload: body.as_bytes().to_vec(),
        }),
    )
    .await;
    match published {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            tracing::warn!(%client_id, "设备已掉线，清理会话");
            drop_client(shared, client_id).await;
            false
        }
        Ok(Err(err)) => {
            tracing::warn!(%client_id, %err, "下发 MQTT 失败");
            false
        }
        Err(_) => {
            tracing::warn!(
                %client_id,
                timeout_ms = MQTT_WRITE_TIMEOUT.as_millis() as u64,
                "下发 MQTT 超时（设备侧写队列拥塞），放弃本条"
            );
            false
        }
    }
}

fn frame_step_ms(cfg: &MqttUdpConfig) -> u32 {
    if cfg.frame_duration_ms == 0 {
        60
    } else {
        cfg.frame_duration_ms
    }
}

fn frame_duration(shared: &Shared) -> Duration {
    Duration::from_millis(u64::from(frame_step_ms(&shared.cfg)))
}

/// 源自 ws.rs:1104。断档超过一帧就把时钟快进到现在，避免积压帧被一次补发完。
fn snap_send_clock(next_send_at: &mut Instant, now: Instant, frame: Duration) {
    if *next_send_at + frame < now {
        *next_send_at = now;
    }
}

/// 源自 ws.rs:1121。
///
/// ws 版在等待时还读 WebSocket，用来接 abort。这里没有那条 socket，
/// 取消信号改走 `watch`。对齐方式不变：等的是绝对时刻，不是重新 sleep 一帧。
async fn wait_until_send_slot(
    next_send_at: &mut Instant,
    frame: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        if *cancel.borrow() {
            return false;
        }
        let now = Instant::now();
        snap_send_clock(next_send_at, now, frame);
        let wait = next_send_at.saturating_duration_since(now);
        if wait.is_zero() {
            return true;
        }
        tokio::select! {
          _ = tokio::time::sleep(wait) => return true,
          changed = cancel.changed() => {
            if changed.is_err() || *cancel.borrow() {
              return false;
            }
          }
        }
    }
}

/// 发完最后一帧后，等设备把缓冲里的尾帧播完再发 `tts stop`。
///
/// 为什么需要：`drive_playback` 只保证帧「发出去」，设备解码队列里还压着
/// `prebuffer_count` 帧（默认 5 ≈ 300ms）。紧接着发 stop 会让设备切出
/// Speaking 状态、回收音频管道，尾帧被丢掉 —— 表现为句子末尾掉帧。
///
/// 公式照抄参考实现（sendAudioHandle.py:58-81）：
///   (prebuffer_count + 2) × frame_duration
/// 多出的 `tail_extra_frames`（默认 2）帧覆盖 UDP 抖动与固件调度延迟。
///
/// 取消信号触发时必须立刻返回，不能白等（用户插话时不能卡 420ms）。
async fn wait_tail_playback(shared: &Shared, cancel: &mut watch::Receiver<bool>) -> bool {
    if *cancel.borrow() {
        return false;
    }
    let frames = u64::try_from(shared.cfg.prebuffer_count)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(shared.cfg.tail_extra_frames));
    let wait = Duration::from_millis(frames.saturating_mul(u64::from(frame_step_ms(&shared.cfg))));
    let sleep = tokio::time::sleep(wait);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
          _ = &mut sleep => return true,
          changed = cancel.changed() => {
            // sender 侧关闭（Err）视为取消；信号被复位（false）则继续等满
            if changed.is_err() || *cancel.borrow() {
              return false;
            }
          }
        }
    }
}

fn mac_from_client_id(client_id: &str) -> String {
    let mut parts = client_id.split("@@@").filter(|part| !part.is_empty());
    let _group = parts.next();
    let mac = match parts.next() {
        Some(mac) => mac,
        None => client_id,
    };
    colon_mac(mac)
}

fn colon_mac(raw: &str) -> String {
    let mac = raw.trim().to_ascii_lowercase();
    if mac.contains(':') {
        return mac;
    }
    if mac.contains('_') {
        return mac.replace('_', ":");
    }
    if mac.len() == 12 && mac.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut out = String::with_capacity(17);
        for (idx, chunk) in mac.as_bytes().chunks(2).enumerate() {
            if idx > 0 {
                out.push(':');
            }
            out.push_str(&String::from_utf8_lossy(chunk));
        }
        return out;
    }
    mac
}

fn nonce_template(session_id: u32) -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // 设备靠首字节 0x01 识别这是音频通道。session id 放在 [4..8)，大端。
    nonce[0] = 0x01;
    nonce[4..8].copy_from_slice(&session_id.to_be_bytes());
    nonce
}

fn random_bytes() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

fn random_session_id(used: &HashMap<u32, String>) -> u32 {
    for _ in 0..8 {
        let bytes = random_bytes();
        let id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if id != 0 && !used.contains_key(&id) {
            return id;
        }
    }
    let mut id = 1u32;
    while id != 0 && used.contains_key(&id) {
        id = id.wrapping_add(1);
    }
    if id == 0 { 1 } else { id }
}

/// 解析 hex 字符串为字节。长度奇数或含非 hex 字符返回 `None`。
///
/// `encode_hex` 的逆操作；用于控制面 `__say_audio` 命令传预编码 Opus 帧
/// （用 hex 而不是 base64，避免为此引入新依赖）。
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// `server` 必须是局域网地址。0.0.0.0 / 127.0.0.1 设备都访问不到。
fn lan_ip() -> Option<String> {
    match local_ip_address::local_ip() {
        Ok(ip) if usable_v4(&ip) => return Some(ip.to_string()),
        Ok(ip) => tracing::warn!(%ip, "local_ip 不是可用的局域网地址，改为扫描网卡"),
        Err(err) => tracing::warn!(%err, "local_ip 失败，改为扫描网卡"),
    }
    let listed = local_ip_address::list_afinet_netifas().ok()?;
    for (_name, ip) in listed {
        if usable_v4(&ip) {
            return Some(ip.to_string());
        }
    }
    None
}

fn usable_v4(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified() && !v4.is_broadcast(),
        IpAddr::V6(_) => false,
    }
}

fn uplink_defaults() -> AudioParams {
    AudioParams {
        format: default_opus(),
        sample_rate: default_uplink_rate(),
        channels: default_channels(),
        frame_duration: default_frame_ms(),
    }
}

#[derive(Deserialize)]
struct HelloIn {
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    audio_params: Option<UplinkAudio>,
    /// 设备能力声明。较新固件带 `glyph_push`（服务端补发本地字库缺失的位图）。
    #[serde(default)]
    features: Option<serde_json::Value>,
    /// 设备实际安装的文字字库。
    /// `charset = "basic"` 是链接进固件的小字库；`"common"` 是从 assets 分区
    /// 加载的完整字库。中文显示依赖后者 —— 如果设备只报 basic，中文很可能渲染不出来。
    #[serde(default)]
    text_font: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct UplinkAudio {
    #[serde(default = "default_opus")]
    format: String,
    #[serde(default = "default_uplink_rate")]
    sample_rate: u32,
    #[serde(default = "default_channels")]
    channels: u8,
    #[serde(default = "default_frame_ms")]
    frame_duration: u32,
}

impl UplinkAudio {
    fn into_params(self) -> AudioParams {
        AudioParams {
            format: self.format,
            sample_rate: self.sample_rate,
            channels: self.channels,
            frame_duration: self.frame_duration,
        }
    }
}

#[derive(Deserialize)]
struct ListenIn {
    state: String,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::sync::mpsc;

    use super::*;
    use crate::mqtt_broker::MqttBrokerConfig;
    use crate::protocol_udp::{UdpAudioHeader, UdpCrypto};
    use crate::udp_audio::UdpAudioConfig;

    const CLIENT_ID: &str = "GID_haimen@@@84:c7:bb:59:93:64@@@84:c7:bb:59:93:64";

    struct MockInner {
        recording_starts: Vec<String>,
        audio_frames: Vec<AudioFrame>,
        generate_calls: usize,
        playback: Vec<AudioFrame>,
    }

    struct MockStrategy {
        inner: StdMutex<MockInner>,
    }

    impl MockStrategy {
        fn new(playback: Vec<AudioFrame>) -> Self {
            Self {
                inner: StdMutex::new(MockInner {
                    recording_starts: Vec::new(),
                    audio_frames: Vec::new(),
                    generate_calls: 0,
                    playback,
                }),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, MockInner> {
            self.inner.lock().unwrap_or_else(|err| err.into_inner())
        }
    }

    #[async_trait]
    impl ResponseStrategy for MockStrategy {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn hello_audio_params(&self, _client_params: &AudioParams) -> AudioParams {
            AudioParams {
                format: "opus".to_owned(),
                sample_rate: 24_000,
                channels: 1,
                frame_duration: 60,
            }
        }

        async fn generate_response(
            &self,
            _audio_buffer: Vec<AudioFrame>,
            _session_id: &str,
        ) -> Result<Vec<AudioFrame>, String> {
            let mut inner = self.lock();
            inner.generate_calls += 1;
            Ok(inner.playback.clone())
        }

        async fn on_recording_start(
            &self,
            _session_key: &str,
            session_id: &str,
        ) -> Result<(), String> {
            self.lock().recording_starts.push(session_id.to_owned());
            Ok(())
        }

        async fn on_audio_frame(
            &self,
            _session_key: &str,
            frame: &AudioFrame,
        ) -> Result<(), String> {
            self.lock().audio_frames.push(frame.clone());
            Ok(())
        }
    }

    struct DeviceLink {
        writer: tokio::net::tcp::OwnedWriteHalf,
        rx: mpsc::Receiver<Vec<u8>>,
        reader: JoinHandle<()>,
    }

    impl DeviceLink {
        async fn connect(addr: SocketAddr) -> Self {
            let mut stream = TcpStream::connect(addr).await.expect("连接 broker");
            stream.set_nodelay(true).expect("TCP_NODELAY");
            stream
                .write_all(&mqtt_connect(CLIENT_ID))
                .await
                .expect("写 CONNECT");
            let (mut reader, writer) = stream.into_split();
            let connack = read_mqtt_packet(&mut reader)
                .await
                .expect("读 CONNACK")
                .expect("broker 关闭");
            assert_eq!(connack.0 >> 4, 2, "CONNACK");
            let (tx, rx) = mpsc::channel(32);
            let reader = tokio::spawn(async move {
                loop {
                    match read_mqtt_packet(&mut reader).await {
                        Ok(Some((header, body))) if header >> 4 == 3 => {
                            if let Some(payload) = publish_payload(&body) {
                                if tx.send(payload).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            });
            Self { writer, rx, reader }
        }

        async fn publish(&mut self, topic: &str, payload: &[u8]) {
            self.writer
                .write_all(&mqtt_publish(topic, payload))
                .await
                .expect("写 PUBLISH");
        }

        async fn wait_type(&mut self, kind: &str, timeout: Duration) -> serde_json::Value {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                assert!(!left.is_zero(), "等待 {kind} 超时");
                let payload = tokio::time::timeout(left, self.rx.recv())
                    .await
                    .unwrap_or_else(|_| panic!("等待 {kind} 超时"))
                    .unwrap_or_else(|| panic!("下行通道关闭，还没等到 {kind}"));
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    if value.get("type").and_then(|item| item.as_str()) == Some(kind) {
                        return value;
                    }
                }
            }
        }
    }

    impl Drop for DeviceLink {
        fn drop(&mut self) {
            self.reader.abort();
        }
    }

    struct Env {
        transport: MqttUdpTransport,
        link: DeviceLink,
        mock: Arc<MockStrategy>,
        udp: UdpSocket,
    }

    impl Env {
        async fn boot(playback: Vec<AudioFrame>) -> Self {
            let mock = Arc::new(MockStrategy::new(playback));
            let cfg = MqttUdpConfig {
                broker: MqttBrokerConfig {
                    bind_addr: "127.0.0.1:0".to_owned(),
                    ..MqttBrokerConfig::default()
                },
                udp: UdpAudioConfig {
                    bind_addr: "127.0.0.1:0".to_owned(),
                },
                publish_topic_template: "device-server".to_owned(),
                downlink_topic_template: "devices/p2p/{mac}".to_owned(),
                prebuffer_count: 5,
                frame_duration_ms: 60,
                tail_extra_frames: 2,
                advertise_ip: None,
            };
            let transport = MqttUdpTransport::start(cfg, mock.clone())
                .await
                .expect("启动传输层");
            let addr: SocketAddr = transport.broker_addr().parse().expect("broker 地址");
            let link = DeviceLink::connect(addr).await;
            let udp = UdpSocket::bind("127.0.0.1:0").await.expect("绑定设备 UDP");
            Self {
                transport,
                link,
                mock,
                udp,
            }
        }

        async fn hello(&mut self) -> serde_json::Value {
            let payload = br#"{"type":"hello","transport":"udp","audio_params":{"format":"opus","sample_rate":16000,"channels":1,"frame_duration":60}}"#;
            self.link.publish("device-server", payload).await;
            self.link.wait_type("hello", Duration::from_secs(2)).await
        }

        async fn shutdown(self) {
            self.transport.shutdown().await;
        }
    }

    fn decode_hex16(text: &str) -> [u8; 16] {
        assert_eq!(text.len(), 32, "hex 长度应为 32: {text}");
        let mut out = [0u8; 16];
        for index in 0..16 {
            out[index] = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex");
        }
        out
    }

    fn crypto_from_hello(hello: &serde_json::Value) -> UdpCrypto {
        let key = hello["udp"]["key"].as_str().expect("key");
        let nonce = hello["udp"]["nonce"].as_str().expect("nonce");
        UdpCrypto::from_hex(key, nonce).expect("crypto")
    }

    async fn send_uplink(
        env: &Env,
        crypto: &UdpCrypto,
        timestamp: u32,
        sequence: u32,
        payload: &[u8],
    ) {
        let header = crypto.build_header(
            u16::try_from(payload.len()).expect("长度"),
            timestamp,
            sequence,
        );
        let packet = crypto.seal(&header, payload).expect("加密上行");
        env.udp
            .send_to(&packet, env.transport.udp_addr())
            .await
            .expect("发送上行");
    }

    async fn wait_frames(mock: &MockStrategy, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if mock.lock().audio_frames.len() >= count {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "音频帧没有在 2 秒内到达"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    struct DownPacket {
        header: UdpAudioHeader,
        payload: Vec<u8>,
        at: Instant,
    }

    async fn collect_downlink(env: &Env, crypto: &UdpCrypto, count: usize) -> Vec<DownPacket> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut buf = [0u8; 2048];
        while out.len() < count {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!left.is_zero(), "下行音频没收齐，只有 {}", out.len());
            let (len, _) = tokio::time::timeout(left, env.udp.recv_from(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("等下行音频超时，已有 {}", out.len()))
                .expect("recv");
            let at = Instant::now();
            let (header, payload) = crypto.open(&buf[..len]).expect("解密下行");
            out.push(DownPacket {
                header,
                payload,
                at,
            });
        }
        out
    }

    async fn paced_downlink() -> (Env, Vec<DownPacket>) {
        let frames: Vec<AudioFrame> = (0..8)
            .map(|index| AudioFrame {
                timestamp: 0,
                data: vec![0xA0 + index],
            })
            .collect();
        let mut env = Env::boot(frames).await;
        let hello = env.hello().await;
        let crypto = crypto_from_hello(&hello);
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "listen start 没有生效"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 1_000, 1, b"up").await;
        wait_frames(&env.mock, 1).await;
        env.link
            .publish("device-server", br#"{"type":"listen","state":"stop"}"#)
            .await;
        let packets = collect_downlink(&env, &crypto, 8).await;
        (env, packets)
    }

    #[tokio::test]
    async fn hello_response_has_16_byte_key_nonce_and_valid_port() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["transport"], "udp");
        assert_eq!(hello["audio_params"]["sample_rate"], 24_000);
        let key = decode_hex16(hello["udp"]["key"].as_str().expect("key"));
        let nonce = decode_hex16(hello["udp"]["nonce"].as_str().expect("nonce"));
        assert_eq!(key.len(), 16);
        assert_eq!(nonce.len(), 16);
        let port = hello["udp"]["port"].as_u64().expect("port");
        assert!(port > 0 && port < 65_536);
        assert_eq!(port as u16, env.transport.udp_addr().port());
        assert_eq!(env.transport.online_device_count().await, 1);
        env.shutdown().await;
    }

    #[tokio::test]
    async fn hello_server_is_lan_ip() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        let server = hello["udp"]["server"].as_str().expect("server");
        let ip: IpAddr = server.parse().expect("ip");
        assert_ne!(server, "0.0.0.0");
        assert_ne!(server, "127.0.0.1");
        assert!(!ip.is_loopback());
        assert!(!ip.is_unspecified());
        env.shutdown().await;
    }

    #[tokio::test]
    async fn hello_nonce_type_byte_is_one() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        let nonce = decode_hex16(hello["udp"]["nonce"].as_str().expect("nonce"));
        assert_eq!(nonce[0], 0x01);
        assert_eq!(&nonce[1..4], &[0, 0, 0]);
        assert_eq!(&nonce[8..], &[0, 0, 0, 0, 0, 0, 0, 0]);
        env.shutdown().await;
    }

    #[tokio::test]
    async fn listen_before_hello_is_ignored() {
        let mut env = Env::boot(Vec::new()).await;
        env.link
            .publish(
                "device-server",
                br#"{"type":"listen","state":"start","mode":"auto"}"#,
            )
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if env.transport.online_device_count().await == 1 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "listen 没有被处理");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(env.mock.lock().recording_starts.is_empty());
        let late = tokio::time::timeout(Duration::from_millis(200), env.link.rx.recv()).await;
        assert!(late.is_err(), "没 hello 之前不应该有下行");
        let hello = env.hello().await;
        assert_eq!(hello["type"], "hello");
        assert!(env.mock.lock().recording_starts.is_empty());
        env.shutdown().await;
    }

    #[tokio::test]
    async fn listen_start_calls_on_recording_start() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        let session_id = hello["session_id"].as_str().expect("session").to_owned();
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "on_recording_start 没有被调用"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(env.mock.lock().recording_starts, vec![session_id]);
        env.shutdown().await;
    }

    #[tokio::test]
    async fn udp_uplink_invokes_on_audio_frame() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        let crypto = crypto_from_hello(&hello);
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 12_345, 7, b"opus-frame").await;
        wait_frames(&env.mock, 1).await;
        let frames = env.mock.lock().audio_frames.clone();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp, 12_345);
        assert_eq!(frames[0].data, b"opus-frame");
        env.shutdown().await;
    }

    #[tokio::test]
    async fn listen_stop_emits_tts_start() {
        let mut env = Env::boot(Vec::new()).await;
        env.hello().await;
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        env.link
            .publish("device-server", br#"{"type":"listen","state":"stop"}"#)
            .await;
        let tts = env.link.wait_type("tts", Duration::from_secs(2)).await;
        assert_eq!(tts["state"], "start");
        assert_eq!(env.mock.lock().generate_calls, 1);
        env.shutdown().await;
    }

    #[tokio::test]
    async fn prebuffer_frames_are_sent_back_to_back() {
        let (env, packets) = paced_downlink().await;
        for pair in packets.windows(2).take(4) {
            let gap = pair[1].at.saturating_duration_since(pair[0].at);
            assert!(
                gap < Duration::from_millis(40),
                "预缓冲帧间隔应远小于 60ms，实际 {gap:?}"
            );
        }
        env.shutdown().await;
    }

    #[tokio::test]
    async fn steady_frames_are_spaced_about_60ms() {
        let (env, packets) = paced_downlink().await;
        let gaps: Vec<Duration> = packets
            .windows(2)
            .skip(4)
            .map(|pair| pair[1].at.saturating_duration_since(pair[0].at))
            .collect();
        assert!(!gaps.is_empty());
        for gap in gaps {
            assert!(
                (Duration::from_millis(40)..Duration::from_millis(90)).contains(&gap),
                "稳态帧间隔应约 60ms，实际 {gap:?}"
            );
        }
        env.shutdown().await;
    }

    #[tokio::test]
    async fn downlink_sequence_starts_at_one() {
        let (env, packets) = paced_downlink().await;
        let sequences: Vec<u32> = packets
            .iter()
            .map(|packet| packet.header.sequence)
            .collect();
        assert_eq!(sequences, (1..=8).collect::<Vec<_>>());
        env.shutdown().await;
    }

    #[tokio::test]
    async fn downlink_timestamp_starts_nonzero_and_steps_by_60() {
        let (env, packets) = paced_downlink().await;
        for (index, packet) in packets.iter().enumerate() {
            let timestamp = packet.header.timestamp;
            assert_ne!(timestamp, 0);
            assert_eq!(timestamp, 1 + index as u32 * 60);
            assert_eq!(packet.payload, vec![0xA0 + index as u8]);
        }
        env.shutdown().await;
    }

    #[tokio::test]
    async fn goodbye_unregisters_udp_session() {
        let mut env = Env::boot(Vec::new()).await;
        let hello = env.hello().await;
        let crypto = crypto_from_hello(&hello);
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 10, 1, b"keep").await;
        wait_frames(&env.mock, 1).await;
        env.link
            .publish("device-server", br#"{"type":"goodbye"}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // goodbye 只注销 UDP，MQTT 还在。再进录音后，旧 session 的包也进不来。
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        send_uplink(&env, &crypto, 11, 2, b"stale").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(env.mock.lock().audio_frames.len(), 1);
        env.shutdown().await;
    }

    /// 把已经进 socket 的下行清掉。安静 `quiet` 还没新包就停。
    async fn drain_udp(env: &Env, quiet: Duration) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut buf = [0u8; 2048];
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "打断后下行还在响，回放没有停"
            );
            match tokio::time::timeout(quiet, env.udp.recv_from(&mut buf)).await {
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
    }

    #[tokio::test]
    async fn abort_keeps_udp_session_for_barge_in() {
        // 帧数够多，看到 tts start 时回放还在 Playing。
        let playback: Vec<AudioFrame> = (0..12u8)
            .map(|index| AudioFrame {
                timestamp: 0,
                data: vec![0xB0 + index],
            })
            .collect();
        let mut env = Env::boot(playback).await;
        let hello = env.hello().await;
        let crypto = crypto_from_hello(&hello);
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 10, 1, b"first").await;
        wait_frames(&env.mock, 1).await;
        env.link
            .publish("device-server", br#"{"type":"listen","state":"stop"}"#)
            .await;
        let tts = env.link.wait_type("tts", Duration::from_secs(2)).await;
        assert_eq!(tts["state"], "start");

        // Playing 中打断。设备接着 listen start，并继续用同一把 UDP 密钥。
        env.link
            .publish("device-server", br#"{"type":"abort"}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(env.mock.lock().generate_calls, 1);
        drain_udp(&env, Duration::from_millis(200)).await;

        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if env.mock.lock().recording_starts.len() >= 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "打断后的 listen start 没有生效"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 20, 2, b"barge").await;
        wait_frames(&env.mock, 2).await;
        assert_eq!(env.mock.lock().audio_frames[1].data, b"barge");

        env.link
            .publish("device-server", br#"{"type":"listen","state":"stop"}"#)
            .await;
        let down = collect_downlink(&env, &crypto, 1).await;
        assert_eq!(down[0].payload, vec![0xB0]);
        assert_eq!(env.mock.lock().generate_calls, 2);

        // 打断时发过一次 tts stop，要等第二轮自己的 start 之后的 stop，才是 Ready。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut saw_start = false;
        loop {
            let tts = env.link.wait_type("tts", Duration::from_secs(2)).await;
            match tts["state"].as_str() {
                Some("start") => saw_start = true,
                Some("stop") if saw_start => break,
                _ => {}
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "第二轮没有等到 tts stop"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        drain_udp(&env, Duration::from_millis(200)).await;
        env.link
            .publish("device-server", br#"{"type":"abort"}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if env.mock.lock().recording_starts.len() >= 3 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &crypto, 30, 3, b"again").await;
        wait_frames(&env.mock, 3).await;
        assert_eq!(env.mock.lock().audio_frames[2].data, b"again");
        env.link
            .publish("device-server", br#"{"type":"listen","state":"stop"}"#)
            .await;
        let again = collect_downlink(&env, &crypto, 1).await;
        assert_eq!(again[0].payload, vec![0xB0]);
        assert_eq!(env.mock.lock().generate_calls, 3);
        env.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_hello_rotates_key_and_nonce() {
        let mut env = Env::boot(Vec::new()).await;
        let first = env.hello().await;
        let second = env.hello().await;
        let key1 = first["udp"]["key"].as_str().expect("key1").to_owned();
        let key2 = second["udp"]["key"].as_str().expect("key2").to_owned();
        let nonce1 = first["udp"]["nonce"].as_str().expect("nonce1").to_owned();
        let nonce2 = second["udp"]["nonce"].as_str().expect("nonce2").to_owned();
        assert_ne!(key1, key2);
        assert_ne!(nonce1, nonce2);
        let old_crypto = crypto_from_hello(&first);
        let new_crypto = crypto_from_hello(&second);
        env.link
            .publish("device-server", br#"{"type":"listen","state":"start"}"#)
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !env.mock.lock().recording_starts.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        send_uplink(&env, &old_crypto, 1, 1, b"old").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            env.mock.lock().audio_frames.is_empty(),
            "旧 key 的包应被丢弃"
        );
        send_uplink(&env, &new_crypto, 2, 1, b"new").await;
        wait_frames(&env.mock, 1).await;
        assert_eq!(env.mock.lock().audio_frames[0].data, b"new");
        env.shutdown().await;
    }

    fn mqtt_connect(client_id: &str) -> Vec<u8> {
        let mut body = Vec::new();
        append_mqtt_str(&mut body, "MQTT");
        body.push(4);
        body.push(0x02);
        body.extend_from_slice(&30u16.to_be_bytes());
        append_mqtt_str(&mut body, client_id);
        finish_packet(0x10, &body)
    }

    fn mqtt_publish(topic: &str, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        append_mqtt_str(&mut body, topic);
        body.extend_from_slice(payload);
        finish_packet(0x30, &body)
    }

    fn append_mqtt_str(buf: &mut Vec<u8>, text: &str) {
        let bytes = text.as_bytes();
        let len = u16::try_from(bytes.len()).expect("字符串过长");
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(bytes);
    }

    fn finish_packet(first: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![first];
        out.extend(encode_varint(body.len()));
        out.extend_from_slice(body);
        out
    }

    fn encode_varint(mut value: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value % 128) as u8;
            value /= 128;
            if value > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn publish_payload(body: &[u8]) -> Option<Vec<u8>> {
        if body.len() < 2 {
            return None;
        }
        let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
        let payload_at = 2 + topic_len;
        if body.len() < payload_at {
            return None;
        }
        Some(body[payload_at..].to_vec())
    }

    async fn read_mqtt_packet(
        read: &mut tokio::net::tcp::OwnedReadHalf,
    ) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut first = [0u8; 1];
        match read.read_exact(&mut first).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }
        let mut remaining = 0usize;
        let mut multiplier = 1usize;
        for _ in 0..4 {
            let mut byte = [0u8; 1];
            read.read_exact(&mut byte).await?;
            remaining += usize::from(byte[0] & 0x7f) * multiplier;
            if byte[0] & 0x80 == 0 {
                let mut body = vec![0u8; remaining];
                if remaining > 0 {
                    read.read_exact(&mut body).await?;
                }
                return Ok(Some((first[0], body)));
            }
            multiplier = multiplier.saturating_mul(128);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mqtt 变长整数过长",
        ))
    }
}
