//! 最小 MQTT 3.1.1 broker。
//!
//! 小智固件走 MQTT 时从不 `SUBSCRIBE`。标准 broker 只给已订阅的连接投递，
//! 设备就收不到 hello / stt / tts / mcp。这里对**这条 TCP 连接**直接写 PUBLISH，
//! 不建订阅转发关系。

use std::collections::HashMap;
use std::fmt;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, watch};

/// 变长整数上限（MQTT 规定最多 4 字节）。
const VARINT_MAX: u32 = 268_435_455;

/// 单连接默认最大包体：256 KiB。
const DEFAULT_MAX_PACKET_SIZE: usize = 256 * 1024;

const WRITE_QUEUE: usize = 32;
const INCOMING_QUEUE: usize = 256;

/// MQTT 控制报文类型（固定头高 4 位）。
const CONNECT: u8 = 1;
const CONNACK: u8 = 2;
const PUBLISH: u8 = 3;
const PUBACK: u8 = 4;
const SUBSCRIBE: u8 = 8;
const SUBACK: u8 = 9;
const UNSUBSCRIBE: u8 = 10;
const UNSUBACK: u8 = 11;
const PINGREQ: u8 = 12;
const PINGRESP: u8 = 13;
const DISCONNECT: u8 = 14;

/// broker 启动参数。
#[derive(Debug, Clone)]
pub struct MqttBrokerConfig {
    /// 监听地址，如 `"0.0.0.0:1883"`。测试用 `"127.0.0.1:0"` 让内核分配端口。
    pub bind_addr: String,
    /// 单连接最大包体（防止恶意大包）。`Default` 为 256 KiB。
    pub max_packet_size: usize,
    /// 连接鉴权密钥。`None` 表示不校验（向后兼容）。
    ///
    /// 开启后，设备必须用 `derive_mqtt_password(auth_key, client_id)` 算出的
    /// 密码连接；控制面客户端用 client_id = [`ADMIN_CLIENT_ID`]。
    /// 不设的话，局域网内任何人都能发 `haimen/ctl/<client_id>` 注入任意播报，
    /// 也能用真设备的 client_id 把它顶掉。
    pub auth_key: Option<String>,
    /// 迁移期兼容的旧密码。`None` = 严格模式。
    ///
    /// 为什么需要：设备只在**开机**时拉 OTA 拿新密码，MQTT 连不上也不会去重拉。
    /// 直接开启鉴权会让在线设备立刻被拒，只能靠人手断电恢复。
    /// 开启鉴权时先填上旧密码跑一阵，等所有设备都重启过再清空这个字段。
    pub legacy_password: Option<String>,
}

impl Default for MqttBrokerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:1883".to_owned(),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            auth_key: None,
            legacy_password: None,
        }
    }
}

/// 一条从设备上来的应用消息。
#[derive(Debug, Clone)]
pub struct BrokerIncoming {
    /// 发起连接的 client_id（haimen 靠它反解设备 MAC）。
    pub client_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
    /// QoS 0 时为 None。
    pub packet_id: Option<u16>,
}

/// 一条要下发给设备的消息。
#[derive(Debug, Clone)]
pub struct BrokerOutgoing {
    /// 目标 client_id。
    pub client_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
}

/// broker 句柄，可 clone，内部用 Arc 共享连接表。
///
/// [`MqttBrokerHandle::start`] 只返回句柄。上行通道在 `start` 里建好，
/// 第一次 [`MqttBrokerHandle::incoming`] 把唯一的 `Receiver` 交出去。
#[derive(Clone)]
pub struct MqttBrokerHandle {
    shared: Arc<BrokerShared>,
    incoming_rx: Arc<std::sync::Mutex<Option<mpsc::Receiver<BrokerIncoming>>>>,
    local_addr: SocketAddr,
}

struct BrokerShared {
    registry: Mutex<HashMap<String, ConnEntry>>,
    incoming_tx: mpsc::Sender<BrokerIncoming>,
    max_packet_size: usize,
    conn_ids: AtomicU64,
    auth_key: Option<String>,
    legacy_password: Option<String>,
}

struct ConnEntry {
    tx: mpsc::Sender<Vec<u8>>,
    conn_id: u64,
    /// 同一 client_id 重连时把旧连接踢掉，避免 `publish_to` 打到旧 socket。
    cancel: watch::Sender<bool>,
}

#[derive(Debug)]
pub enum MqttBrokerError {
    /// 套接字或读写失败。
    Io(std::io::Error),
    /// 报文不符合本 broker 实现的 MQTT 3.1.1 子集，或超过长度上限。
    Protocol(String),
    /// 保留给「句柄尚未启动」的调用方匹配。`start` 失败时直接返回 [`Io`](Self::Io)
    /// 或 [`Protocol`](Self::Protocol)，成功返回的句柄已经在监听。
    NotStarted,
}

impl fmt::Display for MqttBrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "MQTT broker I/O 错误: {err}"),
            Self::Protocol(msg) => write!(f, "MQTT 协议错误: {msg}"),
            Self::NotStarted => write!(f, "MQTT broker 尚未启动"),
        }
    }
}

impl std::error::Error for MqttBrokerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Protocol(_) | Self::NotStarted => None,
        }
    }
}

impl From<std::io::Error> for MqttBrokerError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl MqttBrokerHandle {
    /// 绑定 `cfg.bind_addr` 并 spawn accept 任务，返回已启动的句柄。
    ///
    /// 上行消息不从这里返回。请调用 [`incoming`](Self::incoming) 取走接收端；
    /// 那个 `Receiver` 不能克隆，只能取一次。
    pub async fn start(cfg: MqttBrokerConfig) -> Result<Self, MqttBrokerError> {
        if cfg.max_packet_size == 0 {
            return Err(protocol("max_packet_size 必须大于 0"));
        }
        let listener = TcpListener::bind(&cfg.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_QUEUE);
        let shared = Arc::new(BrokerShared {
            registry: Mutex::new(HashMap::new()),
            incoming_tx,
            max_packet_size: cfg.max_packet_size,
            conn_ids: AtomicU64::new(1),
            auth_key: cfg.auth_key.clone(),
            legacy_password: cfg.legacy_password.clone(),
        });
        let accept_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            accept_loop(listener, accept_shared).await;
        });
        Ok(Self {
            shared,
            incoming_rx: Arc::new(std::sync::Mutex::new(Some(incoming_rx))),
            local_addr,
        })
    }

    /// 实际监听地址。`bind_addr` 使用端口 0 时，用它拿到内核分配的端口。
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 向指定 client 下发一条 QoS 0 PUBLISH。
    ///
    /// client 不在线，或写任务已经退出，返回 `Ok(false)`。
    /// 下行不查订阅表：固件根本不订阅，只能对着这条连接写。
    pub async fn publish_to(&self, msg: BrokerOutgoing) -> Result<bool, MqttBrokerError> {
        let tx = {
            let registry = self.shared.registry.lock().await;
            registry.get(&msg.client_id).map(|entry| entry.tx.clone())
        };
        let Some(tx) = tx else {
            return Ok(false);
        };
        let frame = encode_publish_qos0(&msg.topic, &msg.payload)?;
        match tx.send(frame).await {
            Ok(()) => Ok(true),
            Err(_) => {
                // send 失败说明写任务已退出。锁里只改表，不在这里做 I/O。
                let mut registry = self.shared.registry.lock().await;
                if registry
                    .get(&msg.client_id)
                    .is_some_and(|entry| entry.tx.is_closed())
                {
                    registry.remove(&msg.client_id);
                }
                Ok(false)
            }
        }
    }

    /// 当前在线 client_id，按字典序排列，便于诊断。
    pub async fn connected_clients(&self) -> Vec<String> {
        let registry = self.shared.registry.lock().await;
        let mut ids: Vec<String> = registry.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 取走上行消息接收端。
    ///
    /// 内部是容量 256 的 tokio mpsc。第一次调用返回 `start` 时创建的 `Receiver`；
    /// 再次调用（包括 clone 出来的句柄）得到一个已经关闭的空接收端，因为
    /// `Receiver` 不能克隆。请在设备连上之前取走并一直持有。
    pub fn incoming(&self) -> mpsc::Receiver<BrokerIncoming> {
        let mut slot = self
            .incoming_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(rx) = slot.take() {
            rx
        } else {
            let (_tx, rx) = mpsc::channel(1);
            rx
        }
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<BrokerShared>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // 关闭 Nagle，否则 CONNACK / PINGRESP 这种小包会被攒住，设备一直等心跳。
                if let Err(err) = stream.set_nodelay(true) {
                    tracing::debug!(%peer, %err, "设置 TCP_NODELAY 失败");
                }
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, shared).await {
                        tracing::debug!(%peer, %err, "MQTT 连接关闭");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(%err, "MQTT accept 失败");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    shared: Arc<BrokerShared>,
) -> Result<(), MqttBrokerError> {
    let (mut reader, writer) = stream.into_split();
    let (write_tx, write_rx) = mpsc::channel(WRITE_QUEUE);
    tokio::spawn(async move {
        write_loop(writer, write_rx).await;
    });

    let first = match read_packet(&mut reader, shared.max_packet_size).await? {
        Some(packet) => packet,
        None => return Ok(()),
    };
    if first.packet_type != CONNECT || first.flags != 0 {
        return Err(protocol("第一个报文必须是 CONNECT"));
    }
    let connect = parse_connect(&first.body)?;
    // ── 鉴权：开启后密码必须是 HMAC(auth_key, client_id) ──
    if let Some(auth_key) = shared.auth_key.as_deref() {
        let supplied = connect
            .password
            .as_deref()
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .unwrap_or_default();
        let expected = derive_mqtt_password(auth_key, &connect.client_id);
        // 常量时间比较：长度先对齐再逐字节异或，避免按字节提前返回。
        let matches = |candidate: &str| {
            supplied.len() == candidate.len()
                && supplied
                    .bytes()
                    .zip(candidate.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        };
        // 迁移期旧密码只对「设备身份」放行，绝不放行管理端。
        //
        // 否则知道那个共享静态密码（曾经随 OTA 发给所有设备）的人，
        // 直接用 client_id = "haimen-admin" 连上来就能发控制主题 ——
        // 等于用旧密码换到了 HMAC 本应独占的管理权限。
        let is_device_identity = connect.client_id.starts_with(DEVICE_CLIENT_ID_PREFIX);
        let legacy_ok = is_device_identity
            && shared
                .legacy_password
                .as_deref()
                .is_some_and(|legacy| !legacy.is_empty() && matches(legacy));
        let ok = matches(&expected) || legacy_ok;
        if ok && legacy_ok {
            tracing::warn!(
              client_id = %connect.client_id,
              "MQTT 用了迁移期旧密码连接，设备重启后会自动切到派生密码"
            );
        }
        if !ok {
            tracing::warn!(
              client_id = %connect.client_id,
              username = connect.username.as_deref().unwrap_or("-"),
              "MQTT 鉴权失败，拒绝连接"
            );
            let _ = send_frame(&write_tx, encode_connack(0x04)).await;
            return Ok(());
        }
    }
    let keepalive_secs = connect.keepalive_secs;
    tracing::debug!(
      client_id = %connect.client_id,
      keepalive_secs,
      clean_session = connect.clean_session,
      username = connect.username.as_deref().unwrap_or("-"),
      password_len = connect.password.as_ref().map(Vec::len).unwrap_or(0),
      "接受 CONNECT"
    );

    let conn_id = shared.conn_ids.fetch_add(1, Ordering::Relaxed);
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    {
        let mut registry = shared.registry.lock().await;
        if let Some(old) = registry.insert(
            connect.client_id.clone(),
            ConnEntry {
                tx: write_tx.clone(),
                conn_id,
                cancel: cancel_tx,
            },
        ) {
            let _ = old.cancel.send(true);
        }
    }

    let client_id = connect.client_id;
    // 先登记再回 CONNACK：客户端读到 CONNACK 时，publish_to 已经能找到它。
    // 读完 CONNECT 不回 CONNACK 的话，esp_mqtt_client 会一直等到 Connect() 超时。
    let result = async {
        send_frame(&write_tx, encode_connack(0x00)).await?;
        conn_loop(
            &mut reader,
            &shared,
            &write_tx,
            &mut cancel_rx,
            &client_id,
            keepalive_secs,
        )
        .await
    }
    .await;

    unregister_if_current(&shared, &client_id, conn_id).await;
    tracing::debug!(
      %client_id,
      keepalive_secs,
      "MQTT 连接结束"
    );
    result
}

async fn conn_loop<R>(
    reader: &mut R,
    shared: &BrokerShared,
    write_tx: &mpsc::Sender<Vec<u8>>,
    cancel_rx: &mut watch::Receiver<bool>,
    client_id: &str,
    keepalive_secs: u16,
) -> Result<(), MqttBrokerError>
where
    R: AsyncRead + Unpin,
{
    // MQTT 3.1.1 §3.1.2.10：keepalive 为 0 表示不启用。
    //
    // 判活只看【入站】报文。不能看出站 —— 写成功不代表对端还活着，
    // 半开连接（设备断网/掉电但 TCP 未 RST）会一直被当成在线：
    // `publish_to` 返回成功、`prune_sessions` 清不掉、消息静默丢失。
    //
    // 1.5 倍是 MQTT 规范给客户端的宽限（它可能刚好卡在边界上）。
    let idle_timeout = if keepalive_secs == 0 {
        None
    } else {
        Some(Duration::from_millis(u64::from(keepalive_secs) * 1500))
    };

    loop {
        tokio::select! {
          changed = cancel_rx.changed() => {
            if changed.is_err() || *cancel_rx.borrow() {
              return Ok(());
            }
          }
          // 写任务退出会丢掉 Receiver，这里才能在没有下行报文时也把连接摘掉。
          _ = write_tx.closed() => {
            return Err(broken_pipe("连接写任务已退出"));
          }
          // 空闲超时：每收到一个报文循环就会重来，定时器随之重置。
          _ = async {
            match idle_timeout {
              Some(d) => tokio::time::sleep(d).await,
              None => std::future::pending::<()>().await,
            }
          } => {
            tracing::warn!(
              client_id,
              keepalive_secs,
              idle_timeout_ms = keepalive_secs as u64 * 1500,
              "MQTT 空闲超时，断开半开连接并摘除注册表"
            );
            return Err(broken_pipe("keepalive 超时"));
          }
          incoming = read_packet(reader, shared.max_packet_size) => {
            match incoming? {
              None => return Ok(()),
              Some(packet) => {
                let control = handle_packet(
                  packet.packet_type,
                  packet.flags,
                  &packet.body,
                  client_id,
                  write_tx,
                  &shared.incoming_tx,
                )
                .await?;
                if matches!(control, ConnControl::Close) {
                  return Ok(());
                }
              }
            }
          }
        }
    }
}

async fn write_loop<W>(mut writer: W, mut rx: mpsc::Receiver<Vec<u8>>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if writer.write_all(&frame).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

async fn unregister_if_current(shared: &BrokerShared, client_id: &str, conn_id: u64) {
    let mut registry = shared.registry.lock().await;
    let still_ours = registry
        .get(client_id)
        .is_some_and(|entry| entry.conn_id == conn_id);
    if still_ours {
        registry.remove(client_id);
    }
}

enum ConnControl {
    Continue,
    Close,
}

async fn handle_packet(
    packet_type: u8,
    flags: u8,
    body: &[u8],
    client_id: &str,
    write_tx: &mpsc::Sender<Vec<u8>>,
    incoming_tx: &mpsc::Sender<BrokerIncoming>,
) -> Result<ConnControl, MqttBrokerError> {
    match packet_type {
        PUBLISH => {
            let parsed = parse_publish(flags, body)?;
            let packet_id = parsed.packet_id;
            let incoming = BrokerIncoming {
                client_id: client_id.to_owned(),
                topic: parsed.topic,
                payload: parsed.payload,
                packet_id,
            };
            // 上层把 Receiver 丢了也不能拆掉设备连接，否则语音链路会跟着断。
            if incoming_tx.send(incoming).await.is_err() {
                tracing::debug!(%client_id, "上行接收端已关闭，丢弃 PUBLISH");
            }
            if let Some(id) = packet_id {
                send_frame(write_tx, encode_puback(id)).await?;
            }
            Ok(ConnControl::Continue)
        }
        PUBACK => {
            if body.len() != 2 {
                return Err(protocol("PUBACK 长度应为 2"));
            }
            Ok(ConnControl::Continue)
        }
        SUBSCRIBE => {
            // 必须回 SUBACK，否则客户端会重传。granted QoS 按请求值，但不登记转发。
            let (packet_id, granted) = parse_subscribe(body)?;
            send_frame(write_tx, encode_suback(packet_id, &granted)?).await?;
            Ok(ConnControl::Continue)
        }
        UNSUBSCRIBE => {
            let packet_id = parse_unsubscribe(body)?;
            send_frame(write_tx, encode_unsuback(packet_id)).await?;
            Ok(ConnControl::Continue)
        }
        PINGREQ => {
            // 不回 PINGRESP 的话，设备会在 keepalive（小智默认 240 秒）后主动断开。
            send_frame(write_tx, encode_pingresp()).await?;
            Ok(ConnControl::Continue)
        }
        DISCONNECT => Ok(ConnControl::Close),
        CONNECT => Err(protocol("重复 CONNECT")),
        other => Err(protocol(format!("不支持的报文类型 {other}"))),
    }
}

async fn send_frame(tx: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) -> Result<(), MqttBrokerError> {
    tx.send(frame)
        .await
        .map_err(|_| broken_pipe("连接写任务已退出"))
}

struct DecodedPacket {
    packet_type: u8,
    flags: u8,
    body: Vec<u8>,
}

/// 固定头第 1 字节：高 4 位类型，低 4 位 flags。
///
/// 规格里的 `type(1B) | flags(1B)` 是字段说明，不是两个独立字节。
/// PINGRESP 必须是 `[0xD0, 0x00]`（13 << 4），`esp_mqtt_client` 才认。
fn split_type_flags(byte: u8) -> (u8, u8) {
    (byte >> 4, byte & 0x0F)
}

async fn read_packet<R>(
    reader: &mut R,
    max_packet_size: usize,
) -> Result<Option<DecodedPacket>, MqttBrokerError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 1];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let (packet_type, flags) = split_type_flags(header[0]);
    let remaining = read_remaining_length(reader).await?;
    let body_len = usize::try_from(remaining).map_err(|_| protocol("报文长度超出平台 usize"))?;
    if body_len > max_packet_size {
        return Err(protocol(format!(
            "报文长度 {body_len} 超过上限 {max_packet_size}"
        )));
    }
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        reader.read_exact(&mut body).await?;
    }
    Ok(Some(DecodedPacket {
        packet_type,
        flags,
        body,
    }))
}

async fn read_remaining_length<R>(reader: &mut R) -> Result<u32, MqttBrokerError>
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    loop {
        if filled == 4 {
            return Err(protocol("变长整数超过 4 字节"));
        }
        reader.read_exact(&mut buf[filled..filled + 1]).await?;
        filled += 1;
        match decode_remaining_length(&buf[..filled]) {
            Ok((value, _)) => return Ok(value),
            Err(VarintError::Truncated) => continue,
            Err(err) => return Err(varint_to_broker(err)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarintError {
    Truncated,
    TooLong,
    Overflow,
}

fn varint_to_broker(err: VarintError) -> MqttBrokerError {
    let msg = match err {
        VarintError::Truncated => "变长整数被截断",
        VarintError::TooLong => "变长整数超过 4 字节",
        VarintError::Overflow => "变长整数溢出",
    };
    protocol(msg)
}

/// 解码 remaining length。成功时返回 `(值, 消耗的字节数)`。
fn decode_remaining_length(input: &[u8]) -> Result<(u32, usize), VarintError> {
    let mut value: u32 = 0;
    let mut multiplier: u32 = 1;
    let mut index = 0usize;
    loop {
        if index >= input.len() {
            return Err(VarintError::Truncated);
        }
        let byte = input[index];
        let delta = u32::from(byte & 0x7F)
            .checked_mul(multiplier)
            .ok_or(VarintError::Overflow)?;
        value = value.checked_add(delta).ok_or(VarintError::Overflow)?;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
        index += 1;
        // 第 4 字节仍带续位，说明至少还要第 5 字节，MQTT 不允许。
        if index == 4 {
            return Err(VarintError::TooLong);
        }
        multiplier = multiplier.checked_mul(128).ok_or(VarintError::Overflow)?;
    }
}

fn encode_remaining_length(mut value: u32) -> Result<Vec<u8>, MqttBrokerError> {
    if value > VARINT_MAX {
        return Err(protocol(format!("变长整数 {value} 超过 {VARINT_MAX}")));
    }
    let mut out = Vec::with_capacity(4);
    loop {
        let mut encoded = u8::try_from(value % 128).map_err(|_| protocol("变长整数编码失败"))?;
        value /= 128;
        if value > 0 {
            encoded |= 0x80;
        }
        out.push(encoded);
        if value == 0 {
            return Ok(out);
        }
        if out.len() == 4 {
            return Err(protocol("变长整数超过 4 字节"));
        }
    }
}

fn encode_packet(packet_type: u8, flags: u8, body: &[u8]) -> Result<Vec<u8>, MqttBrokerError> {
    let len = u32::try_from(body.len()).map_err(|_| protocol("报文长度超出变长整数范围"))?;
    let mut out = Vec::with_capacity(5 + body.len());
    out.push((packet_type << 4) | (flags & 0x0F));
    out.extend(encode_remaining_length(len)?);
    out.extend_from_slice(body);
    Ok(out)
}

/// session_present 固定 0：没有持久会话，`clean_session=false` 也不恢复状态。
fn encode_connack(return_code: u8) -> Vec<u8> {
    vec![(CONNACK << 4), 0x02, 0x00, return_code]
}

/// 控制面客户端（发布 `haimen/ctl/<设备>` 命令）的固定 client_id。
///
/// 设备**不允许**发控制主题：否则一台被攻陷的设备就能操纵其它设备。
/// 这个身份由 broker 的 HMAC 校验背书，伪造需要知道 auth_key。
pub const ADMIN_CLIENT_ID: &str = "haimen-admin";

/// 设备 client_id 的前缀（OTA 下发的格式：`GID_haimen@@@<mac>@@@<mac>`）。
///
/// 迁移期旧密码只对这个前缀的 client_id 生效，防止有人用旧密码
/// 冒充 [`ADMIN_CLIENT_ID`] 拿到控制面权限。
pub const DEVICE_CLIENT_ID_PREFIX: &str = "GID_haimen@@@";

/// 由 auth_key 和 client_id 派生 MQTT 密码（hex 的 HMAC-SHA256）。
///
/// OTA 下发 mqtt 配置时用它生成设备密码，broker 在 CONNECT 时重算比较。
pub fn derive_mqtt_password(auth_key: &str, client_id: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(auth_key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(client_id.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn encode_pingresp() -> Vec<u8> {
    vec![(PINGRESP << 4), 0x00]
}

fn encode_puback(packet_id: u16) -> Vec<u8> {
    let mut out = vec![(PUBACK << 4), 0x02];
    out.extend_from_slice(&packet_id.to_be_bytes());
    out
}

fn encode_unsuback(packet_id: u16) -> Vec<u8> {
    let mut out = vec![(UNSUBACK << 4), 0x02];
    out.extend_from_slice(&packet_id.to_be_bytes());
    out
}

fn encode_suback(packet_id: u16, granted: &[u8]) -> Result<Vec<u8>, MqttBrokerError> {
    let mut body = Vec::with_capacity(2 + granted.len());
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(granted);
    encode_packet(SUBACK, 0, &body)
}

fn encode_publish_qos0(topic: &str, payload: &[u8]) -> Result<Vec<u8>, MqttBrokerError> {
    let topic_bytes = topic.as_bytes();
    if topic_bytes.len() > usize::from(u16::MAX) {
        return Err(protocol("topic 超过 65535 字节"));
    }
    let mut body = Vec::with_capacity(2 + topic_bytes.len() + payload.len());
    body.extend_from_slice(&(topic_bytes.len() as u16).to_be_bytes());
    body.extend_from_slice(topic_bytes);
    body.extend_from_slice(payload);
    encode_packet(PUBLISH, 0, &body)
}

struct ParsedConnect {
    client_id: String,
    username: Option<String>,
    password: Option<Vec<u8>>,
    keepalive_secs: u16,
    clean_session: bool,
}

fn parse_connect(body: &[u8]) -> Result<ParsedConnect, MqttBrokerError> {
    let mut pos = 0usize;
    let protocol_name = read_mqtt_bytes(body, &mut pos)?;
    if protocol_name != b"MQTT" {
        return Err(protocol("只支持 MQTT 3.1.1（协议名必须是 MQTT）"));
    }
    let level = *body
        .get(pos)
        .ok_or_else(|| protocol("CONNECT 缺少协议级别"))?;
    pos += 1;
    if level != 4 {
        return Err(protocol(format!(
            "只支持 MQTT 3.1.1（protocol level = 4），收到 {level}"
        )));
    }
    let flags = *body
        .get(pos)
        .ok_or_else(|| protocol("CONNECT 缺少连接标志"))?;
    pos += 1;
    if flags & 0x01 != 0 {
        return Err(protocol("CONNECT 标志保留位必须为 0"));
    }
    let clean_session = flags & 0x02 != 0;
    let will_flag = flags & 0x04 != 0;
    let will_qos = (flags >> 3) & 0x03;
    let password_flag = flags & 0x40 != 0;
    let username_flag = flags & 0x80 != 0;
    if will_qos == 3 || (will_qos > 0 && !will_flag) {
        return Err(protocol("CONNECT Will QoS 非法"));
    }
    if password_flag && !username_flag {
        return Err(protocol("CONNECT 有密码标志但没有用户名标志"));
    }
    if pos + 2 > body.len() {
        return Err(protocol("CONNECT 缺少 keepalive"));
    }
    let keepalive_secs = u16::from_be_bytes([body[pos], body[pos + 1]]);
    pos += 2;

    // client_id 按长度前缀的二进制读。小智的值是 ASCII，但非法 UTF-8 不能直接踢线。
    let client_id = String::from_utf8_lossy(read_mqtt_bytes(body, &mut pos)?).into_owned();
    if will_flag {
        let _will_topic = read_mqtt_bytes(body, &mut pos)?;
        let _will_payload = read_mqtt_bytes(body, &mut pos)?;
    }
    let username = if username_flag {
        Some(String::from_utf8_lossy(read_mqtt_bytes(body, &mut pos)?).into_owned())
    } else {
        None
    };
    let password = if password_flag {
        Some(read_mqtt_bytes(body, &mut pos)?.to_vec())
    } else {
        None
    };
    if pos != body.len() {
        return Err(protocol("CONNECT 载荷尾部有多余字节"));
    }
    Ok(ParsedConnect {
        client_id,
        username,
        password,
        keepalive_secs,
        clean_session,
    })
}

struct ParsedPublish {
    topic: String,
    payload: Vec<u8>,
    packet_id: Option<u16>,
}

fn parse_publish(flags: u8, body: &[u8]) -> Result<ParsedPublish, MqttBrokerError> {
    let qos = (flags >> 1) & 0x03;
    if qos == 3 {
        return Err(protocol("PUBLISH QoS 位非法"));
    }
    if qos == 2 {
        return Err(protocol("不支持 QoS 2"));
    }
    let mut pos = 0usize;
    let topic_bytes = read_mqtt_bytes(body, &mut pos)?;
    let topic = std::str::from_utf8(topic_bytes)
        .map_err(|_| protocol("PUBLISH topic 不是 UTF-8"))?
        .to_owned();
    let packet_id = if qos == 1 {
        if pos + 2 > body.len() {
            return Err(protocol("QoS 1 PUBLISH 缺少 packet id"));
        }
        let id = u16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;
        Some(id)
    } else {
        None
    };
    Ok(ParsedPublish {
        topic,
        payload: body[pos..].to_vec(),
        packet_id,
    })
}

fn parse_subscribe(body: &[u8]) -> Result<(u16, Vec<u8>), MqttBrokerError> {
    if body.len() < 2 {
        return Err(protocol("SUBSCRIBE 缺少 packet id"));
    }
    let packet_id = u16::from_be_bytes([body[0], body[1]]);
    let mut pos = 2usize;
    if pos >= body.len() {
        return Err(protocol("SUBSCRIBE 没有主题过滤器"));
    }
    let mut granted = Vec::new();
    while pos < body.len() {
        let _filter = read_mqtt_bytes(body, &mut pos)?;
        let requested = *body
            .get(pos)
            .ok_or_else(|| protocol("SUBSCRIBE 缺少请求的 QoS"))?;
        pos += 1;
        // 低 2 位就是请求的 QoS。原样授予，不降级，也不据此建立路由。
        granted.push(requested & 0x03);
    }
    Ok((packet_id, granted))
}

fn parse_unsubscribe(body: &[u8]) -> Result<u16, MqttBrokerError> {
    if body.len() < 2 {
        return Err(protocol("UNSUBSCRIBE 缺少 packet id"));
    }
    let packet_id = u16::from_be_bytes([body[0], body[1]]);
    let mut pos = 2usize;
    if pos >= body.len() {
        return Err(protocol("UNSUBSCRIBE 没有主题过滤器"));
    }
    while pos < body.len() {
        let _filter = read_mqtt_bytes(body, &mut pos)?;
    }
    Ok(packet_id)
}

fn read_mqtt_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8], MqttBrokerError> {
    if *pos + 2 > buf.len() {
        return Err(protocol("MQTT 字符串长度被截断"));
    }
    let len = usize::from(u16::from_be_bytes([buf[*pos], buf[*pos + 1]]));
    *pos += 2;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| protocol("MQTT 字符串长度溢出"))?;
    if end > buf.len() {
        return Err(protocol("MQTT 字符串内容被截断"));
    }
    let slice = &buf[*pos..end];
    *pos = end;
    Ok(slice)
}

fn protocol(msg: impl Into<String>) -> MqttBrokerError {
    MqttBrokerError::Protocol(msg.into())
}

fn broken_pipe(msg: &'static str) -> MqttBrokerError {
    MqttBrokerError::Io(std::io::Error::new(ErrorKind::BrokenPipe, msg))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    use super::*;

    const XIAOZHI_CLIENT_ID: &str = "GID_test@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64";

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建测试 runtime");
        runtime.block_on(fut)
    }

    fn append_mqtt_bytes(buf: &mut Vec<u8>, data: &[u8]) {
        let len = u16::try_from(data.len()).expect("测试字符串过长");
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(data);
    }

    fn finish_packet(type_and_flags: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![type_and_flags];
        out.extend(
            encode_remaining_length(u32::try_from(body.len()).expect("body 过长"))
                .expect("编码长度"),
        );
        out.extend_from_slice(body);
        out
    }

    async fn decode_one(bytes: &[u8]) -> DecodedPacket {
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        read_packet(&mut cursor, DEFAULT_MAX_PACKET_SIZE)
            .await
            .expect("读测试报文")
            .expect("测试报文被截断")
    }

    async fn start_broker() -> MqttBrokerHandle {
        start_broker_with_auth(None).await
    }

    async fn start_broker_with_auth(auth_key: Option<String>) -> MqttBrokerHandle {
        MqttBrokerHandle::start(MqttBrokerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            auth_key,
            legacy_password: None,
        })
        .await
        .expect("启动 broker")
    }

    async fn start_broker_with_auth_and_legacy(
        auth_key: String,
        legacy_password: String,
    ) -> MqttBrokerHandle {
        MqttBrokerHandle::start(MqttBrokerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            auth_key: Some(auth_key),
            legacy_password: Some(legacy_password),
        })
        .await
        .expect("启动 broker")
    }

    async fn read_frame(stream: &mut TcpStream) -> DecodedPacket {
        tokio::time::timeout(
            Duration::from_secs(2),
            read_packet(stream, DEFAULT_MAX_PACKET_SIZE),
        )
        .await
        .expect("读报文超时")
        .expect("读报文失败")
        .expect("对端提前关闭")
    }

    fn connect_body(
        client_id: &[u8],
        keepalive: u16,
        clean_session: bool,
        username: Option<&[u8]>,
        password: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        append_mqtt_bytes(&mut body, b"MQTT");
        body.push(4);
        let mut flags = 0u8;
        if clean_session {
            flags |= 0x02;
        }
        if username.is_some() {
            flags |= 0x80;
        }
        if password.is_some() {
            flags |= 0x40;
        }
        body.push(flags);
        body.extend_from_slice(&keepalive.to_be_bytes());
        append_mqtt_bytes(&mut body, client_id);
        if let Some(name) = username {
            append_mqtt_bytes(&mut body, name);
        }
        if let Some(secret) = password {
            append_mqtt_bytes(&mut body, secret);
        }
        body
    }

    async fn handshake(addr: SocketAddr, client_id: &str) -> TcpStream {
        handshake_with_keepalive(addr, client_id, 240).await
    }

    async fn handshake_with_keepalive(
        addr: SocketAddr,
        client_id: &str,
        keepalive_secs: u16,
    ) -> TcpStream {
        let mut stream = TcpStream::connect(addr).await.expect("连接 broker");
        stream.set_nodelay(true).expect("客户端 TCP_NODELAY");
        let body = connect_body(
            client_id.as_bytes(),
            keepalive_secs,
            true,
            Some(b"user"),
            Some(b"pass"),
        );
        let packet = finish_packet(0x10, &body);
        stream.write_all(&packet).await.expect("写 CONNECT");
        let connack = read_frame(&mut stream).await;
        assert_eq!(connack.packet_type, CONNACK);
        assert_eq!(connack.flags, 0);
        assert_eq!(connack.body, vec![0x00, 0x00]);
        stream
    }

    async fn wait_for_client(handle: &MqttBrokerHandle, client_id: &str, present: bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let clients = handle.connected_clients().await;
            let found = clients.iter().any(|id| id == client_id);
            if found == present {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("客户端 {client_id} 在线状态没有变成 {present}，当前 {clients:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// MQTT keepalive 超时必须真的摘掉连接。
    ///
    /// 为什么重要：不检测的话，设备断网/掉电但 TCP 没 RST 时会变成半开连接，
    /// `publish_to` 一直返回成功、`prune_sessions` 也清不掉，
    /// 表现为「服务端日志说转发成功，设备毫无反应」。
    #[tokio::test]
    async fn keepalive_timeout_drops_half_open_connection() {
        let handle = start_broker().await;
        // keepalive = 1 秒 → 1.5 倍 = 1.5 秒超时。
        let mut stream = handshake_with_keepalive(handle.local_addr(), "keepalive-victim", 1).await;
        wait_for_client(&handle, "keepalive-victim", true).await;

        // 什么都不发，等超时。
        tokio::time::sleep(Duration::from_millis(2500)).await;

        wait_for_client(&handle, "keepalive-victim", false).await;

        // 连接应当已经被服务端关闭：这里读会立刻结束或报错。
        let mut buf = [0u8; 16];
        let _ = stream.read(&mut buf).await;
    }

    /// keepalive = 0 表示不启用，不能被我们的定时器误杀。
    #[tokio::test]
    async fn keepalive_disabled_keeps_connection_alive() {
        let handle = start_broker().await;
        let _stream = handshake_with_keepalive(handle.local_addr(), "keepalive-off", 0).await;
        wait_for_client(&handle, "keepalive-off", true).await;

        tokio::time::sleep(Duration::from_millis(2500)).await;
        wait_for_client(&handle, "keepalive-off", true).await;
    }

    /// 派生函数必须是确定的、且不同 client_id 得到不同密码。
    #[test]
    fn derive_mqtt_password_is_deterministic() {
        let a = derive_mqtt_password("secret", "dev-1");
        assert_eq!(a, derive_mqtt_password("secret", "dev-1"));
        assert_ne!(a, derive_mqtt_password("secret", "dev-2"));
        assert_ne!(a, derive_mqtt_password("other", "dev-1"));
        // hex 的 SHA256 = 64 字符
        assert_eq!(a.len(), 64);
    }

    /// 开启鉴权后，密码不对必须被拒绝（CONNACK 0x04），且不进在线列表。
    #[tokio::test]
    async fn auth_rejects_wrong_password() {
        let handle = start_broker_with_auth(Some("k".to_string())).await;
        let mut stream = TcpStream::connect(handle.local_addr())
            .await
            .expect("连接 broker");
        let body = connect_body(
            "dev-1".as_bytes(),
            60,
            true,
            Some(b"haimen"),
            Some(b"wrong"),
        );
        let packet = finish_packet(0x10, &body);
        stream.write_all(&packet).await.expect("写 CONNECT");
        let connack = read_frame(&mut stream).await;
        assert_eq!(connack.packet_type, CONNACK);
        assert_eq!(connack.body, vec![0x00, 0x04], "应返回 0x04 认证失败");

        // 拒绝后不应登记
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !handle
                .connected_clients()
                .await
                .iter()
                .any(|id| id == "dev-1"),
            "鉴权失败的连接不能进在线列表"
        );
    }

    /// 开启鉴权后，派生密码正确就应该正常连上。
    #[tokio::test]
    async fn auth_accepts_correct_password() {
        let key = "k";
        let handle = start_broker_with_auth(Some(key.to_string())).await;
        let password = derive_mqtt_password(key, "dev-1");
        let mut stream = TcpStream::connect(handle.local_addr())
            .await
            .expect("连接 broker");
        let body = connect_body(
            "dev-1".as_bytes(),
            60,
            true,
            Some(b"haimen"),
            Some(password.as_bytes()),
        );
        let packet = finish_packet(0x10, &body);
        stream.write_all(&packet).await.expect("写 CONNECT");
        let connack = read_frame(&mut stream).await;
        assert_eq!(connack.body, vec![0x00, 0x00], "应返回 0x00 接受");
        wait_for_client(&handle, "dev-1", true).await;
    }

    /// 迁移期：开启了鉴权但 auth_grace 生效时，旧密码仍能连上。
    ///
    /// 这条保证「开启鉴权」不会让在线设备立刻失联 —— 设备只在开机时拉 OTA，
    /// 必须先让它用旧密码连上、重启后再切到派生密码。
    #[tokio::test]
    async fn auth_grace_accepts_legacy_password() {
        let handle = start_broker_with_auth_and_legacy("k".to_string(), "haimen".to_string()).await;
        let mut stream = TcpStream::connect(handle.local_addr())
            .await
            .expect("连接 broker");
        // 必须是设备格式的 client_id —— 迁移期旧密码只对这个前缀放行。
        let device_id = "GID_haimen@@@dev-1@@@dev-1";
        let body = connect_body(
            device_id.as_bytes(),
            60,
            true,
            Some(b"haimen"),
            Some(b"haimen"),
        );
        let packet = finish_packet(0x10, &body);
        stream.write_all(&packet).await.expect("写 CONNECT");
        let connack = read_frame(&mut stream).await;
        assert_eq!(connack.body, vec![0x00, 0x00], "迁移期应接受旧密码");
        wait_for_client(&handle, device_id, true).await;
    }

    /// 迁移期旧密码**不能**用来冒充管理端。
    ///
    /// 否则知道共享静态密码（曾随 OTA 发给所有设备）的人，
    /// 用 client_id = haimen-admin 连上来就能发控制主题，
    /// 等于用旧密码换到了 HMAC 本该独占的管理权限。
    #[tokio::test]
    async fn auth_grace_rejects_admin_with_legacy_password() {
        let handle =
            start_broker_with_auth_and_legacy("k".to_string(), "haimen".to_string()).await;
        let mut stream = TcpStream::connect(handle.local_addr())
            .await
            .expect("连接 broker");
        let body = connect_body(
            ADMIN_CLIENT_ID.as_bytes(),
            60,
            true,
            Some(b"haimen"),
            Some(b"haimen"),
        );
        let packet = finish_packet(0x10, &body);
        stream.write_all(&packet).await.expect("写 CONNECT");
        let connack = read_frame(&mut stream).await;
        assert_eq!(
            connack.body,
            vec![0x00, 0x04],
            "迁移期旧密码不应能连成管理端"
        );
        assert!(
            !handle
                .connected_clients()
                .await
                .iter()
                .any(|id| id == ADMIN_CLIENT_ID),
            "被拒的管理端连接不能进在线列表"
        );
    }

    #[test]
    fn varint_encode_decode_known_values() {
        let cases = [
            (0u32, vec![0x00]),
            (127, vec![0x7F]),
            (128, vec![0x80, 0x01]),
            (16_383, vec![0xFF, 0x7F]),
            (16_384, vec![0x80, 0x80, 0x01]),
            (2_097_151, vec![0xFF, 0xFF, 0x7F]),
            (268_435_455, vec![0xFF, 0xFF, 0xFF, 0x7F]),
        ];
        for (value, bytes) in cases {
            assert_eq!(encode_remaining_length(value).expect("编码"), bytes);
            assert_eq!(
                decode_remaining_length(&bytes).expect("解码"),
                (value, bytes.len())
            );
        }
        // 后面多出来的字节不属于这个变长整数。
        assert_eq!(
            decode_remaining_length(&[0x80, 0x01, 0xFF]).expect("128 只占两字节"),
            (128, 2)
        );
    }

    #[test]
    fn varint_rejects_more_than_four_bytes() {
        let too_long = [0x80u8, 0x80, 0x80, 0x80, 0x01];
        assert_eq!(
            decode_remaining_length(&too_long),
            Err(VarintError::TooLong)
        );
        assert_eq!(
            decode_remaining_length(&[0x80, 0x80, 0x80, 0x80]),
            Err(VarintError::TooLong)
        );
        assert_eq!(
            decode_remaining_length(&[0x80]),
            Err(VarintError::Truncated)
        );
        assert!(encode_remaining_length(VARINT_MAX + 1).is_err());
    }

    #[test]
    fn parse_connect_from_byte_stream() {
        block_on(async {
            // 手写短包，锁住大端长度和 keepalive，不经过编码器。
            let short: &[u8] = &[
                0x10, 0x0E, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0xF0, 0x00, 0x02,
                b'a', b'b',
            ];
            let decoded = decode_one(short).await;
            assert_eq!(decoded.packet_type, CONNECT);
            let parsed = parse_connect(&decoded.body).expect("解析短 CONNECT");
            assert_eq!(parsed.client_id, "ab");
            assert_eq!(parsed.keepalive_secs, 240);
            assert!(parsed.clean_session);
            assert!(parsed.username.is_none());

            let body = connect_body(
                XIAOZHI_CLIENT_ID.as_bytes(),
                240,
                false,
                Some(b"user"),
                Some(b"secret"),
            );
            let packet = finish_packet(0x10, &body);
            let decoded = decode_one(&packet).await;
            let parsed = parse_connect(&decoded.body).expect("解析小智 CONNECT");
            assert_eq!(parsed.client_id, XIAOZHI_CLIENT_ID);
            assert_eq!(parsed.keepalive_secs, 240);
            assert!(!parsed.clean_session);
            assert_eq!(parsed.username.as_deref(), Some("user"));
            assert_eq!(parsed.password.as_deref(), Some(b"secret".as_slice()));

            // 非法 UTF-8 仍按长度前缀读出来，不把连接直接判死。
            let raw_id = b"gid\xff";
            let body = connect_body(raw_id, 60, true, None, None);
            let packet = finish_packet(0x10, &body);
            let decoded = decode_one(&packet).await;
            let parsed = parse_connect(&decoded.body).expect("解析二进制 client_id");
            assert_eq!(parsed.client_id, String::from_utf8_lossy(raw_id));
            assert_eq!(parsed.keepalive_secs, 60);
            assert!(parsed.clean_session);
        });
    }

    #[test]
    fn parse_publish_qos0_and_qos1() {
        block_on(async {
            let qos0 = [0x30u8, 0x05, 0x00, 0x01, b't', b'h', b'i'];
            let decoded = decode_one(&qos0).await;
            assert_eq!(decoded.packet_type, PUBLISH);
            let parsed = parse_publish(decoded.flags, &decoded.body).expect("QoS 0");
            assert_eq!(parsed.topic, "t");
            assert_eq!(parsed.payload, b"hi");
            assert_eq!(parsed.packet_id, None);

            let qos1 = [0x32u8, 0x07, 0x00, 0x01, b't', 0x00, 0x07, b'h', b'i'];
            let decoded = decode_one(&qos1).await;
            let parsed = parse_publish(decoded.flags, &decoded.body).expect("QoS 1");
            assert_eq!(parsed.topic, "t");
            assert_eq!(parsed.payload, b"hi");
            assert_eq!(parsed.packet_id, Some(7));
        });
    }

    #[test]
    fn connack_bytes_are_session_present_zero() {
        assert_eq!(encode_connack(0x00), vec![0x20, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn pingresp_bytes() {
        assert_eq!(encode_pingresp(), vec![0xD0, 0x00]);
    }

    #[test]
    fn handshake_then_uplink_publish_reaches_incoming() {
        block_on(async {
            let handle = start_broker().await;
            let mut incoming = handle.incoming();
            let mut stream = handshake(handle.local_addr(), XIAOZHI_CLIENT_ID).await;
            wait_for_client(&handle, XIAOZHI_CLIENT_ID, true).await;

            let mut body = Vec::new();
            append_mqtt_bytes(&mut body, b"device-server");
            body.extend_from_slice(b"{\"type\":\"hello\"}");
            let packet = finish_packet(0x30, &body);
            stream.write_all(&packet).await.expect("写上行 PUBLISH");

            let msg = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                .await
                .expect("等上行超时")
                .expect("上行通道已关闭");
            assert_eq!(msg.client_id, XIAOZHI_CLIENT_ID);
            assert_eq!(msg.topic, "device-server");
            assert_eq!(msg.payload, b"{\"type\":\"hello\"}");
            assert_eq!(msg.packet_id, None);
        });
    }

    #[test]
    fn publish_to_delivers_qos0_publish() {
        block_on(async {
            let handle = start_broker().await;
            let offline = handle
                .publish_to(BrokerOutgoing {
                    client_id: "missing".to_owned(),
                    topic: "t".to_owned(),
                    payload: b"x".to_vec(),
                })
                .await
                .expect("离线 publish");
            assert!(!offline);

            let mut stream = handshake(handle.local_addr(), "down-client").await;
            let delivered = handle
                .publish_to(BrokerOutgoing {
                    client_id: "down-client".to_owned(),
                    topic: "devices/p2p/84_c7_bb_59_93_64".to_owned(),
                    payload: b"hello-down".to_vec(),
                })
                .await
                .expect("下发");
            assert!(delivered);

            let packet = read_frame(&mut stream).await;
            assert_eq!(packet.packet_type, PUBLISH);
            assert_eq!(packet.flags, 0);
            let parsed = parse_publish(packet.flags, &packet.body).expect("解析下行");
            assert_eq!(parsed.topic, "devices/p2p/84_c7_bb_59_93_64");
            assert_eq!(parsed.payload, b"hello-down");
            assert_eq!(parsed.packet_id, None);
        });
    }

    #[test]
    fn disconnect_removes_client() {
        block_on(async {
            let handle = start_broker().await;
            let mut stream = handshake(handle.local_addr(), "gone").await;
            wait_for_client(&handle, "gone", true).await;
            stream.shutdown().await.expect("关闭写侧");
            drop(stream);
            wait_for_client(&handle, "gone", false).await;
            let still = handle
                .publish_to(BrokerOutgoing {
                    client_id: "gone".to_owned(),
                    topic: "t".to_owned(),
                    payload: b"nope".to_vec(),
                })
                .await
                .expect("断线后下发");
            assert!(!still);
        });
    }

    #[test]
    fn pingreq_returns_pingresp() {
        block_on(async {
            let handle = start_broker().await;
            let mut stream = handshake(handle.local_addr(), "ping-client").await;
            stream.write_all(&[0xC0, 0x00]).await.expect("写 PINGREQ");
            let packet = read_frame(&mut stream).await;
            assert_eq!(packet.packet_type, PINGRESP);
            assert_eq!(packet.flags, 0);
            assert!(packet.body.is_empty());
        });
    }

    #[test]
    fn subscribe_and_unsubscribe_are_acked_without_routing() {
        block_on(async {
            let handle = start_broker().await;
            let mut stream = handshake(handle.local_addr(), "sub-client").await;

            let mut body = vec![0x00, 0x01];
            append_mqtt_bytes(&mut body, b"devices/p2p/x");
            body.push(1);
            append_mqtt_bytes(&mut body, b"other");
            body.push(0);
            stream
                .write_all(&finish_packet(0x82, &body))
                .await
                .expect("写 SUBSCRIBE");
            let suback = read_frame(&mut stream).await;
            assert_eq!(suback.packet_type, SUBACK);
            assert_eq!(suback.body, vec![0x00, 0x01, 0x01, 0x00]);

            let mut body = vec![0x00, 0x02];
            append_mqtt_bytes(&mut body, b"devices/p2p/x");
            stream
                .write_all(&finish_packet(0xA2, &body))
                .await
                .expect("写 UNSUBSCRIBE");
            let unsuback = read_frame(&mut stream).await;
            assert_eq!(unsuback.packet_type, UNSUBACK);
            assert_eq!(unsuback.body, vec![0x00, 0x02]);
        });
    }

    #[test]
    fn qos1_uplink_is_delivered_then_puback() {
        block_on(async {
            let handle = start_broker().await;
            let mut incoming = handle.incoming();
            let mut stream = handshake(handle.local_addr(), "qos1-client").await;

            let mut body = Vec::new();
            append_mqtt_bytes(&mut body, b"device-server");
            body.extend_from_slice(&9u16.to_be_bytes());
            body.extend_from_slice(b"up");
            stream
                .write_all(&finish_packet(0x32, &body))
                .await
                .expect("写 QoS1 PUBLISH");

            let msg = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                .await
                .expect("等 QoS1 上行超时")
                .expect("上行通道已关闭");
            assert_eq!(msg.topic, "device-server");
            assert_eq!(msg.payload, b"up");
            assert_eq!(msg.packet_id, Some(9));

            let puback = read_frame(&mut stream).await;
            assert_eq!(puback.packet_type, PUBACK);
            assert_eq!(puback.body, vec![0x00, 0x09]);
        });
    }
}
