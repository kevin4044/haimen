//! 固件元数据/二进制存储的抽象与公共类型。
//!
//! # 为什么拆成两个 trait
//!
//! 元数据（设备上报、升级目标）是小而结构化的数据，未来很可能迁到 SQL；
//! 固件二进制是 2-10MB 的 blob，更适合文件系统或对象存储。把两者拆开以后：
//!
//! - 切 SQL 只需实现 [`FirmwareMetadataStore`]；
//! - 换对象存储只需实现 [`FirmwareBlobStore`]；
//! - OTA、Web API、UI 层都不需要改。
//!
//! 默认的文件实现见 [`crate::firmware::file_store`]。

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 状态文件 schema 版本。SQL 实现可以据此做迁移。
pub const FIRMWARE_STATE_SCHEMA_VERSION: u32 = 1;

/// 单个固件 blob 的大小上限（16MB）。上游 app 镜像约 2-4MB，留足余量。
pub const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;

/// 存储层错误。所有实现共用，调用方可以只按大类处理。
#[derive(Debug, Error)]
pub enum FirmwareStoreError {
    #[error("固件存储未安装")]
    NotInstalled,
    #[error("非法固件文件名: {0}")]
    InvalidName(String),
    #[error("固件超过大小上限（{} 字节）", MAX_BLOB_BYTES)]
    TooLarge,
    #[error("固件存储 I/O 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("固件状态序列化失败: {0}")]
    Codec(#[from] serde_json::Error),
    #[error("固件存储不可用: {0}")]
    Unavailable(String),
}

/// 校验固件 blob 名。
///
/// 只允许 `[A-Za-z0-9._-]`、必须以 `.bin` 结尾、不能以 `.` 开头、不能含 `..`。
/// 这样文件名既能直接拼进 URL，也能安全地 `dir.join(name)`，不会路径穿越。
pub fn validate_blob_name(name: &str) -> Result<(), FirmwareStoreError> {
    let invalid = || FirmwareStoreError::InvalidName(name.to_string());
    if name.is_empty() || name.len() > 160 {
        return Err(invalid());
    }
    if name.starts_with('.') || name.contains("..") {
        return Err(invalid());
    }
    if !name.ends_with(".bin") {
        return Err(invalid());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(invalid());
    }
    Ok(())
}

/// 设备在 OTA 请求里自报的固件/硬件信息。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFirmwareReport {
    /// 归一化后的冒号小写 MAC，作为存储主键。
    pub device_id: String,
    /// `application.name`
    #[serde(default)]
    pub app_name: String,
    /// `application.version`（设备当前固件版本，点分数字）
    #[serde(default)]
    pub app_version: String,
    /// `board.type`（对应 manifest 的 `board`）
    #[serde(default)]
    pub board_type: String,
    /// `board.name`（对应 manifest 的 `name`）
    #[serde(default)]
    pub board_name: String,
    /// `chip_model_name`（如 esp32s3）
    #[serde(default)]
    pub chip_model: String,
    /// 请求里原始的 `mac_address`，排查用
    #[serde(default)]
    pub mac: String,
    /// 设备上报的 OTA 分区大小；缺省表示没上报分区表
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ota_partition_size: Option<u64>,
    /// 上报时间（Unix 毫秒）
    #[serde(default)]
    pub reported_at_ms: i64,
}

/// 用户从 UI 指定的升级目标。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareTarget {
    /// 下发给设备的版本，必须是不带 `v` 的点分数字。
    pub version: String,
    /// 设置目标时设备当前版本。用于区分升级 / 降级 / 同版本重刷：
    /// - `from_version < version`：升级，上报版本 >= 目标视为成功；
    /// - `from_version > version`：降级，只有上报版本 == 目标才算成功，
    ///   设备仍停在旧版本时要继续重试；
    /// - `from_version == version`：同版本重刷，无法从版本判断是否刷成功，
    ///   按「一次性下发」处理。
    ///
    /// 旧状态文件没有该字段时为空串，按原来的升级语义兼容处理。
    #[serde(default)]
    pub from_version: String,
    /// 固件 blob 名（`FirmwareBlobStore` 的 key）。
    pub file: String,
    /// release 资产名，排查用。
    #[serde(default)]
    pub asset: String,
    /// 提取后 app 镜像的 sha256（hex）。
    #[serde(default)]
    pub sha256: String,
    /// 设置目标时设备的板型；设备换板/换机时据此自动清理。
    #[serde(default)]
    pub board_type: String,
    #[serde(default)]
    pub board_name: String,
    /// 设置时间（Unix 毫秒）
    #[serde(default)]
    pub installed_at_ms: i64,
    /// 已下发次数。达到上限后强制清理，避免设备侧版本串不匹配导致无限重刷。
    #[serde(default)]
    pub deliveries: u32,
    /// 是否至少下发过一次。未下发过时不能因为「上报版本已等于目标」而清除，
    /// 否则同版本重刷会在真正下发前被自己清掉。
    #[serde(default)]
    pub delivered: bool,
}

/// 一台设备的完整存储记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceFirmwareRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<DeviceFirmwareReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<FirmwareTarget>,
}

/// 存储层一次读写的全量快照。
///
/// 设备数量上限 256，整快照写入的代价可接受；SQL 实现可以在一个事务里
/// upsert 全部记录并删除已移除的行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareSnapshot {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceFirmwareRecord>,
}

impl Default for FirmwareSnapshot {
    fn default() -> Self {
        Self {
            schema_version: FIRMWARE_STATE_SCHEMA_VERSION,
            devices: BTreeMap::new(),
        }
    }
}

fn default_schema_version() -> u32 {
    FIRMWARE_STATE_SCHEMA_VERSION
}

/// 元数据存储。默认实现是 JSON 文件；未来可替换为 SQL。
#[async_trait::async_trait]
pub trait FirmwareMetadataStore: Send + Sync {
    /// 读取全量快照。文件不存在应返回 `FirmwareSnapshot::default()`。
    async fn load(&self) -> Result<FirmwareSnapshot, FirmwareStoreError>;

    /// 事务性保存全量快照。
    async fn save(&self, snapshot: &FirmwareSnapshot) -> Result<(), FirmwareStoreError>;
}

/// 固件二进制存储。默认实现是本地文件；未来可替换为对象存储/DB BLOB。
#[async_trait::async_trait]
pub trait FirmwareBlobStore: Send + Sync {
    async fn exists(&self, name: &str) -> Result<bool, FirmwareStoreError>;

    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), FirmwareStoreError>;

    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareStoreError>;

    async fn list(&self) -> Result<Vec<String>, FirmwareStoreError>;

    async fn remove(&self, name: &str) -> Result<(), FirmwareStoreError>;
}

/// 宽容解析点分数字版本：允许 `v` 前缀与 `-dev` 之类后缀。
///
/// 上游固件的版本比较只接受纯点分十进制；这里放宽一点，是为了让设备上报的
/// 自定义版本（如 `2.5.2-dev`）仍能参与「是否已到目标版本」的判断。
/// 解析失败返回 `None`，调用方不要据此下结论。
pub fn parse_version(raw: &str) -> Option<Vec<u64>> {
    let s = raw.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let numeric: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if numeric.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for part in numeric.trim_matches('.').split('.') {
        if part.is_empty() {
            return None;
        }
        parts.push(part.parse().ok()?);
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// 比较两个版本。缺失段按 0 处理；前缀相等时更长的版本更新（与上游一致，
/// 例如 `2.5.2.0` > `2.5.2`）。任一版本无法解析返回 `None`。
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let a = parse_version(a)?;
    let b = parse_version(b)?;
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(a.len().cmp(&b.len()))
}

/// 版本是否相等（都能解析时按数字比较，否则退回字符串比较）。
pub fn versions_equal(a: &str, b: &str) -> bool {
    match compare_versions(a, b) {
        Some(ordering) => ordering == Ordering::Equal,
        None => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_name_validation_blocks_traversal() {
        assert!(validate_blob_name("2.5.2-bread-compact-wifi-abcdef123456.bin").is_ok());
        for bad in [
            "",
            "../x.bin",
            "a/b.bin",
            "a\\b.bin",
            ".hidden.bin",
            "x..bin",
            "x.txt",
            "x bin.bin",
        ] {
            assert!(validate_blob_name(bad).is_err(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn version_parse_and_compare_follow_upstream() {
        assert_eq!(parse_version("2.5.2"), Some(vec![2, 5, 2]));
        assert_eq!(parse_version("v2.5.2"), Some(vec![2, 5, 2]));
        assert_eq!(parse_version("2.5.2-dev"), Some(vec![2, 5, 2]));
        assert_eq!(parse_version(" 2.5.2 "), Some(vec![2, 5, 2]));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("2..3"), None);

        assert_eq!(compare_versions("2.5.2", "2.5.1"), Some(Ordering::Greater));
        assert_eq!(compare_versions("2.5.10", "2.5.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("2.5.2", "2.5.2.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("v2.5.2", "2.5.2"), Some(Ordering::Equal));
        assert!(versions_equal("2.5.2-dev", "2.5.2"));
        assert!(!versions_equal("2.5.1", "2.5.2"));
    }

    #[test]
    fn snapshot_round_trips_json() {
        let mut snapshot = FirmwareSnapshot::default();
        snapshot.devices.insert(
            "84:c7:bb:59:93:64".to_string(),
            DeviceFirmwareRecord {
                report: Some(DeviceFirmwareReport {
                    device_id: "84:c7:bb:59:93:64".to_string(),
                    app_version: "2.5.1".to_string(),
                    ..DeviceFirmwareReport::default()
                }),
                target: Some(FirmwareTarget {
                    version: "2.5.2".to_string(),
                    file: "2.5.2-bread-compact-wifi-abcdef123456.bin".to_string(),
                    ..FirmwareTarget::default()
                }),
            },
        );
        let json = serde_json::to_string(&snapshot).expect("序列化");
        let back: FirmwareSnapshot = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(
            back.devices["84:c7:bb:59:93:64"]
                .target
                .as_ref()
                .expect("target")
                .version,
            "2.5.2"
        );
    }
}
