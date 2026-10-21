//! 小智固件 OTA：release 查询、指定版本升级、目标管理。
//!
//! 本模块只负责控制面（Web UI/API）。设备侧的 OTA 握手与固件下载在
//! `haimen_xiaozhi::firmware` / `ota` 中。
//!
//! # 端点
//!
//! - `GET  /api/v1/xiaozhi/firmware` — 版本列表 + 每台设备当前版本/最新匹配/目标
//! - `POST /api/v1/xiaozhi/firmware/upgrade` — 下载并设置升级目标
//! - `POST /api/v1/xiaozhi/firmware/target/clear` — 清除目标
//!
//! 具体实现见 `archive`（ZIP/ESP 镜像提取）与 `github`（release/manifest）。

pub mod archive;
pub mod github;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use axum::{Json, extract::Query};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use haimen_xiaozhi::devices::{device_directory, normalize_device_id};
use haimen_xiaozhi::firmware::{
    self, DeviceFirmwareReport, FirmwareStoreConfig, FirmwareTarget, JsonFileMetadataStore,
    LocalFileBlobStore,
};
use haimen_xiaozhi::mqtt_udp::say_hub;

use github::{FirmwareCandidate, GithubError, ReleaseInfo, ReleaseManifest, ReleaseService};

/// 上游固件仓库。需求固定为 78/xiaozhi-esp32；测试通过 base URL 覆盖。
pub const FIRMWARE_REPOSITORY: &str = "78/xiaozhi-esp32";

/// release zip 的大小上限（32MB）。真实资产约 2-3MB，留足余量。
const MAX_ZIP_BYTES: usize = 32 * 1024 * 1024;

type ApiError = (StatusCode, Json<Value>);

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
// GitHub 服务单例
// ═══════════════════════════════════════════════════════════════════════════════

static SERVICE: OnceLock<Result<ReleaseService, String>> = OnceLock::new();

fn service() -> Result<&'static ReleaseService, String> {
    SERVICE
        .get_or_init(ReleaseService::from_env)
        .as_ref()
        .map_err(Clone::clone)
}

fn map_github_error(err: GithubError) -> ApiError {
    match err {
        GithubError::Http(detail) => {
            error_response(StatusCode::BAD_GATEWAY, "github_unavailable", detail)
        }
        GithubError::RateLimited(reset) => error_response(
            StatusCode::BAD_GATEWAY,
            "github_rate_limited",
            match reset {
                Some(reset) => {
                    format!("GitHub API 限流，重置时间 {reset}；可配置 GITHUB_TOKEN 提高限额")
                }
                None => "GitHub API 限流，请稍后重试或配置 GITHUB_TOKEN".to_string(),
            },
        ),
        GithubError::Parse(detail) => {
            error_response(StatusCode::BAD_GATEWAY, "github_unavailable", detail)
        }
        GithubError::NotFound(tag) => error_response(
            StatusCode::NOT_FOUND,
            "release_not_found",
            format!("release 不存在: {tag}"),
        ),
        GithubError::ManifestMissing(tag) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "extract_failed",
            format!("release 缺少 release-manifest.json: {tag}"),
        ),
        GithubError::BoardNotFound { board, name } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "board_not_found",
            format!("该版本没有匹配此板型的固件（board={board}, name={name}）"),
        ),
        GithubError::BoardAmbiguous(detail) => {
            error_response(StatusCode::CONFLICT, "board_ambiguous", detail)
        }
        GithubError::ChipMismatch { device, firmware } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "chip_mismatch",
            format!("芯片型号不匹配：设备={device}, 固件={firmware}"),
        ),
        GithubError::AssetNotFound(detail) => {
            error_response(StatusCode::UNPROCESSABLE_ENTITY, "asset_not_found", detail)
        }
        GithubError::VariantNotFound(detail) => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "variant_not_found",
            format!("所选固件变体不存在: {detail}"),
        ),
    }
}

fn map_download_error(err: GithubError) -> ApiError {
    match err {
        GithubError::RateLimited(reset) => map_github_error(GithubError::RateLimited(reset)),
        other => error_response(
            StatusCode::BAD_GATEWAY,
            "download_failed",
            other.to_string(),
        ),
    }
}

fn map_store_error(err: firmware::FirmwareStoreError) -> ApiError {
    if matches!(err, firmware::FirmwareStoreError::NotInstalled) {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "firmware_store_unavailable",
            "固件存储未安装，升级功能不可用",
        );
    }
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "cache_write_failed",
        err.to_string(),
    )
}

fn map_archive_error(err: archive::ArchiveError) -> ApiError {
    let code = if matches!(err, archive::ArchiveError::TooLarge) {
        "firmware_too_large"
    } else {
        "extract_failed"
    };
    error_response(StatusCode::UNPROCESSABLE_ENTITY, code, err.to_string())
}

/// 精确匹配失败但存在相似候选时，要求用户先选择具体固件变体。
fn selection_required(candidates: Vec<FirmwareCandidate>) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "ok": false,
            "error": "selection_required",
            "detail": "未精确匹配板型，请从候选固件中选择",
            "candidates": candidates,
        })),
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
// 存储安装
// ═══════════════════════════════════════════════════════════════════════════════

/// 启动时安装默认文件存储。
///
/// 默认目录 `~/.haimen/xiaozhi-firmware/`（随现有 Docker 数据卷持久化），
/// 可用 `HAIMEN_XIAOZHI_FIRMWARE_DIR` 覆盖到宿主机其他挂载目录。
pub async fn install_store_from_settings() -> Result<PathBuf, String> {
    let dir = std::env::var_os("HAIMEN_XIAOZHI_FIRMWARE_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| crate::config::settings::get_settings_dir().join("xiaozhi-firmware"));

    let cfg = FirmwareStoreConfig {
        metadata: Arc::new(JsonFileMetadataStore::new(dir.join("state.json"))),
        blobs: Arc::new(LocalFileBlobStore::new(dir.join("bin"))),
        gc_on_start: true,
    };
    firmware::install_firmware_store(cfg)
        .await
        .map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(dir)
}

// ═══════════════════════════════════════════════════════════════════════════════
// 概览
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Deserialize)]
pub struct OverviewQuery {
    /// `refresh=1` / `refresh=true` 绕过 GitHub 缓存。
    #[serde(default)]
    refresh: Option<String>,
}

impl OverviewQuery {
    fn wants_refresh(&self) -> bool {
        self.refresh.as_deref().is_some_and(|value| {
            value.eq_ignore_ascii_case("true") || value == "1" || value.eq_ignore_ascii_case("yes")
        })
    }
}

#[derive(Debug, Default)]
struct DeviceEntry {
    name: String,
    room: String,
    report: Option<DeviceFirmwareReport>,
    target: Option<FirmwareTarget>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn rfc3339_from_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// 合并设备目录、在线设备、上报与目标，得到固件面板要展示的设备列表。
async fn collect_devices() -> Vec<(String, DeviceEntry)> {
    let mut devices: BTreeMap<String, DeviceEntry> = BTreeMap::new();

    if let Some(directory) = device_directory() {
        for device in directory.entries() {
            let entry = devices.entry(device.mac.clone()).or_default();
            entry.name = device.display_name().to_string();
            entry.room = device.room.clone();
        }
    }

    if let Some(hub) = say_hub() {
        for info in hub.online_devices().await {
            let mac = normalize_device_id(&info.device_id);
            if mac.is_empty() {
                continue;
            }
            let entry = devices.entry(mac).or_default();
            if entry.name.trim().is_empty() {
                entry.name = info.name.clone();
            }
            if entry.room.trim().is_empty() {
                entry.room = info.room.clone();
            }
        }
    }

    for report in firmware::reports() {
        let entry = devices.entry(report.device_id.clone()).or_default();
        if entry.name.trim().is_empty() {
            entry.name = if report.mac.trim().is_empty() {
                report.device_id.clone()
            } else {
                report.mac.clone()
            };
        }
        entry.report = Some(report);
    }

    for (device_id, target) in firmware::targets() {
        devices.entry(device_id).or_default().target = Some(target);
    }

    devices
        .into_iter()
        .map(|(device_id, mut entry)| {
            if entry.name.trim().is_empty() {
                entry.name = device_id.clone();
            }
            (device_id, entry)
        })
        .collect()
}

fn release_json(release: &ReleaseInfo) -> Value {
    json!({
        "tag": release.tag,
        "version": release.version,
        "name": release.name,
        "published_at": release.published_at,
        "prerelease": release.prerelease,
        "html_url": release.html_url,
    })
}

fn report_json(report: &DeviceFirmwareReport) -> Value {
    json!({
        "version": report.app_version,
        "board_type": report.board_type,
        "board_name": report.board_name,
        "chip_model": report.chip_model,
        "app_name": report.app_name,
        "reported_at_ms": report.reported_at_ms,
        "ota_partition_size": report.ota_partition_size,
    })
}

/// 根据当前版本与最新版本给出展示状态。
fn update_state(current: &str, latest: &str) -> (&'static str, Option<String>) {
    match firmware::compare_versions(current, latest) {
        Some(std::cmp::Ordering::Less) => ("update_available", None),
        Some(std::cmp::Ordering::Equal) => ("latest", None),
        Some(std::cmp::Ordering::Greater) => ("ahead", None),
        None => (
            "error",
            Some(format!("无法解析固件版本（当前={current}, 最新={latest}）")),
        ),
    }
}

/// 匹配结果转成设备的最新固件视图 + 状态 + 候选固件。
fn latest_state(
    latest_release: &ReleaseInfo,
    manifest: Option<&ReleaseManifest>,
    report: &DeviceFirmwareReport,
) -> (
    Option<Value>,
    Vec<FirmwareCandidate>,
    &'static str,
    Option<String>,
) {
    match github::match_firmware(
        latest_release,
        manifest,
        &report.board_type,
        &report.board_name,
        &report.chip_model,
    ) {
        Ok(matched) => {
            let (state, error) = update_state(&report.app_version, &matched.version);
            (
                Some(json!({
                    "tag": latest_release.tag,
                    "version": matched.version,
                    "asset": matched.asset.name,
                    "published_at": latest_release.published_at,
                    "html_url": latest_release.html_url,
                })),
                Vec::new(),
                state,
                error,
            )
        }
        Err(GithubError::BoardNotFound { .. }) | Err(GithubError::BoardAmbiguous(_)) => {
            // 精确匹配失败不代表没有可用的固件：厂商目录前缀、自定义构建名
            // 都可能让 board/name 对不上。给出候选让用户自己确认。
            let candidates = manifest
                .map(|manifest| {
                    github::candidate_firmwares(
                        latest_release,
                        manifest,
                        &report.board_type,
                        &report.board_name,
                        &report.chip_model,
                    )
                })
                .unwrap_or_default();
            if candidates.is_empty() {
                (
                    None,
                    Vec::new(),
                    "unsupported",
                    Some("上游最新版本没有匹配此板型的固件资产".to_string()),
                )
            } else {
                // 候选存在不代表设备还没升级：设备可能已经刷到该版本，
                // 只是板型仍无法精确匹配。这里用版本号决定展示状态，
                // 避免升级完成后一直显示「需选择」。
                let release_version = manifest
                    .map(|manifest| manifest.version.trim_start_matches(['v', 'V']).to_string())
                    .unwrap_or_else(|| latest_release.version.clone());
                // 同时把 latest 指向最可能的候选 release，供 UI 作为默认版本；
                // 否则 UI 会回退到 releases[0]，可能选中 prerelease 而候选
                // 来自另一个 stable release（Cursor Bugbot #12 第二条）。
                let latest_json = candidates.first().map(|top| {
                    json!({
                        "tag": latest_release.tag,
                        "version": release_version,
                        "asset": top.asset.clone(),
                        "published_at": latest_release.published_at,
                        "html_url": latest_release.html_url,
                    })
                });
                let (comparison, comparison_error) =
                    update_state(&report.app_version, &release_version);
                let (state, error) = match comparison {
                    "update_available" => (
                        "needs_selection",
                        Some("未精确匹配板型，请选择候选固件".to_string()),
                    ),
                    "latest" => ("latest", None),
                    "ahead" => ("ahead", None),
                    _ => (
                        "needs_selection",
                        comparison_error
                            .or_else(|| Some("无法解析固件版本，请选择候选固件".to_string())),
                    ),
                };
                (latest_json, candidates, state, error)
            }
        }
        Err(GithubError::AssetNotFound(_)) => (
            None,
            Vec::new(),
            "unsupported",
            Some("上游最新版本没有匹配此板型的固件资产".to_string()),
        ),
        Err(err @ GithubError::ChipMismatch { .. }) => {
            (None, Vec::new(), "unsupported", Some(err.to_string()))
        }
        Err(err) => (None, Vec::new(), "error", Some(err.to_string())),
    }
}

/// `GET /api/v1/xiaozhi/firmware`
///
/// 即使 GitHub 不可用也返回 200：设备当前版本、目标仍可见，只把
/// `error` / 每设备 `state` 标成错误，避免整个面板打不开。
pub async fn get_xiaozhi_firmware(Query(query): Query<OverviewQuery>) -> Json<Value> {
    let mut releases: Vec<ReleaseInfo> = Vec::new();
    let mut fetched_at = String::new();
    let mut stale = false;
    let mut overview_error: Option<String> = None;
    let mut svc: Option<&ReleaseService> = None;
    let mut latest_error: Option<String> = None;

    match service() {
        Ok(service) => {
            svc = Some(service);
            match service.releases(query.wants_refresh()).await {
                Ok(page) => {
                    fetched_at = rfc3339_from_ms(page.fetched_at_ms);
                    stale = page.stale;
                    releases = page.releases;
                }
                Err(err) => overview_error = Some(err.to_string()),
            }
        }
        Err(err) => overview_error = Some(err),
    }

    let latest = github::latest_release(&releases).cloned();
    let manifest = match (svc, latest.as_ref()) {
        (Some(service), Some(release)) => match service.manifest(&release.tag).await {
            Ok(manifest) => Some(manifest),
            // 老 release 可能没有 manifest；match_firmware 会按资产名回退。
            Err(GithubError::ManifestMissing(_)) => None,
            Err(err) => {
                latest_error = Some(err.to_string());
                None
            }
        },
        _ => None,
    };

    let mut devices_json = Vec::new();
    for (device_id, entry) in collect_devices().await {
        let target_json = match entry.target.as_ref() {
            Some(target) => {
                let file_present = firmware::firmware_file_exists(&target.file).await;
                Some(json!({
                    "version": target.version,
                    "asset": target.asset,
                    "sha256": target.sha256,
                    "installed_at_ms": target.installed_at_ms,
                    "deliveries": target.deliveries,
                    "file_present": file_present,
                }))
            }
            None => None,
        };

        let (latest_json, candidates, state, device_error) = match entry.report.as_ref() {
            None => (None, Vec::new(), "unknown", None),
            Some(report) => match latest.as_ref() {
                Some(release) => latest_state(release, manifest.as_ref(), report),
                None => {
                    let detail = latest_error
                        .clone()
                        .unwrap_or_else(|| "没有可用的稳定 release".to_string());
                    (None, Vec::new(), "error", Some(detail))
                }
            },
        };

        devices_json.push(json!({
            "device_id": device_id,
            "name": entry.name,
            "room": entry.room,
            "report": entry.report.as_ref().map(report_json),
            "target": target_json,
            "latest": latest_json,
            "candidates": candidates,
            "state": state,
            "error": device_error,
        }));
    }

    Json(json!({
        "ok": true,
        "repository": FIRMWARE_REPOSITORY,
        // UI 会在此基础上拼 `/releases`，这里给仓库根地址而不是 release 页。
        "repository_url": "https://github.com/78/xiaozhi-esp32",
        "fetched_at": fetched_at,
        "stale": stale,
        "error": overview_error,
        "releases": releases.iter().map(release_json).collect::<Vec<_>>(),
        "devices": devices_json,
    }))
}

// ═══════════════════════════════════════════════════════════════════════════════
// 升级
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct UpgradeRequest {
    /// MAC / MQTT client_id / 设备目录里的名字。
    device: String,
    /// tag 或版本号，如 `v2.5.2` / `2.5.2`。
    version: String,
    /// 用户在候选列表里选择的 `full_name`；精确匹配时可省略。
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClearTargetRequest {
    device: String,
}

#[derive(Debug, Deserialize)]
pub struct ForgetReportRequest {
    device: String,
}

static UPGRADE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn upgrade_lock() -> &'static tokio::sync::Mutex<()> {
    UPGRADE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn resolve_device_mac(identity: &str) -> Option<String> {
    let identity = identity.trim();
    if identity.is_empty() {
        return None;
    }
    if let Some(directory) = device_directory()
        && let Some(device) = directory.find(identity)
    {
        return Some(device.mac.clone());
    }
    let mac = normalize_device_id(identity);
    if mac.is_empty() { None } else { Some(mac) }
}

fn normalize_tag(raw: &str) -> String {
    let raw = raw.trim();
    match raw.strip_prefix(['v', 'V']) {
        Some(rest) => format!("v{rest}"),
        None => format!("v{raw}"),
    }
}

fn valid_version_input(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

/// 先查缓存的 release 列表；找不到再按 tag 单独请求（覆盖列表之外的老版本）。
async fn resolve_release(
    service: &ReleaseService,
    raw_version: &str,
) -> Result<ReleaseInfo, ApiError> {
    let raw = raw_version.trim();
    if !valid_version_input(raw) {
        return Err(invalid_request(
            "version 只能是版本号或 tag（字母/数字/./-/_）",
        ));
    }
    let tag = normalize_tag(raw);
    let wanted_version = raw.trim_start_matches(['v', 'V']);
    if let Ok(page) = service.releases(false).await
        && let Some(release) = page.releases.into_iter().find(|release| {
            release.tag.eq_ignore_ascii_case(&tag)
                || release.version.eq_ignore_ascii_case(wanted_version)
        })
    {
        return Ok(release);
    }
    service.release_by_tag(&tag).await.map_err(map_github_error)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn sanitize_filename_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

fn firmware_file_name(version: &str, full_name: &str, sha256: &str) -> String {
    let version = version.trim_start_matches(['v', 'V']);
    let short = sha256.get(..12).unwrap_or(sha256);
    format!(
        "{}-{}-{}.bin",
        sanitize_filename_component(version),
        sanitize_filename_component(full_name),
        sanitize_filename_component(short)
    )
}

pub(crate) fn http_base(headers: &HeaderMap) -> String {
    let is_secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proto| {
            proto.eq_ignore_ascii_case("https") || proto.eq_ignore_ascii_case("wss")
        });
    let scheme = if is_secure { "https" } else { "http" };
    let host = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .or_else(|| headers.get("host").and_then(|value| value.to_str().ok()))
        .unwrap_or("localhost:9527");
    format!("{scheme}://{host}")
}

/// 执行下载 + 提取 + 落盘 + 设置目标。测试可直接传入 fake `ReleaseService`。
async fn upgrade_with_service(
    service: &ReleaseService,
    identity: &str,
    raw_version: &str,
    variant: Option<&str>,
    base_url: &str,
) -> Result<Value, ApiError> {
    if !firmware::is_installed() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "firmware_store_unavailable",
            "固件存储未安装，升级功能不可用",
        ));
    }
    let mac = resolve_device_mac(identity).ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, "device_unknown", "设备 ID 不能为空")
    })?;
    let report = firmware::report_for(&mac).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            "device_unknown",
            "还没有收到该设备的固件上报；请先重启设备，让它拉一次 OTA",
        )
    })?;

    let release = resolve_release(service, raw_version).await?;
    let manifest = match service.manifest(&release.tag).await {
        Ok(manifest) => Some(manifest),
        Err(GithubError::ManifestMissing(_)) => None,
        Err(err) => return Err(map_github_error(err)),
    };

    // 有 variant：用户已从候选列表里明确选择，按 full_name 取变体；
    // 没有 variant：先走精确匹配，匹配不上但有候选时返回 409 让用户选。
    let matched = match variant.map(str::trim).filter(|value| !value.is_empty()) {
        Some(selected) => {
            let Some(manifest) = manifest.as_ref() else {
                return Err(error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "variant_not_found",
                    "该 release 没有 release-manifest.json，无法按候选变体升级",
                ));
            };
            github::match_candidate(&release, manifest, selected, &report.chip_model)
                .map_err(map_github_error)?
        }
        None => match github::match_firmware(
            &release,
            manifest.as_ref(),
            &report.board_type,
            &report.board_name,
            &report.chip_model,
        ) {
            Ok(matched) => matched,
            Err(err) => {
                if matches!(
                    err,
                    GithubError::BoardNotFound { .. } | GithubError::BoardAmbiguous(_)
                ) && let Some(manifest) = manifest.as_ref()
                {
                    let candidates = github::candidate_firmwares(
                        &release,
                        manifest,
                        &report.board_type,
                        &report.board_name,
                        &report.chip_model,
                    );
                    if !candidates.is_empty() {
                        return Err(selection_required(candidates));
                    }
                }
                return Err(map_github_error(err));
            }
        },
    };

    // 先拿 zip 校验和；失败不阻塞（老 release 可能既没有 digest 也没有 SHA256SUMS）。
    let expected_zip_sha = match service.zip_sha256(&release.tag, &matched.asset).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(tag = %release.tag, error = %err, "获取固件 zip 校验和失败，跳过校验");
            None
        }
    };
    let zip = service
        .download(&matched.asset.download_url, MAX_ZIP_BYTES)
        .await
        .map_err(map_download_error)?;
    if let Some(expected) = expected_zip_sha {
        let actual = sha256_hex(&zip);
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                "checksum_mismatch",
                format!(
                    "固件 zip 校验失败：期望 {expected}，实际 {actual}（资产 {}）",
                    matched.asset.name
                ),
            ));
        }
    }

    let extracted =
        archive::extract_app_image(&zip, archive::MAX_APP_BYTES).map_err(map_archive_error)?;
    if let Some(partition_size) = report.ota_partition_size
        && extracted.bytes.len() as u64 > partition_size
    {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "firmware_too_large",
            format!(
                "固件 {} 字节超过设备 OTA 分区 {} 字节",
                extracted.bytes.len(),
                partition_size
            ),
        ));
    }
    if let Some(app_version) = extracted.version.as_deref()
        && !app_version.trim().is_empty()
        && !firmware::versions_equal(app_version, &matched.version)
    {
        tracing::warn!(
            tag = %release.tag,
            manifest_version = %matched.version,
            app_version = %app_version,
            "release manifest 版本与 app 镜像内版本不一致，以 manifest 为准"
        );
    }

    let app_sha = sha256_hex(&extracted.bytes);
    let file = firmware_file_name(&matched.version, &matched.variant.full_name, &app_sha);
    let size_bytes = extracted.bytes.len();
    firmware::store_firmware_file(&file, extracted.bytes)
        .await
        .map_err(map_store_error)?;

    let target = FirmwareTarget {
        version: matched.version.clone(),
        from_version: report.app_version.clone(),
        file: file.clone(),
        asset: matched.asset.name.clone(),
        sha256: app_sha.clone(),
        board_type: report.board_type.clone(),
        board_name: report.board_name.clone(),
        installed_at_ms: now_ms(),
        deliveries: 0,
        delivered: false,
    };
    firmware::set_firmware_target(&mac, target)
        .await
        .map_err(map_store_error)?;

    tracing::warn!(
        device_id = %mac,
        version = %matched.version,
        asset = %matched.asset.name,
        file = %file,
        "已设置小智固件升级目标（设备重启后生效）"
    );

    let base = base_url.trim_end_matches('/');
    Ok(json!({
        "ok": true,
        "target": {
            "version": matched.version,
            "asset": matched.asset.name,
            "file": file,
            "sha256": app_sha,
            "size_bytes": size_bytes,
            "file_url": format!("{base}/xiaozhi/firmware/{file}"),
        },
        "message": "已设置升级目标，重启设备后生效",
    }))
}

/// `POST /api/v1/xiaozhi/firmware/upgrade`
pub async fn upgrade_xiaozhi_firmware(
    headers: HeaderMap,
    Json(req): Json<UpgradeRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.device.trim().is_empty() {
        return Err(invalid_request("device 不能为空"));
    }
    let service = service()
        .map_err(|err| error_response(StatusCode::BAD_GATEWAY, "github_unavailable", err))?;
    let Ok(_guard) = upgrade_lock().try_lock() else {
        return Err(error_response(
            StatusCode::CONFLICT,
            "upgrade_in_progress",
            "已有固件升级任务正在进行，请稍后再试",
        ));
    };
    let base = http_base(&headers);
    let value = upgrade_with_service(
        service,
        &req.device,
        &req.version,
        req.variant.as_deref(),
        &base,
    )
    .await?;
    Ok(Json(value))
}

/// `POST /api/v1/xiaozhi/firmware/target/clear`
pub async fn clear_xiaozhi_firmware_target(
    Json(req): Json<ClearTargetRequest>,
) -> Result<Json<Value>, ApiError> {
    if !firmware::is_installed() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "firmware_store_unavailable",
            "固件存储未安装，升级功能不可用",
        ));
    }
    let mac = resolve_device_mac(&req.device).ok_or_else(|| invalid_request("device 不能为空"))?;
    firmware::clear_firmware_target(&mac)
        .await
        .map_err(map_store_error)?;
    tracing::warn!(device_id = %mac, "已清除小智固件升级目标");
    Ok(Json(json!({ "ok": true, "device": mac })))
}

/// `POST /api/v1/xiaozhi/firmware/report/forget`
///
/// 删除一台设备的上报与目标。用于清理 CD 自检/已废弃设备，避免它们一直
/// 出现在固件面板里。设备下次重启拉 OTA 会重新上报。
pub async fn forget_xiaozhi_firmware_report(
    Json(req): Json<ForgetReportRequest>,
) -> Result<Json<Value>, ApiError> {
    if !firmware::is_installed() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "firmware_store_unavailable",
            "固件存储未安装，升级功能不可用",
        ));
    }
    let mac = resolve_device_mac(&req.device).ok_or_else(|| invalid_request("device 不能为空"))?;
    let removed = firmware::forget_device(&mac)
        .await
        .map_err(map_store_error)?;
    tracing::warn!(device_id = %mac, removed, "清理小智设备固件上报");
    Ok(Json(
        json!({ "ok": true, "device": mac, "removed": removed }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tag_accepts_both_forms() {
        assert_eq!(normalize_tag("2.5.2"), "v2.5.2");
        assert_eq!(normalize_tag("v2.5.2"), "v2.5.2");
        assert_eq!(normalize_tag(" V2.5.2 "), "v2.5.2");
    }

    #[test]
    fn version_input_rejects_paths() {
        assert!(valid_version_input("2.5.2"));
        assert!(valid_version_input("v2.5.2-rc1"));
        assert!(!valid_version_input("../../etc/passwd"));
        assert!(!valid_version_input("2.5.2/evil"));
        assert!(!valid_version_input(""));
    }

    #[test]
    fn file_name_is_safe_and_version_has_no_v() {
        let name = firmware_file_name(
            "v2.5.2",
            "espressif/esp-box-3",
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        );
        assert_eq!(name, "2.5.2-espressif-esp-box-3-abcdef123456.bin");
        assert!(firmware::validate_blob_name(&name).is_ok());
    }

    #[test]
    fn update_state_maps_versions() {
        assert_eq!(update_state("2.5.1", "2.5.2").0, "update_available");
        assert_eq!(update_state("2.5.2", "2.5.2").0, "latest");
        assert_eq!(update_state("2.6.0", "2.5.2").0, "ahead");
        assert_eq!(update_state("dev", "2.5.2").0, "error");
    }

    #[test]
    fn overview_query_accepts_common_refresh_spellings() {
        for value in ["1", "true", "TRUE", "yes"] {
            assert!(
                OverviewQuery {
                    refresh: Some(value.to_string())
                }
                .wants_refresh()
            );
        }
        for value in ["0", "false", ""] {
            assert!(
                !OverviewQuery {
                    refresh: Some(value.to_string())
                }
                .wants_refresh()
            );
        }
        assert!(!OverviewQuery::default().wants_refresh());
    }

    fn vendor_release_and_manifest() -> (ReleaseInfo, ReleaseManifest) {
        let release = ReleaseInfo {
            tag: "v2.5.2".to_string(),
            version: "2.5.2".to_string(),
            name: "v2.5.2".to_string(),
            published_at: "2026-10-11T20:49:36Z".to_string(),
            prerelease: false,
            draft: false,
            html_url: "https://github.com/78/xiaozhi-esp32/releases/tag/v2.5.2".to_string(),
            assets: vec![github::ReleaseAsset {
                name: "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip".to_string(),
                download_url: "https://example.test/fw.zip".to_string(),
                size: 2_536_926,
                digest: None,
            }],
        };
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "2.5.2".to_string(),
            tag: "v2.5.2".to_string(),
            variants: vec![github::ManifestVariant {
                board: "spotpear/sp-esp32-s3-1.28-box".to_string(),
                name: "sp-esp32-s3-1.28-box".to_string(),
                full_name: "spotpear-sp-esp32-s3-1.28-box".to_string(),
                target: "esp32s3".to_string(),
            }],
        };
        (release, manifest)
    }

    #[test]
    fn vendor_board_status_uses_version_not_exact_match() {
        let (release, manifest) = vendor_release_and_manifest();
        let mut report = DeviceFirmwareReport {
            device_id: "90:70:69:f5:7a:04".to_string(),
            app_version: "2.5.2".to_string(),
            board_type: "sp-esp32-s3-1.28-box".to_string(),
            board_name: "sp-esp32-s3-1.28-box".to_string(),
            chip_model: "esp32s3".to_string(),
            ..DeviceFirmwareReport::default()
        };

        // 已刷到目标版本：虽然板型仍无法精确匹配，状态应是 latest，
        // 而不是一直 needs_selection。
        let (latest, candidates, state, error) = latest_state(&release, Some(&manifest), &report);
        assert_eq!(state, "latest");
        assert!(error.is_none());
        assert!(!candidates.is_empty());
        let latest = latest.expect("latest 视图");
        assert_eq!(latest["tag"], "v2.5.2");
        assert_eq!(latest["version"], "2.5.2");
        assert_eq!(latest["asset"], "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip");

        // 仍低于目标版本：需要用户选择候选。
        report.app_version = "2.4.2".to_string();
        let (latest, candidates, state, error) = latest_state(&release, Some(&manifest), &report);
        assert_eq!(state, "needs_selection");
        assert!(error.is_some());
        assert!(latest.is_some(), "候选状态也要给出默认版本");
        assert_eq!(candidates[0].full_name, "spotpear-sp-esp32-s3-1.28-box");
    }
}

/// 端到端集成测试：本地 fake GitHub + 合成 release zip，不访问外网。
#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::sync::Arc;

    use axum::Json;
    use axum::Router;
    use axum::routing::get;
    use sha2::{Digest, Sha256};

    use haimen_xiaozhi::firmware::{
        FirmwareStoreConfig, JsonFileMetadataStore, LocalFileBlobStore,
    };

    static STORE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    const TEST_VERSION: &str = "9.9.9";
    const TEST_BOARD: &str = "test-board";
    const TEST_TAG: &str = "v9.9.9";
    const APP_OFFSET: usize = 0x1_0000;
    const APP_SLOT_SIZE: u32 = 0x10_0000;

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// 构造 IDF 真实布局的 app 镜像：1 个段 + 16 字节对齐块末尾的 checksum + 尾部 SHA256。
    fn build_app_image(version: &str) -> Vec<u8> {
        let mut desc = vec![0u8; 256];
        desc[0..4].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
        desc[16..16 + version.len()].copy_from_slice(version.as_bytes());
        desc[48..55].copy_from_slice(b"xiaozhi");

        let mut image = vec![0u8; 24];
        image[0] = 0xE9;
        image[1] = 1;
        image[23] = 1; // hash_appended
        image.extend_from_slice(&0x3C00_0000u32.to_le_bytes());
        image.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        image.extend_from_slice(&desc);

        let checksum = desc.iter().fold(0xEFu8, |acc, byte| acc ^ byte);
        let unpadded = image.len();
        let aligned = (unpadded + 1).div_ceil(16) * 16;
        image.resize(aligned, 0);
        image[aligned - 1] = checksum;
        let digest = Sha256::digest(&image);
        image.extend_from_slice(&digest);
        image
    }

    /// 把 app 镜像放进 merged-bin 的 factory 分区。
    fn build_merged(app: &[u8]) -> Vec<u8> {
        let mut merged = vec![0xFFu8; APP_OFFSET];
        let mut entry = [0u8; 32];
        entry[0..2].copy_from_slice(&0xAA50u16.to_le_bytes());
        entry[2] = 0; // ESP_PARTITION_TYPE_APP
        entry[3] = 0x10; // factory
        entry[4..8].copy_from_slice(&(APP_OFFSET as u32).to_le_bytes());
        entry[8..12].copy_from_slice(&APP_SLOT_SIZE.to_le_bytes());
        entry[12..19].copy_from_slice(b"factory");
        merged[0x8000..0x8020].copy_from_slice(&entry);
        merged[0x8020..0x8040].fill(0xFF);
        merged.extend_from_slice(app);
        merged
    }

    /// 手工拼一个 stored ZIP（本模块不校验 CRC）。
    fn build_stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
        let name = name.as_bytes();
        let mut zip = Vec::new();
        zip.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // stored
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&(data.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(data.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(name);
        zip.extend_from_slice(data);

        let central_offset = zip.len();
        let mut central = Vec::new();
        central.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(name);
        zip.extend_from_slice(&central);

        zip.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&(central.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(central_offset as u32).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip
    }

    async fn install_temp_store(dir: &std::path::Path) {
        let cfg = FirmwareStoreConfig {
            metadata: Arc::new(JsonFileMetadataStore::new(dir.join("state.json"))),
            blobs: Arc::new(LocalFileBlobStore::new(dir.join("bin"))),
            gc_on_start: true,
        };
        firmware::install_firmware_store(cfg)
            .await
            .expect("安装临时固件存储");
    }

    /// 起一个只服务 release 列表 / manifest / zip 的本地 GitHub。
    async fn start_fake_github(zip: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        start_fake_github_with(zip, TEST_BOARD, TEST_BOARD, TEST_BOARD).await
    }

    /// 同上，但允许 manifest 的 board 路径 / 短名 / 资产 slug 不同，用于模拟厂商目录板。
    async fn start_fake_github_with(
        zip: Vec<u8>,
        board_path: &'static str,
        board_name: &'static str,
        full_name: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("绑定 fake GitHub");
        let addr = listener.local_addr().expect("fake GitHub 地址");
        let base = format!("http://{addr}");
        let zip_sha = sha256_hex(&zip);
        let asset_name = format!("{TEST_TAG}_{full_name}.zip");
        let release = json!({
            "tag_name": TEST_TAG,
            "name": TEST_TAG,
            "published_at": "2026-10-21T00:00:00Z",
            "prerelease": false,
            "draft": false,
            "html_url": format!("{base}/releases/tag/{TEST_TAG}"),
            "assets": [{
                "name": asset_name,
                "browser_download_url": format!("{base}/download/{asset_name}"),
                "size": zip.len(),
                "digest": format!("sha256:{zip_sha}"),
            }],
        });
        let manifest = json!({
            "schema_version": 1,
            "version": TEST_VERSION,
            "tag": TEST_TAG,
            "variants": [{
                "board": board_path,
                "name": board_name,
                "full_name": full_name,
                "target": "esp32s3",
            }],
        });

        let releases_body = json!([release.clone()]);
        let manifest_path =
            format!("/78/xiaozhi-esp32/releases/download/{TEST_TAG}/release-manifest.json");
        let download_path = format!("/download/{TEST_TAG}_{full_name}.zip");
        let tag_path = format!("/repos/78/xiaozhi-esp32/releases/tags/{TEST_TAG}");
        let app = Router::new()
            .route(
                "/repos/78/xiaozhi-esp32/releases",
                get({
                    let body = releases_body;
                    move || {
                        let body = body.clone();
                        async move { Json(body) }
                    }
                }),
            )
            .route(
                &tag_path,
                get({
                    let body = release;
                    move || {
                        let body = body.clone();
                        async move { Json(body) }
                    }
                }),
            )
            .route(
                &manifest_path,
                get({
                    let body = manifest;
                    move || {
                        let body = body.clone();
                        async move { Json(body) }
                    }
                }),
            )
            .route(
                &download_path,
                get({
                    move || {
                        let zip = zip.clone();
                        async move { zip }
                    }
                }),
            );

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, handle)
    }

    #[tokio::test]
    async fn upgrade_installs_app_image_and_survives_restart() {
        let _guard = STORE_LOCK.lock().await;
        let app = build_app_image(TEST_VERSION);
        let merged = build_merged(&app);
        let zip = build_stored_zip("merged-binary.bin", &merged);
        let (base, handle) = start_fake_github(zip).await;
        let service =
            ReleaseService::new(Some(base.clone()), Some(base.clone())).expect("创建 fake 服务");

        let dir = tempfile::tempdir().expect("tempdir");
        install_temp_store(dir.path()).await;

        let mac = "aa:bb:cc:dd:ee:99";
        firmware::record_device_report(DeviceFirmwareReport {
            device_id: mac.to_string(),
            app_name: "xiaozhi".to_string(),
            app_version: "9.9.8".to_string(),
            board_type: TEST_BOARD.to_string(),
            board_name: TEST_BOARD.to_string(),
            chip_model: "esp32s3".to_string(),
            mac: mac.to_string(),
            ota_partition_size: Some(APP_SLOT_SIZE as u64),
            reported_at_ms: 1,
        })
        .await;

        let result = upgrade_with_service(&service, mac, "9.9.9", None, "http://haimen:9527")
            .await
            .expect("升级应成功");
        assert_eq!(result["target"]["version"], TEST_VERSION);
        assert_eq!(result["target"]["size_bytes"], app.len());
        let file = result["target"]["file"].as_str().expect("file").to_string();
        assert!(file.ends_with(".bin"));

        let stored = firmware::load_firmware_file(&file)
            .await
            .expect("读 blob")
            .expect("blob 应存在");
        assert_eq!(stored, app, "落盘的必须是 app 分区镜像，而不是 merged-bin");
        assert_eq!(
            firmware::target_for(mac).map(|target| target.version),
            Some(TEST_VERSION.to_string())
        );

        let info = firmware::firmware_info_for(mac, "http://haimen:9527")
            .await
            .expect("应下发固件");
        assert_eq!(info.force, 1);
        assert_eq!(
            info.url,
            format!("http://haimen:9527/xiaozhi/firmware/{file}")
        );

        // 模拟 haimen 重启：同一目录重新安装，target 必须从 state.json 恢复。
        install_temp_store(dir.path()).await;
        assert_eq!(
            firmware::target_for(mac).map(|target| target.version),
            Some(TEST_VERSION.to_string()),
            "重启后目标应仍然存在"
        );

        // 设备刷完重启并上报新版本 → 目标自动清除。
        firmware::record_device_report(DeviceFirmwareReport {
            device_id: mac.to_string(),
            app_name: "xiaozhi".to_string(),
            app_version: TEST_VERSION.to_string(),
            board_type: TEST_BOARD.to_string(),
            board_name: TEST_BOARD.to_string(),
            chip_model: "esp32s3".to_string(),
            mac: mac.to_string(),
            ota_partition_size: Some(APP_SLOT_SIZE as u64),
            reported_at_ms: 2,
        })
        .await;
        assert!(firmware::target_for(mac).is_none(), "升级完成后应清除目标");

        handle.abort();
    }

    /// 厂商目录板：manifest board 带 `vendor/` 前缀，设备只报短 type。
    /// 精确匹配失败时必须返回候选，用户选择 full_name 后才能升级。
    #[tokio::test]
    async fn vendor_prefixed_board_requires_selection_then_upgrades() {
        let _guard = STORE_LOCK.lock().await;
        let app = build_app_image(TEST_VERSION);
        let merged = build_merged(&app);
        let zip = build_stored_zip("merged-binary.bin", &merged);
        let (base, handle) =
            start_fake_github_with(zip, "vendor/test-board", "test-board", "vendor-test-board")
                .await;
        let service =
            ReleaseService::new(Some(base.clone()), Some(base.clone())).expect("创建 fake 服务");

        let dir = tempfile::tempdir().expect("tempdir");
        install_temp_store(dir.path()).await;

        let mac = "aa:bb:cc:dd:ee:aa";
        firmware::record_device_report(DeviceFirmwareReport {
            device_id: mac.to_string(),
            app_name: "xiaozhi".to_string(),
            app_version: "9.9.8".to_string(),
            board_type: "test-board".to_string(),
            board_name: "test-board".to_string(),
            chip_model: "esp32s3".to_string(),
            mac: mac.to_string(),
            ota_partition_size: Some(APP_SLOT_SIZE as u64),
            reported_at_ms: 1,
        })
        .await;

        let (status, Json(body)) =
            upgrade_with_service(&service, mac, "9.9.9", None, "http://haimen:9527")
                .await
                .expect_err("未选择候选时应要求选择");
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "应返回 409 selection_required"
        );
        assert_eq!(body["error"], "selection_required");
        let candidates = body["candidates"].as_array().expect("候选数组");
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate["full_name"] == "vendor-test-board"),
            "应包含 vendor-test-board 候选: {body}"
        );
        assert!(
            firmware::target_for(mac).is_none(),
            "未选择候选时不应设置升级目标"
        );

        let result = upgrade_with_service(
            &service,
            mac,
            "9.9.9",
            Some("vendor-test-board"),
            "http://haimen:9527",
        )
        .await
        .expect("选择候选后应升级成功");
        assert_eq!(result["target"]["asset"], "v9.9.9_vendor-test-board.zip");
        assert_eq!(
            firmware::target_for(mac).map(|target| target.version),
            Some(TEST_VERSION.to_string())
        );

        handle.abort();
    }
}
