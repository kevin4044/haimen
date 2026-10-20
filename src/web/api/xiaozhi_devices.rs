//! 小智设备配置与在线状态 REST API
//!
//! 提供 `settings.toml` 中 `[[xiaozhi.devices]]`（设备身份、名字、房间）的读写，
//! 以及当前 MQTT/UDP 传输层在线设备的实时状态聚合。
//!
//! # 端点
//!
//! - `GET /api/v1/settings/xiaozhi/devices` — 已配置设备 + 在线设备
//! - `PUT /api/v1/settings/xiaozhi/devices` — 全量替换设备表并立即生效
//!
//! # 为什么保存后要重装设备目录
//!
//! 设备目录（`haimen_xiaozhi::devices`）是进程级全局槽，负责把设备身份映射成
//! 「名字 + 房间」。启动时由 gateway 安装一次；Web 端保存后必须重新安装，
//! 否则房间上下文注入和 notify 按房间寻址都要等重启才生效。

use axum::{Json, http::StatusCode};
use haimen_xiaozhi::XiaozhiDeviceConfig;
use haimen_xiaozhi::devices::{DeviceDirectory, normalize_device_id};
use serde::Deserialize;

use crate::config::settings::AppConfig;

/// 加载当前配置（文件不存在时返回默认值）
fn load_config() -> AppConfig {
    crate::config::settings::load_settings()
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// `PUT` 请求体：全量替换设备表。
///
/// 元素直接反序列化成 `XiaozhiDeviceConfig`，因此 `mac` / `client_id`
/// 也能作为 `device_id` 的别名（见该类型的 serde 定义）。
#[derive(Debug, Deserialize)]
pub struct UpdateDevicesRequest {
    #[serde(default)]
    devices: Vec<XiaozhiDeviceConfig>,
}

fn error_response(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "success": false,
            "error": message.into(),
        })),
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
// 设备 JSON 序列化
// ═══════════════════════════════════════════════════════════════════════════════

/// 已配置设备列表的 JSON 形式。
///
/// 用 `DeviceDirectory::from_config` 归一化：`device_id` 统一成冒号小写 MAC
/// （方便前端和在线列表按 MAC 去重），空身份/重复 MAC 的条目会被跳过。
fn configured_devices_json(configs: &[XiaozhiDeviceConfig]) -> Vec<serde_json::Value> {
    DeviceDirectory::from_config(configs)
        .entries()
        .iter()
        .map(|device| {
            serde_json::json!({
                "device_id": device.mac,
                "name": device.name,
                "room": device.room,
                "configured": true,
            })
        })
        .collect()
}

/// 在线设备列表的 JSON 形式。
///
/// `say_hub()` 为 `None` 表示 MQTT 传输层没跑（websocket 模式），
/// 此时没有「在线设备」这个概念，返回空数组。
async fn online_devices_json() -> Vec<serde_json::Value> {
    let Some(hub) = haimen_xiaozhi::mqtt_udp::say_hub() else {
        return Vec::new();
    };
    let directory = haimen_xiaozhi::devices::device_directory();

    let mut list = Vec::new();
    for info in hub.online_devices().await {
        let state = match hub.busy_state(&info.client_id).await {
            None => "idle",
            Some(state) => state,
        };
        let configured = directory
            .as_ref()
            .is_some_and(|dir| dir.for_client_id(&info.client_id).is_some());
        list.push(serde_json::json!({
            "client_id": info.client_id,
            "device_id": info.device_id,
            "name": info.name,
            "room": info.room,
            "online": true,
            "state": state,
            "configured": configured,
        }));
    }
    list
}

// ═══════════════════════════════════════════════════════════════════════════════
// 路由处理函数
// ═══════════════════════════════════════════════════════════════════════════════

/// `GET /api/v1/settings/xiaozhi/devices`
///
/// 返回已配置设备（`[[xiaozhi.devices]]`）与当前在线设备。
/// 已配置但离线的设备也会出现在 `devices` 里，前端固定按它渲染表格。
pub async fn get_xiaozhi_devices() -> Json<serde_json::Value> {
    let cfg = load_config();
    Json(serde_json::json!({
        "success": true,
        "data": {
            "devices": configured_devices_json(&cfg.xiaozhi.devices),
            "online": online_devices_json().await,
        }
    }))
}

/// `PUT /api/v1/settings/xiaozhi/devices`
///
/// 全量替换设备表：校验身份非空、归一化后不重复，写盘后立即重装全局设备目录。
pub async fn update_xiaozhi_devices(
    Json(body): Json<UpdateDevicesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if body.devices.len() > 256 {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "设备数量不能超过 256 台",
        ));
    }

    // 校验与清洗：device_id 去空白后非空；name/room 也去掉首尾空白。
    // 重复判定按归一化 MAC（client_id 会先取出 MAC 段），
    // 这样 `84:c7:...`、`84_C7_...`、`84c7...` 三种写法不会被当成三台设备。
    let mut normalized_seen = std::collections::HashSet::new();
    let mut devices = Vec::with_capacity(body.devices.len());
    for item in body.devices {
        let device_id = item.device_id.trim();
        if device_id.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "设备 ID 不能为空（MAC 或 MQTT client_id）",
            ));
        }
        let mac = normalize_device_id(device_id);
        if !normalized_seen.insert(mac.clone()) {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("设备 ID 重复：{mac}"),
            ));
        }
        devices.push(XiaozhiDeviceConfig {
            device_id: device_id.to_string(),
            name: item.name.trim().to_string(),
            room: item.room.trim().to_string(),
        });
    }

    let mut cfg = load_config();
    cfg.xiaozhi.devices = devices;

    if let Err(e) = crate::config::settings::save_settings(&cfg) {
        tracing::warn!(error = %e, "保存小智设备配置到文件失败");
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存配置失败: {e}"),
        ));
    }

    // 立即生效：对话侧的房间上下文注入、notify 的按设备名/房间寻址都读这个全局槽。
    haimen_xiaozhi::devices::install_devices_from_config(&cfg.xiaozhi.devices);

    Ok(Json(serde_json::json!({
        "success": true,
        "data": {
            "devices": configured_devices_json(&cfg.xiaozhi.devices),
        }
    })))
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

    /// 请求体按真实 HTTP 路径反序列化，顺带覆盖 `mac` / `client_id` 别名。
    fn parse_request(body: serde_json::Value) -> UpdateDevicesRequest {
        serde_json::from_value(body).expect("请求体反序列化失败")
    }

    /// 每个用例结束都把全局设备目录清空，避免污染同进程其它用例。
    fn reset_directory() {
        haimen_xiaozhi::devices::install_devices_from_config(&[]);
    }

    #[test]
    fn put_persists_config_and_installs_directory() {
        run_with_temp_home(|_| {
            let req = parse_request(serde_json::json!({
                "devices": [
                    { "device_id": "84:C7:BB:59:93:64", "name": " 客厅小智 ", "room": " 客厅 " },
                    { "mac": "84_c7_bb_59_93_65", "name": "卧室小智", "room": "卧室" },
                ]
            }));
            let res = block_on(update_xiaozhi_devices(Json(req))).unwrap();
            let devices = &res.0["data"]["devices"];
            assert_eq!(devices.as_array().unwrap().len(), 2);
            // device_id 统一归一化成冒号小写 MAC
            assert_eq!(devices[0]["device_id"], "84:c7:bb:59:93:64");
            assert_eq!(devices[0]["name"], "客厅小智");
            assert_eq!(devices[0]["room"], "客厅");
            assert_eq!(devices[0]["configured"], true);

            // 写盘：重新加载 settings.toml 应能看到两台设备
            let saved = crate::config::settings::load_settings()
                .unwrap()
                .expect("settings.toml 应已写入");
            assert_eq!(saved.xiaozhi.devices.len(), 2);
            assert_eq!(saved.xiaozhi.devices[0].device_id, "84:C7:BB:59:93:64");
            assert_eq!(saved.xiaozhi.devices[0].name, "客厅小智");

            // 立即生效：全局目录已按新配置安装
            let dir = haimen_xiaozhi::devices::device_directory().expect("设备目录应已安装");
            let living = dir.find("84_c7_bb_59_93_64").expect("按 MAC 查找客厅设备");
            assert_eq!(living.room, "客厅");
            let bedroom = dir
                .for_client_id("GID_haimen@@@84_c7_bb_59_93_65@@@84_c7_bb_59_93_65")
                .expect("按 client_id 查找卧室设备");
            assert_eq!(bedroom.room, "卧室");

            reset_directory();
        });
    }

    #[test]
    fn put_rejects_empty_device_id() {
        run_with_temp_home(|_| {
            let req = parse_request(serde_json::json!({
                "devices": [{ "device_id": "   ", "name": "无名", "room": "客厅" }]
            }));
            let (status, body) = block_on(update_xiaozhi_devices(Json(req))).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body.0["success"], false);
            assert!(
                body.0["error"]
                    .as_str()
                    .unwrap()
                    .contains("设备 ID 不能为空"),
                "{}",
                body.0["error"]
            );
            // 校验失败不能写盘
            assert!(
                crate::config::settings::load_settings().unwrap().is_none(),
                "校验失败的请求不应写入 settings.toml"
            );
            reset_directory();
        });
    }

    #[test]
    fn put_rejects_duplicate_normalized_mac() {
        run_with_temp_home(|_| {
            let req = parse_request(serde_json::json!({
                "devices": [
                    { "device_id": "84:c7:bb:59:93:64", "name": "第一个", "room": "客厅" },
                    { "device_id": "84_C7_BB_59_93_64", "name": "第二个", "room": "卧室" },
                ]
            }));
            let (status, body) = block_on(update_xiaozhi_devices(Json(req))).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                body.0["error"].as_str().unwrap().contains("设备 ID 重复"),
                "{}",
                body.0["error"]
            );
            assert!(
                crate::config::settings::load_settings().unwrap().is_none(),
                "校验失败的请求不应写入 settings.toml"
            );
            reset_directory();
        });
    }

    #[test]
    fn get_returns_configured_devices_with_online_array() {
        run_with_temp_home(|_| {
            let req = parse_request(serde_json::json!({
                "devices": [
                    { "device_id": "84c7bb599364", "name": "", "room": "" },
                ]
            }));
            let _ = block_on(update_xiaozhi_devices(Json(req))).unwrap();

            let res = block_on(get_xiaozhi_devices());
            let data = &res.0["data"];
            assert_eq!(res.0["success"], true);
            assert_eq!(data["devices"][0]["device_id"], "84:c7:bb:59:93:64");
            assert_eq!(data["devices"][0]["name"], "");
            assert_eq!(data["devices"][0]["room"], "");
            assert_eq!(data["devices"][0]["configured"], true);
            // 没有安装 say_hub（websocket 模式）时在线列表为空数组
            assert!(data["online"].is_array());
            assert_eq!(data["online"].as_array().unwrap().len(), 0);

            reset_directory();
        });
    }

    #[test]
    fn put_accepts_client_id_alias_and_empty_device_list() {
        run_with_temp_home(|_| {
            let req = parse_request(serde_json::json!({
                "devices": [{
                    "client_id": "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64",
                    "name": "客厅小智",
                    "room": "客厅",
                }]
            }));
            let res = block_on(update_xiaozhi_devices(Json(req))).unwrap();
            assert_eq!(
                res.0["data"]["devices"][0]["device_id"],
                "84:c7:bb:59:93:64"
            );

            // 空列表 = 清空设备表
            let res = block_on(update_xiaozhi_devices(Json(UpdateDevicesRequest {
                devices: Vec::new(),
            })))
            .unwrap();
            assert_eq!(res.0["data"]["devices"].as_array().unwrap().len(), 0);
            let saved = crate::config::settings::load_settings().unwrap().unwrap();
            assert!(saved.xiaozhi.devices.is_empty());

            reset_directory();
        });
    }
}
