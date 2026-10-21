//! 小智自定义主题 assets 管理。
//!
//! 这条链路是 **控制面直连**：
//! 浏览器生成 `assets.bin` → 上传到 haimen → haimen 通过设备 MCP
//! `self.assets.set_download_url` + `self.reboot` 让设备自己下载。
//! 不经过 LLM / Agent / ASR / TTS。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use axum::{
    Json,
    body::Bytes,
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use haimen_xiaozhi::device_mcp::{DeviceMcpError, device_mcp_hub};
use haimen_xiaozhi::mqtt_udp::{SayDeviceInfo, say_hub};

type ApiError = (StatusCode, Json<Value>);

/// 单次上传上限；assets 分区通常是 8MB，这里先按 8MB 卡住。
const MAX_ASSETS_BYTES: usize = 8 * 1024 * 1024;
const ASSETS_HEADER_SIZE: usize = 12;
const ASSETS_TABLE_ENTRY_SIZE: usize = 44;
const ASSETS_NAME_LENGTH: usize = 32;
const MCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn error_response(status: StatusCode, code: &str, detail: impl Into<String>) -> ApiError {
    (
        status,
        Json(json!({
            "ok": false,
            "error": code,
            "detail": detail.into(),
        })),
    )
}

fn invalid_request(detail: impl Into<String>) -> ApiError {
    error_response(StatusCode::BAD_REQUEST, "invalid_request", detail)
}

// ═══════════════════════════════════════════════════════════════════════════════
// assets.bin 校验
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct AssetEntry {
    name: String,
    size: usize,
    offset: usize,
}

#[derive(Debug)]
struct ParsedAssets {
    entries: Vec<AssetEntry>,
    index: Value,
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn entry_name(table: &[u8], offset: usize) -> Option<String> {
    let raw = table.get(offset..offset + ASSETS_NAME_LENGTH)?;
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    // 固件用 `std::string(asset_name)` 读取；32 字节没有 NUL 会越界。
    if end == 0 || end == ASSETS_NAME_LENGTH {
        return None;
    }
    let name = std::str::from_utf8(&raw[..end]).ok()?.to_owned();
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(name)
}

/// 解析并校验 assets.bin；错误字符串直接作为 422 的 detail。
fn parse_assets_bin(data: &[u8]) -> Result<ParsedAssets, String> {
    if data.len() < ASSETS_HEADER_SIZE {
        return Err("assets.bin 小于 12 字节头".to_owned());
    }
    let total_files = u32_at(data, 0).ok_or("文件数缺失")? as usize;
    let stored_checksum = u32_at(data, 4).ok_or("checksum 缺失")?;
    let combined_len = u32_at(data, 8).ok_or("combined_len 缺失")? as usize;
    if combined_len != data.len() - ASSETS_HEADER_SIZE {
        return Err(format!(
            "combined_len={combined_len} 与实际长度 {} 不一致",
            data.len() - ASSETS_HEADER_SIZE
        ));
    }
    if total_files == 0 || total_files > 256 {
        return Err(format!("文件数非法: {total_files}"));
    }
    let table_len = total_files
        .checked_mul(ASSETS_TABLE_ENTRY_SIZE)
        .ok_or("文件表长度溢出")?;
    if table_len > combined_len {
        return Err("文件表超出 combined 数据".to_owned());
    }
    let combined = &data[ASSETS_HEADER_SIZE..];
    let checksum = combined
        .iter()
        .fold(0u32, |acc, byte| acc + u32::from(*byte))
        & 0xffff;
    if checksum != stored_checksum {
        return Err(format!(
            "checksum 不匹配: 存储 {stored_checksum:#06x}, 计算 {checksum:#06x}"
        ));
    }

    let mut entries = Vec::with_capacity(total_files);
    let mut names = HashSet::with_capacity(total_files);
    for index in 0..total_files {
        let base = index * ASSETS_TABLE_ENTRY_SIZE;
        let name = entry_name(combined, base).ok_or_else(|| format!("第 {index} 项文件名非法"))?;
        if !names.insert(name.clone()) {
            return Err(format!("文件名重复: {name}"));
        }
        let size = u32_at(combined, base + ASSETS_NAME_LENGTH).ok_or("文件大小缺失")? as usize;
        let relative =
            u32_at(combined, base + ASSETS_NAME_LENGTH + 4).ok_or("文件偏移缺失")? as usize;
        let _width = u16_at(combined, base + ASSETS_NAME_LENGTH + 8).ok_or("图片宽度缺失")?;
        let _height = u16_at(combined, base + ASSETS_NAME_LENGTH + 10).ok_or("图片高度缺失")?;
        let offset = table_len.checked_add(relative).ok_or("文件偏移溢出")?;
        let end = offset
            .checked_add(2)
            .and_then(|value| value.checked_add(size))
            .ok_or("文件范围溢出")?;
        if end > combined_len {
            return Err(format!("文件 {name} 超出 combined 数据"));
        }
        if combined.get(offset..offset + 2) != Some(b"ZZ") {
            return Err(format!("文件 {name} 缺少 0x5A5A 前缀"));
        }
        entries.push(AssetEntry { name, size, offset });
    }

    let index_entry = entries
        .iter()
        .find(|entry| entry.name == "index.json")
        .ok_or("assets.bin 缺少 index.json")?;
    let start = index_entry.offset + 2;
    let index_bytes = combined
        .get(start..start + index_entry.size)
        .ok_or("index.json 数据越界")?;
    let index: Value = serde_json::from_slice(index_bytes).map_err(|err| {
        format!(
            "index.json 不是合法 JSON（{} 字节）: {err}",
            index_entry.size
        )
    })?;
    let object = index.as_object().ok_or("index.json 必须是 JSON 对象")?;
    if let Some(version) = object.get("version") {
        let Some(version) = version.as_u64() else {
            return Err("index.json.version 必须是数字".to_owned());
        };
        if version > 1 {
            return Err(format!("不支持的 assets version: {version}"));
        }
    }
    if let Some(hide) = object.get("hide_subtitle")
        && !hide.is_boolean()
    {
        return Err("index.json.hide_subtitle 必须是 bool".to_owned());
    }
    if let Some(skin) = object.get("skin")
        && !skin.is_object()
    {
        return Err("index.json.skin 必须是 object".to_owned());
    }
    if let Some(collection) = object.get("emoji_collection") {
        let Some(collection) = collection.as_array() else {
            return Err("index.json.emoji_collection 必须是数组".to_owned());
        };
        for item in collection {
            let Some(item) = item.as_object() else {
                return Err("emoji_collection 项必须是 object".to_owned());
            };
            if !item.get("name").is_some_and(Value::is_string)
                || !item.get("file").is_some_and(Value::is_string)
            {
                return Err("emoji_collection 项必须有 name/file 字符串".to_owned());
            }
        }
    }
    if let Some(font) = object.get("text_font")
        && !font.is_string()
    {
        return Err("index.json.text_font 必须是字符串".to_owned());
    }

    Ok(ParsedAssets { entries, index })
}

// ═══════════════════════════════════════════════════════════════════════════════
// 文件存储
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
struct AssetStore {
    dir: PathBuf,
}

#[derive(Debug, Clone)]
struct StoredAsset {
    file: String,
    size: usize,
    sha256: String,
}

impl AssetStore {
    fn default_dir() -> PathBuf {
        let root = std::env::var_os("HAIMEN_XIAOZHI_FIRMWARE_DIR")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| {
                crate::config::settings::get_settings_dir().join("xiaozhi-firmware")
            });
        root.join("assets")
    }

    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn from_default() -> Self {
        Self::new(Self::default_dir())
    }

    fn path_for(&self, file: &str) -> Option<PathBuf> {
        valid_asset_file_name(file).then(|| self.dir.join(file))
    }

    async fn save(&self, bytes: &[u8]) -> Result<StoredAsset, std::io::Error> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let sha256 = hex::encode(Sha256::digest(bytes));
        let file = format!("assets-{}.bin", &sha256[..16]);
        let path = self.dir.join(&file);
        let tmp = self.dir.join(format!(".{file}.tmp"));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(StoredAsset {
            file,
            size: bytes.len(),
            sha256,
        })
    }

    async fn list(&self) -> Result<Vec<Value>, std::io::Error> {
        let mut out = Vec::new();
        let mut reader = match tokio::fs::read_dir(&self.dir).await {
            Ok(reader) => reader,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(err),
        };
        while let Some(entry) = reader.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !valid_asset_file_name(name) {
                continue;
            }
            let metadata = match entry.metadata().await {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => continue,
            };
            let bytes = match tokio::fs::read(entry.path()).await {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or_default();
            out.push(json!({
                "file": name,
                "size": metadata.len(),
                "sha256": hex::encode(Sha256::digest(&bytes)),
                "modified_at_ms": modified,
            }));
        }
        out.sort_by(|left, right| {
            right["modified_at_ms"]
                .as_i64()
                .unwrap_or_default()
                .cmp(&left["modified_at_ms"].as_i64().unwrap_or_default())
        });
        Ok(out)
    }

    async fn read(&self, file: &str) -> Result<Vec<u8>, std::io::Error> {
        let path = self
            .path_for(file)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "非法文件名"))?;
        tokio::fs::read(path).await
    }
}

fn valid_asset_file_name(file: &str) -> bool {
    let Some(rest) = file.strip_prefix("assets-") else {
        return false;
    };
    let Some(hash) = rest.strip_suffix(".bin") else {
        return false;
    };
    hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_asset_file_name(file: &str) -> Result<(), ApiError> {
    if valid_asset_file_name(file) {
        Ok(())
    } else {
        Err(invalid_request("assets 文件名非法"))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 设备 / MCP
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct DeviceQuery {
    device: String,
}

#[derive(Debug, Deserialize)]
pub struct ApplyRequest {
    device: String,
    file: String,
}

struct OnlineDevice {
    mac: String,
    client_id: String,
}

async fn find_online(identity: &str) -> Result<Option<OnlineDevice>, ApiError> {
    let mac = super::xiaozhi_firmware::resolve_device_mac(identity)
        .ok_or_else(|| invalid_request("device 不能为空"))?;
    let Some(hub) = say_hub() else {
        return Ok(None);
    };
    let devices = hub.online_devices().await;
    Ok(devices
        .into_iter()
        .find(|device: &SayDeviceInfo| device.device_id.eq_ignore_ascii_case(&mac))
        .map(|device| OnlineDevice {
            mac,
            client_id: device.client_id,
        }))
}

fn mcp_text_json(result: &Value) -> Option<Value> {
    let text = result
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    serde_json::from_str(text).ok()
}

async fn call_tool(client_id: &str, tool: &str, arguments: Value) -> Result<Value, DeviceMcpError> {
    let Some(hub) = device_mcp_hub() else {
        return Err(DeviceMcpError::Unsupported(
            "设备 MCP 控制面未启动".to_owned(),
        ));
    };
    hub.call_tool(client_id, tool, arguments, MCP_TIMEOUT).await
}

async fn read_system_info(client_id: &str) -> Result<Value, DeviceMcpError> {
    let result = call_tool(client_id, "self.get_system_info", json!({})).await?;
    mcp_text_json(&result).ok_or_else(|| {
        DeviceMcpError::ToolError("self.get_system_info 返回的不是 JSON 文本".to_owned())
    })
}

fn assets_partition_size(system_info: &Value) -> Option<u64> {
    system_info
        .get("partition_table")?
        .as_array()?
        .iter()
        .find(|part| part.get("label").and_then(Value::as_str) == Some("assets"))
        .and_then(|part| part.get("size"))
        .and_then(Value::as_u64)
        .filter(|size| *size > 0)
}

fn screen_info_from(result: &Value) -> Option<Value> {
    mcp_text_json(result).and_then(|value| {
        let width = value.get("width")?.as_u64()?;
        let height = value.get("height")?.as_u64()?;
        Some(json!({
            "width": width,
            "height": height,
            "monochrome": value.get("monochrome").and_then(Value::as_bool).unwrap_or(false),
        }))
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// apply 并发锁
// ═══════════════════════════════════════════════════════════════════════════════

static APPLYING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

struct ApplyGuard {
    mac: String,
}

impl ApplyGuard {
    fn acquire(mac: &str) -> Option<Self> {
        let mut applying = APPLYING
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !applying.insert(mac.to_owned()) {
            return None;
        }
        Some(Self {
            mac: mac.to_owned(),
        })
    }
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        if let Some(applying) = APPLYING.get() {
            let mut applying = applying.lock().unwrap_or_else(|err| err.into_inner());
            applying.remove(&self.mac);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════════════════

/// `POST /api/v1/xiaozhi/assets` — 上传并校验 assets.bin。
pub async fn upload_xiaozhi_assets(body: Bytes) -> Result<Json<Value>, ApiError> {
    if body.len() > MAX_ASSETS_BYTES {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "assets_too_large",
            format!(
                "assets.bin {} 字节，超过上限 {}",
                body.len(),
                MAX_ASSETS_BYTES
            ),
        ));
    }
    if body.is_empty() {
        return Err(invalid_request("assets.bin 为空"));
    }
    let parsed = parse_assets_bin(&body).map_err(|detail| {
        error_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid_assets", detail)
    })?;
    tracing::warn!(
        files = parsed.entries.len(),
        hide_subtitle = parsed
            .index
            .get("hide_subtitle")
            .and_then(|value| value.as_bool()),
        "assets.bin 校验通过"
    );

    let store = AssetStore::from_default();
    let stored = store.save(&body).await.map_err(|err| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "assets_unavailable",
            format!("保存 assets.bin 失败: {err}"),
        )
    })?;
    tracing::warn!(
        file = %stored.file,
        size = stored.size,
        "已保存小智自定义主题 assets.bin"
    );
    Ok(Json(json!({
        "ok": true,
        "file": stored.file,
        "size": stored.size,
        "sha256": stored.sha256,
        "url": format!("/xiaozhi/assets/{}", stored.file),
    })))
}

/// `GET /api/v1/xiaozhi/assets` — 列出已上传的 assets.bin。
pub async fn list_xiaozhi_assets() -> Result<Json<Value>, ApiError> {
    let store = AssetStore::from_default();
    let assets = store.list().await.map_err(|err| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "assets_unavailable",
            format!("读取 assets 目录失败: {err}"),
        )
    })?;
    Ok(Json(json!({ "ok": true, "assets": assets })))
}

/// `GET /xiaozhi/assets/{file}` — 设备下载 assets.bin。
pub async fn get_xiaozhi_asset(AxumPath(file): AxumPath<String>) -> Result<Response, ApiError> {
    validate_asset_file_name(&file)?;
    let store = AssetStore::from_default();
    let bytes = store.read(&file).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            error_response(
                StatusCode::NOT_FOUND,
                "assets_not_found",
                "assets 文件不存在",
            )
        } else {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "assets_unavailable",
                format!("读取 assets 文件失败: {err}"),
            )
        }
    })?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// `GET /api/v1/xiaozhi/assets/device-info` — 屏幕分辨率 / assets 分区 / MCP 状态。
pub async fn get_xiaozhi_assets_device_info(
    Query(query): Query<DeviceQuery>,
) -> Result<Json<Value>, ApiError> {
    let mac = super::xiaozhi_firmware::resolve_device_mac(&query.device)
        .ok_or_else(|| invalid_request("device 不能为空"))?;
    let Some(online) = find_online(&mac).await? else {
        return Ok(Json(json!({
            "ok": true,
            "device": mac,
            "online": false,
            "client_id": null,
            "mcp": false,
            "screen": null,
            "assets_partition_size": null,
            "warning": "设备不在线（或 MQTT 传输层未启动）",
        })));
    };

    let mut warning = None;
    let mut system_info = None;
    match read_system_info(&online.client_id).await {
        Ok(value) => system_info = Some(value),
        Err(err) => warning = Some(format!("读取设备分区失败: {err}")),
    }
    let screen = match call_tool(&online.client_id, "self.screen.get_info", json!({})).await {
        Ok(result) => screen_info_from(&result),
        Err(err) => {
            let message = format!("读取屏幕信息失败: {err}");
            warning = Some(match warning {
                Some(previous) => format!("{previous}; {message}"),
                None => message,
            });
            None
        }
    };
    let partition = system_info.as_ref().and_then(assets_partition_size);
    let mcp = device_mcp_hub().is_some();
    Ok(Json(json!({
        "ok": true,
        "device": online.mac,
        "online": true,
        "client_id": online.client_id,
        "mcp": mcp,
        "screen": screen,
        "assets_partition_size": partition,
        "warning": warning,
    })))
}

/// `POST /api/v1/xiaozhi/assets/apply` — 设下载 URL 并重启设备。
pub async fn apply_xiaozhi_assets(
    headers: HeaderMap,
    Json(request): Json<ApplyRequest>,
) -> Result<Json<Value>, ApiError> {
    let mac = super::xiaozhi_firmware::resolve_device_mac(&request.device)
        .ok_or_else(|| invalid_request("device 不能为空"))?;
    validate_asset_file_name(&request.file)?;
    let store = AssetStore::from_default();
    let path = store
        .path_for(&request.file)
        .ok_or_else(|| invalid_request("assets 文件名非法"))?;
    let file_size = tokio::fs::metadata(&path)
        .await
        .map_err(|_| {
            error_response(
                StatusCode::NOT_FOUND,
                "assets_not_found",
                "assets 文件不存在，请先上传",
            )
        })?
        .len() as usize;

    let Some(online) = find_online(&mac).await? else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "device_offline",
            "设备不在线，无法下发 assets",
        ));
    };
    let _guard = ApplyGuard::acquire(&online.mac).ok_or_else(|| {
        error_response(
            StatusCode::CONFLICT,
            "apply_in_progress",
            "该设备已有主题下发任务在进行",
        )
    })?;

    if let Ok(system_info) = read_system_info(&online.client_id).await
        && let Some(partition) = assets_partition_size(&system_info)
        && file_size as u64 > partition
    {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "assets_too_large",
            format!("assets.bin {file_size} 字节，超过设备 assets 分区 {partition} 字节"),
        ));
    }

    let base = haimen_xiaozhi::notify::device_http_base()
        .unwrap_or_else(|| super::xiaozhi_firmware::http_base(&headers));
    let url = format!(
        "{}/xiaozhi/assets/{}",
        base.trim_end_matches('/'),
        request.file
    );
    if let Err(err) = call_tool(
        &online.client_id,
        "self.assets.set_download_url",
        json!({ "url": url }),
    )
    .await
    {
        return Err(map_mcp_error(err));
    }
    let reboot = match device_mcp_hub() {
        Some(hub) => hub
            .notify_tool(&online.client_id, "self.reboot", json!({}))
            .await
            .is_ok(),
        None => false,
    };
    tracing::warn!(
        device_id = %online.mac,
        file = %request.file,
        size = file_size,
        reboot,
        "已下发小智自定义主题"
    );
    Ok(Json(json!({
        "ok": true,
        "device": online.mac,
        "file": request.file,
        "url": url,
        "reboot": reboot,
    })))
}

fn map_mcp_error(err: DeviceMcpError) -> ApiError {
    match err {
        DeviceMcpError::Offline => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "device_offline",
            "设备 MCP 连接已断开",
        ),
        DeviceMcpError::Unsupported(detail) => error_response(
            StatusCode::CONFLICT,
            "mcp_unavailable",
            format!("设备不支持 assets 下发: {detail}"),
        ),
        DeviceMcpError::Timeout => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "mcp_timeout",
            "设备 MCP 调用超时",
        ),
        DeviceMcpError::ToolError(detail) => error_response(
            StatusCode::BAD_GATEWAY,
            "apply_failed",
            format!("设备返回错误: {detail}"),
        ),
        DeviceMcpError::Transport(detail) => error_response(
            StatusCode::BAD_GATEWAY,
            "apply_failed",
            format!("设备 MCP 传输失败: {detail}"),
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn build_assets(files: &[(&str, &[u8])]) -> Vec<u8> {
        let count = files.len();
        let table_len = count * ASSETS_TABLE_ENTRY_SIZE;
        let data_len: usize = files.iter().map(|(_, data)| 2 + data.len()).sum();
        let mut combined = vec![0u8; table_len + data_len];
        let mut data_offset = table_len;
        for (index, (name, data)) in files.iter().enumerate() {
            let base = index * ASSETS_TABLE_ENTRY_SIZE;
            let name_bytes = name.as_bytes();
            combined[base..base + name_bytes.len().min(ASSETS_NAME_LENGTH)]
                .copy_from_slice(&name_bytes[..name_bytes.len().min(ASSETS_NAME_LENGTH)]);
            combined[base + 32..base + 36].copy_from_slice(&(data.len() as u32).to_le_bytes());
            combined[base + 36..base + 40]
                .copy_from_slice(&((data_offset - table_len) as u32).to_le_bytes());
            combined[data_offset] = 0x5a;
            combined[data_offset + 1] = 0x5a;
            combined[data_offset + 2..data_offset + 2 + data.len()].copy_from_slice(data);
            data_offset += 2 + data.len();
        }
        let checksum = combined
            .iter()
            .fold(0u32, |acc, byte| acc + u32::from(*byte))
            & 0xffff;
        let mut out = Vec::with_capacity(ASSETS_HEADER_SIZE + combined.len());
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&(combined.len() as u32).to_le_bytes());
        out.extend_from_slice(&combined);
        out
    }

    fn valid_assets() -> Vec<u8> {
        build_assets(&[("index.json", br#"{"version":1,"hide_subtitle":true}"#)])
    }

    #[test]
    fn parse_valid_assets() {
        let parsed = parse_assets_bin(&valid_assets()).expect("合法 assets");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].name, "index.json");
        assert_eq!(parsed.index["hide_subtitle"], true);
    }

    #[test]
    fn reject_bad_checksum() {
        let mut bytes = valid_assets();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(parse_assets_bin(&bytes).unwrap_err().contains("checksum"));
    }

    #[test]
    fn reject_missing_index() {
        let bytes = build_assets(&[("font.bin", b"font")]);
        assert!(parse_assets_bin(&bytes).unwrap_err().contains("index.json"));
    }

    #[test]
    fn reject_path_in_name() {
        let bytes = build_assets(&[("../index.json", b"{}")]);
        assert!(parse_assets_bin(&bytes).unwrap_err().contains("文件名非法"));
    }

    #[test]
    fn valid_file_name_rules() {
        assert!(valid_asset_file_name("assets-0123456789abcdef.bin"));
        assert!(!valid_asset_file_name("assets-0123.bin"));
        assert!(!valid_asset_file_name("../assets-0123456789abcdef.bin"));
        assert!(!valid_asset_file_name("assets-0123456789abcdeg.bin"));
    }

    #[tokio::test]
    async fn store_roundtrip_and_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AssetStore::new(dir.path().to_path_buf());
        let stored = store.save(&valid_assets()).await.expect("保存");
        assert!(valid_asset_file_name(&stored.file));
        assert_eq!(
            store.read(&stored.file).await.expect("读取"),
            valid_assets()
        );
        let list = store.list().await.expect("列表");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["file"], stored.file);
    }
}
