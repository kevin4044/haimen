//! 从 78/xiaozhi-esp32 的 release ZIP 中提取设备 OTA 可用的 ESP app 镜像。
//!
//! release 资产通常把整片 flash 合并成一个 `merged-binary.bin`：0x8000 处是
//! 分区表，随后是 bootloader、app、NVS 等各分区镜像。设备 OTA 只能写 app
//! 分区镜像，因此本模块负责：
//!
//! 1. 用极简只读 ZIP 解析器解出 ZIP 中的 `.bin`（`flate2` 解 deflate）；
//! 2. 在合并镜像的分区表中找到 app 分区，也可直接处理 app-only 镜像；
//! 3. 解析并校验 ESP image（checksum / 可选 SHA256），裁出完整 app 镜像；
//! 4. 从 `esp_app_desc_t` 中读取版本与项目名。
//!
//! 本模块是纯函数、无网络，方便单元测试。

use std::io::Read;

use flate2::read::DeflateDecoder;
use sha2::{Digest, Sha256};

/// app 镜像大小上限（16 MiB）。
pub const MAX_APP_BYTES: usize = 16 * 1024 * 1024;

/// assets 分区镜像大小上限（16 MiB）。
pub const MAX_ASSETS_BYTES: usize = 16 * 1024 * 1024;

/// 解压后的固件条目上限（32 MiB）。
///
/// release zip 内是整片 flash 的 merged-bin（含 bootloader/分区表/app/assets），
/// 可能明显大于 app 镜像本身；这里单独放宽，最终 app 镜像仍受 `MAX_APP_BYTES`
/// 约束。上限存在的意义是防 zip bomb。
const MAX_MERGED_BYTES: usize = 32 * 1024 * 1024;

/// ZIP EOCD 结束记录签名（`PK\x05\x06`）。
const EOCD_SIGNATURE: u32 = 0x0605_4b50;
/// ZIP central directory 条目签名（`PK\x01\x02`）。
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
/// ZIP local file header 签名（`PK\x03\x04`）。
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
/// 不含注释的 EOCD 最小长度。
const EOCD_MIN_LEN: usize = 22;
/// ZIP 注释最大长度（u16）。
const MAX_ZIP_COMMENT: usize = u16::MAX as usize;
/// 在文件末尾多大范围内搜索 EOCD（注释最大长度 + EOCD 自身）。
const EOCD_SEARCH_LEN: usize = MAX_ZIP_COMMENT + EOCD_MIN_LEN;
/// ZIP64 的 u16 占位值。
const ZIP64_U16: u16 = u16::MAX;
/// ZIP64 的 u32 占位值。
const ZIP64_U32: u32 = u32::MAX;

/// 合并镜像中分区表的固定偏移。
const PARTITION_TABLE_OFFSET: usize = 0x8000;
/// 分区表条目大小。
const PARTITION_ENTRY_SIZE: usize = 32;
/// 分区表最多条目数（0xC00 字节 / 32）。
const MAX_PARTITION_ENTRIES: usize = 0xC00 / PARTITION_ENTRY_SIZE;
/// 分区表 magic（真实固件中字节序为 `AA 50`）。
const PARTITION_MAGIC: [u8; 2] = [0xAA, 0x50];
/// 分区类型 app。
const PARTITION_TYPE_APP: u8 = 0;
/// app 分区子类型：factory（部分文档/实现里写作 0x10）。
const PARTITION_SUBTYPE_FACTORY: u8 = 0x00;
/// app 分区子类型：按任务约定的 factory（真实 ESP-IDF 中为 ota_0）。
const PARTITION_SUBTYPE_FACTORY_ALT: u8 = 0x10;
/// app 分区子类型：ota_0 回退（真实 ESP-IDF 中为 ota_1）。
const PARTITION_SUBTYPE_OTA_0: u8 = 0x11;

/// ESP image header 长度。
const IMAGE_HEADER_SIZE: usize = 24;
/// ESP image magic。
const ESP_IMAGE_MAGIC: u8 = 0xE9;
/// ESP image 最大段数。
const MAX_IMAGE_SEGMENTS: u8 = 16;
/// ESP image checksum 初值。
const ESP_CHECKSUM_INITIAL: u8 = 0xEF;
/// SHA256 长度。
const SHA256_LEN: usize = 32;
/// `esp_app_desc_t` 位于 image 起点之后的偏移（正好是第一个段的数据起点）。
const APP_DESC_OFFSET: usize = 32;
/// `esp_app_desc_t` magic。
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// 读取 version/project 需要的最小描述符长度（version 结束 + project 长度）。
const APP_DESC_MIN_LEN: usize = 80;

/// app 镜像提取失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("ZIP 结构不合法: {0}")]
    InvalidZip(String),
    #[error("ZIP 中没有可用的 .bin 固件")]
    NoFirmwareEntry,
    #[error("不支持的压缩方式: {0}")]
    UnsupportedCompression(u16),
    #[error("不支持 ZIP64")]
    Zip64Unsupported,
    #[error("固件超过大小上限")]
    TooLarge,
    #[error("合并固件中找不到应用分区: {0}")]
    PartitionNotFound(String),
    #[error("不是有效的 ESP 应用镜像: {0}")]
    InvalidImage(String),
    #[error("应用镜像校验失败: {0}")]
    Integrity(String),
    #[error("合并固件中找不到 assets 分区: {0}")]
    AssetsNotFound(String),
    #[error("不是有效的 assets 镜像: {0}")]
    InvalidAssets(String),
}

/// 提取出的 app 镜像及其元数据。
#[derive(Debug, Clone)]
pub struct ExtractedApp {
    /// 完整 app 镜像（含 checksum 对齐填充与可选的尾部 SHA256）。
    pub bytes: Vec<u8>,
    /// `esp_app_desc_t.version`，空值时为 `None`。
    pub version: Option<String>,
    /// `esp_app_desc_t.project_name`，空值时为 `None`。
    pub project: Option<String>,
}

/// 从 release ZIP 中提取 app 镜像。
///
/// `max_app_bytes` 约束最终 app 镜像大小；解压后的 merged-bin 另按
/// [`MAX_MERGED_BYTES`] 封顶（防止 zip bomb）。
pub fn extract_app_image(
    zip_bytes: &[u8],
    max_app_bytes: usize,
) -> Result<ExtractedApp, ArchiveError> {
    let entries = parse_zip_entries(zip_bytes)?;
    let entry = select_firmware_entry(&entries)?;
    let firmware = read_entry(zip_bytes, entry, MAX_MERGED_BYTES)?;
    extract_from_firmware(&firmware, max_app_bytes)
}

/// 从 release ZIP 的 merged-bin 中提取 assets 分区镜像。
///
/// 主题下发会整体覆盖 assets 分区；为了保住唤醒词 `srmodels.bin` 和
/// `common` 中文字体，haimen 在 apply 前从官方固件包取一份 factory assets
/// 作为基础资源（见 `xiaozhi_firmware::factory_assets_for_device`）。
pub fn extract_assets_image(
    zip_bytes: &[u8],
    max_assets_bytes: usize,
) -> Result<Vec<u8>, ArchiveError> {
    let entries = parse_zip_entries(zip_bytes)?;
    let entry = select_firmware_entry(&entries)?;
    let firmware = read_entry(zip_bytes, entry, MAX_MERGED_BYTES)?;
    extract_assets_from_firmware(&firmware, max_assets_bytes)
}

fn extract_assets_from_firmware(
    firmware: &[u8],
    max_assets_bytes: usize,
) -> Result<Vec<u8>, ArchiveError> {
    let partition = find_assets_partition(firmware)?;
    if partition.offset >= firmware.len() {
        return Err(ArchiveError::InvalidAssets(format!(
            "assets 分区 offset 0x{:X} 超出固件长度 {}",
            partition.offset,
            firmware.len()
        )));
    }
    let end = partition
        .offset
        .saturating_add(partition.size)
        .min(firmware.len());
    let slice = &firmware[partition.offset..end];
    if slice.len() < 12 {
        return Err(ArchiveError::InvalidAssets(
            "assets 分区不足 12 字节".into(),
        ));
    }
    let total_files = u32_le(slice, 0).unwrap_or(0);
    let combined_len = u32_le(slice, 8).unwrap_or(0) as usize;
    if total_files == 0 || combined_len == 0 {
        return Err(ArchiveError::InvalidAssets("assets 头文件为空".into()));
    }
    let total_len = 12usize
        .checked_add(combined_len)
        .ok_or_else(|| ArchiveError::InvalidAssets("assets 长度溢出".into()))?;
    if total_len > slice.len() || total_len > max_assets_bytes {
        return Err(ArchiveError::InvalidAssets(format!(
            "assets 镜像 {total_len} 字节超过分区/上限"
        )));
    }
    Ok(slice[..total_len].to_vec())
}

fn find_assets_partition(firmware: &[u8]) -> Result<AppPartition, ArchiveError> {
    let table_start = PARTITION_TABLE_OFFSET;
    if firmware.len() < table_start + PARTITION_ENTRY_SIZE {
        return Err(ArchiveError::AssetsNotFound(
            "固件不足 0x8000 字节，无法读取分区表".into(),
        ));
    }
    let table_end =
        (table_start + MAX_PARTITION_ENTRIES * PARTITION_ENTRY_SIZE).min(firmware.len());
    let mut saw_magic = false;
    let mut cursor = table_start;
    while cursor + PARTITION_ENTRY_SIZE <= table_end {
        if !partition_magic_ok(firmware, cursor) {
            break;
        }
        saw_magic = true;
        let partition_type = firmware[cursor + 2];
        let subtype = firmware[cursor + 3];
        let offset = u32_le(firmware, cursor + 4).unwrap_or(0) as usize;
        let size = u32_le(firmware, cursor + 8).unwrap_or(0) as usize;
        let label_bytes = &firmware[cursor + 12..cursor + 28];
        let label_end = label_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(label_bytes.len());
        let label = String::from_utf8_lossy(&label_bytes[..label_end]);
        if size > 0
            && (label.eq_ignore_ascii_case("assets") || (partition_type == 1 && subtype == 0x82))
        {
            return Ok(AppPartition { offset, size });
        }
        cursor += PARTITION_ENTRY_SIZE;
    }
    if !saw_magic {
        return Err(ArchiveError::AssetsNotFound(
            "0x8000 处没有有效的分区表".into(),
        ));
    }
    Err(ArchiveError::AssetsNotFound(
        "分区表中没有 assets 分区".into(),
    ))
}

// ---------------------------------------------------------------------------
// 极简只读 ZIP 解析
// ---------------------------------------------------------------------------

/// central directory 中的一个文件条目。
struct ZipEntry {
    name: String,
    method: u16,
    compressed_size: usize,
    uncompressed_size: usize,
    local_header_offset: usize,
}

fn u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = data.get(offset..end)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes = data.get(offset..end)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn zip_u16(data: &[u8], offset: usize, field: &str) -> Result<u16, ArchiveError> {
    u16_le(data, offset).ok_or_else(|| ArchiveError::InvalidZip(format!("{field} 越界")))
}

fn zip_u32(data: &[u8], offset: usize, field: &str) -> Result<u32, ArchiveError> {
    u32_le(data, offset).ok_or_else(|| ArchiveError::InvalidZip(format!("{field} 越界")))
}

/// 从文件末尾 64 KiB 内向前查找 EOCD，注释长度必须与文件末尾对齐。
fn find_eocd(zip: &[u8]) -> Result<usize, ArchiveError> {
    if zip.len() < EOCD_MIN_LEN {
        return Err(ArchiveError::InvalidZip("文件太短，不是有效 ZIP".into()));
    }
    let last = zip.len() - EOCD_MIN_LEN;
    let first = zip.len().saturating_sub(EOCD_SEARCH_LEN);
    for offset in (first..=last).rev() {
        if u32_le(zip, offset) != Some(EOCD_SIGNATURE) {
            continue;
        }
        let Some(comment_len) = u16_le(zip, offset + 20) else {
            continue;
        };
        if offset + EOCD_MIN_LEN + comment_len as usize == zip.len() {
            return Ok(offset);
        }
    }
    Err(ArchiveError::InvalidZip(
        "找不到 ZIP 结束记录（EOCD）".into(),
    ))
}

/// 解析 central directory，返回所有文件条目。
fn parse_zip_entries(zip: &[u8]) -> Result<Vec<ZipEntry>, ArchiveError> {
    let eocd = find_eocd(zip)?;
    let disk_number = zip_u16(zip, eocd + 4, "EOCD 磁盘号")?;
    let cd_disk = zip_u16(zip, eocd + 6, "EOCD 目录起始磁盘号")?;
    let entry_count = zip_u16(zip, eocd + 10, "EOCD 条目数")?;
    let cd_size = zip_u32(zip, eocd + 12, "EOCD 目录大小")?;
    let cd_offset = zip_u32(zip, eocd + 16, "EOCD 目录偏移")?;

    // EOCD 中的 ZIP64 标记：条目数/大小/偏移达到占位值。
    if entry_count == ZIP64_U16 || cd_size == ZIP64_U32 || cd_offset == ZIP64_U32 {
        return Err(ArchiveError::Zip64Unsupported);
    }
    if disk_number != 0 || cd_disk != 0 {
        return Err(ArchiveError::InvalidZip("不支持多卷 ZIP".into()));
    }

    let cd_start = cd_offset as usize;
    let cd_end = cd_start
        .checked_add(cd_size as usize)
        .filter(|end| *end <= zip.len())
        .ok_or_else(|| ArchiveError::InvalidZip("central directory 超出文件范围".into()))?;

    let mut entries = Vec::with_capacity(entry_count as usize);
    let mut cursor = cd_start;
    for _ in 0..entry_count {
        if cursor + 46 > cd_end {
            return Err(ArchiveError::InvalidZip("central directory 被截断".into()));
        }
        if u32_le(zip, cursor) != Some(CENTRAL_SIGNATURE) {
            return Err(ArchiveError::InvalidZip(format!(
                "central directory 签名错误（偏移 0x{cursor:X}）"
            )));
        }
        let flags = zip_u16(zip, cursor + 8, "条目 flags")?;
        let method = zip_u16(zip, cursor + 10, "条目压缩方式")?;
        let compressed_size = zip_u32(zip, cursor + 20, "条目压缩大小")?;
        let uncompressed_size = zip_u32(zip, cursor + 24, "条目解压大小")?;
        let name_len = zip_u16(zip, cursor + 28, "条目名长度")? as usize;
        let extra_len = zip_u16(zip, cursor + 30, "条目 extra 长度")? as usize;
        let comment_len = zip_u16(zip, cursor + 32, "条目注释长度")? as usize;
        let disk_start = zip_u16(zip, cursor + 34, "条目磁盘号")?;
        let local_header_offset = zip_u32(zip, cursor + 42, "条目 local header 偏移")?;

        // 任一分量达到占位值即视为 ZIP64 条目。
        if compressed_size == ZIP64_U32
            || uncompressed_size == ZIP64_U32
            || local_header_offset == ZIP64_U32
            || disk_start == ZIP64_U16
        {
            return Err(ArchiveError::Zip64Unsupported);
        }

        let name_start = cursor + 46;
        let name_end = name_start
            .checked_add(name_len)
            .filter(|end| *end <= cd_end)
            .ok_or_else(|| ArchiveError::InvalidZip("条目名越界".into()))?;
        let next = name_end
            .checked_add(extra_len)
            .and_then(|end| end.checked_add(comment_len))
            .filter(|end| *end <= cd_end)
            .ok_or_else(|| ArchiveError::InvalidZip("central directory 条目越界".into()))?;
        let name = String::from_utf8_lossy(&zip[name_start..name_end]).into_owned();

        // bit 0 表示加密，只读场景直接拒绝。
        if flags & 0x0001 != 0 {
            return Err(ArchiveError::InvalidZip(format!(
                "不支持加密的 ZIP 条目: {name}"
            )));
        }

        entries.push(ZipEntry {
            name,
            method,
            compressed_size: compressed_size as usize,
            uncompressed_size: uncompressed_size as usize,
            local_header_offset: local_header_offset as usize,
        });
        cursor = next;
    }

    Ok(entries)
}

/// 取文件名（忽略 ZIP 内的目录前缀）。
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// 是否 macOS 打包产生的元数据文件（`__MACOSX/`、`._xxx`）。
fn is_metadata_entry(name: &str) -> bool {
    name.starts_with("__MACOSX/") || basename(name).starts_with("._")
}

/// 从条目列表中挑选固件 bin。
///
/// 规则：优先 `merged-binary.bin`；否则唯一 `.bin`；多个 `.bin` 时优先
/// 文件名含 `merged` 或 `app` 的唯一项。
fn select_firmware_entry(entries: &[ZipEntry]) -> Result<&ZipEntry, ArchiveError> {
    let files: Vec<&ZipEntry> = entries
        .iter()
        .filter(|entry| !entry.name.ends_with('/') && !is_metadata_entry(&entry.name))
        .collect();

    if let Some(entry) = files
        .iter()
        .copied()
        .find(|entry| basename(&entry.name).eq_ignore_ascii_case("merged-binary.bin"))
    {
        return Ok(entry);
    }

    let bins: Vec<&ZipEntry> = files
        .iter()
        .copied()
        .filter(|entry| {
            let lower = entry.name.to_ascii_lowercase();
            lower.ends_with(".bin")
        })
        .collect();
    match bins.len() {
        0 => Err(ArchiveError::NoFirmwareEntry),
        1 => Ok(bins[0]),
        _ => {
            for keyword in ["merged", "app"] {
                let matches: Vec<&ZipEntry> = bins
                    .iter()
                    .copied()
                    .filter(|entry| entry.name.to_ascii_lowercase().contains(keyword))
                    .collect();
                if matches.len() == 1 {
                    return Ok(matches[0]);
                }
            }
            Err(ArchiveError::NoFirmwareEntry)
        }
    }
}

/// 读取并解压一个条目，`max_bytes` 为解压长度上限。
fn read_entry(zip: &[u8], entry: &ZipEntry, max_bytes: usize) -> Result<Vec<u8>, ArchiveError> {
    let local = entry.local_header_offset;
    if u32_le(zip, local) != Some(LOCAL_SIGNATURE) {
        return Err(ArchiveError::InvalidZip(format!(
            "local header 签名错误（偏移 0x{local:X}）"
        )));
    }
    let name_len = zip_u16(zip, local + 26, "local header 文件名长度")? as usize;
    let extra_len = zip_u16(zip, local + 28, "local header extra 长度")? as usize;
    let data_start = local
        .checked_add(30)
        .and_then(|offset| offset.checked_add(name_len))
        .and_then(|offset| offset.checked_add(extra_len))
        .ok_or_else(|| ArchiveError::InvalidZip("local header 偏移溢出".into()))?;
    let data_end = data_start
        .checked_add(entry.compressed_size)
        .filter(|end| *end <= zip.len())
        .ok_or_else(|| ArchiveError::InvalidZip("固件数据超出 ZIP 范围".into()))?;

    // 解压前先用 central directory 声明的大小挡住明显超限的条目。
    if entry.uncompressed_size > max_bytes {
        return Err(ArchiveError::TooLarge);
    }

    let raw = &zip[data_start..data_end];
    let mut out = Vec::new();
    match entry.method {
        0 => {
            if raw.len() > max_bytes {
                return Err(ArchiveError::TooLarge);
            }
            out.extend_from_slice(raw);
        }
        8 => {
            // take(max + 1)：最多多读 1 字节用于判断是否超限，避免 zip bomb。
            let limit = (max_bytes as u64).saturating_add(1);
            DeflateDecoder::new(raw)
                .take(limit)
                .read_to_end(&mut out)
                .map_err(|err| ArchiveError::InvalidZip(format!("deflate 解压失败: {err}")))?;
        }
        other => return Err(ArchiveError::UnsupportedCompression(other)),
    }

    if out.len() > max_bytes {
        return Err(ArchiveError::TooLarge);
    }
    // 解压长度应与 central directory 声明一致；不一致说明 ZIP 被篡改或损坏。
    if out.len() != entry.uncompressed_size {
        return Err(ArchiveError::InvalidZip(format!(
            "解压长度与 central directory 不一致（实际 {}，声明 {}）",
            out.len(),
            entry.uncompressed_size
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 合并镜像 / app 镜像解析
// ---------------------------------------------------------------------------

/// 分区表中的 app 分区。
struct AppPartition {
    offset: usize,
    size: usize,
}

/// 从解压出的固件二进制中提取 app 镜像。
fn extract_from_firmware(
    firmware: &[u8],
    max_app_bytes: usize,
) -> Result<ExtractedApp, ArchiveError> {
    if firmware.is_empty() {
        return Err(ArchiveError::InvalidImage("固件内容为空".into()));
    }

    // app-only 资产（如 TTS 资源包中的 app bin）本身就以 0xE9 开头。
    // 但 ESP32-S3/C3 的 merged-bin 在 0x0 处就是 bootloader，同样以 0xE9 开头，
    // 所以直接解析失败时需要回退到分区表。
    if firmware[0] == ESP_IMAGE_MAGIC {
        match parse_app_image(firmware, 0, firmware.len(), max_app_bytes) {
            Ok(app) => return Ok(app),
            Err(ArchiveError::InvalidImage(reason)) => {
                return match extract_from_partition(firmware, max_app_bytes) {
                    // 没有分区表，说明这份数据本来就是（损坏的）app-only 镜像。
                    Err(ArchiveError::PartitionNotFound(_)) => {
                        Err(ArchiveError::InvalidImage(reason))
                    }
                    other => other,
                };
            }
            Err(other) => return Err(other),
        }
    }

    extract_from_partition(firmware, max_app_bytes)
}

/// 在 0x8000 分区表中找到 app 分区并解析。
fn extract_from_partition(
    firmware: &[u8],
    max_app_bytes: usize,
) -> Result<ExtractedApp, ArchiveError> {
    let partition = find_app_partition(firmware)?;
    let start = partition.offset;
    if start >= firmware.len() {
        return Err(ArchiveError::InvalidImage(format!(
            "应用分区 offset 0x{start:X} 超出固件长度 {}",
            firmware.len()
        )));
    }
    // 镜像不能越过分区末尾；裁剪过的 merged-bin 可能短于分区声明大小，
    // 因此同时受输入长度约束。
    let max_end = start.saturating_add(partition.size).min(firmware.len());
    parse_app_image(firmware, start, max_end, max_app_bytes)
}

/// 校验分区表项 magic；兼容 `AA 50`（真实固件）与 `50 AA` 两种字节序写法。
fn partition_magic_ok(firmware: &[u8], offset: usize) -> bool {
    let Some(bytes) = firmware.get(offset..offset + PARTITION_MAGIC.len()) else {
        return false;
    };
    bytes == PARTITION_MAGIC || bytes == [PARTITION_MAGIC[1], PARTITION_MAGIC[0]]
}

/// 遍历 0x8000 分区表，按 factory / ota_0 / 第一个 app 分区的优先级挑选。
fn find_app_partition(firmware: &[u8]) -> Result<AppPartition, ArchiveError> {
    let table_start = PARTITION_TABLE_OFFSET;
    if firmware.len() < table_start + PARTITION_ENTRY_SIZE {
        return Err(ArchiveError::PartitionNotFound(
            "固件不足 0x8000 字节，无法读取分区表".into(),
        ));
    }

    let table_end =
        (table_start + MAX_PARTITION_ENTRIES * PARTITION_ENTRY_SIZE).min(firmware.len());
    let mut first_app: Option<AppPartition> = None;
    let mut factory_alt: Option<AppPartition> = None;
    let mut factory: Option<AppPartition> = None;
    let mut ota_0: Option<AppPartition> = None;
    let mut saw_magic = false;

    let mut cursor = table_start;
    while cursor + PARTITION_ENTRY_SIZE <= table_end {
        if !partition_magic_ok(firmware, cursor) {
            break;
        }
        saw_magic = true;
        let partition_type = firmware[cursor + 2];
        let subtype = firmware[cursor + 3];
        let offset = u32_le(firmware, cursor + 4).unwrap_or(0) as usize;
        let size = u32_le(firmware, cursor + 8).unwrap_or(0) as usize;
        if partition_type == PARTITION_TYPE_APP && size > 0 {
            let partition = AppPartition { offset, size };
            if subtype == PARTITION_SUBTYPE_FACTORY_ALT {
                factory_alt.get_or_insert(partition);
            } else if subtype == PARTITION_SUBTYPE_FACTORY {
                factory.get_or_insert(partition);
            } else if subtype == PARTITION_SUBTYPE_OTA_0 {
                ota_0.get_or_insert(partition);
            } else {
                first_app.get_or_insert(partition);
            }
        }
        cursor += PARTITION_ENTRY_SIZE;
    }

    if !saw_magic {
        return Err(ArchiveError::PartitionNotFound(
            "0x8000 处没有有效的分区表，固件可能不是 merged-binary.bin".into(),
        ));
    }

    // 说明：真实 ESP-IDF 中 factory=0x00、ota_0=0x10、ota_1=0x11；
    // 任务约定里 factory=0x10、ota_0=0x11。两者都覆盖，优先取任务约定值。
    factory_alt
        .or(factory)
        .or(ota_0)
        .or(first_app)
        .ok_or_else(|| ArchiveError::PartitionNotFound("分区表中没有 type=0 的 app 分区".into()))
}

/// 按 16 字节对齐向上取整。
fn align_up(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

/// 读取 `esp_app_desc_t` 中的定长字符串（NUL 截断）。
fn read_c_string(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// 解析位于 `start` 的 ESP app 镜像。
///
/// `max_end` 是镜像可用的绝对结束偏移（min(分区末尾, 输入长度)），
/// `max_app_bytes` 是镜像总长上限。
fn parse_app_image(
    firmware: &[u8],
    start: usize,
    max_end: usize,
    max_app_bytes: usize,
) -> Result<ExtractedApp, ArchiveError> {
    let header_end = start
        .checked_add(IMAGE_HEADER_SIZE)
        .ok_or_else(|| ArchiveError::InvalidImage("镜像偏移溢出".into()))?;
    let header = firmware
        .get(start..header_end)
        .ok_or_else(|| ArchiveError::InvalidImage("ESP 镜像头被截断".into()))?;
    if header[0] != ESP_IMAGE_MAGIC {
        return Err(ArchiveError::InvalidImage(format!(
            "ESP 镜像 magic 应为 0xE9，实际 0x{:02X}",
            header[0]
        )));
    }
    let segment_count = header[1];
    if segment_count == 0 || segment_count > MAX_IMAGE_SEGMENTS {
        return Err(ArchiveError::InvalidImage(format!(
            "ESP 镜像段数不合法: {segment_count}"
        )));
    }
    let hash_appended = header[23] == 1;

    // 逐段解析，同时累计 ESP checksum（所有段数据字节异或，初值 0xEF）。
    let mut checksum = ESP_CHECKSUM_INITIAL;
    let mut cursor = header_end;
    for index in 0..segment_count {
        let load_addr = u32_le(firmware, cursor)
            .ok_or_else(|| ArchiveError::InvalidImage(format!("第 {index} 个段头被截断")))?;
        let data_len = u32_le(firmware, cursor + 4)
            .ok_or_else(|| ArchiveError::InvalidImage(format!("第 {index} 个段头被截断")))?;
        let _ = load_addr;
        cursor += 8;
        let data_end = cursor
            .checked_add(data_len as usize)
            .filter(|end| *end <= firmware.len())
            .ok_or_else(|| {
                ArchiveError::InvalidImage(format!("第 {index} 个段数据被截断（长度 {data_len}）"))
            })?;
        for byte in &firmware[cursor..data_end] {
            checksum ^= *byte;
        }
        cursor = data_end;
    }

    // checksum 字节位于「段数据 + 1B checksum」按 16 字节对齐后的最后一个字节。
    // 注意不是紧跟段数据：真实 IDF 镜像（v2.5.2 bread-compact-wifi）在段数据后
    // 还有若干 0x00 填充，checksum 在对齐块末尾。
    let unpadded_len = cursor - start;
    let aligned_len = align_up(unpadded_len + 1, 16);
    let checksum_pos = start
        .checked_add(aligned_len - 1)
        .filter(|pos| *pos < firmware.len())
        .ok_or_else(|| ArchiveError::InvalidImage("ESP 镜像 checksum 被截断".into()))?;
    let stored_checksum = firmware[checksum_pos];
    if stored_checksum != checksum {
        return Err(ArchiveError::Integrity(format!(
            "ESP 镜像 checksum 不匹配（计算值 0x{checksum:02X}，实际 0x{stored_checksum:02X}）"
        )));
    }

    // 镜像长度：段数据 + 1B checksum 按 16 对齐，hash 追加时再加 32B。
    let image_len = aligned_len + if hash_appended { SHA256_LEN } else { 0 };
    if image_len > max_app_bytes {
        return Err(ArchiveError::TooLarge);
    }
    let image_end = start
        .checked_add(image_len)
        .filter(|end| *end <= max_end)
        .ok_or_else(|| {
            ArchiveError::InvalidImage(format!(
                "应用镜像超出可用范围（需要 {image_len} 字节，可用 {}）",
                max_end.saturating_sub(start)
            ))
        })?;

    // 校验尾部 SHA256：覆盖 image 起点到 hash 之前的全部字节。
    if hash_appended {
        let hash_pos = image_end - SHA256_LEN;
        let digest = Sha256::digest(&firmware[start..hash_pos]);
        let digest: &[u8] = &digest;
        if digest != &firmware[hash_pos..image_end] {
            return Err(ArchiveError::Integrity("应用镜像 SHA256 校验失败".into()));
        }
    }

    // esp_app_desc_t 位于 image + 32。
    let desc_start = start + APP_DESC_OFFSET;
    let desc_end = desc_start + APP_DESC_MIN_LEN;
    if desc_end > image_end {
        return Err(ArchiveError::InvalidImage(
            "应用镜像太短，缺少 esp_app_desc_t".into(),
        ));
    }
    let desc = &firmware[desc_start..desc_end];
    let desc_magic = u32_le(desc, 0).unwrap_or(0);
    if desc_magic != APP_DESC_MAGIC {
        return Err(ArchiveError::InvalidImage(format!(
            "esp_app_desc_t magic 错误: 0x{desc_magic:08X}"
        )));
    }
    let version = read_c_string(&desc[16..48]);
    let project = read_c_string(&desc[48..80]);

    Ok(ExtractedApp {
        bytes: firmware[start..image_end].to_vec(),
        version,
        project,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    use super::*;

    /// 合成 merged-bin 中第一个 app 分区的偏移。
    const APP_BASE_OFFSET: usize = 0x1_0000;
    /// 合成分区表中 app 分区声明的大小（1 MiB）。
    const APP_SLOT_SIZE: u32 = 0x0010_0000;
    /// 合成 app 镜像中第二段数据（payload）的起点。
    const PAYLOAD_OFFSET: usize = IMAGE_HEADER_SIZE + 8 + 256 + 8;

    /// 构造最小的 `esp_app_desc_t`（256 字节）。
    fn build_app_desc(version: &str, project: &str) -> Vec<u8> {
        let mut desc = vec![0u8; 256];
        desc[0..4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        desc[16..16 + version.len()].copy_from_slice(version.as_bytes());
        desc[48..48 + project.len()].copy_from_slice(project.as_bytes());
        desc
    }

    /// 构造合法的 app 镜像：两个段、checksum、可选尾部 SHA256。
    fn build_app_image_with_desc(desc: Vec<u8>, with_hash: bool) -> Vec<u8> {
        let payload: &[u8] = b"firmware-payload";
        let mut image = vec![0u8; IMAGE_HEADER_SIZE];
        image[0] = ESP_IMAGE_MAGIC;
        image[1] = 2;
        image[2] = 0x02;
        image[3] = 0x20;
        image[23] = u8::from(with_hash);

        let mut checksum = ESP_CHECKSUM_INITIAL;
        let segments: [(u32, &[u8]); 2] = [(0x3F40_0000, &desc), (0x3F80_0000, payload)];
        for (load_addr, data) in segments {
            image.extend_from_slice(&load_addr.to_le_bytes());
            image.extend_from_slice(&(data.len() as u32).to_le_bytes());
            image.extend_from_slice(data);
            for byte in data {
                checksum ^= *byte;
            }
        }
        // 真实 IDF 镜像：checksum 放在「段数据 + 1B」16 字节对齐块的最后一个字节。
        let unpadded = image.len();
        let aligned = align_up(unpadded + 1, 16);
        image.resize(aligned, 0);
        image[aligned - 1] = checksum;
        if with_hash {
            let digest = Sha256::digest(&image);
            image.extend_from_slice(&digest);
        }
        image
    }

    fn build_app_image(version: &str, project: &str, with_hash: bool) -> Vec<u8> {
        build_app_image_with_desc(build_app_desc(version, project), with_hash)
    }

    /// 构造 32 字节分区表项。
    fn build_partition_entry(
        partition_type: u8,
        subtype: u8,
        offset: u32,
        size: u32,
        label: &str,
    ) -> Vec<u8> {
        let mut entry = vec![0u8; PARTITION_ENTRY_SIZE];
        entry[0..2].copy_from_slice(&PARTITION_MAGIC);
        entry[2] = partition_type;
        entry[3] = subtype;
        entry[4..8].copy_from_slice(&offset.to_le_bytes());
        entry[8..12].copy_from_slice(&size.to_le_bytes());
        let label = label.as_bytes();
        let label_len = label.len().min(16);
        entry[12..12 + label_len].copy_from_slice(&label[..label_len]);
        entry
    }

    /// 构造 merged-bin：0x8000 分区表 + 依次放置的 app 镜像。
    fn build_merged_with_apps(apps: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut table = Vec::new();
        for (index, (subtype, _)) in apps.iter().enumerate() {
            let offset = (APP_BASE_OFFSET + index * APP_SLOT_SIZE as usize) as u32;
            table.extend_from_slice(&build_partition_entry(
                PARTITION_TYPE_APP,
                *subtype,
                offset,
                APP_SLOT_SIZE,
                &format!("app{index}"),
            ));
        }
        // 结束项：magic 为 0xFF，解析到此结束。
        table.extend_from_slice(&[0xFFu8; PARTITION_ENTRY_SIZE]);

        let mut firmware = vec![0xFFu8; PARTITION_TABLE_OFFSET + table.len()];
        firmware[PARTITION_TABLE_OFFSET..PARTITION_TABLE_OFFSET + table.len()]
            .copy_from_slice(&table);
        for (index, (_, app)) in apps.iter().enumerate() {
            let offset = APP_BASE_OFFSET + index * APP_SLOT_SIZE as usize;
            if firmware.len() < offset {
                firmware.resize(offset, 0xFF);
            }
            firmware.extend_from_slice(app);
        }
        firmware
    }

    /// 手工拼一个 ZIP（central directory 的 crc32 填 0，本模块不校验）。
    fn build_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut zip = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in entries {
            let local_offset = zip.len() as u32;
            let (method, payload) = if *deflate {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(data).expect("写入 deflate 数据");
                (8u16, encoder.finish().expect("完成 deflate 压缩"))
            } else {
                (0u16, data.to_vec())
            };
            let name = name.as_bytes();

            // local file header
            zip.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            zip.extend_from_slice(&20u16.to_le_bytes()); // 解压所需版本
            zip.extend_from_slice(&0u16.to_le_bytes()); // flags
            zip.extend_from_slice(&method.to_le_bytes());
            zip.extend_from_slice(&0u16.to_le_bytes()); // time
            zip.extend_from_slice(&0u16.to_le_bytes()); // date
            zip.extend_from_slice(&0u32.to_le_bytes()); // crc32
            zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            zip.extend_from_slice(&(data.len() as u32).to_le_bytes());
            zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
            zip.extend_from_slice(&0u16.to_le_bytes()); // extra
            zip.extend_from_slice(name);
            zip.extend_from_slice(&payload);

            // central directory file header
            central.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&0u32.to_le_bytes()); // crc32
            central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk start
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name);
        }

        let cd_offset = zip.len() as u32;
        zip.extend_from_slice(&central);

        // EOCD
        zip.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // disk number
        zip.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        zip.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        zip.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        zip.extend_from_slice(&(central.len() as u32).to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // comment len
        zip
    }

    fn error_of(zip: &[u8], max_app_bytes: usize) -> ArchiveError {
        extract_app_image(zip, max_app_bytes).expect_err("应当提取失败")
    }

    #[test]
    fn extracts_factory_app_from_stored_zip() {
        let app = build_app_image("1.2.3", "xiaozhi", true);
        let merged = build_merged_with_apps(&[(0x10, app.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, app);
        assert_eq!(extracted.version.as_deref(), Some("1.2.3"));
        assert_eq!(extracted.project.as_deref(), Some("xiaozhi"));
    }

    #[test]
    fn extracts_app_from_deflate_zip() {
        let app = build_app_image("1.2.3", "xiaozhi", true);
        let merged = build_merged_with_apps(&[(0x10, app.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, true)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, app);
    }

    #[test]
    fn extracts_app_only_bin() {
        let app = build_app_image("0.9.0", "app-only", false);
        let zip = build_zip(&[("xiaozhi.bin", &app, true)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, app);
        assert_eq!(extracted.version.as_deref(), Some("0.9.0"));
        assert_eq!(extracted.project.as_deref(), Some("app-only"));
    }

    #[test]
    fn selects_merged_binary_among_multiple_bins() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let merged = build_merged_with_apps(&[(0x10, app.clone())]);
        let zip = build_zip(&[
            ("bootloader.bin", &[0u8; 32], false),
            ("merged-binary.bin", &merged, false),
        ]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, app);
    }

    #[test]
    fn selects_app_named_bin_when_several_bins() {
        let app = build_app_image("2.0.0", "xiaozhi", true);
        let other = build_app_image("1.0.0", "other", true);
        let zip = build_zip(&[
            ("bootloader.bin", &other, false),
            ("application.bin", &app, false),
        ]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn falls_back_to_ota_0_partition() {
        let app = build_app_image("3.0.0", "xiaozhi", true);
        // 按任务约定的子类型：0x11 为 ota_0（真实 ESP-IDF 中为 ota_1）。
        let merged = build_merged_with_apps(&[(0x11, app.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.version.as_deref(), Some("3.0.0"));
    }

    #[test]
    fn picks_factory_before_ota() {
        let factory = build_app_image("9.9.9", "factory", true);
        let ota = build_app_image("1.0.0", "ota", true);
        let merged = build_merged_with_apps(&[(0x11, ota), (0x10, factory.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, factory);
    }

    #[test]
    fn supports_real_idf_factory_subtype() {
        let factory = build_app_image("1.1.1", "real-factory", true);
        let merged = build_merged_with_apps(&[(0x00, factory.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, factory);
    }

    #[test]
    fn picks_first_app_partition_when_subtype_unknown() {
        let app = build_app_image("4.0.0", "any-app", true);
        let merged = build_merged_with_apps(&[(0x20, app.clone())]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let extracted = extract_app_image(&zip, MAX_APP_BYTES).expect("应成功提取");
        assert_eq!(extracted.bytes, app);
    }

    #[test]
    fn rejects_truncated_app_image() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let mut merged = build_merged_with_apps(&[(0x10, app)]);
        merged.truncate(merged.len() - 16);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let error = error_of(&zip, MAX_APP_BYTES);
        assert!(
            matches!(error, ArchiveError::InvalidImage(_)),
            "实际错误: {error}"
        );
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let mut app = build_app_image("1.0.0", "xiaozhi", false);
        // 改动段数据中的 1 字节（不改变长度），checksum 必然不匹配。
        app[PAYLOAD_OFFSET] ^= 0xFF;
        let merged = build_merged_with_apps(&[(0x10, app)]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let error = error_of(&zip, MAX_APP_BYTES);
        assert!(
            matches!(error, ArchiveError::Integrity(_)),
            "实际错误: {error}"
        );
    }

    #[test]
    fn rejects_hash_mismatch() {
        let mut app = build_app_image("1.0.0", "xiaozhi", true);
        // 只改动尾部 SHA256 的最后一字节，checksum 仍然合法。
        let last = app.len() - 1;
        app[last] ^= 0xFF;
        let merged = build_merged_with_apps(&[(0x10, app)]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let error = error_of(&zip, MAX_APP_BYTES);
        assert!(
            matches!(error, ArchiveError::Integrity(_)),
            "实际错误: {error}"
        );
    }

    #[test]
    fn rejects_wrong_image_magic() {
        let mut app = build_app_image("1.0.0", "xiaozhi", true);
        app[0] = 0x00;
        let merged = build_merged_with_apps(&[(0x10, app)]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let error = error_of(&zip, MAX_APP_BYTES);
        assert!(
            matches!(error, ArchiveError::InvalidImage(_)),
            "实际错误: {error}"
        );
    }

    #[test]
    fn rejects_wrong_desc_magic() {
        let mut desc = build_app_desc("1.0.0", "xiaozhi");
        desc[0..4].copy_from_slice(&0u32.to_le_bytes());
        let app = build_app_image_with_desc(desc, true);
        let merged = build_merged_with_apps(&[(0x10, app)]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        let error = error_of(&zip, MAX_APP_BYTES);
        assert!(
            matches!(error, ArchiveError::InvalidImage(_)),
            "实际错误: {error}"
        );
    }

    #[test]
    fn reports_no_firmware_entry() {
        let zip = build_zip(&[("README.md", b"no firmware here", false)]);
        assert!(matches!(
            error_of(&zip, MAX_APP_BYTES),
            ArchiveError::NoFirmwareEntry
        ));
    }

    #[test]
    fn reports_partition_not_found() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        // 0x8000 处没有分区表（直接是 app 数据）。
        let mut firmware = vec![0xFFu8; PARTITION_TABLE_OFFSET];
        firmware.extend_from_slice(&app);
        let zip = build_zip(&[("merged-binary.bin", &firmware, false)]);

        assert!(matches!(
            error_of(&zip, MAX_APP_BYTES),
            ArchiveError::PartitionNotFound(_)
        ));
    }

    #[test]
    fn rejects_zip64_marker() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let zip = build_zip(&[("xiaozhi.bin", &app, false)]);
        let mut zip = zip;
        let eocd = zip.len() - EOCD_MIN_LEN;
        // 把 EOCD 的总条目数改成 0xFFFF，触发 ZIP64 标记。
        zip[eocd + 10..eocd + 12].copy_from_slice(&ZIP64_U16.to_le_bytes());

        assert!(matches!(
            error_of(&zip, MAX_APP_BYTES),
            ArchiveError::Zip64Unsupported
        ));
    }

    #[test]
    fn rejects_unsupported_compression() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let mut zip = build_zip(&[("xiaozhi.bin", &app, true)]);
        let eocd = zip.len() - EOCD_MIN_LEN;
        let cd_offset = u32_le(&zip, eocd + 16).expect("central directory 偏移") as usize;
        // 把 local header 与 central directory 的压缩方式都改成 12（bzip2）。
        zip[8..10].copy_from_slice(&12u16.to_le_bytes());
        zip[cd_offset + 10..cd_offset + 12].copy_from_slice(&12u16.to_le_bytes());

        assert!(matches!(
            error_of(&zip, MAX_APP_BYTES),
            ArchiveError::UnsupportedCompression(12)
        ));
    }

    #[test]
    fn rejects_oversized_firmware() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let app_len = app.len();
        let merged = build_merged_with_apps(&[(0x10, app)]);
        let zip = build_zip(&[("merged-binary.bin", &merged, false)]);

        // merged-bin 本身允许较大（整片 flash），但最终 app 镜像不能超过 max_app_bytes。
        assert!(matches!(
            error_of(&zip, app_len.saturating_sub(1)),
            ArchiveError::TooLarge
        ));
    }

    #[test]
    fn rejects_truncated_zip() {
        let app = build_app_image("1.0.0", "xiaozhi", true);
        let mut zip = build_zip(&[("xiaozhi.bin", &app, false)]);
        zip.truncate(zip.len() - 8);

        assert!(matches!(
            error_of(&zip, MAX_APP_BYTES),
            ArchiveError::InvalidZip(_)
        ));
    }
}
