//! 一次性通知音频的内存托管：生成 → 托管 → 被拉取后失效。
//!
//! 这是 notify 通路的音频中转层：TTS 合成出的 Ogg Opus 放进内存表，
//! 设备凭一次性 URL 拉取，拉完即删。不做持久化、不做重定向，
//! 因为设备固件的 HTTP 客户端能力很弱（不跟随 3xx）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

/// 一次性的通知音频。
///
/// 为什么放内存：一条通知音频 10~40KB，够小；设备拉一次就不再需要，
/// 放磁盘要额外处理清理和并发。进程重启丢弃也无所谓（通知本身就过期了）。
pub struct SayAudioStore {
    items: Mutex<HashMap<String, SayAudioItem>>,
}

struct SayAudioItem {
    bytes: Vec<u8>,
    created_at: Instant,
    /// 单次拉取后就删；这个 TTL 只是兜底（设备可能因为不在 Idle 而没来拉）
    ttl: Duration,
}

impl Default for SayAudioStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SayAudioStore {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
        }
    }

    /// 托管一段音频，返回一次性 URL 用的 id。
    ///
    /// 为什么 id 用 uuid：URL 会出现在日志和 notify 消息里，
    /// 随机 id 避免同网段的人枚举 id 把通知内容拉走。
    pub fn insert(&self, bytes: Vec<u8>, ttl: Duration) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.lock_items().insert(
            id.clone(),
            SayAudioItem {
                bytes,
                created_at: Instant::now(),
                ttl,
            },
        );
        id
    }

    /// 取出并删除条目。
    ///
    /// 为什么必须在同一把锁里 remove：同一 id 并发来拉（设备重试、
    /// 网络重传）时只应有一个拿到字节，否则同一句话会播两遍。
    pub fn take(&self, id: &str) -> Option<Vec<u8>> {
        self.lock_items().remove(id).map(|item| item.bytes)
    }

    /// 清掉过期条目，由 web 服务启动处的 30s 定时任务调用。
    ///
    /// 为什么需要兜底：设备不在 Idle 时 notify 被静默丢弃、不会来拉，
    /// 没人 take 的条目会一直占内存；TTL 到期就当这条通知已作废。
    pub fn sweep(&self) {
        let now = Instant::now();
        self.lock_items()
            .retain(|_, item| now.duration_since(item.created_at) < item.ttl);
    }

    /// 锁中毒时取回数据继续用：内存表不是关键状态，
    /// 因为别的线程 panic 过就拒绝服务，得不偿失。
    fn lock_items(&self) -> MutexGuard<'_, HashMap<String, SayAudioItem>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 进程内唯一的音频表。
///
/// 为什么用全局槽而不是 axum State / 给 `add_routes` 加参数：
/// `add_routes` 是泛型 `Router<S>`，挂 State 会污染调用方的状态类型、
/// 迫使改签名；效仿 `ota.rs` 的全局槽模式，路由处理、合成接口和
/// 定时清理任务共用同一份。
pub fn say_audio_store() -> Arc<SayAudioStore> {
    static SLOT: OnceLock<Arc<SayAudioStore>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(SayAudioStore::new())).clone()
}

/// `GET /xiaozhi/say/{id}` 的处理函数，实际 URL 形如 `/xiaozhi/say/<uuid>.ogg`。
///
/// 设备固件的 HTTP 客户端不跟随 3xx、也不解析 JSON，
/// 所以这里要么 200 直给字节，要么 404 纯文本，绝不返回重定向；
/// 也不加 Cache-Control 之类的缓存头，避免影响设备的请求行为。
pub async fn handle_say_audio(Path(id): Path<String>) -> Response {
    // axum 的路径参数必须占完整的一段，写不出 `/xiaozhi/say/{id}.ogg`，
    // 所以路由挂 `/xiaozhi/say/{id}`，在这里把 `.ogg` 后缀剥掉再查表。
    let id = id.strip_suffix(".ogg").unwrap_or(&id);
    match say_audio_store().take(id) {
        Some(bytes) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, "audio/ogg")], bytes).into_response()
        }
        // 404 用纯文本说明：一次性 URL 重放、TTL 过期、id 不存在都走这里，
        // 设备看不懂没关系，这行文字是给人排查日志用的。
        None => (
            StatusCode::NOT_FOUND,
            "通知音频不存在或已被取走（一次性 URL）",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_take_returns_bytes_exactly_once() {
        let store = SayAudioStore::new();
        let id = store.insert(b"fake-ogg".to_vec(), Duration::from_secs(60));

        assert_eq!(
            store.take(&id).as_deref(),
            Some(b"fake-ogg".as_slice()),
            "第一次 take 应拿到原始字节"
        );
        assert!(
            store.take(&id).is_none(),
            "一次性：第二次 take 必须返回 None"
        );
    }

    #[test]
    fn sweep_removes_expired_but_keeps_fresh() {
        let store = SayAudioStore::new();
        let expired = store.insert(vec![1], Duration::ZERO);
        let fresh = store.insert(vec![2], Duration::from_secs(60));

        store.sweep();

        assert!(store.take(&expired).is_none(), "过期条目应被 sweep 清掉");
        assert_eq!(store.take(&fresh), Some(vec![2]), "未过期条目应保留");
    }

    /// 通过真实 HTTP 服务器验证路由行为（无新增依赖，仿 ws.rs 的测试写法）：
    /// 命中 → 200 + audio/ogg + Content-Length；重放同一 URL → 404；未知 id → 404。
    #[tokio::test]
    async fn route_serves_audio_once_then_404() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }

        let app = crate::add_routes(axum::Router::new(), Arc::new(crate::EchoStrategy));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定测试端口失败");
        let addr = listener.local_addr().expect("获取测试端口失败");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("测试服务器启动失败");
        });

        let id = say_audio_store().insert(b"fake-ogg-bytes".to_vec(), Duration::from_secs(60));

        let resp = http_get(addr, &format!("/xiaozhi/say/{id}.ogg")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "命中应 200：{resp}");
        let lower = resp.to_lowercase();
        assert!(
            lower.contains("content-type: audio/ogg"),
            "Content-Type 应为 audio/ogg：{resp}"
        );
        assert!(
            lower.contains("content-length: 14"),
            "应带 Content-Length：{resp}"
        );
        assert!(resp.ends_with("fake-ogg-bytes"), "body 应为音频字节");

        let replay = http_get(addr, &format!("/xiaozhi/say/{id}.ogg")).await;
        assert!(
            replay.starts_with("HTTP/1.1 404"),
            "一次性 URL 重放应 404：{replay}"
        );
        assert!(
            replay.contains("一次性"),
            "404 body 应为纯文本说明：{replay}"
        );

        let missing = http_get(addr, "/xiaozhi/say/not-exist.ogg").await;
        assert!(
            missing.starts_with("HTTP/1.1 404"),
            "未知 id 应 404：{missing}"
        );
    }
}
