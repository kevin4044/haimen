//! 小智固件 OTA：设备上报、升级目标、固件 blob 与 OTA 下发。
//!
//! # 存储抽象
//!
//! - [`FirmwareMetadataStore`]：设备上报 + 升级目标（默认 JSON 文件，可换 SQL）
//! - [`FirmwareBlobStore`]：app 镜像字节（默认本地目录，可换对象存储）
//!
//! 本模块的 facade 只通过这两个 trait 访问存储，不直接碰文件系统。
//!
//! # 一次性目标语义
//!
//! 用户设置目标后，设备下一次拉 OTA 会拿到 `firmware{version,url,force:1}`。
//! 设备刷写成功、重启并上报目标版本（或更新版本）后，目标会自动清除，
//! 避免 `force=1` 让设备每次启动都重复刷机。另设 `MAX_DELIVERIES` 兜底：
//! 设备侧版本串对不上时最多下发 3 次。

mod file_store;
mod store;

use std::cmp::Ordering;
use std::collections::HashSet;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::types::FirmwareInfo;

pub use file_store::{JsonFileMetadataStore, LocalFileBlobStore};
pub use store::{
    DeviceFirmwareRecord, DeviceFirmwareReport, FIRMWARE_STATE_SCHEMA_VERSION, FirmwareBlobStore,
    FirmwareMetadataStore, FirmwareSnapshot, FirmwareStoreError, FirmwareTarget, MAX_BLOB_BYTES,
    compare_versions, parse_version, validate_blob_name, versions_equal,
};

/// 同一目标最多下发次数。设备应用升级后上报的版本串可能带自定义后缀，
/// 数字比较仍可能失败；达到上限后强制清除，避免无限重刷。
pub const MAX_DELIVERIES: u32 = 3;

/// 安装存储后端。默认文件实现的 `state_path` / `cache_dir` 由根 crate 传入
/// （位于 `~/.haimen/xiaozhi-firmware/`，随宿主机数据卷持久化）。
pub struct FirmwareStoreConfig {
    pub metadata: Arc<dyn FirmwareMetadataStore>,
    pub blobs: Arc<dyn FirmwareBlobStore>,
    /// 启动时删除未被任何 target 引用的 blob。默认实现建议 true。
    pub gc_on_start: bool,
}

struct FirmwareRuntime {
    metadata: Arc<dyn FirmwareMetadataStore>,
    blobs: Arc<dyn FirmwareBlobStore>,
    /// 内存快照：UI/OTA 的同步读都走它，避免每次 OTA 都查存储。
    snapshot: RwLock<FirmwareSnapshot>,
    /// 串行化「改快照 + 落盘」，保证写入顺序；不跨 await 持有 snapshot 锁。
    mutation: tokio::sync::Mutex<()>,
}

static RUNTIME: RwLock<Option<Arc<FirmwareRuntime>>> = RwLock::new(None);

fn runtime() -> Option<Arc<FirmwareRuntime>> {
    RUNTIME
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

/// 存储是否已安装。未安装时 OTA 仍正常（不下发 firmware），Web 端返回 503。
pub fn is_installed() -> bool {
    runtime().is_some()
}

/// 安装（覆盖式）存储后端并加载快照。
///
/// 加载失败/状态文件损坏时按空快照启动，保证 OTA 与 Web 服务不被存储问题拖垮。
pub async fn install_firmware_store(cfg: FirmwareStoreConfig) -> Result<(), FirmwareStoreError> {
    let FirmwareStoreConfig {
        metadata,
        blobs,
        gc_on_start,
    } = cfg;

    let snapshot = match metadata.load().await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!(error = %err, "固件状态加载失败，按空状态启动");
            FirmwareSnapshot::default()
        }
    };
    if snapshot.schema_version > FIRMWARE_STATE_SCHEMA_VERSION {
        tracing::warn!(
            got = snapshot.schema_version,
            supported = FIRMWARE_STATE_SCHEMA_VERSION,
            "固件状态 schema 版本高于当前程序，按兼容模式读取"
        );
    }

    if gc_on_start {
        match blobs.list().await {
            Ok(names) => {
                let referenced: HashSet<&str> = snapshot
                    .devices
                    .values()
                    .filter_map(|record| record.target.as_ref())
                    .map(|target| target.file.as_str())
                    .collect();
                for name in names {
                    if referenced.contains(name.as_str()) {
                        continue;
                    }
                    match blobs.remove(&name).await {
                        Ok(()) => tracing::warn!(file = %name, "清理未被引用的固件文件"),
                        Err(err) => {
                            tracing::warn!(file = %name, error = %err, "清理固件文件失败")
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "固件目录扫描失败，跳过启动 GC"),
        }
    }

    let rt = Arc::new(FirmwareRuntime {
        metadata,
        blobs,
        snapshot: RwLock::new(snapshot),
        mutation: tokio::sync::Mutex::new(()),
    });
    *RUNTIME.write().unwrap_or_else(|err| err.into_inner()) = Some(rt);
    Ok(())
}

/// 记录设备上报，并在满足条件时清除已完成的升级目标。
///
/// 清除条件（任一）：
/// - 已下发过，且上报版本 >= 目标版本（升级/降级成功）；
/// - 板型与设置目标时不一致（设备换板/换机）；
/// - 下发次数达到 [`MAX_DELIVERIES`]。
pub async fn record_device_report(report: DeviceFirmwareReport) {
    let Some(rt) = runtime() else {
        return;
    };
    let _guard = rt.mutation.lock().await;
    let mut candidate = read_snapshot(&rt);
    let device_id = report.device_id.clone();
    let entry = candidate.devices.entry(device_id.clone()).or_default();

    let mut clear_reason = None;
    if let Some(target) = entry.target.as_ref() {
        if !report.board_type.is_empty()
            && !target.board_type.is_empty()
            && !target.board_type.eq_ignore_ascii_case(&report.board_type)
        {
            clear_reason = Some("board_type_changed");
        } else if !report.board_name.is_empty()
            && !target.board_name.is_empty()
            && !target.board_name.eq_ignore_ascii_case(&report.board_name)
        {
            clear_reason = Some("board_name_changed");
        } else if target.delivered && target_reached(&report.app_version, target) {
            clear_reason = Some("version_reached");
        } else if target.deliveries >= MAX_DELIVERIES {
            clear_reason = Some("delivery_limit");
        }
    }
    if let Some(reason) = clear_reason {
        entry.target = None;
        tracing::warn!(device_id = %device_id, reason, "清除已完成的固件升级目标");
    }
    entry.report = Some(report);

    write_snapshot(&rt, candidate.clone());
    if let Err(err) = rt.metadata.save(&candidate).await {
        tracing::warn!(error = %err, "保存设备固件上报失败（内存已更新）");
    }
}

/// 为一次 OTA 响应取固件段。
///
/// 只有目标存在且固件文件实际可读时才下发；每次下发会增加 `deliveries`。
/// 返回的 `force` 固定为数字 `1`（上游固件只认数字，`true` 会被忽略）。
pub async fn firmware_info_for(device_id: &str, base_url: &str) -> Option<FirmwareInfo> {
    let rt = runtime()?;
    let target = {
        let snapshot = rt.snapshot.read().unwrap_or_else(|err| err.into_inner());
        snapshot.devices.get(device_id)?.target.clone()?
    };

    if target.deliveries >= MAX_DELIVERIES {
        tracing::warn!(
            device_id = %device_id,
            file = %target.file,
            "固件目标下发次数达到上限，清除目标"
        );
        let _ = clear_firmware_target(device_id).await;
        return None;
    }

    match rt.blobs.exists(&target.file).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                device_id = %device_id,
                file = %target.file,
                "升级目标文件不存在，本次不下发固件"
            );
            return None;
        }
        Err(err) => {
            tracing::warn!(device_id = %device_id, error = %err, "检查固件文件失败");
            return None;
        }
    }

    let _guard = rt.mutation.lock().await;
    let mut candidate = read_snapshot(&rt);
    let record = candidate.devices.get_mut(device_id)?;
    let target = record.target.as_mut()?;
    target.deliveries = target.deliveries.saturating_add(1);
    target.delivered = true;
    let version = target.version.clone();
    let file = target.file.clone();
    // 同版本重刷无法从版本号判断设备是否真的刷写成功（前后都是同一版本）。
    // 采用一次性下发：这次 OTA 响应带出固件 URL 后立即清目标，避免
    // 「下载失败 → 下次上报被当成成功清目标」和「刷成功后每次启动都重刷」。
    // 设备下载失败时用户重新在 UI 里设置一次即可。
    let one_shot_reflash = !target.from_version.trim().is_empty()
        && versions_equal(&target.from_version, &target.version);
    if one_shot_reflash {
        record.target = None;
        tracing::warn!(
            device_id = %device_id,
            version = %version,
            "同版本重刷目标已一次性下发，清除目标"
        );
    }
    write_snapshot(&rt, candidate.clone());
    if let Err(err) = rt.metadata.save(&candidate).await {
        tracing::warn!(error = %err, "保存固件下发计数失败（内存已更新）");
    }

    let base = base_url.trim_end_matches('/');
    Some(FirmwareInfo {
        version,
        url: format!("{base}/xiaozhi/firmware/{file}"),
        force: 1,
    })
}

/// 设置升级目标（强一致：落盘失败则不提交内存，返回错误）。
pub async fn set_firmware_target(
    device_id: &str,
    target: FirmwareTarget,
) -> Result<(), FirmwareStoreError> {
    let rt = runtime().ok_or(FirmwareStoreError::NotInstalled)?;
    let _guard = rt.mutation.lock().await;
    let mut candidate = read_snapshot(&rt);
    candidate
        .devices
        .entry(device_id.to_string())
        .or_default()
        .target = Some(target);
    rt.metadata.save(&candidate).await?;
    write_snapshot(&rt, candidate);
    Ok(())
}

/// 清除升级目标（强一致）。
pub async fn clear_firmware_target(device_id: &str) -> Result<(), FirmwareStoreError> {
    let rt = runtime().ok_or(FirmwareStoreError::NotInstalled)?;
    let _guard = rt.mutation.lock().await;
    let mut candidate = read_snapshot(&rt);
    let Some(record) = candidate.devices.get_mut(device_id) else {
        return Ok(());
    };
    if record.target.take().is_none() {
        return Ok(());
    }
    rt.metadata.save(&candidate).await?;
    write_snapshot(&rt, candidate);
    Ok(())
}

/// 删除一台设备的上报与目标，返回是否真的删除了记录。
///
/// 用于清理 CD 自检/已废弃设备，避免它们一直出现在固件面板里。
/// 强一致：落盘失败时不提交内存。
pub async fn forget_device(device_id: &str) -> Result<bool, FirmwareStoreError> {
    let rt = runtime().ok_or(FirmwareStoreError::NotInstalled)?;
    let _guard = rt.mutation.lock().await;
    let mut candidate = read_snapshot(&rt);
    if candidate.devices.remove(device_id).is_none() {
        return Ok(false);
    }
    rt.metadata.save(&candidate).await?;
    write_snapshot(&rt, candidate);
    Ok(true)
}

/// 写入一个固件 blob。
pub async fn store_firmware_file(name: &str, bytes: Vec<u8>) -> Result<(), FirmwareStoreError> {
    validate_blob_name(name)?;
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(FirmwareStoreError::TooLarge);
    }
    let rt = runtime().ok_or(FirmwareStoreError::NotInstalled)?;
    rt.blobs.put(name, bytes).await
}

/// 读取一个固件 blob（设备下载路由用）。
pub async fn load_firmware_file(name: &str) -> Result<Option<Vec<u8>>, FirmwareStoreError> {
    validate_blob_name(name)?;
    let rt = runtime().ok_or(FirmwareStoreError::NotInstalled)?;
    rt.blobs.get(name).await
}

/// 固件 blob 是否存在。
pub async fn firmware_file_exists(name: &str) -> bool {
    let Some(rt) = runtime() else {
        return false;
    };
    match rt.blobs.exists(name).await {
        Ok(exists) => exists,
        Err(err) => {
            tracing::warn!(file = %name, error = %err, "检查固件文件失败");
            false
        }
    }
}

/// 单台设备的最近一次上报（同步内存读）。
pub fn report_for(device_id: &str) -> Option<DeviceFirmwareReport> {
    let rt = runtime()?;
    let snapshot = rt.snapshot.read().unwrap_or_else(|err| err.into_inner());
    snapshot.devices.get(device_id)?.report.clone()
}

/// 所有设备上报（同步内存读，按 MAC 排序）。
pub fn reports() -> Vec<DeviceFirmwareReport> {
    let Some(rt) = runtime() else {
        return Vec::new();
    };
    let snapshot = rt.snapshot.read().unwrap_or_else(|err| err.into_inner());
    snapshot
        .devices
        .values()
        .filter_map(|record| record.report.clone())
        .collect()
}

/// 单台设备的升级目标（同步内存读）。
pub fn target_for(device_id: &str) -> Option<FirmwareTarget> {
    let rt = runtime()?;
    let snapshot = rt.snapshot.read().unwrap_or_else(|err| err.into_inner());
    snapshot.devices.get(device_id)?.target.clone()
}

/// 所有升级目标（同步内存读，按 MAC 排序）。
pub fn targets() -> Vec<(String, FirmwareTarget)> {
    let Some(rt) = runtime() else {
        return Vec::new();
    };
    let snapshot = rt.snapshot.read().unwrap_or_else(|err| err.into_inner());
    snapshot
        .devices
        .iter()
        .filter_map(|(device_id, record)| {
            record
                .target
                .clone()
                .map(|target| (device_id.clone(), target))
        })
        .collect()
}

fn read_snapshot(rt: &FirmwareRuntime) -> FirmwareSnapshot {
    rt.snapshot
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn write_snapshot(rt: &FirmwareRuntime, snapshot: FirmwareSnapshot) {
    *rt.snapshot.write().unwrap_or_else(|err| err.into_inner()) = snapshot;
}

/// 设备上报的当前版本是否证明目标已经刷写成功。
///
/// 不能简单用 `current >= target`：
/// - 升级（from < target）：上报 >= target 才算成功；
/// - 降级（from > target）：设备失败时会继续上报旧版本（> target），
///   只有上报 == target 才算成功，否则要继续重试；
/// - 同版本重刷（from == target）：版本号无法证明是否成功，改由
///   [`firmware_info_for`] 一次性下发后清目标，本函数不参与。
/// - 旧状态文件没有 `from_version`（空串）时保持原升级语义，兼容存量目标。
fn target_reached(current: &str, target: &FirmwareTarget) -> bool {
    let target_version = target.version.trim();
    if target_version.is_empty() {
        return false;
    }
    let Some(ordering) = compare_versions(current, target_version) else {
        // 版本串无法解析时只做字符串兜底，避免误清目标。
        return versions_equal(current, target_version);
    };
    match ordering {
        Ordering::Equal => true,
        Ordering::Greater => {
            // 只有升级目标才把 current > target 当成成功；降级目标下
            // current > target 说明设备还在旧版本，必须继续重试。
            if target.from_version.trim().is_empty() {
                return true;
            }
            compare_versions(&target.from_version, target_version)
                .is_some_and(|from_order| from_order == Ordering::Less)
        }
        Ordering::Less => false,
    }
}

/// `GET /xiaozhi/firmware/{file}`：设备公开下载固件。
///
/// 仅允许 [`validate_blob_name`] 通过的文件名，且只从已安装的 Blob 层读取。
/// 显式设置 Content-Length：上游 `Ota::Upgrade` 用 `GetBodyLength()` 判断
/// 总大小，缺这个头会直接失败。
pub async fn handle_firmware_file(Path(file): Path<String>) -> Response {
    if let Err(err) = validate_blob_name(&file) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "ok": false,
                "error": "invalid_firmware_name",
                "detail": err.to_string(),
            })),
        )
            .into_response();
    }
    if !is_installed() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "ok": false,
                "error": "firmware_store_unavailable",
                "detail": "固件存储未安装",
            })),
        )
            .into_response();
    }
    match load_firmware_file(&file).await {
        Ok(Some(bytes)) => {
            let len = bytes.len();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, len)
                .header(header::ETAG, format!("\"{file}\""))
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!(file = %file, error = %err, "读取固件文件失败");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// 测试用全局锁：`install_firmware_store` 是进程级覆盖式安装，所有改全局
/// 固件状态的测试（含 ota.rs）都必须持有同一把锁。
#[cfg(test)]
pub(crate) fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    #[derive(Default)]
    struct MemMeta {
        snapshot: StdMutex<FirmwareSnapshot>,
    }

    #[async_trait]
    impl FirmwareMetadataStore for MemMeta {
        async fn load(&self) -> Result<FirmwareSnapshot, FirmwareStoreError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }

        async fn save(&self, snapshot: &FirmwareSnapshot) -> Result<(), FirmwareStoreError> {
            *self.snapshot.lock().unwrap() = snapshot.clone();
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemBlobs {
        files: StdMutex<HashMap<String, Vec<u8>>>,
    }

    #[async_trait]
    impl FirmwareBlobStore for MemBlobs {
        async fn exists(&self, name: &str) -> Result<bool, FirmwareStoreError> {
            validate_blob_name(name)?;
            Ok(self.files.lock().unwrap().contains_key(name))
        }

        async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), FirmwareStoreError> {
            validate_blob_name(name)?;
            self.files.lock().unwrap().insert(name.to_string(), bytes);
            Ok(())
        }

        async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareStoreError> {
            validate_blob_name(name)?;
            Ok(self.files.lock().unwrap().get(name).cloned())
        }

        async fn list(&self) -> Result<Vec<String>, FirmwareStoreError> {
            Ok(self.files.lock().unwrap().keys().cloned().collect())
        }

        async fn remove(&self, name: &str) -> Result<(), FirmwareStoreError> {
            self.files.lock().unwrap().remove(name);
            Ok(())
        }
    }

    async fn install_mem() {
        let rt = FirmwareStoreConfig {
            metadata: Arc::new(MemMeta::default()),
            blobs: Arc::new(MemBlobs::default()),
            gc_on_start: false,
        };
        install_firmware_store(rt).await.expect("安装内存存储");
    }

    fn report(mac: &str, version: &str, board: &str) -> DeviceFirmwareReport {
        DeviceFirmwareReport {
            device_id: mac.to_string(),
            app_name: "xiaozhi".to_string(),
            app_version: version.to_string(),
            board_type: board.to_string(),
            board_name: board.to_string(),
            chip_model: "esp32s3".to_string(),
            mac: mac.to_string(),
            ota_partition_size: Some(4 * 1024 * 1024),
            reported_at_ms: 1,
        }
    }

    fn target(version: &str, board: &str, file: &str) -> FirmwareTarget {
        FirmwareTarget {
            version: version.to_string(),
            from_version: String::new(),
            file: file.to_string(),
            asset: format!("v{version}_{board}.zip"),
            sha256: "deadbeef".to_string(),
            board_type: board.to_string(),
            board_name: board.to_string(),
            installed_at_ms: 1,
            deliveries: 0,
            delivered: false,
        }
    }

    #[tokio::test]
    async fn target_is_served_with_numeric_force_then_cleared() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:01";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1, 2, 3])
            .await
            .expect("写固件");
        let mut upgrade = target("2.5.2", "bread-compact-wifi", file);
        upgrade.from_version = "2.5.1".to_string();
        set_firmware_target(mac, upgrade).await.expect("设置目标");

        record_device_report(report(mac, "2.5.1", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_some(), "旧版本不应清除目标");

        let info = firmware_info_for(mac, "http://192.168.1.10:9527/")
            .await
            .expect("应下发固件");
        assert_eq!(info.version, "2.5.2");
        assert_eq!(info.force, 1);
        assert_eq!(
            info.url,
            "http://192.168.1.10:9527/xiaozhi/firmware/2.5.2-bread-compact-wifi-abcdef123456.bin"
        );

        record_device_report(report(mac, "2.5.2", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_none(), "升级完成后应自动清除目标");
    }

    #[tokio::test]
    async fn same_version_reflash_is_one_shot() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:02";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1]).await.expect("写固件");
        let mut reflash = target("2.5.2", "bread-compact-wifi", file);
        reflash.from_version = "2.5.2".to_string();
        set_firmware_target(mac, reflash).await.expect("设置目标");

        // 同版本重刷：首次上报不能清目标，否则永远下发不出去。
        record_device_report(report(mac, "2.5.2", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_some());
        assert!(
            firmware_info_for(mac, "http://h").await.is_some(),
            "应下发一次"
        );
        assert!(
            target_for(mac).is_none(),
            "同版本重刷无法从版本判断成功，按一次性下发后清除"
        );
    }

    #[tokio::test]
    async fn downgrade_target_retries_until_device_reports_target() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:06";
        let file = "2.4.0-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1]).await.expect("写固件");
        let mut downgrade = target("2.4.0", "bread-compact-wifi", file);
        downgrade.from_version = "2.5.2".to_string();
        set_firmware_target(mac, downgrade).await.expect("设置目标");

        record_device_report(report(mac, "2.5.2", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_some(), "下发前不能清目标");
        assert!(
            firmware_info_for(mac, "http://h").await.is_some(),
            "第一次下发"
        );

        // 设备下载失败/未刷成功，重启后仍上报旧版本：必须继续重试，
        // 不能因为 current(2.5.2) >= target(2.4.0) 被误判为成功。
        record_device_report(report(mac, "2.5.2", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_some(), "降级失败后目标应保留重试");
        assert!(
            firmware_info_for(mac, "http://h").await.is_some(),
            "失败后应再次下发"
        );

        // 真正降到目标版本后才清除。
        record_device_report(report(mac, "2.4.0", "bread-compact-wifi")).await;
        assert!(target_for(mac).is_none(), "降级成功应清除目标");
    }

    #[tokio::test]
    async fn delivery_limit_clears_target() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:03";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1]).await.expect("写固件");
        set_firmware_target(mac, target("2.5.2", "bread-compact-wifi", file))
            .await
            .expect("设置目标");

        for _ in 0..MAX_DELIVERIES {
            assert!(firmware_info_for(mac, "http://h").await.is_some());
        }
        assert!(
            firmware_info_for(mac, "http://h").await.is_none(),
            "达到上限后不再下发"
        );
        assert!(target_for(mac).is_none(), "达到上限后应清除目标");
    }

    #[tokio::test]
    async fn board_change_clears_target() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:04";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1]).await.expect("写固件");
        set_firmware_target(mac, target("2.5.2", "bread-compact-wifi", file))
            .await
            .expect("设置目标");
        record_device_report(report(mac, "2.5.1", "espressif/esp-box-3")).await;
        assert!(target_for(mac).is_none(), "板型变化应清除目标");
    }

    #[tokio::test]
    async fn forget_device_removes_report_and_target() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:07";
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![1]).await.expect("写固件");
        record_device_report(report(mac, "2.5.1", "bread-compact-wifi")).await;
        set_firmware_target(mac, target("2.5.2", "bread-compact-wifi", file))
            .await
            .expect("设置目标");
        assert!(report_for(mac).is_some());
        assert!(target_for(mac).is_some());

        assert!(forget_device(mac).await.expect("删除设备"));
        assert!(report_for(mac).is_none(), "上报应被删除");
        assert!(target_for(mac).is_none(), "目标应被删除");
        assert!(
            !forget_device(mac).await.expect("重复删除"),
            "第二次应为 no-op"
        );
    }

    #[tokio::test]
    async fn missing_file_does_not_advertise_firmware() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let mac = "aa:bb:cc:dd:ee:05";
        set_firmware_target(
            mac,
            target(
                "2.5.2",
                "bread-compact-wifi",
                "2.5.2-bread-compact-wifi-ffffffffffff.bin",
            ),
        )
        .await
        .expect("设置目标");
        assert!(firmware_info_for(mac, "http://h").await.is_none());
    }

    #[tokio::test]
    async fn serving_reads_blob_and_sets_content_length() {
        let _lock = test_lock().lock().await;
        install_mem().await;
        let file = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        store_firmware_file(file, vec![0xE9, 1, 2, 3])
            .await
            .expect("写固件");
        let response = handle_firmware_file(Path(file.to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("4")
        );
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .expect("读 body");
        assert_eq!(&body[..], &[0xE9, 1, 2, 3]);
    }
}
