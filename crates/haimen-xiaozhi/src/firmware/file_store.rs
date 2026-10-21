//! 默认存储实现：JSON 状态文件 + 本地固件目录。
//!
//! 这里是唯一直接碰文件系统的位置。facade / OTA / Web API 只依赖
//! [`FirmwareMetadataStore`] / [`FirmwareBlobStore`]，因此未来换 SQL 或对象存储
//! 时不需要改调用方。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::store::{
    FirmwareBlobStore, FirmwareMetadataStore, FirmwareSnapshot, FirmwareStoreError, MAX_BLOB_BYTES,
    validate_blob_name,
};

/// JSON 文件元数据存储。
///
/// 写入是「临时文件 + rename」；状态文件损坏时先备份为
/// `state.json.corrupt.<毫秒>.bak`，再返回错误由 facade 决定是否按空状态启动。
pub struct JsonFileMetadataStore {
    path: PathBuf,
    /// 串行化写入，避免两次保存交错 rename。
    write_lock: tokio::sync::Mutex<()>,
}

impl JsonFileMetadataStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 当前状态文件路径（日志/排查用）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl FirmwareMetadataStore for JsonFileMetadataStore {
    async fn load(&self) -> Result<FirmwareSnapshot, FirmwareStoreError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => match serde_json::from_slice::<FirmwareSnapshot>(&bytes) {
                Ok(snapshot) => Ok(snapshot),
                Err(err) => {
                    backup_corrupt(&self.path).await;
                    Err(FirmwareStoreError::Codec(err))
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(FirmwareSnapshot::default())
            }
            Err(err) => Err(FirmwareStoreError::Io(err)),
        }
    }

    async fn save(&self, snapshot: &FirmwareSnapshot) -> Result<(), FirmwareStoreError> {
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_vec_pretty(snapshot)?;
        let tmp = temp_sibling(&self.path, "state");
        tokio::fs::write(&tmp, &data).await?;
        rename_replace(&tmp, &self.path).await
    }
}

/// 本地目录固件存储。
pub struct LocalFileBlobStore {
    dir: PathBuf,
}

impl LocalFileBlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 当前固件目录（日志/排查用）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, name: &str) -> Result<PathBuf, FirmwareStoreError> {
        validate_blob_name(name)?;
        Ok(self.dir.join(name))
    }
}

#[async_trait]
impl FirmwareBlobStore for LocalFileBlobStore {
    async fn exists(&self, name: &str) -> Result<bool, FirmwareStoreError> {
        let path = self.path_for(name)?;
        match tokio::fs::metadata(&path).await {
            Ok(meta) => Ok(meta.is_file()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(FirmwareStoreError::Io(err)),
        }
    }

    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), FirmwareStoreError> {
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(FirmwareStoreError::TooLarge);
        }
        let path = self.path_for(name)?;
        tokio::fs::create_dir_all(&self.dir).await?;
        let tmp = temp_sibling(&path, "bin");
        tokio::fs::write(&tmp, &bytes).await?;
        rename_replace(&tmp, &path).await
    }

    async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, FirmwareStoreError> {
        let path = self.path_for(name)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                if bytes.len() > MAX_BLOB_BYTES {
                    return Err(FirmwareStoreError::TooLarge);
                }
                Ok(Some(bytes))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(FirmwareStoreError::Io(err)),
        }
    }

    async fn list(&self) -> Result<Vec<String>, FirmwareStoreError> {
        let mut out = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            // 目录还没创建时等价于空列表（首次启动 GC 会走到这里）。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(FirmwareStoreError::Io(err)),
        };
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if validate_blob_name(&name).is_ok() {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    async fn remove(&self, name: &str) -> Result<(), FirmwareStoreError> {
        let path = self.path_for(name)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(FirmwareStoreError::Io(err)),
        }
    }
}

fn temp_sibling(path: &Path, tag: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "firmware".to_string());
    path.with_file_name(format!(".{name}.{tag}.{}.{now}.tmp", std::process::id()))
}

/// rename 覆盖已有文件。Windows 上 rename 到已存在路径会失败，先删目标再重试。
async fn rename_replace(from: &Path, to: &Path) -> Result<(), FirmwareStoreError> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = tokio::fs::remove_file(to).await;
            tokio::fs::rename(from, to)
                .await
                .map_err(FirmwareStoreError::Io)
        }
    }
}

async fn backup_corrupt(path: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let backup = path.with_file_name(format!(
        "{}.corrupt.{now}.bak",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "state.json".to_string())
    ));
    match tokio::fs::rename(path, &backup).await {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            backup = %backup.display(),
            "固件状态文件损坏，已备份，将按空状态启动"
        ),
        Err(err) => tracing::warn!(
            path = %path.display(),
            error = %err,
            "固件状态文件损坏且备份失败，将按空状态启动"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::store::{DeviceFirmwareRecord, FirmwareSnapshot};

    #[tokio::test]
    async fn metadata_round_trips_and_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let store = JsonFileMetadataStore::new(&path);

        let empty = store.load().await.expect("缺失文件应返回空快照");
        assert!(empty.devices.is_empty());

        let mut snapshot = FirmwareSnapshot::default();
        snapshot.devices.insert(
            "aa:bb:cc:dd:ee:ff".to_string(),
            DeviceFirmwareRecord::default(),
        );
        store.save(&snapshot).await.expect("保存");
        let loaded = store.load().await.expect("加载");
        assert!(loaded.devices.contains_key("aa:bb:cc:dd:ee:ff"));
    }

    #[tokio::test]
    async fn corrupt_state_is_backed_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        tokio::fs::write(&path, b"{not json")
            .await
            .expect("写坏文件");
        let store = JsonFileMetadataStore::new(&path);
        assert!(store.load().await.is_err());
        assert!(!path.exists(), "损坏文件应被移走");

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .expect("列目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt."))
            .collect();
        assert_eq!(backups.len(), 1, "应生成一个 .corrupt.*.bak");
    }

    #[tokio::test]
    async fn blob_store_round_trips_and_validates_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LocalFileBlobStore::new(dir.path().join("bin"));
        let name = "2.5.2-bread-compact-wifi-abcdef123456.bin";
        assert!(!store.exists(name).await.expect("exists"));
        store.put(name, vec![1, 2, 3]).await.expect("put");
        assert!(store.exists(name).await.expect("exists"));
        assert_eq!(store.get(name).await.expect("get"), Some(vec![1, 2, 3]));
        assert_eq!(store.list().await.expect("list"), vec![name.to_string()]);
        store.remove(name).await.expect("remove");
        assert!(!store.exists(name).await.expect("exists"));
        assert!(store.get("../x.bin").await.is_err());
    }

    #[tokio::test]
    async fn blob_store_rejects_oversized_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LocalFileBlobStore::new(dir.path());
        let result = store.put("huge.bin", vec![0u8; MAX_BLOB_BYTES + 1]).await;
        assert!(matches!(result, Err(FirmwareStoreError::TooLarge)));
    }
}
