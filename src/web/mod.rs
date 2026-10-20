pub mod api;
pub mod r#static;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::config::settings::{AsrConfig, TtsConfig};
use crate::gateway::webhook::WebhookState;

/// 服务器配置
pub struct ServeConfig {
    pub host: String,
    pub port: u16,
    /// 启动成功后自动打开浏览器
    pub auto_open: bool,
}

/// `POST /api/v1/xiaozhi/say` 的合成器实现（根 crate 侧）。
///
/// 为什么实现在根 crate 而不是 haimen-xiaozhi：合成要读共享的 TTS 配置、
/// 走 `tts_factory`、调 Opus 编码器，这些都在根 crate。haimen-xiaozhi
/// 只定义 `SaySynthesizer` trait 并在这里注入。
/// 把 CLI 音色覆盖叠加到 TTS 配置上，返回新配置（不改原对象）。
///
/// 单独抽出来是为了可测：notify 播报和正常对话回复必须用同一个音色，
/// 两边解析路径一旦分叉，用户就会听到「同一个人两种嗓子」。
/// 正常回复侧对应 `AsrLlmTtsStrategy::synthesize_*` 里的同样处理。
fn apply_voice_override(cfg: &TtsConfig, voice_override: Option<&str>) -> TtsConfig {
    let mut work = cfg.clone();
    if let Some(voice) = voice_override {
        work.providers
            .entry(work.active_provider.clone())
            .or_default()
            .insert("voice".to_string(), voice.to_string());
    }
    work
}

struct XiaozhiSaySynthesizer {
    tts_config: Arc<RwLock<TtsConfig>>,
    voice_override: Option<String>,
}

#[async_trait::async_trait]
impl haimen_xiaozhi::notify::SaySynthesizer for XiaozhiSaySynthesizer {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, String> {
        // 音色解析与正常回复（synthesize_status_pcm）逐行一致：
        // clone 共享配置 → 注入 voice override → 由工厂建 provider。
        // 在单独的块里取放 RwLock，避免 guard 跨越 .await。
        let provider = {
            let cfg = self.tts_config.read().map_err(|e| e.to_string())?;
            let work_cfg = apply_voice_override(&cfg, self.voice_override.as_deref());
            crate::tts_factory::create_tts_provider(&work_cfg)?
        };
        let resp = provider
            .synthesize(univoice::tts::TtsRequest {
                text: text.to_string(),
                options: None,
            })
            .await
            .map_err(|e| format!("TTS 合成失败: {e}"))?;
        if resp.audio.is_empty() {
            return Err("TTS 返回空音频".to_string());
        }
        // resp.audio 是 PCM16 mono 24kHz；60ms 帧与设备下行节奏一致
        let frames = crate::xiaozhi_tts::pcm_to_opus_frames(&resp.audio, 24_000, 60)?;
        wrap_ogg_opus(&frames)
    }
}

/// 把裸 Opus 帧封装成 Ogg 容器字节，供设备 HTTP 拉流播放。
///
/// 关键决定：
/// - OpusHead 的采样率字段按输出采样率 24000 声明（Opus 内部固定 48k，
///   这个字段只是给解码器的重采样提示）；pre_skip=312 是 libopus 常用 lookahead。
/// - granule 从 pre_skip 起每帧 += 2880（60ms × 48kHz，granule 恒为 48k 单位），
///   notify.rs 就用最后一包的 granule 反算 duration_ms。
/// - OpusHead / OpusTags 各独占一页（EndPage），末帧 EndStream ——
///   设备固件的 Ogg 解析器按最保守的分页假设实现。
fn wrap_ogg_opus(frames: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};

    if frames.is_empty() {
        return Err("没有可封装的音频帧".to_string());
    }

    // stream serial 只需在并发流间可区分。根 crate 的依赖白名单不允许
    // 为此新增 uuid，用「时间戳低位 ^ 递增计数」拼一个足够随机的值。
    let serial = {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(16)
    };

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = PacketWriter::new(&mut cursor);

        // OpusHead，19 字节
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels = mono
        head.extend_from_slice(&312u16.to_le_bytes()); // pre_skip
        head.extend_from_slice(&24_000u32.to_le_bytes()); // 输出采样率
        head.extend_from_slice(&0i16.to_le_bytes()); // gain
        head.push(0); // channel mapping family 0
        writer
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| format!("写 OpusHead 失败: {e}"))?;

        // OpusTags，16 字节（空 vendor、空注释列表）
        let mut tags = Vec::with_capacity(16);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        writer
            .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| format!("写 OpusTags 失败: {e}"))?;

        let mut granule: u64 = 312; // pre_skip
        for (index, frame) in frames.iter().enumerate() {
            granule += 2880; // 60ms × 48kHz
            let end = if index + 1 == frames.len() {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            writer
                .write_packet(frame.clone(), serial, end, granule)
                .map_err(|e| format!("写音频帧失败: {e}"))?;
        }
    }
    Ok(cursor.into_inner())
}

/// 启动 HTTP 服务器
///
/// - `config`: 服务器地址配置
/// - `webhook_state`: 可选的 Webhook 处理器（GitHub Webhook）
/// - `xiaozhi_strategy`: 可选的 xiaozhi WebSocket 响应策略，为 None 时不挂载 xiaozhi 路由
/// - `asr_config`: 共享的 ASR 配置（Arc<RwLock>），Web API 保存时同步更新，实现运行时热加载
/// - `tts_config`: 共享的 TTS 配置（Arc<RwLock>），Web API 保存时同步更新，实现运行时热加载
/// - `cancel`: 共享取消令牌，收到取消信号时触发优雅关闭
// 启动协调函数，参数都是互不相关的独立配置/句柄；打包成 struct 反而更晦涩。
#[allow(clippy::too_many_arguments)]
pub async fn start(
    config: ServeConfig,
    webhook_state: Option<WebhookState>,
    xiaozhi_strategy: Option<Arc<dyn haimen_xiaozhi::ResponseStrategy>>,
    xiaozhi_config: haimen_xiaozhi::XiaozhiConfig,
    asr_config: Arc<RwLock<AsrConfig>>,
    tts_config: Arc<RwLock<TtsConfig>>,
    // CLI 音色覆盖（`--xiaozhi-tts-voice`），与正常对话回复走同一个值。
    // 必须传进来而不是固定 None：notify 播报和对话回复必须是同一个声音，
    // 否则用户会听到「同一个人一会儿一个嗓子」。`start_all` 那条路径
    // 构建策略时 override 本来就是 None，所以传 None 与之一致。
    voice_override: Option<String>,
    shared_agent: crate::gateway::agent_handle::SharedAgent,
    cancel: CancellationToken,
) -> Result<(), String> {
    use haimen_xiaozhi;

    // 安装小智传输配置：决定 OTA 下发 websocket 还是 mqtt 段。
    // 必须在挂载路由前调用 —— handle_ota 用的是全局槽，默认 websocket。
    let say_token = xiaozhi_config.say_token.clone(); // xiaozhi_config 随后被 move，先取
    haimen_xiaozhi::set_xiaozhi_ota_config(xiaozhi_config);

    // say 接口的运行时安装：token / HTTP 端口由 web 层提供，
    // advertise_ip 由 gateway 在 MQTT 传输层启动后另行安装。
    haimen_xiaozhi::notify::set_say_token(say_token.clone());
    if say_token.is_empty() {
        tracing::warn!("[xiaozhi] say_token 为空，/api/v1/xiaozhi/say 鉴权未启用（内网自用模式）");
    }
    haimen_xiaozhi::notify::set_say_http_port(config.port);
    haimen_xiaozhi::notify::set_say_synthesizer(Arc::new(XiaozhiSaySynthesizer {
        tts_config: tts_config.clone(),
        // 与正常对话回复用同一个音色解析结果（见 synthesize 里的注释）。
        voice_override,
    }));

    // 构建路由
    // ASR 路由使用 asr_config 作为 axum State（独立于 TTS 配置）
    let asr_routes = axum::Router::new()
        .route(
            "/api/v1/settings/asr",
            axum::routing::get(api::voice_settings::get_asr_settings),
        )
        .route(
            "/api/v1/settings/asr",
            axum::routing::put(api::voice_settings::update_asr_settings),
        )
        .route(
            "/api/v1/settings/asr/verify",
            axum::routing::post(api::voice_settings::verify_asr_credentials),
        )
        .with_state(asr_config);

    // TTS 路由使用 tts_config 作为 axum State
    let tts_routes = axum::Router::new()
        .route(
            "/api/v1/settings/tts",
            axum::routing::get(api::voice_settings::get_tts_settings),
        )
        .route(
            "/api/v1/settings/tts",
            axum::routing::put(api::voice_settings::update_tts_settings),
        )
        .route(
            "/api/v1/settings/tts/voices",
            axum::routing::get(api::voice_settings::list_tts_voices),
        )
        .route(
            "/api/v1/settings/tts/verify",
            axum::routing::post(api::voice_settings::verify_tts_credentials),
        )
        .with_state(tts_config);

    let agent_routes = axum::Router::new()
        .route(
            "/api/v1/settings/agent",
            axum::routing::get(api::agent_settings::get_agent_settings),
        )
        .route(
            "/api/v1/settings/agent",
            axum::routing::put(api::agent_settings::update_agent_settings),
        )
        .route(
            "/api/v1/settings/agent/verify",
            axum::routing::post(api::agent_settings::verify_agent_credentials),
        )
        .route(
            "/api/v1/agent/providers",
            axum::routing::get(api::agent_settings::list_agent_providers),
        )
        .route(
            "/api/v1/agent/logs",
            axum::routing::get(api::agent_logs::get_agent_logs),
        )
        // Agent 共享句柄注入 State：PUT 保存后重建 Agent 并换入，实现运行时热切换
        .with_state(shared_agent);

    // 消息渠道（Connector）：飞书/钉钉配置读写 + 可用状态
    let connectors_routes = axum::Router::new()
        .route(
            "/api/v1/settings/connectors",
            axum::routing::get(api::connectors::get_connectors_settings),
        )
        .route(
            "/api/v1/settings/connectors",
            axum::routing::put(api::connectors::update_connectors_settings),
        )
        .route(
            "/api/v1/connectors/status",
            axum::routing::get(api::connectors::get_connectors_status),
        );

    let app = if let Some(state) = webhook_state {
        let mut r = axum::Router::new()
            .route("/health", axum::routing::get(health_handler))
            .route("/api/v1/system/info", axum::routing::get(api::system::info))
            .route(
                "/webhook/github",
                axum::routing::post(api::webhook::handle_github_webhook),
            )
            .with_state(state);
        if let Some(strategy) = xiaozhi_strategy {
            r = haimen_xiaozhi::add_routes(r, strategy);
        }
        r = r.merge(asr_routes);
        r = r.merge(tts_routes);
        r = r.merge(agent_routes);
        r = r.merge(connectors_routes);
        r.fallback(r#static::handle)
    } else {
        let mut r = axum::Router::new()
            .route("/health", axum::routing::get(health_handler))
            .route("/api/v1/system/info", axum::routing::get(api::system::info));
        if let Some(strategy) = xiaozhi_strategy {
            r = haimen_xiaozhi::add_routes(r, strategy);
        }
        r = r.merge(asr_routes);
        r = r.merge(tts_routes);
        r = r.merge(agent_routes);
        r = r.merge(connectors_routes);
        r.fallback(r#static::handle)
    };

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| format!("无效的监听地址: {}", e))?;

    // 一次性通知音频的 TTL 兜底清理：设备不在 Idle 时 notify 被静默丢弃、
    // 不会来拉，没人取的条目会一直占内存。30s 扫一次足够（音频 TTL 是分钟级）。
    // 挂 cancel 令牌是为了跟 HTTP 服务一起退出，不留孤儿任务。
    {
        let store = haimen_xiaozhi::say_audio_store();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            // interval 第一次立即触发，跳过一次，避免启动时做无用功。
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = tick.tick() => store.sweep(),
                    _ = cancel.cancelled() => break,
                }
            }
        });
    }

    tracing::info!("Web 服务器启动于 http://{addr}");

    // 打印 Web UI 地址（始终可见，不受日志级别影响）
    let listen_all = config.host == "0.0.0.0" || config.host == "::";
    if listen_all {
        // 监听所有接口时，同时打印 localhost 和真实 LAN IP
        println!("🌐 Web UI: http://127.0.0.1:{}", config.port);
        if let Ok(ip) = local_ip_address::local_ip() {
            println!("🌐 Web UI (LAN): http://{}:{}", ip, config.port);
        }
    } else {
        println!("🌐 Web UI: http://{}:{}", config.host, config.port);
    }

    // 自动打开浏览器（仅在用户要求时）
    if config.auto_open {
        open_browser(&config.host, config.port);
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址失败: {}", e))?;

    // 优雅关闭：优先等待共享取消令牌，同时也响应 SIGINT/SIGTERM
    let graceful = graceful_shutdown(cancel);

    // 必须带 connect info：OTA handler 要按对端 IP 做来源校验。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(graceful)
    .await
    .map_err(|e| format!("服务器错误: {}", e))
}

/// 组合信号 + CancellationToken 的优雅关闭 Future
async fn graceful_shutdown(cancel: CancellationToken) {
    tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("收到关闭信号，HTTP 服务器正在关闭...");
        }
        _ = cancel.cancelled() => {
            tracing::info!("网关已停止，HTTP 服务器正在关闭...");
        }
    }
}

/// 健康检查：返回 JSON
async fn health_handler() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "service": "haimen"
    }))
}

/// 等待 SIGINT（Ctrl+C）或 SIGTERM 用于优雅关闭
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("无法安装 Ctrl+C 处理器");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法安装 SIGTERM 处理器")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl+C，正在关闭..."),
        _ = terminate => tracing::info!("收到 SIGTERM，正在关闭..."),
    }
}

/// 自动打开浏览器到 Web 控制台
///
/// - `0.0.0.0` 会自动转为 `127.0.0.1`（浏览器无法访问 `0.0.0.0`）
/// - 打开失败只记录 warning，不阻塞启动流程
fn open_browser(host: &str, port: u16) {
    let display_host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    let url = format!("http://{}:{}", display_host, port);
    match webbrowser::open(&url) {
        Ok(()) => tracing::info!(url = %url, "已自动打开浏览器"),
        Err(e) => tracing::warn!(url = %url, error = %e, "自动打开浏览器失败"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── notify 音色必须与正常回复一致（Cursor Bugbot 在 PR #9 上报过回归）──

    fn tts_cfg_with_voice(voice: &str) -> TtsConfig {
        let mut cfg = TtsConfig::default();
        cfg.active_provider = "doubao".to_string();
        cfg.providers.insert(
            "doubao".to_string(),
            std::collections::HashMap::from([("voice".to_string(), voice.to_string())]),
        );
        cfg
    }

    #[test]
    fn apply_voice_override_replaces_active_provider_voice() {
        let cfg = tts_cfg_with_voice("config-voice");
        let out = apply_voice_override(&cfg, Some("cli-voice"));
        assert_eq!(
            out.providers["doubao"]["voice"], "cli-voice",
            "CLI 覆盖必须作用到 notify 播报，否则和正常回复是两个声音"
        );
        // 原配置不能被改（共享配置是 Arc<RwLock>，就地改会污染正常回复）
        assert_eq!(cfg.providers["doubao"]["voice"], "config-voice");
    }

    #[test]
    fn apply_voice_override_none_keeps_configured_voice() {
        let cfg = tts_cfg_with_voice("config-voice");
        let out = apply_voice_override(&cfg, None);
        assert_eq!(out.providers["doubao"]["voice"], "config-voice");
    }

    #[test]
    fn apply_voice_override_creates_entry_when_provider_missing() {
        let mut cfg = TtsConfig::default();
        cfg.active_provider = "doubao".to_string(); // providers 为空
        let out = apply_voice_override(&cfg, Some("cli-voice"));
        assert_eq!(out.providers["doubao"]["voice"], "cli-voice");
    }

    /// 编一段静音 PCM 走真实封装路径，再用 ogg 读端按序读回：
    /// OpusHead → OpusTags → 数据帧，且末包 granule 正确、带流结束标记。
    #[test]
    fn wrap_ogg_opus_round_trips_via_reader() {
        // 3 帧 60ms @ 24kHz 的静音 PCM16
        let pcm = vec![0u8; 3 * 60 * 24 * 2];
        let frames = crate::xiaozhi_tts::pcm_to_opus_frames(&pcm, 24_000, 60).expect("编码失败");
        assert_eq!(frames.len(), 3);
        let ogg_bytes = wrap_ogg_opus(&frames).expect("封装失败");

        let mut reader = ogg::reading::PacketReader::new(std::io::Cursor::new(ogg_bytes));
        let head = reader
            .read_packet()
            .expect("读 OpusHead")
            .expect("缺 OpusHead");
        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(head.absgp_page(), 0);
        let tags = reader
            .read_packet()
            .expect("读 OpusTags")
            .expect("缺 OpusTags");
        assert_eq!(&tags.data[..8], b"OpusTags");
        assert_eq!(tags.absgp_page(), 0);

        let mut last_granule = 0u64;
        let mut frame_count = 0u64;
        while let Some(packet) = reader.read_packet().expect("读数据帧") {
            frame_count += 1;
            assert_eq!(packet.data, frames[(frame_count - 1) as usize]);
            last_granule = packet.absgp_page();
        }
        assert_eq!(frame_count, 3, "数据帧数应一致");
        // granule 恒为 48k 单位：312 (pre_skip) + 3 × 2880 (60ms × 48kHz)
        assert_eq!(last_granule, 312 + 3 * 2880);
    }

    #[test]
    fn wrap_ogg_opus_rejects_empty_frames() {
        assert!(wrap_ogg_opus(&[]).is_err());
    }
}
