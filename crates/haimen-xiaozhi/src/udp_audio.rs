//! 单个 UDP socket 服务多台设备：收包、按 session 解密、回包。
//!
//! 会话主键是包头里的 `session_id`（nonce[4..8) / ssrc）。源地址只用来回包，
//! 因为 NAT 重绑定和重连都会改端口。加密细节在 `protocol_udp`，这里只做 I/O。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::protocol_udp::{UdpAudioHeader, UdpCrypto, UdpProtoError};

/// 单包上限。16 字节头之外，载荷不能超过 1484，否则总长超过 1500。
const MAX_DATAGRAM_LEN: usize = 1500;
const MAX_PAYLOAD_LEN: usize = MAX_DATAGRAM_LEN - UdpAudioHeader::SIZE;

/// UDP 音频通道配置。
#[derive(Debug, Clone)]
pub struct UdpAudioConfig {
    /// 监听地址，如 `"0.0.0.0:8888"`。端口写 0 时由系统分配。
    pub bind_addr: String,
}

/// 一个已经知道对端地址的会话。
pub struct UdpSession {
    /// 对端地址。收到首个上行包后才确定，之后 NAT 变化会更新。
    pub peer: SocketAddr,
    /// 会话 ID，等于我们下发的 nonce[4..8)。
    pub session_id: u32,
}

/// 收到并解密后的音频包。
#[derive(Debug, Clone)]
pub struct UdpIncomingAudio {
    /// 这个包的源地址。
    pub peer: SocketAddr,
    /// nonce[4..8)。
    pub session_id: u32,
    /// 设备填写的序列号。
    pub sequence: u32,
    /// 设备填写的时间戳。
    pub timestamp: u32,
    /// 已解密的 Opus 载荷。
    pub payload: Vec<u8>,
}

struct SessionEntry {
    crypto: UdpCrypto,
    /// 首包到达前是 `None`，不能靠 bind 地址猜对端。
    established: Option<UdpSession>,
    /// 下一次 `send_audio` 使用的序列号。register 时重置为 1。
    next_sequence: u32,
}

struct UdpInner {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    sessions: Arc<tokio::sync::Mutex<HashMap<u32, SessionEntry>>>,
    incoming: Mutex<Option<mpsc::Receiver<UdpIncomingAudio>>>,
    recv_task: Mutex<Option<JoinHandle<()>>>,
}

/// UDP 音频句柄。clone 共享同一个 socket 和会话表。
#[derive(Clone)]
pub struct UdpAudioHandle {
    inner: Arc<UdpInner>,
}

impl UdpAudioHandle {
    /// 绑定 socket 并启动唯一的接收循环。
    pub async fn start(cfg: UdpAudioConfig) -> Result<Self, UdpAudioError> {
        let socket = UdpSocket::bind(&cfg.bind_addr)
            .await
            .map_err(UdpAudioError::Io)?;
        let local_addr = socket.local_addr().map_err(UdpAudioError::Io)?;
        let socket = Arc::new(socket);
        let sessions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(64);

        let recv_socket = Arc::clone(&socket);
        let recv_sessions = Arc::clone(&sessions);
        let recv_task = tokio::spawn(async move {
            recv_loop(recv_socket, recv_sessions, tx).await;
        });

        Ok(Self {
            inner: Arc::new(UdpInner {
                socket,
                local_addr,
                sessions,
                incoming: Mutex::new(Some(rx)),
                recv_task: Mutex::new(Some(recv_task)),
            }),
        })
    }

    /// 实际监听地址。`bind_addr` 端口为 0 时，这里能拿到系统分配的端口。
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// 注册会话。设备每次 hello 都会把 `remote_sequence_` 清零，所以这里把发送序号重置为 1。
    pub async fn register_session(&self, session_id: u32, crypto: UdpCrypto) {
        let mut sessions = self.inner.sessions.lock().await;
        sessions.insert(
            session_id,
            SessionEntry {
                crypto,
                established: None,
                next_sequence: 1,
            },
        );
    }

    /// 注销会话。之后该 `session_id` 的包按未知会话丢弃。
    pub async fn unregister_session(&self, session_id: u32) {
        let mut sessions = self.inner.sessions.lock().await;
        sessions.remove(&session_id);
    }

    /// 取解密后的上行音频。只能取一次。
    pub fn incoming(&self) -> mpsc::Receiver<UdpIncomingAudio> {
        let mut guard = lock_mutex(&self.inner.incoming);
        guard.take().unwrap_or_else(closed_receiver)
    }

    /// 向指定会话发一帧音频。
    ///
    /// `timestamp` 必须非 0：上下行共用同一把 key，全 0 时间戳会和上行首包撞 IV。
    /// 序列号从 1 开始由内部递增，调用方不要自己填。
    pub async fn send_audio(
        &self,
        session_id: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), UdpAudioError> {
        if timestamp == 0 {
            return Err(UdpAudioError::SendFailed(
                "timestamp 必须非 0，避免上下行共用密钥时 IV 复用".into(),
            ));
        }
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(UdpAudioError::SendFailed(format!(
                "载荷 {} 字节超过单包上限 {MAX_PAYLOAD_LEN}（数据报总长须 <= {MAX_DATAGRAM_LEN}）",
                payload.len()
            )));
        }

        let (peer, packet) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(&session_id)
                .ok_or(UdpAudioError::UnknownSession(session_id))?;
            let peer = entry
                .established
                .as_ref()
                .map(|session| session.peer)
                .ok_or_else(|| {
                    UdpAudioError::SendFailed("尚未收到该会话的上行包，不知道对端地址".into())
                })?;
            // 序号先占住再组包。发送失败会留空洞，但不能回退，否则设备会当成重放丢掉。
            let sequence = entry.next_sequence;
            if sequence == 0 {
                return Err(UdpAudioError::SendFailed(
                    "发送序列号已回绕到 0，需要重新 hello".into(),
                ));
            }
            entry.next_sequence = sequence.wrapping_add(1);

            let payload_len = u16::try_from(payload.len()).map_err(|_| {
                UdpAudioError::SendFailed(format!("载荷 {} 字节无法装进 u16", payload.len()))
            })?;
            let header = entry.crypto.build_header(payload_len, timestamp, sequence);
            let packet = entry
                .crypto
                .seal(&header, payload)
                .map_err(UdpAudioError::Proto)?;
            (peer, packet)
        };

        // 必须用接收时的同一个 socket。换 socket 会换本地地址，设备按源地址过滤会丢掉。
        let sent = self
            .inner
            .socket
            .send_to(&packet, peer)
            .await
            .map_err(UdpAudioError::Io)?;
        if sent != packet.len() {
            return Err(UdpAudioError::SendFailed(format!(
                "UDP 只写出 {sent}/{} 字节",
                packet.len()
            )));
        }
        Ok(())
    }

    /// 停止接收循环。`recv_from` 是 cancel-safe 的，直接 abort 不会吞掉已读包。
    pub async fn shutdown(self) {
        let task = {
            let mut guard = lock_mutex(&self.inner.recv_task);
            guard.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn recv_loop(
    socket: Arc<UdpSocket>,
    sessions: Arc<tokio::sync::Mutex<HashMap<u32, SessionEntry>>>,
    tx: mpsc::Sender<UdpIncomingAudio>,
) {
    let mut buf = [0u8; 2048];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(item) => item,
            Err(err) => {
                tracing::warn!(%err, "UDP 接收失败");
                // abort 会打断这次 sleep。没有这点间隔的话，socket 出错会变成忙等。
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                continue;
            }
        };

        if len < UdpAudioHeader::SIZE {
            tracing::debug!(len, %peer, "UDP 包短于包头，丢弃");
            continue;
        }
        if len > MAX_DATAGRAM_LEN {
            tracing::debug!(len, %peer, "UDP 包超过 1500 字节，丢弃");
            continue;
        }

        let header = match UdpAudioHeader::decode(&buf[..len]) {
            Ok(header) => header,
            Err(err) => {
                tracing::debug!(%err, %peer, "UDP 包头无法解析，丢弃");
                continue;
            }
        };
        // 不按源地址认会话：NAT 端口一变，旧地址就对不上了。
        let session_id = header.ssrc;

        let incoming = {
            let mut sessions = sessions.lock().await;
            let Some(entry) = sessions.get_mut(&session_id) else {
                tracing::debug!(session_id, %peer, "丢弃未知 session 的 UDP 包");
                continue;
            };
            match entry.crypto.open(&buf[..len]) {
                Ok((opened, payload)) => {
                    // 对端只在首次成功解密时锁定，之后不再用包源覆盖。
                    //
                    // 原因：这条链路是 AES-CTR，没有 MAC —— 理论上任何长度
                    // 匹配的报文都能"解密成功"（内容是垃圾）。而 session_id
                    // （= header 里的 ssrc）在包头里是明文，嗅探到一个包就能读到。
                    // 如果每次都拿包源覆盖 established.peer，局域网里的嗅探者
                    // 伪造一包就能把下行音频劫持到自己那里；
                    // 把注入的伪包丢给 ASR 也会污染识别结果。
                    //
                    // 会话在 hello 时由 register_session 重置，所以同一会话内
                    // 对端地址是稳定的；真出现变化（NAT rebinding）按告警处理，
                    // 需要重新 hello 才能恢复。
                    let peer_matches = match &entry.established {
                        None => {
                            entry.established = Some(UdpSession { peer, session_id });
                            true
                        }
                        Some(existing) => existing.peer == peer,
                    };
                    if !peer_matches {
                        tracing::warn!(
                            session_id,
                            %peer,
                            "UDP 源地址与已建立的对端不一致，丢弃该包（不覆盖对端）"
                        );
                        None
                    } else {
                        Some(UdpIncomingAudio {
                            peer,
                            session_id,
                            sequence: opened.sequence,
                            timestamp: opened.timestamp,
                            payload,
                        })
                    }
                }
                Err(err) => {
                    tracing::warn!(session_id, %peer, %err, "UDP 解密失败，丢弃该包");
                    None
                }
            }
        };
        let Some(incoming) = incoming else {
            continue;
        };
        if tx.try_send(incoming).is_err() {
            tracing::debug!(session_id, "上行音频通道已满或已关闭，丢弃该包");
        }
    }
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

/// UDP 音频通道错误。
#[derive(Debug)]
pub enum UdpAudioError {
    /// socket 收发或绑定失败。
    Io(std::io::Error),
    /// `session_id` 没有注册，或已经注销。
    UnknownSession(u32),
    /// 组包或加解密失败。
    Proto(UdpProtoError),
    /// 参数不合法，或还不知道往哪发。
    SendFailed(String),
}

impl std::fmt::Display for UdpAudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "UDP I/O 错误：{err}"),
            Self::UnknownSession(session_id) => {
                write!(f, "未知的 UDP 会话 {session_id}")
            }
            Self::Proto(err) => write!(f, "UDP 协议错误：{err}"),
            Self::SendFailed(reason) => write!(f, "UDP 发送失败：{reason}"),
        }
    }
}

impl std::error::Error for UdpAudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Proto(err) => Some(err),
            Self::UnknownSession(_) | Self::SendFailed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn encode_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }

    fn test_crypto(session_id: u32) -> UdpCrypto {
        let key = [0x11u8; 16];
        let mut nonce = [0u8; 16];
        nonce[0] = 0x01;
        nonce[4..8].copy_from_slice(&session_id.to_be_bytes());
        UdpCrypto::from_hex(&encode_hex(&key), &encode_hex(&nonce)).expect("测试密钥")
    }

    fn seal(crypto: &UdpCrypto, timestamp: u32, sequence: u32, payload: &[u8]) -> Vec<u8> {
        let header = crypto.build_header(
            u16::try_from(payload.len()).expect("测试载荷"),
            timestamp,
            sequence,
        );
        crypto.seal(&header, payload).expect("测试组包")
    }

    async fn start_loopback() -> UdpAudioHandle {
        UdpAudioHandle::start(UdpAudioConfig {
            bind_addr: "127.0.0.1:0".into(),
        })
        .await
        .expect("绑定回环端口")
    }

    #[test]
    fn local_addr_port_is_assigned() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                assert_ne!(handle.local_addr().port(), 0);
                handle.shutdown().await;
            });
    }

    #[test]
    fn registered_session_receives_plaintext() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x1122_3344;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                let packet = seal(&crypto, 60, 1, b"opus-frame");
                client
                    .send_to(&packet, handle.local_addr())
                    .await
                    .expect("发送上行");

                let message = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("等待上行超时")
                    .expect("通道关闭");
                assert_eq!(message.session_id, session_id);
                assert_eq!(message.sequence, 1);
                assert_eq!(message.timestamp, 60);
                assert_eq!(message.payload, b"opus-frame");
                assert_eq!(message.peer, client.local_addr().expect("客户端地址"));
                handle.shutdown().await;
            });
    }

    #[test]
    fn unknown_session_is_dropped() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let known = 0x0102_0304;
                handle.register_session(known, test_crypto(known)).await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                let unknown = seal(&test_crypto(0x00ab_cdef), 60, 1, b"stale");
                client
                    .send_to(&unknown, handle.local_addr())
                    .await
                    .expect("发送未知会话");
                let known_packet = seal(&test_crypto(known), 60, 1, b"keep");
                client
                    .send_to(&known_packet, handle.local_addr())
                    .await
                    .expect("发送已知会话");

                let message = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("等待已知包超时")
                    .expect("通道关闭");
                assert_eq!(message.payload, b"keep");
                assert!(
                    tokio::time::timeout(Duration::from_millis(150), incoming.recv())
                        .await
                        .is_err(),
                    "未知会话的包不应进入 incoming"
                );
                handle.shutdown().await;
            });
    }

    #[test]
    fn unregistered_session_is_dropped() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x5566_7788;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;
                handle.unregister_session(session_id).await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                let packet = seal(&crypto, 60, 1, b"gone");
                client
                    .send_to(&packet, handle.local_addr())
                    .await
                    .expect("发送已注销会话");

                assert!(
                    tokio::time::timeout(Duration::from_millis(200), incoming.recv())
                        .await
                        .is_err(),
                    "注销后的包应被丢弃"
                );
                handle.shutdown().await;
            });
    }

    #[test]
    fn send_audio_can_be_opened_by_peer() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0xa1b2_c3d4;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                let uplink = seal(&crypto, 60, 1, b"punch");
                client
                    .send_to(&uplink, handle.local_addr())
                    .await
                    .expect("打洞");
                let punched = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("打洞包超时")
                    .expect("通道关闭");
                assert_eq!(punched.payload, b"punch");

                handle
                    .send_audio(session_id, 120, b"downlink")
                    .await
                    .expect("下行");
                let mut buf = [0u8; 2048];
                let (len, from) =
                    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
                        .await
                        .expect("等待下行超时")
                        .expect("接收下行");
                assert_eq!(from, handle.local_addr());
                let (header, payload) = crypto.open(&buf[..len]).expect("对端应能解密");
                assert_eq!(payload, b"downlink");
                assert_eq!(header.ssrc, session_id);
                assert_ne!(header.timestamp, 0);
                handle.shutdown().await;
            });
    }

    #[test]
    fn send_audio_sequence_starts_at_one_and_increments() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x1020_3040;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                client
                    .send_to(&seal(&crypto, 60, 7, b"up"), handle.local_addr())
                    .await
                    .expect("打洞");
                tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("打洞包超时")
                    .expect("通道关闭");

                handle
                    .send_audio(session_id, 60, b"a")
                    .await
                    .expect("第一帧");
                handle
                    .send_audio(session_id, 120, b"b")
                    .await
                    .expect("第二帧");

                let mut buf = [0u8; 2048];
                let mut sequences = Vec::new();
                for _ in 0..2 {
                    let (len, _) =
                        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
                            .await
                            .expect("等待下行超时")
                            .expect("接收下行");
                    let (header, _) = crypto.open(&buf[..len]).expect("解密下行");
                    sequences.push(header.sequence);
                }
                assert_eq!(sequences, vec![1, 2]);
                handle.shutdown().await;
            });
    }

    #[test]
    fn send_audio_rejects_zero_timestamp() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x99aa_bbcc;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                let err = handle
                    .send_audio(session_id, 0, b"nope")
                    .await
                    .expect_err("timestamp 0 必须失败");
                assert!(matches!(err, UdpAudioError::SendFailed(_)));

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                client
                    .send_to(&seal(&crypto, 60, 1, b"up"), handle.local_addr())
                    .await
                    .expect("打洞");
                tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("打洞包超时")
                    .expect("通道关闭");

                handle
                    .send_audio(session_id, 180, b"ts")
                    .await
                    .expect("非 0 timestamp");
                let mut buf = [0u8; 2048];
                let (len, _) =
                    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
                        .await
                        .expect("等待下行超时")
                        .expect("接收下行");
                let (header, _) = crypto.open(&buf[..len]).expect("解密下行");
                assert_ne!(header.timestamp, 0);
                assert_eq!(header.timestamp, 180);
                handle.shutdown().await;
            });
    }

    /// 已建立的对端被锁定：另一个源地址发来的合法包必须被丢弃。
    ///
    /// AES-CTR 没有 MAC，session_id 又是明文，所以"能解密"不等于"是设备"。
    /// 如果每次都用包源覆盖 established.peer，局域网嗅探者伪造一包就能
    /// 把下行音频劫持走、并把垃圾数据喂进 ASR。
    #[test]
    fn packet_from_different_peer_is_dropped() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x0a0b_0c0d;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                // 真正的设备端先发一包，锁定对端
                let genuine = UdpSocket::bind("127.0.0.1:0").await.expect("设备 socket");
                genuine
                    .send_to(&seal(&crypto, 60, 1, b"from-device"), handle.local_addr())
                    .await
                    .expect("发送合法包");
                let message = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("应收到设备包")
                    .expect("通道关闭");
                assert_eq!(message.payload, b"from-device");

                // 伪造者用另一个源端口发合法加密包 → 必须被丢弃
                let attacker = UdpSocket::bind("127.0.0.1:0").await.expect("伪造 socket");
                attacker
                    .send_to(
                        &seal(&crypto, 120, 2, b"from-attacker"),
                        handle.local_addr(),
                    )
                    .await
                    .expect("发送伪包");
                assert!(
                    tokio::time::timeout(Duration::from_millis(300), incoming.recv())
                        .await
                        .is_err(),
                    "异源包不应进入上行通道"
                );

                // 对端没被覆盖：设备再发包仍应正常收到
                genuine
                    .send_to(&seal(&crypto, 180, 3, b"device-again"), handle.local_addr())
                    .await
                    .expect("设备再发");
                let message = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("设备包应仍能收到")
                    .expect("通道关闭");
                assert_eq!(message.payload, b"device-again");
                handle.shutdown().await;
            });
    }

    #[test]
    fn oversized_payload_is_rejected() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
                let err = handle
                    .send_audio(1, 60, &payload)
                    .await
                    .expect_err("超长载荷必须失败");
                assert!(matches!(err, UdpAudioError::SendFailed(_)));
                assert!(err.to_string().contains("1484"), "{err}");
                handle.shutdown().await;
            });
    }

    #[test]
    fn tampered_payload_is_dropped_without_panic() {
        tokio::runtime::Runtime::new()
            .expect("测试运行时")
            .block_on(async move {
                let handle = start_loopback().await;
                let mut incoming = handle.incoming();
                let session_id = 0x0d0e_0f10;
                let crypto = test_crypto(session_id);
                handle
                    .register_session(session_id, test_crypto(session_id))
                    .await;

                let client = UdpSocket::bind("127.0.0.1:0").await.expect("客户端 socket");
                let mut tampered = seal(&crypto, 60, 1, b"intact-payload");
                // 截掉一字节后 payload_len 和实际长度对不上，open 会失败。
                // CTR 本身不认证，只翻密文字节不会报错。
                tampered.pop();
                client
                    .send_to(&tampered, handle.local_addr())
                    .await
                    .expect("发送篡改包");
                let good = seal(&crypto, 120, 2, b"still-alive");
                client
                    .send_to(&good, handle.local_addr())
                    .await
                    .expect("发送合法包");

                let message = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                    .await
                    .expect("合法包应仍能收到")
                    .expect("通道关闭");
                assert_eq!(message.payload, b"still-alive");
                assert_eq!(message.sequence, 2);
                handle.shutdown().await;
            });
    }
}
