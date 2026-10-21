//! GitHub Releases 查询与按板型匹配固件。
//!
//! 目标仓库固定为 `78/xiaozhi-esp32`。这里处理三件事：
//!
//! 1. **release 目录缓存**：未认证的 GitHub API 每小时只有 60 次额度，固件页面
//!    每刷新一次都打 API 很快就会耗尽。目录缓存 600 秒；强制刷新失败时退回旧
//!    缓存并标记 `stale`，让前端还能展示上次的版本列表。
//! 2. **按板型匹配**：新 release 带 `release-manifest.json`，其中 variant 的
//!    `board` 是 `<厂商>/<板名>`，`full_name` 是资产文件名里的 slug；老 release
//!    没有 manifest，只能按资产名后缀推断。
//! 3. **完整性**：资产哈希优先用 GitHub API 返回的 `digest`，没有则回退到 release
//!    里的 `SHA256SUMS`。
//!
//! 环境变量（见 [`ReleaseService::from_env`]）：
//!
//! - `HAIMEN_XIAOZHI_GITHUB_API_BASE`：GitHub API 基地址，默认 `https://api.github.com`
//! - `HAIMEN_XIAOZHI_GITHUB_DOWNLOAD_BASE`：资产下载基地址，默认 `https://github.com`
//! - `GITHUB_TOKEN` / `HAIMEN_GITHUB_TOKEN`：可选令牌，仅作为请求头使用，绝不记录

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// 目标仓库（`owner/repo`）。
const REPO: &str = "78/xiaozhi-esp32";

/// 默认 GitHub API 地址。
const DEFAULT_API_BASE: &str = "https://api.github.com";

/// 默认资产下载地址。
const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com";

/// release 目录缓存有效期（毫秒）。
const CATALOG_TTL_MS: i64 = 600_000;

/// 普通 JSON 请求超时。
const JSON_TIMEOUT: Duration = Duration::from_secs(15);

/// 固件下载超时（固件包有几 MB，给足时间）。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// release 列表每页条数。
const RELEASES_PER_PAGE: usize = 30;

/// 非精确匹配时最多返回多少个候选固件。
const MAX_CANDIDATES: usize = 12;

// ═══════════════════════════════════════════════════════════════════════════════
// 对外数据结构
// ═══════════════════════════════════════════════════════════════════════════════

/// release 目录页：版本列表 + 缓存元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasesPage {
    /// 按发布时间降序排列的 release。
    pub releases: Vec<ReleaseInfo>,
    /// 数据抓取时间（Unix 毫秒）。
    pub fetched_at_ms: i64,
    /// 是否来自过期缓存（刷新失败时的降级结果）。
    pub stale: bool,
}

/// 单个 GitHub release 的精简信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// release tag，例如 `v2.5.2`。
    pub tag: String,
    /// 去掉前导 `v` 的版本号，例如 `2.5.2`。
    pub version: String,
    /// release 标题。
    pub name: String,
    /// 发布时间（RFC3339 字符串，GitHub 返回的 UTC 时间）。
    pub published_at: String,
    /// 是否为预发布版本。
    pub prerelease: bool,
    /// 是否为草稿。
    pub draft: bool,
    /// release 页面地址。
    pub html_url: String,
    /// release 资产列表。
    pub assets: Vec<ReleaseAsset>,
}

/// release 中的单个资产。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    /// 资产文件名，例如 `v2.5.2_bread-compact-wifi.zip`。
    pub name: String,
    /// 浏览器下载地址（`browser_download_url`）。
    pub download_url: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// GitHub 提供的摘要，形如 `sha256:<hex>`。
    pub digest: Option<String>,
}

/// `release-manifest.json` 的内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// manifest schema 版本。
    pub schema_version: u32,
    /// 固件版本号，例如 `2.5.2`。
    pub version: String,
    /// 对应 release tag，例如 `v2.5.2`。
    pub tag: String,
    /// 该版本支持的所有板型。
    pub variants: Vec<ManifestVariant>,
}

/// manifest 中的一个板型变体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVariant {
    /// 板型全名，例如 `alientek/atk-dnesp32s3`。
    pub board: String,
    /// 板型短名，例如 `atk-dnesp32s3`。
    pub name: String,
    /// 资产 slug，例如 `alientek-atk-dnesp32s3`。
    pub full_name: String,
    /// 芯片目标，例如 `esp32s3`。
    pub target: String,
}

/// 匹配到固件后的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedFirmware {
    /// 固件版本号（去前导 `v`）。
    pub version: String,
    /// 命中的板型变体。
    pub variant: ManifestVariant,
    /// 命中的 release 资产。
    pub asset: ReleaseAsset,
}

/// 精确匹配失败时提供给用户选择的候选固件。
///
/// 设备上报的 `board.type` 是 `config.json` 里的短类型（如
/// `sp-esp32-s3-1.28-box`），而 manifest 的 `board` 是板子目录路径
/// （如 `spotpear/sp-esp32-s3-1.28-box`）。两者对不上时不再直接报
/// 「无固件」，而是按名称相似度给出候选，由用户确认后再刷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareCandidate {
    /// 用户选择时回传的稳定标识（manifest 的 `full_name`）。
    pub full_name: String,
    /// manifest 的板型路径。
    pub board: String,
    /// manifest 的板型短名。
    pub name: String,
    /// 芯片目标。
    pub target: String,
    /// 对应的 release 资产名。
    pub asset: String,
    /// 资产大小（字节）。
    pub asset_size: u64,
    /// 芯片型号是否与设备一致（设备未上报芯片时视为 true）。
    pub chip_match: bool,
    /// 相似度分数（仅用于排序/展示）。
    pub score: u32,
    /// 中文匹配原因，例如「构建名匹配」。
    pub reason: String,
}

/// GitHub 交互错误。
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("GitHub 请求失败: {0}")]
    Http(String),
    #[error("GitHub API 限流，请稍后重试或配置 GITHUB_TOKEN")]
    RateLimited(Option<String>),
    #[error("GitHub 响应解析失败: {0}")]
    Parse(String),
    #[error("release 不存在: {0}")]
    NotFound(String),
    #[error("release 缺少 release-manifest.json: {0}")]
    ManifestMissing(String),
    #[error("该版本没有匹配此板型的固件（board={board}, name={name}）")]
    BoardNotFound { board: String, name: String },
    #[error("多个候选固件: {0}")]
    BoardAmbiguous(String),
    #[error("芯片型号不匹配: 设备={device}, 固件={firmware}")]
    ChipMismatch { device: String, firmware: String },
    #[error("release 中没有匹配的固件资产: {0}")]
    AssetNotFound(String),
    #[error("用户选择的固件变体不存在: {0}")]
    VariantNotFound(String),
}

// ═══════════════════════════════════════════════════════════════════════════════
// ReleaseService
// ═══════════════════════════════════════════════════════════════════════════════

/// 目录缓存内容。
struct CachedReleases {
    releases: Vec<ReleaseInfo>,
    fetched_at_ms: i64,
}

/// 一次原始 HTTP 响应（已读出 body，便于按状态码分支处理）。
struct HttpReply {
    status: u16,
    body: Vec<u8>,
    rate_limit_reset: Option<String>,
}

/// GitHub Release 查询服务。
///
/// 内部持有两个 HTTP 客户端：JSON 客户端带 `Authorization`，
/// 下载客户端不带（固件资产是公开的，避免令牌跟着重定向跑到别的域名）。
pub struct ReleaseService {
    api_base: String,
    download_base: String,
    client: reqwest::Client,
    download_client: reqwest::Client,
    catalog: Mutex<Option<CachedReleases>>,
    manifests: Mutex<HashMap<String, ReleaseManifest>>,
    sha256sums: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl ReleaseService {
    /// 用显式基地址创建服务（不读取令牌）。
    ///
    /// `None` 表示使用默认地址；显式传入的地址必须以 `http://` 或 `https://` 开头。
    pub fn new(api_base: Option<String>, download_base: Option<String>) -> Result<Self, String> {
        Self::build(api_base, download_base, None)
    }

    /// 从环境变量创建服务（含可选 GitHub 令牌）。
    pub fn from_env() -> Result<Self, String> {
        // 令牌是敏感信息：只放进请求头，绝不写日志或拼进错误信息。
        let token =
            read_optional_env("GITHUB_TOKEN").or_else(|| read_optional_env("HAIMEN_GITHUB_TOKEN"));
        let api_base = read_optional_env("HAIMEN_XIAOZHI_GITHUB_API_BASE");
        let download_base = read_optional_env("HAIMEN_XIAOZHI_GITHUB_DOWNLOAD_BASE");
        Self::build(api_base, download_base, token)
    }

    fn build(
        api_base: Option<String>,
        download_base: Option<String>,
        token: Option<String>,
    ) -> Result<Self, String> {
        let api_base = normalize_base(api_base.as_deref(), DEFAULT_API_BASE, "GitHub API 基地址")?;
        let download_base = normalize_base(
            download_base.as_deref(),
            DEFAULT_DOWNLOAD_BASE,
            "GitHub 下载基地址",
        )?;

        let user_agent = HeaderValue::from_static(concat!("haimen/", env!("CARGO_PKG_VERSION")));
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, user_agent.clone());
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        if let Some(token) = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
        {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| "GitHub Token 含有非法字符".to_string())?;
            headers.insert(AUTHORIZATION, value);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| format!("创建 GitHub HTTP 客户端失败: {e}"))?;

        let mut download_headers = HeaderMap::new();
        download_headers.insert(USER_AGENT, user_agent);
        let download_client = reqwest::Client::builder()
            .default_headers(download_headers)
            .build()
            .map_err(|e| format!("创建固件下载客户端失败: {e}"))?;

        Ok(Self {
            api_base,
            download_base,
            client,
            download_client,
            catalog: Mutex::new(None),
            manifests: Mutex::new(HashMap::new()),
            sha256sums: Mutex::new(HashMap::new()),
        })
    }

    /// 拉取 release 目录。
    ///
    /// - `refresh = false` 且缓存未过期：直接返回缓存（`stale = false`）。
    /// - `refresh = true`：强制打 API；失败时若有旧缓存则返回旧缓存（`stale = true`）。
    /// - 没有任何缓存且请求失败：返回错误。
    pub async fn releases(&self, refresh: bool) -> Result<ReleasesPage, GithubError> {
        let mut guard = self.catalog.lock().await;

        if !refresh {
            if let Some(cached) = guard.as_ref() {
                let age = now_ms().saturating_sub(cached.fetched_at_ms);
                if age < CATALOG_TTL_MS {
                    return Ok(page_from_cache(cached, false));
                }
            }
        }

        let url = format!(
            "{}/repos/{REPO}/releases?per_page={RELEASES_PER_PAGE}",
            self.api_base
        );
        match self.fetch_json(&url).await.and_then(|reply| {
            let body = ensure_success(reply, "release 列表")?;
            parse_releases(&body)
        }) {
            Ok(releases) => {
                let fetched_at_ms = now_ms();
                *guard = Some(CachedReleases {
                    releases: releases.clone(),
                    fetched_at_ms,
                });
                Ok(ReleasesPage {
                    releases,
                    fetched_at_ms,
                    stale: false,
                })
            }
            Err(err) => match guard.as_ref() {
                Some(cached) => Ok(page_from_cache(cached, true)),
                None => Err(err),
            },
        }
    }

    /// 按 tag 查询单个 release。
    pub async fn release_by_tag(&self, tag: &str) -> Result<ReleaseInfo, GithubError> {
        let url = format!("{}/repos/{REPO}/releases/tags/{tag}", self.api_base);
        let reply = self.fetch_json(&url).await?;
        if reply.status == 404 {
            return Err(GithubError::NotFound(tag.to_string()));
        }
        let body = ensure_success(reply, "release 详情")?;
        parse_release(&body)
    }

    /// 拉取某个 tag 的 `release-manifest.json`（按 tag 永久缓存）。
    pub async fn manifest(&self, tag: &str) -> Result<ReleaseManifest, GithubError> {
        {
            let guard = self.manifests.lock().await;
            if let Some(manifest) = guard.get(tag) {
                return Ok(manifest.clone());
            }
        }

        let url = format!(
            "{}/78/xiaozhi-esp32/releases/download/{tag}/release-manifest.json",
            self.download_base
        );
        let reply = self.fetch_json(&url).await?;
        if reply.status == 404 {
            return Err(GithubError::ManifestMissing(tag.to_string()));
        }
        let body = ensure_success(reply, "release-manifest.json")?;
        let manifest = parse_manifest(&body)?;

        let mut guard = self.manifests.lock().await;
        guard.insert(tag.to_string(), manifest.clone());
        Ok(manifest)
    }

    /// 查询固件资产的 SHA-256。
    ///
    /// 优先使用 GitHub API 返回的 `digest`；没有时回退到 `SHA256SUMS`。
    /// 两者都没有返回 `Ok(None)`。
    pub async fn zip_sha256(
        &self,
        tag: &str,
        asset: &ReleaseAsset,
    ) -> Result<Option<String>, GithubError> {
        if let Some(digest) = asset.digest.as_deref() {
            if let Some(hex) = sha256_from_digest(digest) {
                return Ok(Some(hex));
            }
        }

        let sums = self.sha256sums(tag).await?;
        Ok(sums.get(&asset.name).map(|hash| hash.to_ascii_lowercase()))
    }

    /// 下载完整 URL 指向的字节，超过 `max_bytes` 立即失败。
    pub async fn download(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, GithubError> {
        let response = self
            .download_client
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| GithubError::Http(format!("下载固件失败: {e}")))?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(GithubError::Http(format!("下载固件失败 (HTTP {status})")));
        }

        let content_length = response.content_length();
        if let Some(length) = content_length {
            if length > max_bytes as u64 {
                return Err(GithubError::Http(format!(
                    "固件大小 {length} 超过上限 {max_bytes} 字节"
                )));
            }
        }

        let capacity = content_length.unwrap_or(0).min(max_bytes as u64) as usize;
        let mut data = Vec::with_capacity(capacity);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| GithubError::Http(format!("读取固件数据失败: {e}")))?;
            if data.len() + chunk.len() > max_bytes {
                return Err(GithubError::Http(format!(
                    "固件大小超过上限 {max_bytes} 字节"
                )));
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    // ── 内部 HTTP ─────────────────────────────────────────────────────────────

    /// 发一次 JSON 请求并读出完整响应体（状态码由调用方判断）。
    async fn fetch_json(&self, url: &str) -> Result<HttpReply, GithubError> {
        let response = self
            .client
            .get(url)
            .timeout(JSON_TIMEOUT)
            .send()
            .await
            .map_err(|e| GithubError::Http(format!("请求 GitHub 失败: {e}")))?;

        let status = response.status().as_u16();
        let rate_limit_reset = response
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .await
            .map_err(|e| GithubError::Http(format!("读取 GitHub 响应失败: {e}")))?
            .to_vec();

        Ok(HttpReply {
            status,
            body,
            rate_limit_reset,
        })
    }

    /// 拉取并按 tag 缓存 `SHA256SUMS`。
    ///
    /// 404 视为「该 release 没提供校验和」，缓存空表并返回空表。
    async fn sha256sums(&self, tag: &str) -> Result<HashMap<String, String>, GithubError> {
        {
            let guard = self.sha256sums.lock().await;
            if let Some(sums) = guard.get(tag) {
                return Ok(sums.clone());
            }
        }

        let url = format!(
            "{}/78/xiaozhi-esp32/releases/download/{tag}/SHA256SUMS",
            self.download_base
        );
        let reply = self.fetch_json(&url).await?;
        if reply.status == 404 {
            let mut guard = self.sha256sums.lock().await;
            guard.insert(tag.to_string(), HashMap::new());
            return Ok(HashMap::new());
        }

        let body = ensure_success(reply, "SHA256SUMS")?;
        let sums = parse_sha256sums(&String::from_utf8_lossy(&body));

        let mut guard = self.sha256sums.lock().await;
        guard.insert(tag.to_string(), sums.clone());
        Ok(sums)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 纯函数：解析
// ═══════════════════════════════════════════════════════════════════════════════

/// GitHub release 列表 JSON 的原始结构（只取需要的字段）。
#[derive(Debug, Deserialize)]
struct RawRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

/// release 资产 JSON 的原始结构。
#[derive(Debug, Deserialize)]
struct RawAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

impl RawRelease {
    fn into_info(self) -> ReleaseInfo {
        ReleaseInfo {
            version: strip_v(&self.tag_name),
            tag: self.tag_name,
            name: self.name,
            published_at: self.published_at.unwrap_or_default(),
            prerelease: self.prerelease,
            draft: self.draft,
            html_url: self.html_url,
            assets: self
                .assets
                .into_iter()
                .map(|asset| ReleaseAsset {
                    name: asset.name,
                    download_url: asset
                        .browser_download_url
                        .or(asset.download_url)
                        .unwrap_or_default(),
                    size: asset.size,
                    digest: asset.digest,
                })
                .collect(),
        }
    }
}

/// `release-manifest.json` 的原始结构（其余字段忽略）。
#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    version: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    variants: Vec<RawVariant>,
}

/// manifest variant 的原始结构。
#[derive(Debug, Deserialize)]
struct RawVariant {
    #[serde(default)]
    board: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    target: String,
}

/// 解析 release 列表 JSON，按发布时间降序排列。
pub fn parse_releases(json: &[u8]) -> Result<Vec<ReleaseInfo>, GithubError> {
    let raw: Vec<RawRelease> = serde_json::from_slice(json)
        .map_err(|e| GithubError::Parse(format!("release 列表 JSON 无效: {e}")))?;
    let mut releases: Vec<ReleaseInfo> = raw.into_iter().map(RawRelease::into_info).collect();
    // published_at 是 RFC3339 的 UTC 字符串，字典序即时间序。
    releases.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    Ok(releases)
}

/// 解析单个 release JSON（`release_by_tag` 用）。
fn parse_release(json: &[u8]) -> Result<ReleaseInfo, GithubError> {
    let raw: RawRelease = serde_json::from_slice(json)
        .map_err(|e| GithubError::Parse(format!("release JSON 无效: {e}")))?;
    Ok(raw.into_info())
}

/// 解析 `release-manifest.json`。
pub fn parse_manifest(json: &[u8]) -> Result<ReleaseManifest, GithubError> {
    // serde 允许把结构体从 JSON 数组按字段顺序反序列化，这里显式要求顶层是对象，
    // 避免把别处的数组（比如 release 列表）误当成 manifest。
    if first_json_byte(json) != Some(b'{') {
        return Err(GithubError::Parse(
            "release-manifest.json 顶层必须是 JSON 对象".to_string(),
        ));
    }
    let raw: RawManifest = serde_json::from_slice(json)
        .map_err(|e| GithubError::Parse(format!("release-manifest.json 无效: {e}")))?;
    Ok(ReleaseManifest {
        schema_version: raw.schema_version,
        version: raw.version,
        tag: raw.tag,
        variants: raw
            .variants
            .into_iter()
            .map(|variant| ManifestVariant {
                board: variant.board,
                name: variant.name,
                full_name: variant.full_name,
                target: variant.target,
            })
            .collect(),
    })
}

/// 返回 JSON 文本的第一个非空白字节，用于检查顶层类型。
fn first_json_byte(json: &[u8]) -> Option<u8> {
    json.iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

/// 解析 `SHA256SUMS`。
///
/// 每行形如 `<hash>  <name>`，GNU 风格的可执行文件校验和会写成 `<hash> *<name>`，
/// 这里一并去掉名字前的 `*`。空行与 `#` 注释行跳过。
pub fn parse_sha256sums(text: &str) -> HashMap<String, String> {
    let mut sums = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = rest
            .trim_start_matches(char::is_whitespace)
            .trim_start_matches('*')
            .trim();
        let hash = hash.trim();
        if hash.is_empty() || name.is_empty() {
            continue;
        }
        sums.insert(name.to_string(), hash.to_string());
    }
    sums
}

/// 取最新的正式版 release（跳过 draft 与 prerelease）。
pub fn latest_release(releases: &[ReleaseInfo]) -> Option<&ReleaseInfo> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .max_by(|a, b| a.published_at.cmp(&b.published_at))
}

// ═══════════════════════════════════════════════════════════════════════════════
// 纯函数：板型匹配
// ═══════════════════════════════════════════════════════════════════════════════

/// 按设备上报的 board.type / board.name / chip_model 匹配固件。
///
/// - `manifest = Some(..)`：新 release，按 manifest variant 精确匹配。
/// - `manifest = None`：老 release，按资产文件名后缀推断。
pub fn match_firmware(
    release: &ReleaseInfo,
    manifest: Option<&ReleaseManifest>,
    board_type: &str,
    board_name: &str,
    chip_model: &str,
) -> Result<MatchedFirmware, GithubError> {
    match manifest {
        Some(manifest) => {
            match_with_manifest(release, manifest, board_type, board_name, chip_model)
        }
        None => match_without_manifest(release, board_type, board_name, chip_model),
    }
}

/// 有 manifest 时的匹配：board（忽略大小写）→ name → full_name 逐级收敛。
fn match_with_manifest(
    release: &ReleaseInfo,
    manifest: &ReleaseManifest,
    board_type: &str,
    board_name: &str,
    chip_model: &str,
) -> Result<MatchedFirmware, GithubError> {
    let mut candidates: Vec<&ManifestVariant> = manifest
        .variants
        .iter()
        .filter(|variant| variant.board.eq_ignore_ascii_case(board_type))
        .collect();

    if candidates.len() > 1 && !board_name.trim().is_empty() {
        // 先按短名收敛；短名对不上再试 full_name（设备可能上报的是资产 slug）。
        let by_name: Vec<&ManifestVariant> = candidates
            .iter()
            .copied()
            .filter(|variant| variant.name == board_name)
            .collect();
        if !by_name.is_empty() {
            candidates = by_name;
        } else {
            let by_full_name: Vec<&ManifestVariant> = candidates
                .iter()
                .copied()
                .filter(|variant| variant.full_name == board_name)
                .collect();
            if !by_full_name.is_empty() {
                candidates = by_full_name;
            }
        }
    }

    let variant = match candidates.len() {
        0 => {
            return Err(GithubError::BoardNotFound {
                board: board_type.to_string(),
                name: board_name.to_string(),
            });
        }
        1 => candidates[0].clone(),
        _ => {
            return Err(GithubError::BoardAmbiguous(describe_variants(&candidates)));
        }
    };

    // 芯片型号：两边都有值时比较（统一小写并去掉 -/_，兼容 ESP32-S3 这类写法）。
    if !chip_model.trim().is_empty()
        && !variant.target.trim().is_empty()
        && normalize_chip(chip_model) != normalize_chip(&variant.target)
    {
        return Err(GithubError::ChipMismatch {
            device: chip_model.to_string(),
            firmware: variant.target.clone(),
        });
    }

    let version = firmware_version(release, Some(manifest));
    let asset = pick_manifest_asset(release, &variant)?;
    Ok(MatchedFirmware {
        version,
        variant,
        asset,
    })
}

/// 有 manifest 时的资产选择：先试 `{tag}_{full_name}.zip`，再退到唯一后缀。
fn pick_manifest_asset(
    release: &ReleaseInfo,
    variant: &ManifestVariant,
) -> Result<ReleaseAsset, GithubError> {
    let exact_name = format!("{}_{}.zip", release.tag, variant.full_name);
    if let Some(asset) = release.assets.iter().find(|asset| asset.name == exact_name) {
        return Ok(asset.clone());
    }

    let suffix = format!("_{}.zip", variant.full_name);
    let matches: Vec<&ReleaseAsset> = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(&suffix))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(GithubError::AssetNotFound(exact_name)),
        _ => Err(GithubError::AssetNotFound(format!(
            "候选资产不唯一: {}",
            matches
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// 精确匹配失败时，按相似度返回可选的固件变体，交给用户确认。
///
/// 只保留「有对应资产」的变体；按分数、芯片匹配、名称排序，最多
/// [`MAX_CANDIDATES`] 个。分数为 0 的变体不返回，避免把完全无关的板型
/// 塞给用户。
pub fn candidate_firmwares(
    release: &ReleaseInfo,
    manifest: &ReleaseManifest,
    board_type: &str,
    board_name: &str,
    chip_model: &str,
) -> Vec<FirmwareCandidate> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for variant in &manifest.variants {
        if !seen.insert(variant.full_name.to_ascii_lowercase()) {
            continue;
        }
        let (score, reason) = candidate_score(variant, board_type, board_name);
        if score == 0 {
            continue;
        }
        let Ok(asset) = pick_manifest_asset(release, variant) else {
            continue;
        };
        let chip_match = chip_model.trim().is_empty()
            || variant.target.trim().is_empty()
            || normalize_chip(chip_model) == normalize_chip(&variant.target);
        candidates.push(FirmwareCandidate {
            full_name: variant.full_name.clone(),
            board: variant.board.clone(),
            name: variant.name.clone(),
            target: variant.target.clone(),
            asset: asset.name,
            asset_size: asset.size,
            chip_match,
            score,
            reason: reason.to_string(),
        });
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.chip_match.cmp(&a.chip_match))
            .then_with(|| a.name.cmp(&b.name))
    });
    candidates.truncate(MAX_CANDIDATES);
    candidates
}

/// 按用户选择的 `full_name`（也兼容 name/board）取固件变体。
///
/// 仍然执行芯片校验：板型可以由用户确认，但把别的芯片固件刷进去会变砖。
pub fn match_candidate(
    release: &ReleaseInfo,
    manifest: &ReleaseManifest,
    full_name: &str,
    chip_model: &str,
) -> Result<MatchedFirmware, GithubError> {
    let wanted = full_name.trim();
    if wanted.is_empty() {
        return Err(GithubError::VariantNotFound("empty".to_string()));
    }
    let variant = manifest
        .variants
        .iter()
        .find(|variant| {
            variant.full_name.eq_ignore_ascii_case(wanted)
                || variant.name.eq_ignore_ascii_case(wanted)
                || variant.board.eq_ignore_ascii_case(wanted)
        })
        .ok_or_else(|| GithubError::VariantNotFound(wanted.to_string()))?;

    if !chip_model.trim().is_empty()
        && !variant.target.trim().is_empty()
        && normalize_chip(chip_model) != normalize_chip(&variant.target)
    {
        return Err(GithubError::ChipMismatch {
            device: chip_model.to_string(),
            firmware: variant.target.clone(),
        });
    }

    let asset = pick_manifest_asset(release, variant)?;
    Ok(MatchedFirmware {
        version: firmware_version(release, Some(manifest)),
        variant: variant.clone(),
        asset,
    })
}

/// 候选变体的相似度评分 + 中文原因。
fn candidate_score(
    variant: &ManifestVariant,
    board_type: &str,
    board_name: &str,
) -> (u32, &'static str) {
    let board_type = board_type.trim();
    let board_name = board_name.trim();

    if !board_name.is_empty() && variant.name.eq_ignore_ascii_case(board_name) {
        return (100, "构建名匹配");
    }
    if !board_type.is_empty() && variant.board.eq_ignore_ascii_case(board_type) {
        return (95, "板型路径匹配");
    }
    if !board_type.is_empty() && board_basename(&variant.board).eq_ignore_ascii_case(board_type) {
        return (90, "板型目录末段匹配");
    }
    if !board_name.is_empty() && variant.full_name.eq_ignore_ascii_case(board_name) {
        return (85, "资产名匹配");
    }
    if contains_normalized(&variant.name, board_type)
        || contains_normalized(&variant.name, board_name)
    {
        return (60, "名称包含板型");
    }
    if contains_normalized(&variant.full_name, board_type)
        || contains_normalized(&variant.board, board_type)
    {
        return (50, "资产/目录包含板型");
    }
    let overlap = token_overlap(&variant.name, board_type)
        + token_overlap(&variant.name, board_name)
        + token_overlap(&variant.full_name, board_type);
    if overlap >= 2 {
        return (20 + overlap.min(5) as u32 * 5, "名称片段相似");
    }
    (0, "")
}

fn board_basename(board: &str) -> &str {
    board.rsplit('/').next().unwrap_or(board)
}

/// 归一化：只保留 ASCII 字母数字并转小写，用于宽松包含判断。
fn normalize_match_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn contains_normalized(haystack: &str, needle: &str) -> bool {
    let needle = normalize_match_token(needle);
    // 太短的片段（如 "s3"）容易误匹配，要求至少 4 个字符。
    needle.len() >= 4 && normalize_match_token(haystack).contains(&needle)
}

fn token_overlap(a: &str, b: &str) -> usize {
    fn tokens(value: &str) -> HashSet<String> {
        value
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .map(|token| token.to_ascii_lowercase())
            .filter(|token| {
                token.len() >= 2
                    && !matches!(
                        token.as_str(),
                        "esp32"
                            | "esp32s3"
                            | "s3"
                            | "c3"
                            | "p4"
                            | "wifi"
                            | "box"
                            | "board"
                            | "xiaozhi"
                            | "ai"
                    )
            })
            .collect()
    }
    tokens(a).intersection(&tokens(b)).count()
}

/// 老 release（没有 manifest）时按资产名后缀匹配。
///
/// 设备上报的 `board.type` 是 `厂商/板名`，资产文件名里是 `厂商-板名`；
/// 设备上报的 `board.name` 又可能只是板名。因此准备多个候选 slug，按顺序试。
fn match_without_manifest(
    release: &ReleaseInfo,
    board_type: &str,
    board_name: &str,
    chip_model: &str,
) -> Result<MatchedFirmware, GithubError> {
    let slugs = legacy_slugs(board_type, board_name);
    if slugs.is_empty() {
        return Err(GithubError::BoardNotFound {
            board: board_type.to_string(),
            name: board_name.to_string(),
        });
    }

    // 一个资产可能同时命中多个 slug（比如 board_name 正好是 board.type 的后半段），
    // 所以按资产遍历而不是按 slug 遍历，天然去重。
    let mut hits: Vec<(String, ReleaseAsset)> = Vec::new();
    for asset in &release.assets {
        if let Some(slug) = slugs
            .iter()
            .find(|slug| asset.name.ends_with(&format!("_{slug}.zip")))
        {
            hits.push((slug.clone(), asset.clone()));
        }
    }

    match hits.len() {
        0 => Err(GithubError::AssetNotFound(format!(
            "board.type={board_type}, board.name={board_name}"
        ))),
        1 => {
            let (slug, asset) = hits.remove(0);
            // 老 release 没有 manifest，用设备信息合成一个 variant 供上层展示。
            let variant = ManifestVariant {
                board: board_type.to_string(),
                name: if board_name.trim().is_empty() {
                    slug.clone()
                } else {
                    board_name.to_string()
                },
                full_name: slug,
                target: chip_model.to_string(),
            };
            Ok(MatchedFirmware {
                version: firmware_version(release, None),
                variant,
                asset,
            })
        }
        _ => Err(GithubError::BoardAmbiguous(format!(
            "候选资产: {}",
            hits.iter()
                .map(|(_, asset)| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// 老 release 的候选 slug：board_name、board.type 的 `/`→`-` 形式、board.type 本身。
fn legacy_slugs(board_type: &str, board_name: &str) -> Vec<String> {
    let mut slugs: Vec<String> = Vec::new();
    for candidate in [
        board_name.trim().to_string(),
        board_type.trim().replace('/', "-"),
        board_type.trim().to_string(),
    ] {
        if !candidate.is_empty() && !slugs.contains(&candidate) {
            slugs.push(candidate);
        }
    }
    slugs
}

/// 拼接多个候选 variant 的描述（歧义错误用）。
fn describe_variants(variants: &[&ManifestVariant]) -> String {
    variants
        .iter()
        .map(|variant| {
            format!(
                "board={}, name={}, full_name={}",
                variant.board, variant.name, variant.full_name
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// 固件版本号：manifest.version 优先，为空则用 release.version；都去前导 `v`。
fn firmware_version(release: &ReleaseInfo, manifest: Option<&ReleaseManifest>) -> String {
    let version = manifest
        .map(|manifest| manifest.version.trim())
        .filter(|version| !version.is_empty())
        .unwrap_or(release.version.trim());
    strip_v(version)
}

/// 芯片型号归一化：小写并去掉 `-` / `_`。
fn normalize_chip(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect()
}

/// 去掉版本号/tag 的前导 `v`。
fn strip_v(value: &str) -> String {
    value.strip_prefix('v').unwrap_or(value).to_string()
}

/// 从 GitHub `digest`（`sha256:<hex>`）中提取十六进制摘要。
fn sha256_from_digest(digest: &str) -> Option<String> {
    let (algorithm, hex) = digest.trim().split_once(':')?;
    if algorithm.eq_ignore_ascii_case("sha256") && !hex.trim().is_empty() {
        Some(hex.trim().to_ascii_lowercase())
    } else {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 内部工具
// ═══════════════════════════════════════════════════════════════════════════════

/// 读取环境变量，空字符串视为未设置。
fn read_optional_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 归一化基地址：空值用默认值，去掉末尾 `/`，校验 http(s) 前缀。
fn normalize_base(raw: Option<&str>, default: &str, label: &str) -> Result<String, String> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default);
    let value = value.trim_end_matches('/');
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return Err(format!("{label}必须是 http(s) 地址"));
    }
    Ok(value.to_string())
}

/// 把成功的响应体取出来；非 2xx 时映射成限流 / HTTP 错误。
fn ensure_success(reply: HttpReply, what: &str) -> Result<Vec<u8>, GithubError> {
    if (200..300).contains(&reply.status) {
        return Ok(reply.body);
    }
    if is_rate_limited(reply.status, &reply.body) {
        return Err(GithubError::RateLimited(reply.rate_limit_reset));
    }
    let detail = String::from_utf8_lossy(&reply.body);
    let detail = detail.trim();
    let detail: String = detail.chars().take(200).collect();
    if detail.is_empty() {
        Err(GithubError::Http(format!(
            "{what} 请求失败 (HTTP {})",
            reply.status
        )))
    } else {
        Err(GithubError::Http(format!(
            "{what} 请求失败 (HTTP {}): {detail}",
            reply.status
        )))
    }
}

/// GitHub 限流响应：HTTP 403/429 且 body 含 `rate limit`。
fn is_rate_limited(status: u16, body: &[u8]) -> bool {
    if status != 403 && status != 429 {
        return false;
    }
    String::from_utf8_lossy(body)
        .to_ascii_lowercase()
        .contains("rate limit")
}

/// 当前 Unix 毫秒时间戳。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 由缓存构造对外返回的目录页。
fn page_from_cache(cached: &CachedReleases, stale: bool) -> ReleasesPage {
    ReleasesPage {
        releases: cached.releases.clone(),
        fetched_at_ms: cached.fetched_at_ms,
        stale,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;

    use super::*;

    // ── 内联 JSON 夹具 ───────────────────────────────────────────────────────

    const RELEASES_JSON: &str = r#"[
      {
        "tag_name": "v2.0.0",
        "name": "v2.0.0",
        "published_at": "2026-01-02T00:00:00Z",
        "prerelease": false,
        "draft": false,
        "html_url": "https://github.com/78/xiaozhi-esp32/releases/tag/v2.0.0",
        "assets": [
          {
            "name": "v2.0.0_bread-compact-wifi.zip",
            "browser_download_url": "https://github.com/78/xiaozhi-esp32/releases/download/v2.0.0/v2.0.0_bread-compact-wifi.zip",
            "size": 123,
            "digest": "sha256:abc123"
          }
        ]
      },
      {
        "tag_name": "v2.1.0-rc1",
        "name": "v2.1.0-rc1",
        "published_at": "2026-03-01T00:00:00Z",
        "prerelease": true,
        "draft": false,
        "html_url": "https://github.com/78/xiaozhi-esp32/releases/tag/v2.1.0-rc1",
        "assets": []
      },
      {
        "tag_name": "v1.9.0",
        "name": "v1.9.0",
        "published_at": "2026-04-01T00:00:00Z",
        "prerelease": false,
        "draft": true,
        "html_url": "https://github.com/78/xiaozhi-esp32/releases/tag/v1.9.0",
        "assets": []
      },
      {
        "tag_name": "v2.0.1",
        "name": "v2.0.1",
        "published_at": "2026-02-01T00:00:00Z",
        "prerelease": false,
        "draft": false,
        "html_url": "https://github.com/78/xiaozhi-esp32/releases/tag/v2.0.1",
        "assets": []
      }
    ]"#;

    const RELEASE_ONE_JSON: &str = r#"{
      "tag_name": "v2.5.2",
      "name": "v2.5.2",
      "published_at": "2026-10-11T20:49:36Z",
      "prerelease": false,
      "draft": false,
      "html_url": "https://github.com/78/xiaozhi-esp32/releases/tag/v2.5.2",
      "assets": [
        {
          "name": "v2.5.2_bread-compact-wifi.zip",
          "browser_download_url": "https://github.com/78/xiaozhi-esp32/releases/download/v2.5.2/v2.5.2_bread-compact-wifi.zip",
          "size": 4096,
          "digest": "sha256:def456"
        }
      ]
    }"#;

    const MANIFEST_JSON: &str = r#"{
      "schema_version": 1,
      "version": "2.5.2",
      "tag": "v2.5.2",
      "source_sha": "49b4380b3faa0ceadd28a3e78aaf0f311c46c57e",
      "idf_version": "6.1.0",
      "variants": [
        {
          "board": "alientek/atk-dnesp32s3",
          "name": "atk-dnesp32s3",
          "full_name": "alientek-atk-dnesp32s3",
          "target": "esp32s3"
        },
        {
          "board": "bread-compact-esp32",
          "name": "bread-compact-esp32",
          "full_name": "bread-compact-esp32",
          "target": "esp32"
        }
      ]
    }"#;

    const SHA256SUMS_TEXT: &str = "\
def456  v2.5.2_bread-compact-wifi.zip
abc123 *v2.5.2_bread-compact-esp32.zip

";

    // ── JSON 解析 ────────────────────────────────────────────────────────────

    #[test]
    fn parse_releases_sorts_and_strips_v() {
        let releases = parse_releases(RELEASES_JSON.as_bytes()).unwrap();
        assert_eq!(releases.len(), 4);
        // 按 published_at 降序：draft 的 v1.9.0 发布时间最晚，排第一。
        let tags: Vec<&str> = releases.iter().map(|r| r.tag.as_str()).collect();
        assert_eq!(tags, vec!["v1.9.0", "v2.1.0-rc1", "v2.0.1", "v2.0.0"]);

        let v2 = releases.iter().find(|r| r.tag == "v2.0.0").unwrap();
        assert_eq!(v2.version, "2.0.0");
        assert_eq!(v2.name, "v2.0.0");
        assert!(!v2.draft && !v2.prerelease);
        assert!(v2.html_url.ends_with("/v2.0.0"));
        assert_eq!(v2.assets.len(), 1);
        assert_eq!(v2.assets[0].name, "v2.0.0_bread-compact-wifi.zip");
        assert!(
            v2.assets[0]
                .download_url
                .contains("releases/download/v2.0.0")
        );
        assert_eq!(v2.assets[0].size, 123);
        assert_eq!(v2.assets[0].digest.as_deref(), Some("sha256:abc123"));

        let draft = releases.iter().find(|r| r.tag == "v1.9.0").unwrap();
        assert!(draft.draft);
        let prerelease = releases.iter().find(|r| r.tag == "v2.1.0-rc1").unwrap();
        assert!(prerelease.prerelease);
    }

    #[test]
    fn parse_releases_rejects_invalid_json() {
        assert!(matches!(parse_releases(b"{}"), Err(GithubError::Parse(_))));
        assert!(matches!(
            parse_releases(b"not json"),
            Err(GithubError::Parse(_))
        ));
        assert_eq!(parse_releases(b"[]").unwrap().len(), 0);
    }

    #[test]
    fn parse_manifest_reads_variants() {
        let manifest = parse_manifest(MANIFEST_JSON.as_bytes()).unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.version, "2.5.2");
        assert_eq!(manifest.tag, "v2.5.2");
        assert_eq!(manifest.variants.len(), 2);
        assert_eq!(manifest.variants[0].board, "alientek/atk-dnesp32s3");
        assert_eq!(manifest.variants[0].name, "atk-dnesp32s3");
        assert_eq!(manifest.variants[0].full_name, "alientek-atk-dnesp32s3");
        assert_eq!(manifest.variants[0].target, "esp32s3");
        // 顶层必须是对象：数组 / 字段类型不对都算解析失败
        assert!(matches!(parse_manifest(b"[]"), Err(GithubError::Parse(_))));
        assert!(matches!(
            parse_manifest(b"{\"variants\": 1}"),
            Err(GithubError::Parse(_))
        ));
        assert!(matches!(
            parse_manifest(b"not json"),
            Err(GithubError::Parse(_))
        ));
    }

    #[test]
    fn parse_sha256sums_handles_gnu_style() {
        let sums = parse_sha256sums(SHA256SUMS_TEXT);
        assert_eq!(
            sums.get("v2.5.2_bread-compact-wifi.zip")
                .map(String::as_str),
            Some("def456")
        );
        // GNU 风格名字前的 `*` 被去掉
        assert_eq!(
            sums.get("v2.5.2_bread-compact-esp32.zip")
                .map(String::as_str),
            Some("abc123")
        );
        assert_eq!(sums.len(), 2);
        assert!(parse_sha256sums("").is_empty());
        assert!(parse_sha256sums("garbage-line-without-spaces").is_empty());
    }

    #[test]
    fn latest_release_skips_draft_and_prerelease() {
        let releases = parse_releases(RELEASES_JSON.as_bytes()).unwrap();
        let latest = latest_release(&releases).unwrap();
        assert_eq!(latest.tag, "v2.0.1");

        let only_pre: Vec<ReleaseInfo> = releases
            .into_iter()
            .filter(|release| release.draft || release.prerelease)
            .collect();
        assert!(latest_release(&only_pre).is_none());
        assert!(latest_release(&[]).is_none());
    }

    // ── 匹配夹具 ─────────────────────────────────────────────────────────────

    fn make_asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            download_url: format!("https://example.invalid/{name}"),
            size: 1024,
            digest: None,
        }
    }

    fn make_release(tag: &str, assets: &[&str]) -> ReleaseInfo {
        let tag = tag.to_string();
        ReleaseInfo {
            version: strip_v(&tag),
            tag,
            name: String::new(),
            published_at: "2026-01-01T00:00:00Z".to_string(),
            prerelease: false,
            draft: false,
            html_url: String::new(),
            assets: assets.iter().map(|name| make_asset(name)).collect(),
        }
    }

    fn make_variant(board: &str, name: &str, full_name: &str, target: &str) -> ManifestVariant {
        ManifestVariant {
            board: board.to_string(),
            name: name.to_string(),
            full_name: full_name.to_string(),
            target: target.to_string(),
        }
    }

    fn make_manifest(variants: Vec<ManifestVariant>) -> ReleaseManifest {
        ReleaseManifest {
            schema_version: 1,
            version: "2.5.2".to_string(),
            tag: "v2.5.2".to_string(),
            variants,
        }
    }

    // ── match_firmware：manifest 路径 ────────────────────────────────────────

    #[test]
    fn match_manifest_exact_board_and_chip() {
        let release = make_release("v2.5.2", &["v2.5.2_alientek-atk-dnesp32s3.zip"]);
        let manifest = make_manifest(vec![
            make_variant(
                "alientek/atk-dnesp32s3",
                "atk-dnesp32s3",
                "alientek-atk-dnesp32s3",
                "esp32s3",
            ),
            make_variant(
                "alientek/atk-dnesp32s3-box",
                "atk-dnesp32s3-box",
                "alientek-atk-dnesp32s3-box",
                "esp32s3",
            ),
        ]);
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "alientek/atk-dnesp32s3",
            "atk-dnesp32s3",
            "ESP32-S3",
        )
        .unwrap();
        assert_eq!(matched.version, "2.5.2");
        assert_eq!(matched.variant.full_name, "alientek-atk-dnesp32s3");
        assert_eq!(matched.asset.name, "v2.5.2_alientek-atk-dnesp32s3.zip");
    }

    #[test]
    fn match_manifest_narrows_by_name() {
        let release = make_release(
            "v2.5.2",
            &[
                "v2.5.2_bread-compact-esp32.zip",
                "v2.5.2_bread-compact-esp32-128x32.zip",
            ],
        );
        let manifest = make_manifest(vec![
            make_variant(
                "bread-compact-esp32",
                "bread-compact-esp32",
                "bread-compact-esp32",
                "esp32",
            ),
            make_variant(
                "bread-compact-esp32",
                "bread-compact-esp32-128x32",
                "bread-compact-esp32-128x32",
                "esp32",
            ),
        ]);
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32-128x32",
            "esp32",
        )
        .unwrap();
        assert_eq!(matched.variant.name, "bread-compact-esp32-128x32");
        assert_eq!(matched.asset.name, "v2.5.2_bread-compact-esp32-128x32.zip");
    }

    #[test]
    fn match_manifest_falls_back_to_full_name() {
        let release = make_release("v2.5.2", &["v2.5.2_vendor-full-name.zip"]);
        let manifest = make_manifest(vec![
            make_variant("vendor/board", "short-name", "vendor-full-name", "esp32s3"),
            make_variant("vendor/board", "other-name", "vendor-other", "esp32s3"),
        ]);
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "vendor/board",
            "vendor-full-name",
            "esp32s3",
        )
        .unwrap();
        assert_eq!(matched.variant.full_name, "vendor-full-name");
        assert_eq!(matched.asset.name, "v2.5.2_vendor-full-name.zip");
    }

    #[test]
    fn match_manifest_reports_ambiguous_candidates() {
        let release = make_release("v2.5.2", &[]);
        let manifest = make_manifest(vec![
            make_variant("vendor/board", "a", "vendor-a", "esp32s3"),
            make_variant("vendor/board", "b", "vendor-b", "esp32s3"),
        ]);
        let err = match_firmware(
            &release,
            Some(&manifest),
            "vendor/board",
            "unknown",
            "esp32s3",
        )
        .unwrap_err();
        assert!(
            matches!(err, GithubError::BoardAmbiguous(message) if message.contains("vendor-a"))
        );
    }

    #[test]
    fn match_manifest_reports_missing_board() {
        let release = make_release("v2.5.2", &[]);
        let manifest = make_manifest(vec![make_variant(
            "bread-compact-esp32",
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )]);
        let err = match_firmware(
            &release,
            Some(&manifest),
            "alientek/atk-dnesp32s3",
            "atk-dnesp32s3",
            "esp32s3",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GithubError::BoardNotFound { board, name }
                if board == "alientek/atk-dnesp32s3" && name == "atk-dnesp32s3"
        ));
    }

    #[test]
    fn match_manifest_reports_chip_mismatch() {
        let release = make_release("v2.5.2", &["v2.5.2_bread-compact-esp32.zip"]);
        let manifest = make_manifest(vec![make_variant(
            "bread-compact-esp32",
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32s3",
        )]);
        let err = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32c3",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GithubError::ChipMismatch { device, firmware }
                if device == "esp32c3" && firmware == "esp32s3"
        ));

        // 空芯片型号跳过比较
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32",
            "",
        )
        .unwrap();
        assert_eq!(matched.version, "2.5.2");
    }

    #[test]
    fn match_manifest_prefers_exact_asset_name() {
        let release = make_release(
            "v2.5.2",
            &[
                "v2.5.2_bread-compact-esp32.zip",
                "v2.5.2_dup_bread-compact-esp32.zip",
            ],
        );
        let manifest = make_manifest(vec![make_variant(
            "bread-compact-esp32",
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )]);
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )
        .unwrap();
        assert_eq!(matched.asset.name, "v2.5.2_bread-compact-esp32.zip");
    }

    #[test]
    fn match_manifest_reports_missing_asset() {
        let release = make_release("v2.5.2", &["v2.5.2_other.zip"]);
        let manifest = make_manifest(vec![make_variant(
            "bread-compact-esp32",
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )]);
        let err = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )
        .unwrap_err();
        assert!(
            matches!(err, GithubError::AssetNotFound(message) if message.contains("bread-compact"))
        );
    }

    #[test]
    fn match_manifest_uses_manifest_version_with_v_stripped() {
        let release = make_release("v2.5.2", &["v2.5.2_bread-compact-esp32.zip"]);
        let mut manifest = make_manifest(vec![make_variant(
            "bread-compact-esp32",
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )]);
        manifest.version = "v3.0.1".to_string();
        let matched = match_firmware(
            &release,
            Some(&manifest),
            "bread-compact-esp32",
            "bread-compact-esp32",
            "esp32",
        )
        .unwrap();
        assert_eq!(matched.version, "3.0.1");
    }

    // ── match_firmware：老 release（无 manifest）路径 ─────────────────────────

    #[test]
    fn match_without_manifest_uses_vendor_slug() {
        let release = make_release("v2.0.0", &["v2.0.0_alientek-atk-dnesp32s3.zip"]);
        let matched = match_firmware(
            &release,
            None,
            "alientek/atk-dnesp32s3",
            "atk-dnesp32s3",
            "esp32s3",
        )
        .unwrap();
        assert_eq!(matched.version, "2.0.0");
        assert_eq!(matched.variant.full_name, "alientek-atk-dnesp32s3");
        assert_eq!(matched.variant.target, "esp32s3");
        assert_eq!(matched.asset.name, "v2.0.0_alientek-atk-dnesp32s3.zip");
    }

    #[test]
    fn match_without_manifest_uses_short_board_name() {
        let release = make_release("v2.0.0", &["v2.0.0_atk-dnesp32s3.zip"]);
        let matched = match_firmware(
            &release,
            None,
            "alientek/atk-dnesp32s3",
            "atk-dnesp32s3",
            "esp32s3",
        )
        .unwrap();
        assert_eq!(matched.variant.full_name, "atk-dnesp32s3");
        assert_eq!(matched.asset.name, "v2.0.0_atk-dnesp32s3.zip");
    }

    #[test]
    fn match_without_manifest_reports_ambiguous_assets() {
        let release = make_release("v2.0.0", &["v2.0.0_board.zip", "v2.0.0_vendor-board.zip"]);
        let err = match_firmware(&release, None, "vendor/board", "board", "esp32s3").unwrap_err();
        assert!(
            matches!(err, GithubError::BoardAmbiguous(message) if message.contains("候选资产"))
        );
    }

    #[test]
    fn match_without_manifest_reports_missing_asset() {
        let release = make_release("v2.0.0", &["v2.0.0_other.zip"]);
        let err = match_firmware(
            &release,
            None,
            "alientek/atk-dnesp32s3",
            "atk-dnesp32s3",
            "esp32s3",
        )
        .unwrap_err();
        assert!(matches!(err, GithubError::AssetNotFound(_)));
    }

    // ── ReleaseService：本地 fake server ─────────────────────────────────────

    #[derive(Default)]
    struct FakeGithub {
        releases_requests: AtomicUsize,
        fail_releases: AtomicBool,
        manifest_requests: AtomicUsize,
        manifest_missing: AtomicBool,
        tag_requests: AtomicUsize,
        sha_requests: AtomicUsize,
        download_requests: AtomicUsize,
    }

    async fn fake_releases(State(state): State<Arc<FakeGithub>>) -> Response {
        state.releases_requests.fetch_add(1, Ordering::SeqCst);
        if state.fail_releases.load(Ordering::SeqCst) {
            return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        (StatusCode::OK, RELEASES_JSON).into_response()
    }

    async fn fake_release_by_tag(
        State(state): State<Arc<FakeGithub>>,
        Path(tag): Path<String>,
    ) -> Response {
        state.tag_requests.fetch_add(1, Ordering::SeqCst);
        if tag == "missing" {
            return StatusCode::NOT_FOUND.into_response();
        }
        (StatusCode::OK, RELEASE_ONE_JSON).into_response()
    }

    async fn fake_manifest(State(state): State<Arc<FakeGithub>>) -> Response {
        state.manifest_requests.fetch_add(1, Ordering::SeqCst);
        if state.manifest_missing.load(Ordering::SeqCst) {
            return StatusCode::NOT_FOUND.into_response();
        }
        (StatusCode::OK, MANIFEST_JSON).into_response()
    }

    async fn fake_sha256sums(State(state): State<Arc<FakeGithub>>) -> Response {
        state.sha_requests.fetch_add(1, Ordering::SeqCst);
        (StatusCode::OK, SHA256SUMS_TEXT).into_response()
    }

    async fn fake_download(State(state): State<Arc<FakeGithub>>) -> Response {
        state.download_requests.fetch_add(1, Ordering::SeqCst);
        (StatusCode::OK, vec![7u8; 100]).into_response()
    }

    /// 起一个只监听 127.0.0.1 的假 GitHub，返回 base 与计数器。
    async fn spawn_fake_github() -> (String, Arc<FakeGithub>) {
        let state = Arc::new(FakeGithub::default());
        let app = Router::new()
            .route("/repos/78/xiaozhi-esp32/releases", get(fake_releases))
            .route(
                "/repos/78/xiaozhi-esp32/releases/tags/{tag}",
                get(fake_release_by_tag),
            )
            .route(
                "/78/xiaozhi-esp32/releases/download/{tag}/release-manifest.json",
                get(fake_manifest),
            )
            .route(
                "/78/xiaozhi-esp32/releases/download/{tag}/SHA256SUMS",
                get(fake_sha256sums),
            )
            .route("/files/firmware.bin", get(fake_download))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    fn fake_service(base: &str) -> ReleaseService {
        ReleaseService::new(Some(base.to_string()), Some(base.to_string())).unwrap()
    }

    #[tokio::test]
    async fn releases_uses_cache_and_marks_stale_on_refresh_failure() {
        let (base, state) = spawn_fake_github().await;
        let service = fake_service(&base);

        let first = service.releases(false).await.unwrap();
        assert!(!first.stale);
        assert_eq!(first.releases.len(), 4);
        assert_eq!(state.releases_requests.load(Ordering::SeqCst), 1);

        // TTL 内不刷新：计数保持 1，缓存时间戳一致。
        let cached = service.releases(false).await.unwrap();
        assert!(!cached.stale);
        assert_eq!(cached.fetched_at_ms, first.fetched_at_ms);
        assert_eq!(state.releases_requests.load(Ordering::SeqCst), 1);

        // 强制刷新 + 上游 500：返回旧缓存并标记 stale。
        state.fail_releases.store(true, Ordering::SeqCst);
        let stale = service.releases(true).await.unwrap();
        assert!(stale.stale);
        assert_eq!(stale.releases.len(), first.releases.len());
        assert_eq!(stale.fetched_at_ms, first.fetched_at_ms);
        assert_eq!(state.releases_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn releases_errors_without_any_cache() {
        let (base, state) = spawn_fake_github().await;
        state.fail_releases.store(true, Ordering::SeqCst);
        let service = fake_service(&base);
        assert!(matches!(
            service.releases(false).await,
            Err(GithubError::Http(_))
        ));
    }

    #[tokio::test]
    async fn release_by_tag_parses_and_maps_404() {
        let (base, state) = spawn_fake_github().await;
        let service = fake_service(&base);

        let release = service.release_by_tag("v2.5.2").await.unwrap();
        assert_eq!(release.tag, "v2.5.2");
        assert_eq!(release.version, "2.5.2");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(state.tag_requests.load(Ordering::SeqCst), 1);

        let err = service.release_by_tag("missing").await.unwrap_err();
        assert!(matches!(err, GithubError::NotFound(tag) if tag == "missing"));
    }

    #[tokio::test]
    async fn manifest_is_cached_and_404_maps_to_manifest_missing() {
        let (base, state) = spawn_fake_github().await;
        let service = fake_service(&base);

        let manifest = service.manifest("v2.5.2").await.unwrap();
        assert_eq!(manifest.version, "2.5.2");
        assert_eq!(manifest.variants.len(), 2);

        // 第二次命中缓存，不再请求。
        let again = service.manifest("v2.5.2").await.unwrap();
        assert_eq!(again.tag, "v2.5.2");
        assert_eq!(state.manifest_requests.load(Ordering::SeqCst), 1);

        state.manifest_missing.store(true, Ordering::SeqCst);
        let err = service.manifest("v9.9.9").await.unwrap_err();
        assert!(matches!(err, GithubError::ManifestMissing(tag) if tag == "v9.9.9"));
    }

    #[tokio::test]
    async fn zip_sha256_prefers_digest_then_falls_back_to_sums() {
        let (base, state) = spawn_fake_github().await;
        let service = fake_service(&base);

        let with_digest = ReleaseAsset {
            name: "v2.5.2_bread-compact-wifi.zip".to_string(),
            download_url: String::new(),
            size: 1,
            digest: Some("SHA256:DEF456".to_string()),
        };
        assert_eq!(
            service
                .zip_sha256("v2.5.2", &with_digest)
                .await
                .unwrap()
                .as_deref(),
            Some("def456")
        );
        // digest 已经够用，不应请求 SHA256SUMS。
        assert_eq!(state.sha_requests.load(Ordering::SeqCst), 0);

        let without_digest = ReleaseAsset {
            digest: None,
            ..with_digest
        };
        assert_eq!(
            service
                .zip_sha256("v2.5.2", &without_digest)
                .await
                .unwrap()
                .as_deref(),
            Some("def456")
        );
        // 结果按 tag 缓存，第二次不再请求。
        assert_eq!(
            service
                .zip_sha256("v2.5.2", &without_digest)
                .await
                .unwrap()
                .as_deref(),
            Some("def456")
        );
        assert_eq!(state.sha_requests.load(Ordering::SeqCst), 1);

        // 资产不在 SHA256SUMS 里 -> Ok(None)
        let unknown = ReleaseAsset {
            name: "v2.5.2_unknown.zip".to_string(),
            ..without_digest
        };
        assert!(
            service
                .zip_sha256("v2.5.2", &unknown)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn download_enforces_max_bytes() {
        let (base, state) = spawn_fake_github().await;
        let service = fake_service(&base);
        let url = format!("{base}/files/firmware.bin");

        let err = service.download(&url, 10).await.unwrap_err();
        assert!(matches!(err, GithubError::Http(_)));

        let data = service.download(&url, 1024).await.unwrap();
        assert_eq!(data, vec![7u8; 100]);
        assert_eq!(state.download_requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn new_rejects_invalid_base() {
        assert!(ReleaseService::new(Some("ftp://example.com".to_string()), None).is_err());
        assert!(ReleaseService::new(None, Some("not-a-url".to_string())).is_err());
        assert!(ReleaseService::new(Some("https://api.example.com/".to_string()), None).is_ok());
    }

    #[test]
    fn rate_limit_detection() {
        assert!(is_rate_limited(
            403,
            b"{\"message\":\"API rate limit exceeded\"}"
        ));
        assert!(is_rate_limited(429, b"rate limit exceeded"));
        assert!(!is_rate_limited(429, b"slow down"));
        assert!(!is_rate_limited(403, b"forbidden"));
        assert!(!is_rate_limited(500, b"rate limit"));
    }

    // ── 非精确匹配候选 ────────────────────────────────────────────────────────

    #[test]
    fn candidate_firmwares_matches_vendor_prefixed_board_by_name() {
        let release = make_release(
            "v2.5.2",
            &[
                "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip",
                "v2.5.2_alientek-atk-dnesp32s3.zip",
            ],
        );
        let manifest = make_manifest(vec![
            make_variant(
                "spotpear/sp-esp32-s3-1.28-box",
                "sp-esp32-s3-1.28-box",
                "spotpear-sp-esp32-s3-1.28-box",
                "esp32s3",
            ),
            make_variant(
                "alientek/atk-dnesp32s3",
                "atk-dnesp32s3",
                "alientek-atk-dnesp32s3",
                "esp32s3",
            ),
        ]);

        let candidates = candidate_firmwares(
            &release,
            &manifest,
            "sp-esp32-s3-1.28-box",
            "sp-esp32-s3-1.28-box",
            "esp32s3",
        );
        let top = candidates.first().expect("应有候选");
        assert_eq!(top.full_name, "spotpear-sp-esp32-s3-1.28-box");
        assert_eq!(top.score, 100);
        assert!(top.chip_match);
        assert_eq!(top.asset, "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip");
        assert_eq!(top.reason, "构建名匹配");
    }

    #[test]
    fn candidate_firmwares_matches_m5stack_name_without_basename() {
        let release = make_release("v2.5.2", &["v2.5.2_m5stack-core-s3.zip"]);
        let manifest = make_manifest(vec![make_variant(
            "m5stack/core-s3",
            "m5stack-core-s3",
            "m5stack-core-s3",
            "esp32s3",
        )]);
        let candidates = candidate_firmwares(
            &release,
            &manifest,
            "m5stack-core-s3",
            "m5stack-core-s3",
            "esp32s3",
        );
        let top = candidates.first().expect("应有候选");
        assert_eq!(top.full_name, "m5stack-core-s3");
        assert_eq!(top.score, 100);
    }

    #[test]
    fn match_candidate_uses_selected_full_name_and_checks_chip() {
        let release = make_release(
            "v2.5.2",
            &[
                "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip",
                "v2.5.2_bread-compact-wifi.zip",
            ],
        );
        let manifest = make_manifest(vec![
            make_variant(
                "spotpear/sp-esp32-s3-1.28-box",
                "sp-esp32-s3-1.28-box",
                "spotpear-sp-esp32-s3-1.28-box",
                "esp32s3",
            ),
            make_variant(
                "bread-compact-wifi",
                "bread-compact-wifi",
                "bread-compact-wifi",
                "esp32s3",
            ),
        ]);

        let matched = match_candidate(
            &release,
            &manifest,
            "spotpear-sp-esp32-s3-1.28-box",
            "esp32s3",
        )
        .expect("应命中候选");
        assert_eq!(
            matched.asset.name,
            "v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip"
        );
        assert_eq!(matched.version, "2.5.2");

        assert!(matches!(
            match_candidate(&release, &manifest, "not-exist", "esp32s3"),
            Err(GithubError::VariantNotFound(_))
        ));
        assert!(matches!(
            match_candidate(
                &release,
                &manifest,
                "spotpear-sp-esp32-s3-1.28-box",
                "esp32c3"
            ),
            Err(GithubError::ChipMismatch { .. })
        ));
    }
}
