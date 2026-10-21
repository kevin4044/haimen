pub mod devices;
pub mod mqtt;
pub mod mqtt_broker;
pub mod mqtt_udp;
pub mod notify;
mod ota;
mod protocol;
pub mod protocol_udp;
pub mod say_audio;
pub mod strategy;
pub mod types;
pub mod udp_audio;
mod ws;

pub use devices::{DeviceDirectory, install_device_directory, install_devices_from_config};
pub use mqtt_udp::{MqttUdpConfig, MqttUdpError, MqttUdpTransport};
pub use notify::SaySynthesizer;
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

/// 注入 xiaozhi 控制面端点（不包含 OTA / WebSocket 设备通信入口）
///
/// 这组路由不依赖 WebSocket 响应策略，因此**不要求**调用方先构造 strategy：
/// - `POST /api/v1/xiaozhi/say`     — 主动播报：文本 → Ogg Opus → 托管 → 下发 notify
/// - `GET  /api/v1/xiaozhi/devices` — 设备清单：配置 + 在线/忙闲状态
/// - `GET  /xiaozhi/say/{id}`       — 一次性通知音频（实际 URL 带 `.ogg` 后缀，由 handler 剥掉）
///
/// 为什么和 [`add_routes`] 拆开：即使没有 `xiaozhi_strategy`（例如纯 REST /
/// Web UI 场景），say 与 devices 端点也应该可用；OTA 与 WebSocket 才依赖策略。
///
/// # 参数
///
/// * `router` — 待扩展的 axum Router
///
/// 接受任意 State 类型（`S: Clone + Send + Sync + 'static`），
/// 因为 xiaozhi 处理器都走全局槽、不依赖外部状态。
pub fn add_control_routes<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        // 通知音频和 say 接口都用全局槽（say_audio::say_audio_store /
        // notify 的设置槽），不污染 Router 的状态类型。
        .route(
            "/xiaozhi/say/{id}",
            axum::routing::get(say_audio::handle_say_audio),
        )
        .route(
            "/api/v1/xiaozhi/say",
            axum::routing::post(notify::handle_say),
        )
        // 多设备运维：配置设备 + 在线状态 + 忙闲状态，供 Web UI / HA 选目标设备。
        .route(
            "/api/v1/xiaozhi/devices",
            axum::routing::get(notify::handle_devices),
        )
}

/// 向 axum Router 注入 xiaozhi 设备通信与控制面端点
///
/// 路由始终挂载（MQTT 模式下设备仍可能回退到 WebSocket）：
/// - `POST /xiaozhi/ota/`          — OTA 握手，按配置下发 websocket 或 mqtt 段
/// - `GET  /xiaozhi/ws`            — WebSocket 升级端点
/// - 其余 say / devices 端点见 [`add_control_routes`]
///
/// 内部只调用一次 [`add_control_routes`]，避免同一路径被重复注册
/// （axum 对同一路径重复 `route` 会 panic）。
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
    let router = router
        .route("/xiaozhi/ota/", axum::routing::post(ota::handle_ota))
        .route(
            "/xiaozhi/ws",
            axum::routing::get(move |ws, headers| {
                ws::handle_ws_upgrade(ws, headers, strategy.clone())
            }),
        );
    add_control_routes(router)
}
