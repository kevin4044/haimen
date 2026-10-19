pub mod mqtt;
pub mod mqtt_broker;
pub mod mqtt_udp;
mod ota;
mod protocol;
pub mod protocol_udp;
pub mod say_audio;
pub mod strategy;
pub mod types;
pub mod udp_audio;
mod ws;

pub use mqtt_udp::{MqttUdpConfig, MqttUdpError, MqttUdpTransport};
pub use protocol::*;
pub use say_audio::{SayAudioStore, say_audio_store};
pub use strategy::{EchoStrategy, ResponseStrategy};
pub use types::*;

use std::sync::Arc;

use axum::Router;

/// 当配置为 mqtt 时，由根 crate 调用以启动 MQTT+UDP 传输层。
/// websocket 模式下不需要调用。
///
/// M2' 不把这次调用接进根 crate 的启动流程，避免现有 WebSocket 部署多开端口。
pub async fn start_mqtt_transport(
    cfg: MqttUdpConfig,
    strategy: Arc<dyn ResponseStrategy>,
) -> Result<MqttUdpTransport, MqttUdpError> {
    MqttUdpTransport::start(cfg, strategy).await
}

/// 安装 OTA 下发用的小智配置。未调用时默认为 websocket，响应里只有 `websocket` 段。
pub fn set_xiaozhi_ota_config(cfg: XiaozhiConfig) {
    ota::set_xiaozhi_ota_config(cfg);
}

/// 向 axum Router 注入 xiaozhi 设备通信端点
///
/// 路由始终挂载（MQTT 模式下设备仍可能回退到 WebSocket）：
/// - `POST /xiaozhi/ota/`      — OTA 握手，按配置下发 websocket 或 mqtt 段
/// - `GET  /xiaozhi/ws`        — WebSocket 升级端点
/// - `GET  /xiaozhi/say/{id}`  — 一次性通知音频（实际 URL 带 `.ogg` 后缀，由 handler 剥掉）
///
/// # 参数
///
/// * `router` — 待扩展的 axum Router
/// * `strategy` — WebSocket 响应策略，控制设备录音结束后的回放行为
///
/// 接受任意 State 类型（`S: Clone + Send + Sync + 'static`），
/// 因为 xiaozhi 处理器不依赖外部状态。
pub fn add_routes<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    strategy: Arc<dyn ResponseStrategy>,
) -> Router<S> {
    router
        .route("/xiaozhi/ota/", axum::routing::post(ota::handle_ota))
        .route(
            "/xiaozhi/ws",
            axum::routing::get(move |ws, headers| {
                ws::handle_ws_upgrade(ws, headers, strategy.clone())
            }),
        )
        // 通知音频用全局槽（say_audio::say_audio_store），不污染 Router 的状态类型。
        .route(
            "/xiaozhi/say/{id}",
            axum::routing::get(say_audio::handle_say_audio),
        )
}
