//! rumqttc 客户端封装：连接、发布、把上行 PUBLISH 转成事件流。
//!
//! 这里只有 I/O，没有会话状态机。`AsyncClient::new` 不会自己建连，必须把
//! `EventLoop` 放进独立 task 里持续 `poll().await`。否则请求只堆在有界 channel
//! 里，`publish().await` 会在容量打满后永久挂起。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, MqttOptions, Outgoing,
    Packet, QoS,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// MQTT 客户端配置。
#[derive(Debug, Clone)]
pub struct MqttClientConfig {
    /// Broker 主机名或 IP。
    pub host: String,
    /// 明文端口必须是 1883。8883 上固件会强制 TLS。
    pub port: u16,
    /// 每个实例必须唯一，重复 client id 会被 broker 互相踢下线。
    pub client_id: String,
    /// 用户名。为 `None` 时不发送凭据。
    pub username: Option<String>,
    /// 密码。只在用户名存在时使用。
    pub password: Option<String>,
    /// 保活秒数。小智默认 240。`0` 表示关闭保活。
    pub keep_alive_secs: u64,
    /// `AsyncClient` → `EventLoop` 请求 channel 容量。建议 32..64，不能为 0。
    pub request_channel_cap: usize,
}

/// 一条收到的 MQTT 消息。
#[derive(Debug, Clone)]
pub struct MqttIncoming {
    /// 主题。
    pub topic: String,
    /// 原始载荷。QoS0 场景下不保证一定送达。
    pub payload: Vec<u8>,
}

struct MqttInner {
    client: AsyncClient,
    /// 只有收到成功的 ConnAck 才为 true。出错或 Disconnect 后清掉，重连成功再置上。
    /// 事件循环和句柄必须共享这一份，否则 `is_connected()` 永远是 false。
    connected: Arc<AtomicBool>,
    /// mpsc 是单消费者，第一次 `incoming()` 取走。
    incoming: Mutex<Option<mpsc::Receiver<MqttIncoming>>>,
    poller: Mutex<Option<JoinHandle<()>>>,
}

/// 客户端句柄。clone 只复制 `Arc`，所有句柄共享同一个 EventLoop。
#[derive(Clone)]
pub struct MqttClientHandle {
    inner: Arc<MqttInner>,
}

impl MqttClientHandle {
    /// 启动客户端并后台 poll。
    ///
    /// 返回时连接可能尚未建立。调用方用 `is_connected()` 判断，不要假设 `start` 已经连上。
    pub async fn start(cfg: MqttClientConfig) -> Result<Self, MqttClientError> {
        validate_config(&cfg)?;

        // builder 的 set_* 返回 `&mut Self`，不能把链式结果直接塞进按值接收的 `new`。
        let mut options = MqttOptions::new(cfg.client_id, cfg.host, cfg.port);
        options.set_keep_alive(Duration::from_secs(cfg.keep_alive_secs));
        options.set_clean_session(true);
        if let Some(username) = cfg.username {
            let password = cfg.password.unwrap_or_default();
            options.set_credentials(username, password);
        }

        let (client, eventloop) = AsyncClient::new(options, cfg.request_channel_cap);
        let connected = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(cfg.request_channel_cap);
        let poller_connected = Arc::clone(&connected);
        // EventLoop 独占。不要放进 Mutex 给别人 poll，也不要用 select! 取消 poll：
        // 取消可能停在半包读写，库不保证 cancel-safe。关机走 disconnect()。
        let poller = tokio::spawn(async move {
            drive_event_loop(eventloop, poller_connected, tx).await;
        });

        Ok(Self {
            inner: Arc::new(MqttInner {
                client,
                connected,
                incoming: Mutex::new(Some(rx)),
                poller: Mutex::new(Some(poller)),
            }),
        })
    }

    /// 发布一条 QoS0 消息。
    ///
    /// `publish().await` 只表示请求进了 EventLoop 的 channel，不表示已经出网。
    /// 未连接时立刻返回 `NotConnected`，避免在 cap 打满后挂死调用方。
    /// 音频循环不要 await 这个方法。实时发送应交给专用网络 task，避免 channel 满时把节奏卡住。
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), MqttClientError> {
        if !self.is_connected() {
            return Err(MqttClientError::NotConnected);
        }
        self.inner
            .client
            .publish(topic, QoS::AtMostOnce, false, payload)
            .await
            .map_err(|err| MqttClientError::Connect(err.to_string()))
    }

    /// 取上行消息。只能取一次，第二次得到一个已经关闭的接收端。
    pub fn incoming(&self) -> mpsc::Receiver<MqttIncoming> {
        let mut guard = lock_mutex(&self.inner.incoming);
        guard.take().unwrap_or_else(closed_receiver)
    }

    /// 当前是否已收到成功的 ConnAck。
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// 优雅关闭：先发 DISCONNECT，再等 EventLoop 自己退出。
    ///
    /// 不在这里 `select!` 掉 `poll()`。超时仍未退出才 `abort`，避免 poll 卡在建连上时泄漏 task。
    pub async fn shutdown(self) {
        if let Err(err) = self.inner.client.disconnect().await {
            tracing::debug!(%err, "MQTT disconnect 请求未能入队");
        }
        let poller = {
            let mut guard = lock_mutex(&self.inner.poller);
            guard.take()
        };
        let Some(mut poller) = poller else {
            return;
        };
        match tokio::time::timeout(Duration::from_secs(3), &mut poller).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(%err, "MQTT 事件循环异常退出");
            }
            Err(_) => {
                tracing::warn!("等待 MQTT Disconnect 超时，中止事件循环");
                poller.abort();
            }
        }
    }
}

/// 明文场景拒绝 8883：设备固件看到这个端口会强制走 TLS。
fn validate_config(cfg: &MqttClientConfig) -> Result<(), MqttClientError> {
    if cfg.port == 8883 {
        tracing::warn!("MQTT 端口 8883 在明文场景被拒绝：设备固件会强制 TLS");
        return Err(MqttClientError::Connect(
            "明文端口必须是 1883，8883 会被固件强制走 TLS".into(),
        ));
    }
    if cfg.request_channel_cap == 0 {
        return Err(MqttClientError::Connect(
            "request_channel_cap 不能为 0，否则 publish 会在无人及时 poll 时永久挂起".into(),
        ));
    }
    Ok(())
}

async fn drive_event_loop(
    mut eventloop: EventLoop,
    connected: Arc<AtomicBool>,
    tx: mpsc::Sender<MqttIncoming>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                // 每次重连都会再来一次 ConnAck，必须重新标记，不能只在第一次置位。
                if ack.code == ConnectReturnCode::Success {
                    connected.store(true, Ordering::Release);
                    backoff = Duration::from_secs(1);
                    tracing::debug!(session_present = ack.session_present, "MQTT ConnAck 成功");
                } else {
                    connected.store(false, Ordering::Release);
                    tracing::warn!(code = ?ack.code, "MQTT ConnAck 被拒绝");
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let message = MqttIncoming {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                };
                // poll 循环里不能 await 业务 channel，否则 keepalive 和后续 publish 一起停。
                if tx.try_send(message).is_err() {
                    tracing::debug!("MQTT 上行通道已满或已关闭，丢弃该消息");
                }
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                connected.store(false, Ordering::Release);
                tracing::debug!("MQTT Disconnect 已发出，退出事件循环");
                break;
            }
            Ok(_) => {}
            Err(err) => {
                connected.store(false, Ordering::Release);
                if is_fatal(&err) {
                    tracing::warn!(%err, "MQTT 不可恢复错误，停止重连");
                    break;
                }
                // poll 返回错误后，下一次 poll 会自己重连。退避由我们做，库没有内置退避。
                tracing::warn!(%err, ?backoff, "MQTT poll 失败，准备重连");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn is_fatal(err: &ConnectionError) -> bool {
    matches!(
        err,
        ConnectionError::ConnectionRefused(
            ConnectReturnCode::BadUserNamePassword
                | ConnectReturnCode::NotAuthorized
                | ConnectReturnCode::BadClientId,
        ) | ConnectionError::RequestsDone
    )
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn closed_receiver<T>() -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    rx
}

/// MQTT 客户端错误。
#[derive(Debug)]
pub enum MqttClientError {
    /// 配置非法，或客户端请求被 rumqttc 拒绝。
    Connect(String),
    /// 还没有成功的 ConnAck。此时继续 await publish 可能在 channel 满后挂死。
    NotConnected,
    /// 底层 I/O 错误。
    Io(std::io::Error),
}

impl std::fmt::Display for MqttClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(reason) => write!(f, "MQTT 连接失败：{reason}"),
            Self::NotConnected => write!(f, "MQTT 尚未连接"),
            Self::Io(err) => write!(f, "MQTT I/O 错误：{err}"),
        }
    }
}

impl std::error::Error for MqttClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Connect(_) | Self::NotConnected => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(port: u16) -> MqttClientConfig {
        MqttClientConfig {
            host: "127.0.0.1".into(),
            port,
            client_id: "haimen-mqtt-io-test".into(),
            username: None,
            password: None,
            keep_alive_secs: 240,
            request_channel_cap: 32,
        }
    }

    #[test]
    fn plaintext_port_8883_is_rejected() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let err = match MqttClientHandle::start(test_config(8883)).await {
                    Ok(_) => panic!("8883 必须拒绝"),
                    Err(err) => err,
                };
                let message = err.to_string();
                assert!(message.contains("8883"), "{message}");
                assert!(matches!(err, MqttClientError::Connect(_)));
            });
    }

    #[test]
    fn publish_before_connect_returns_not_connected() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                // 端口 1 上没有 broker。start 只负责拉起 poll，不会等 ConnAck。
                let handle = MqttClientHandle::start(test_config(1))
                    .await
                    .expect("start 不应等待连接完成");
                assert!(!handle.is_connected());

                let published = tokio::time::timeout(
                    Duration::from_millis(500),
                    handle.publish("haimen/test", b"ping".to_vec()),
                )
                .await;

                match published {
                    Ok(Err(MqttClientError::NotConnected)) => {}
                    Ok(other) => panic!("未连接时应返回 NotConnected，实际 {other:?}"),
                    Err(_) => panic!("未连接时 publish 挂起"),
                }
                handle.shutdown().await;
            });
    }
}
