# 03 - 任务单（M2' 分解为 T1..T5）

> 实现依据：`02-design.md`（协议规格全部来自固件源码 + 社区服务端交叉验证）
> 每个任务完成后必须 `./scripts/ci.sh` 全绿。
> 验收：`bash specs/mqtt-udp/04-acceptance.sh --task T1`

---

### T1 — `protocol_udp.rs`：UDP 包编解码 + AES-CTR

### 目标

新增 `crates/haimen-xiaozhi/src/protocol_udp.rs`，实现 UDP 音频包的**纯函数**编解码与加解密。
不涉及任何 I/O、socket、MQTT。全部可单元测试。

### 协议（照抄 `02-design.md` §2.3，不要自行发挥）

```
偏移  长度  字段          说明
0     1    type          nonce[0]，约定 0x01
1     1    flags         nonce[1]，约定 0x00
2     2    payload_len   大端
4     4    ssrc          nonce[4..8)
8     4    timestamp     大端，单位毫秒
12    4    sequence      大端，从 1 开始
16    N    payload       AES-128-CTR 加密的 Opus
```

**★ AES-CTR 的关键**：
```rust
// 每个包的计数器块 = 该包自己的 16 字节包头
let mut counter = [0u8; 16];
counter.copy_from_slice(&packet[0..16]);
// 用 counter 作为 AES-CTR 的初始计数器块，对 packet[16..] 加/解密
```

**不是**用 hello 里那个固定 nonce 直接加密 —— 必须先按包覆盖 len/timestamp/sequence。

### 要做的事

**A. 定义公开类型与函数**

```rust
/// UDP 音频包头（16 字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpAudioHeader {
    pub packet_type: u8,   // 通常 0x01
    pub flags: u8,         // 通常 0x00
    pub payload_len: u16,  // 大端存储
    pub ssrc: u32,
    pub timestamp: u32,    // 毫秒
    pub sequence: u32,
}

impl UdpAudioHeader {
    pub const SIZE: usize = 16;
    /// 序列化为 16 字节大端表头
    pub fn encode(&self) -> [u8; 16];
    /// 从至少 16 字节的切片解析
    pub fn decode(buf: &[u8]) -> Result<Self, UdpProtoError>;
}

/// 通道密钥（来自服务器 hello 的 udp.key / udp.nonce）
pub struct UdpCrypto {
    key: [u8; 16],
    /// 基础 nonce：其中 [0..2) 是 type/flags，[4..8) 是 ssrc
    base_nonce: [u8; 16],
}

impl UdpCrypto {
    /// 从 hex 字符串构造（必须正好 32 个 hex 字符 = 16 字节）
    pub fn from_hex(key_hex: &str, nonce_hex: &str) -> Result<Self, UdpProtoError>;

    /// 构造成出站包头：以 base_nonce 为模板，覆盖 len/timestamp/sequence
    pub fn build_header(&self, payload_len: u16, timestamp: u32, sequence: u32)
        -> UdpAudioHeader;

    /// 加密一个出站包：返回 16 字节头 + 加密载荷
    pub fn seal(&self, header: &UdpAudioHeader, payload: &[u8])
        -> Result<Vec<u8>, UdpProtoError>;

    /// 解密一个入站包：从包自身前 16 字节解析头并解密载荷
    /// 返回 (header, 明文 payload)
    pub fn open(&self, packet: &[u8]) -> Result<(UdpAudioHeader, Vec<u8>), UdpProtoError>;
}

/// 错误类型
///
/// 注意：`thiserror` 虽然在工作区依赖里（Cargo.toml:47），但
/// `haimen-xiaozhi` 这个 crate 并没有引用它，而本任务**禁止改动依赖清单**。
/// 所以请手写 `std::fmt::Display` + `std::error::Error`（约 15 行），
/// 不要 `use thiserror::Error`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpProtoError {
    /// hex 字符串非法（长度不是 32，或含非 hex 字符）
    InvalidHex { field: &'static str, reason: String },
    /// 数据包短于 16 字节头
    PacketTooShort { len: usize },
    /// payload_len 字段与实际载荷长度不符
    LengthMismatch { declared: u16, actual: usize },
    /// 载荷过长，无法用 u16 表示
    PayloadTooLarge { len: usize },
}

impl std::fmt::Display for UdpProtoError { /* 中文错误描述 */ }
impl std::error::Error for UdpProtoError {}
```

**B. 错误处理**

用 `thiserror`。需要覆盖的错误场景：
- hex 字符串长度不对 / 含非法字符
- 包短于 16 字节
- `payload_len` 与实际长度不符
- 载荷过长（> u16::MAX）
- AES-CTR 初始化失败（理论上不会，但别 panic）

**C. 单元测试（至少 10 个，覆盖以下场景）**

1. 头编解码 round-trip：任意字段值编码后解码得到相同结构
2. 头字节序：`timestamp = 0x12345678` 编码后 `buf[8..12] == [0x12,0x34,0x56,0x78]`
3. `payload_len` 大端：长度 300 → `buf[2..4] == [0x01, 0x2C]`
4. AES-CTR round-trip：seal 后 open，载荷完全一致
5. **计数器块一致性**：seal 出来的包，前 16 字节必须等于解密时用的计数器块；用固定 key/nonce/seq 断言密文与预期一致（可用已知向量）
6. payload_len 与实际不符 → 报错
7. 包短于 16 字节 → 报错
8. 非法 hex → 报错
9. hex 长度不是 32 → 报错
10. 空载荷（payload_len = 0）能正常 seal/open
11. `build_header` 会保留 base_nonce 的 [0..2) 和 [4..8)，覆盖其余

**D. 导出**

`crates/haimen-xiaozhi/src/lib.rs` 里加 `pub mod protocol_udp;`（或按现有模块声明风格）。

### 硬性约束

- **只改/新增**：`crates/haimen-xiaozhi/src/protocol_udp.rs`、`crates/haimen-xiaozhi/src/lib.rs`
- **不改** `ws.rs` / `protocol.rs` / `strategy.rs` / `types.rs`
- **不加新依赖**：`aes`、`ctr`、`thiserror` 已在 workspace（`thiserror` 若不在，用 `std::fmt::Display` + `std::error::Error` 手写，**不要加依赖**）
- 库代码禁止 `unwrap()` / `expect()` / `panic!()`（测试代码除外）
- 注释用中文，关键处说明「为什么」（尤其是 AES-CTR 计数器块 = 包头这件事）
- 不写 I/O、不碰 socket

### 自检

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```

---

### T2 — `mqtt_broker.rs`：内置最小 MQTT broker

### 为什么必须自建 broker（不可跳过）

**固件从不订阅任何主题**（已三重验证）：
- `main/protocols/mqtt_protocol.cc` 全文只有 `Publish` / `OnMessage`，无 `Subscribe`
- `78/esp-ml307` 的 `EspMqtt::Connect()` 只做 `esp_mqtt_client_init/start`，不订阅
- 全固件仓库 grep `subscribe` → **0 行**；grep `subscribe_topic` → **0 行**

后果：设备走 MQTT 时**永远收不到服务器消息**。而语音链路必须有下行：
hello 响应（UDP key/nonce）、stt、tts start / sentence_start / stop、mcp。

标准 broker（Mosquitto / EMQX 默认配置）只投递"已订阅"的消息 →
**MQTT 模式在它们上面完全不可用**（表现为"连上了但一唤醒就 10 秒超时"）。

自建 broker 才能直接对连接下发 PUBLISH，无需订阅。这也是社区 golang 服务端的做法。

### 目标

在 haimen 内实现一个**最小但正确**的 MQTT 3.1.1 broker，够设备用：
收设备上行 PUBLISH、向指定 client 主动下发 PUBLISH、维持心跳。

### 不做（明确划界）

- 不做 QoS 2（设备只用 QoS 0/1）
- 不做 retained message
- 不做 wildcard 订阅路由（订阅请求照常回 SUBACK，但不建立转发关系 —— 我们只做定向推送）
- 不做 TLS（局域网页明，M2' 不做 8883）
- 不做持久会话（`clean_session=false` 也按 false 应答但不恢复状态）

### 要实现的协议子集

**固定头**：`| type(1B) | flags(1B) | remaining_length(varint) |`
**变长整数编码**：每字节低 7 位有效，最高位为续位，最多 4 字节

| type | 名称 | 方向 | 处理 |
|---|---|---|---|
| 1 | CONNECT | 设备→broker | 解析 client_id / username / password / keepalive / clean_session |
| 2 | CONNACK | broker→设备 | 回 `session_present=0` + `return_code=0`（成功） |
| 3 | PUBLISH | 双向 | 上行：解析 topic + payload，投递给 haimen 上层；下行：由 haimen 主动构造 |
| 4 | PUBACK | 双向 | QoS1 确认 |
| 8 | SUBSCRIBE | 设备→broker | **解析并回 SUBACK（granted_qos 原样）**，但不建立转发关系 |
| 9 | SUBACK | broker→设备 | |
| 10 | UNSUBSCRIBE | 设备→broker | 回 UNSUBACK |
| 11 | UNSUBACK | broker→设备 | |
| 12 | PINGREQ | 设备→broker | 回 PINGRESP |
| 13 | PINGRESP | broker→设备 | |
| 14 | DISCONNECT | 设备→broker | 关闭连接，不回报文 |

**QoS 处理**：
- 收到 QoS 0 PUBLISH：直接投递，不回包
- 收到 QoS 1 PUBLISH：投递后回 PUBACK（用对方的 packet id）
- 向下发 QoS 0 PUBLISH：最简单，优先用这个（设备不要求 ack）

### 公开 API（照此实现，不要自行改签名）

```rust
/// broker 启动参数
#[derive(Debug, Clone)]
pub struct MqttBrokerConfig {
    /// 监听地址，如 "0.0.0.0:1883"
    pub bind_addr: String,
    /// 单连接最大包体（防止恶意大包），默认 256 KiB
    pub max_packet_size: usize,
}

/// 一条从设备上来的应用消息
#[derive(Debug, Clone)]
pub struct BrokerIncoming {
    /// 发起连接的 client_id（haimen 靠它反解设备 MAC）
    pub client_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
    /// QoS 0 时为 None
    pub packet_id: Option<u16>,
}

/// 一条要下发给设备的消息
#[derive(Debug, Clone)]
pub struct BrokerOutgoing {
    /// 目标 client_id
    pub client_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
}

/// broker 句柄，可 clone，内部用 Arc
#[derive(Clone)]
pub struct MqttBrokerHandle {
    /* 内部：注册表 Arc<Mutex<HashMap<String, ConnSender>>> + 接收队列 */
}

impl MqttBrokerHandle {
    /// 启动 broker（绑定监听端口），返回句柄。
    /// 内部 spawn 一个 accept task。
    pub async fn start(cfg: MqttBrokerConfig) -> Result<Self, MqttBrokerError>;

    /// 向指定 client 下发一条消息。client 不在线则返回 Ok(false)。
    pub async fn publish_to(&self, msg: BrokerOutgoing) -> Result<bool, MqttBrokerError>;

    /// 当前在线 client 列表（便于调试/诊断）。
    pub async fn connected_clients(&self) -> Vec<String>;

    /// 取一条上行消息。用 tokio mpsc 的 Receiver。
    /// 建议签名：pub fn take_incoming(&self) -> mpsc::Receiver<BrokerIncoming>
    /// 或在 start() 时就把 Receiver 返回。
    pub fn incoming(&self) -> tokio::sync::mpsc::Receiver<BrokerIncoming>;
}

#[derive(Debug)]
pub enum MqttBrokerError {
    Io(std::io::Error),
    Protocol(String),
    NotStarted,
}
```

**建议**：`MqttBrokerHandle::start` 返回 `(handle, incoming_rx)` 更简单，二选一都行，
但必须在文档注释里写清楚。

### 实现要点（每一条都是坑）

1. **CONNECT 的 client_id 必须按二进制读**，不要假设是 UTF-8 安全字符串。
   小智的 client_id 形如 `GID_xxx@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64`。

2. **读完 CONNECT 必须回 CONNACK**，否则设备的 `esp_mqtt_client` 会一直等，
   `Connect()` 超时返回，表现为"连不上"。

3. **变长整数（remaining_length）**：编码时用小端续位，
   `0x00..0x7F` 一字节；解码要循环到最高位为 0。**最多 4 字节，超出报错。**

4. **PUBLISH 的 topic 是长度前缀的**（2 字节大端长度 + UTF-8 字节），
   QoS>0 时后面还有 2 字节 packet id。

5. **每个连接一个独立 tokio task** 做读循环；写用 `mpsc` 投递到该连接的写 task。
   **不要**在注册表的锁里做 I/O。

6. **断连必须从注册表摘除**，否则 `publish_to` 会一直往死连接写。

7. **keepalive**：设备连上时协商的 keepalive 秒数（小智默认 240），
   超过 1.5 倍无包视为超时 → 主动断开。**M2' 可以先不实现超时检测**，
   但要记录 keepalive 值。

8. **SUBSCRIBE 必须回 SUBACK**，否则某些客户端会重传。

9. **PINGREQ 必须回 PINGRESP**，否则 240 秒后连接被设备断开。

10. **写失败要能感知**：写 task 出错时清理注册表。用 `tokio::sync::mpsc::Sender`
    的 `send().await` 返回 Err 表示写 task 已退出。

### 单元/集成测试（至少 8 个）

**纯函数测试（不需要网络）**：
1. 变长整数编码/解码：`0`、`127`、`128`、`16383`、`16384`、`2097151`、`268435455`
2. 变长整数：超过 4 字节 → 报错
3. CONNECT 报文解析：构造一个真实的 CONNECT 字节流，断言 client_id / keepalive / clean_session
4. PUBLISH 报文解析：QoS0 与 QoS1 各一例，断言 topic / payload / packet_id
5. CONNACK 构造：断言字节序列
6. PINGRESP 构造：断言字节序列 `[0xD0, 0x00]`

**回环集成测试（用 `127.0.0.1:0` 起 broker + 自己写客户端）**：
7. 完整握手：CONNECT → CONNACK → PUBLISH 上行 → 能在 `incoming()` 收到
8. `publish_to` 下发：断言客户端能收到 PUBLISH 且 topic/payload 正确
9. 客户端断开后 `connected_clients()` 不再包含它
10. PINGREQ → PINGRESP

> 测试里不要依赖外部 broker，用 `127.0.0.1:0` 自动选端口。

### 硬性约束

- **只改/新增**：`crates/haimen-xiaozhi/src/mqtt_broker.rs`、`crates/haimen-xiaozhi/src/lib.rs`
- **不改** `ws.rs` / `protocol.rs` / `strategy.rs` / `types.rs` / `ota.rs`
- **不加新依赖**（`tokio` 已在，`serde_json` 不需要 —— broker 只管字节，不解析 JSON）
- 库代码禁止 `unwrap()` / `expect()` / `panic!()`；错误用 `MqttBrokerError`
  手写 `Display` + `std::error::Error`（不要引入 thiserror）
- 注释用中文，关键处说明「为什么」

### 自检

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```

---

### T3 — `mqtt.rs` + `udp_audio.rs`：I/O 层

### 目标

新增两个文件，只做 I/O，不含业务状态机：
- `mqtt.rs`：rumqttc 客户端封装 —— 连接/发布/事件流
- `udp_audio.rs`：单个 UDP socket 服务多设备 —— 收发/分流/序列号

### A. `mqtt.rs`

```rust
/// MQTT 客户端配置
#[derive(Debug, Clone)]
pub struct MqttClientConfig {
    pub host: String,
    pub port: u16,              // 明文必须 1883（8883 固件会强制 TLS）
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub keep_alive_secs: u64,   // 小智默认 240
    pub request_channel_cap: usize, // 建议 32..64，不要 0
}

/// 一条收到的 MQTT 消息
#[derive(Debug, Clone)]
pub struct MqttIncoming {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// 客户端句柄（clone 廉价，内部 Arc）
#[derive(Clone)]
pub struct MqttClientHandle { /* ... */ }

impl MqttClientHandle {
    /// 启动：内部 spawn EventLoop task 持续 poll（★ 必须，否则连接不建立、
    /// publish().await 会在 cap 满后永久挂起）
    pub async fn start(cfg: MqttClientConfig) -> Result<Self, MqttClientError>;

    /// 发布。QoS0 即可（设备用 QoS0）。
    /// ★ 禁止在音频循环里 await 这个 —— 用 try_publish 或专用网络 task
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), MqttClientError>;

    /// 取上行消息
    pub fn incoming(&self) -> tokio::sync::mpsc::Receiver<MqttIncoming>;

    /// 当前是否已连接
    pub fn is_connected(&self) -> bool;

    /// 优雅关闭
    pub async fn shutdown(self);
}

#[derive(Debug)]
pub enum MqttClientError {
    Connect(String),
    NotConnected,
    Io(std::io::Error),
}
```

**实现要点（来自实测验证过的库指南 specs/mqtt-udp/库用法指南.md）**：
1. `AsyncClient::new(options, cap)` 返回 `(AsyncClient, EventLoop)`，cap 建议 32..64
2. **EventLoop 必须独占放进独立 tokio task 持续 `poll().await`**，否则：
   连接不建立；cap 满后 `publish().await` 永久挂起
3. rumqttc 0.24 的 builder 是 `set_keep_alive` / `set_credentials` / `set_clean_session`，
   返回 `&mut Self`，不能把链式结果直接传给按值接收 `MqttOptions` 的 `AsyncClient::new`
4. `publish().await` 只代表请求进入 EventLoop channel，**不代表出网**；
   M2' 用 QoS0，不追踪 ack
5. 每次 `ConnAck` 后重新标记已连接；`poll()` 出错自动重连，**不要**用 `select!` 取消 `poll()`
6. 优雅关闭：`disconnect()` 后等 `Outgoing::Disconnect`

### B. `udp_audio.rs`

```rust
/// UDP 音频通道配置
#[derive(Debug, Clone)]
pub struct UdpAudioConfig {
    /// 监听地址，如 "0.0.0.0:8888"
    pub bind_addr: String,
}

/// 一个会话的收发上下文
pub struct UdpSession {
    /// 对端地址（收到首个上行包后才确定）
    pub peer: std::net::SocketAddr,
    /// 会话 ID（= 我们下发的 nonce[4..8)）
    pub session_id: u32,
}

/// 收到的音频包（已解密）
#[derive(Debug, Clone)]
pub struct UdpIncomingAudio {
    pub peer: std::net::SocketAddr,
    pub session_id: u32,
    pub sequence: u32,
    pub timestamp: u32,
    /// 已解密的 Opus 载荷
    pub payload: Vec<u8>,
}

/// 句柄
#[derive(Clone)]
pub struct UdpAudioHandle { /* ... */ }

impl UdpAudioHandle {
    pub async fn start(cfg: UdpAudioConfig) -> Result<Self, UdpAudioError>;

    /// 实际监听端口（bind_addr 端口为 0 时可取到系统分配的端口）
    pub fn local_addr(&self) -> std::net::SocketAddr;

    /// 注册一个会话（下发 hello 后调用）
    /// 内部记录 session_id → UdpCrypto，之后该 session 的包用对应 key 解密
    pub async fn register_session(&self, session_id: u32, crypto: UdpCrypto);

    /// 注销会话（goodbye / 超时）
    pub async fn unregister_session(&self, session_id: u32);

    /// 取解密后的上行音频
    pub fn incoming(&self) -> tokio::sync::mpsc::Receiver<UdpIncomingAudio>;

    /// 向指定会话发一个音频帧（内部构造 header + AES 加密）
    /// ★ timestamp 必须非 0（规避与上行共用 key 的 IV 复用）
    /// ★ sequence 由内部维护，首次调用必须 >= 1 且严格递增
    pub async fn send_audio(
        &self,
        session_id: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), UdpAudioError>;

    pub async fn shutdown(self);
}

#[derive(Debug)]
pub enum UdpAudioError {
    Io(std::io::Error),
    UnknownSession(u32),
    Proto(UdpProtoError),
    SendFailed(String),
}
```

**实现要点**：
1. **单 socket 服务多设备**：`Arc<tokio::net::UdpSocket>`，`recv_from`/`send_to` 都只借 `&self`
2. **会话识别靠 `session_id`（= nonce[4..8)）**，不要只靠源地址（NAT/重连会变）
3. 收到未知 session 的包 → 丢弃并 `debug!` 日志（可能是过期的旧会话）
4. 发送必须用**同一个 socket、同一个本地 IP**，目标地址取该 session 记录的 `peer`
5. **sequence 严格递增，首包 >= 1**；设备每次 hello 会把 `remote_sequence_` 清零 →
   `register_session` 时要重置我们的发送 sequence
6. **timestamp 必须非 0**（上下行共用 key，非 0 可保证 IV 不碰撞）
7. 单包总长 <= 1500：payload 超过 ~1484 字节要报错
8. 解密失败 → 丢弃 + `warn!`（不要 panic）

### 测试（至少 10 个）

**`mqtt.rs`**：
1. 配置校验：port 8883 时给出警告或拒绝（明文场景）
2. 未连接时 publish 返回 `NotConnected` 而不是挂起

**`udp_audio.rs`（用 `127.0.0.1:0` 回环，不需要真设备）**：
3. 启动后 `local_addr()` 端口非 0
4. `register_session` 后用另一个 socket 发一个合法包 → `incoming()` 能收到，payload 正确
5. 未知 session 的包被丢弃（`incoming()` 收不到）
6. `unregister_session` 后再发包 → 丢弃
7. `send_audio` 后对端能收到合法包（用 `UdpCrypto::open` 验证可解）
8. `send_audio` 的 sequence 从 1 开始严格递增
9. `send_audio` 的 timestamp 非 0
10. 超长 payload 返回错误
11. 解密失败（篡改载荷）→ 丢弃不 panic

### 硬性约束

- **只改/新增**：`crates/haimen-xiaozhi/src/mqtt.rs`、`crates/haimen-xiaozhi/src/udp_audio.rs`、
  `crates/haimen-xiaozhi/src/lib.rs`（加 pub mod）
- **不改** T1/T2 的文件（`protocol_udp.rs` / `mqtt_broker.rs`），除非发现它们有 bug
  （真有 bug 就在注释里说明并最小修改）
- **不改** `ws.rs` / `protocol.rs` / `strategy.rs` / `types.rs` / `ota.rs`
- **不加新依赖**（`rumqttc` / `tokio` / `aes` / `ctr` 都已引入）
- 库代码禁止 `unwrap()` / `expect()` / `panic!()`；错误手写 `Display` + `Error`
- 注释用中文，关键处说明「为什么」

### 自检

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace -- --test-threads=1
```


---

### T4 — `mqtt_udp.rs`：传输层（核心）

### 架构澄清（重要）

haimen **自己托管 MQTT broker**（T2 已实现），所以设备链路**不走 rumqttc 客户端**：

```
设备 ──MQTT──▶ haimen 内置 broker ──incoming()──▶ MqttUdpTransport
                      ▲                              │
                      └────── publish_to() ──────────┘
                                                     ▼
                                              UdpAudioHandle（T3）
                                                     │
                                                     ▼
                                              ResponseStrategy（现有）
```

- `mqtt.rs`（T3 的 rumqttc 客户端）在 M2' 的**设备链路里用不到**，
  它是为「连外部 broker」场景准备的，M2' 不接线。
- T4 直接用 `MqttBrokerHandle` 的 `incoming()` / `publish_to()`。

### 目标

新增 `crates/haimen-xiaozhi/src/mqtt_udp.rs`，实现完整传输层：
MQTT 控制消息 + UDP 音频，驱动现有的 `ResponseStrategy` 产出语音回复。

### 公开 API

```rust
#[derive(Debug, Clone)]
pub struct MqttUdpConfig {
    pub broker: MqttBrokerConfig,
    pub udp: UdpAudioConfig,
    /// 设备上行主题模板，`{mac}` 替换为带冒号的 MAC（如 84:c7:bb:59:93:64）
    pub publish_topic_template: String,
    /// 服务器下行主题模板（设备忽略 topic，仅供可读性/日志）
    pub downlink_topic_template: String,
    /// 预缓冲帧数，默认 5（与 ws.rs 一致）
    pub prebuffer_count: usize,
    /// 帧时长毫秒，默认 60
    pub frame_duration_ms: u32,
}

pub struct MqttUdpTransport { /* ... */ }

impl MqttUdpTransport {
    /// 启动：绑 broker + UDP，内部 spawn 主循环
    pub async fn start(
        cfg: MqttUdpConfig,
        strategy: Arc<dyn ResponseStrategy>,
    ) -> Result<Self, MqttUdpError>;

    /// 日志/诊断用的运行信息
    pub fn broker_addr(&self) -> String;
    pub fn udp_addr(&self) -> std::net::SocketAddr;

    /// 当前在线设备数
    pub async fn online_device_count(&self) -> usize;

    pub async fn shutdown(self);
}

#[derive(Debug)]
pub enum MqttUdpError {
    Broker(MqttBrokerError),
    Udp(UdpAudioError),
    Proto(UdpProtoError),
}
```

### 会话状态

直接复用 `types.rs` 的 `SessionState`（4 态）：
```
AwaitingHello → Ready → Recording → Playing
                          ↑____________|
```

每个连接（按 `client_id` 区分）维护一份会话：
```rust
struct DeviceSession {
    client_id: String,
    device_id: String,        // 从 client_id 的 @@@ 分段里取 MAC
    state: SessionState,
    /// UDP 会话 ID（= 下发 nonce 的 [4..8)）
    udp_session_id: Option<u32>,
    /// 录音阶段缓冲的音频帧（给 strategy 用）
    audio_buffer: Vec<AudioFrame>,
    /// 发送侧帧节奏
    next_send_at: Option<Instant>,
    /// 下行 timestamp 基数（★ 必须非 0，规避 IV 复用）
    downlink_ts: u32,
}
```

### 完整流程（按此实现）

**① 设备连上 broker**
- `MqttBrokerHandle` 内部已有连接；我们通过 `incoming()` 收消息
- 首次收到某 `client_id` 的消息时创建会话，状态 `AwaitingHello`

**② 收到 `{"type":"hello","transport":"udp",...}`**
1. 解析设备上报的 `audio_params`（上行 16000/mono/60，容错：缺失用默认）
2. 调 `strategy.hello_audio_params(&client_params)` 拿到**下行**参数
   （`AsrLlmTtsStrategy` 会返回 24000Hz）
3. 生成会话 ID（u32 随机）与 16 字节 key（随机）
4. 构造 nonce 模板：`01 00 00 00 | session_id(4B BE) | 00 00 00 00 | 00 00 00 00`
5. `udp.register_session(session_id, UdpCrypto::new(key, nonce))`
6. 通过 `broker.publish_to(client_id, downlink_topic, hello_json)` 回 hello：
   ```json
   {"type":"hello","transport":"udp","session_id":"<uuid或session_id>",
    "audio_params":{"format":"opus","sample_rate":24000,"channels":1,"frame_duration":60},
    "udp":{"server":"<局域网IP>","port":<udp端口>,
           "key":"<32 hex>","nonce":"<32 hex>"}}
   ```
   ⚠️ `server` 必须是**局域网 IP**，用 `local_ip_address::local_ip()`，不能是 0.0.0.0/127.0.0.1
7. 状态 → `Ready`

**★ 硬性要求**：必须先收到设备 hello 才能回（提前发会被设备清掉事件位；且会重置 key/seq）。
设备只等 **10 秒**，超时就失败，且**没有任何 ACK**。

**③ 收到 `{"type":"listen","state":"start",...}`**
- 状态 → `Recording`
- 清空 `audio_buffer`
- 调 `strategy.on_recording_start(session_id)`

**④ 收到 UDP 上行音频（`udp.incoming()`）**
- 从 `session_id` 找到会话
- 状态必须是 `Recording`，否则丢弃 + debug 日志
- 构造 `AudioFrame { timestamp, data }` 追加到 `audio_buffer`
- 调 `strategy.on_audio_frame(frame)`（`AsrLlmTtsStrategy` 在这里喂 ASR）

**⑤ 判定"用户说完了"**
两种来源，取先到者（与 ws.rs 语义一致，但这里更简单）：
- 收到 `{"type":"listen","state":"stop"}`（设备主动）
- 或收到 `{"type":"abort"}`

→ 状态 → `Playing`，然后：
```rust
// 优先用流式接口；strategy 的 is_streaming_capable() 为 true 时走 stream
let events_rx = strategy.generate_response_stream(audio_buffer.clone(), &session_id, frame_tx).await;
```
`frame_tx` 是 `mpsc::Sender<PlaybackEvent>`，策略在其中投递：
- `PlaybackEvent::Stt(text)` → 发 `{"type":"stt","text":...}`
- `PlaybackEvent::LlmSentence(text)` → 发 `{"type":"tts","state":"sentence_start","text":...}`
- `PlaybackEvent::Audio(frame)` → **加密后经 UDP 发出**
- `PlaybackEvent::TtsState(...)` → 发对应的 tts 状态消息

⚠️ **发送顺序必须是**：`tts start` → 若干 `sentence_start` + 音频帧 → `tts stop`。
设备要求 **先收到 `tts start` 才会播放音频**，否则丢弃。

**⑥ 帧节奏（从 ws.rs 复制，顶部注释标注来源）**

```
① PREBUFFER_COUNT = 5：收到第一帧音频前先连发 5 帧（300ms）填满设备 jitter buffer
② next_send_at 累加 60ms：next_send_at += 60ms（不是 now + 60ms，否则逐帧变慢）
③ wait_until_send_slot：绝对时刻对齐
④ snap_send_clock：断档（队列空超过 1 帧）后把时钟快进到 now，避免补偿式连发
```

**UDP 特有约束**：
- 必须按 **~60ms/包实时**发送。设备解码队列只有 20 包（1.2s），发太快会**静默丢包**
- 单包总长 <= 1500 字节
- 下行 timestamp **从 1 开始按 60 递增**（非 0，规避 IV 复用）
- 下行 sequence 从 1 开始严格递增，`hello` 时重置

**⑦ 结束**
- 收到 `{"type":"goodbye"}` 或 `{"type":"abort"}` 或长时间无消息：
  - `udp.unregister_session(session_id)`
  - 状态 → `Ready`（不要直接断 MQTT，设备会继续复用连接）
- MQTT 断连（broker 侧感知）→ 清理整个会话

**⑧ 会话超时**
- 设备没有 no-speech 自动回 Idle，**必须服务器驱动**
- `Ready` 状态超过 `no_speech_timeout_ms`（默认 10000）无 listen → 不做特殊处理（M2' 简化）
- `Recording` 超过 30 秒 → 强制 `generate_response`

### 必须做的防御

1. **未知 `session_id` 的 UDP 包** → 丢弃（可能是旧会话残留）
2. **`Recording` 之外收到音频** → 丢弃
3. **设备没发 hello 就先发 listen** → 忽略 + warn
4. **`publish_to` 返回 false（设备掉线）** → 清理会话
5. **JSON 解析失败** → warn + 丢弃，不要 panic
6. **同 `client_id` 重复 hello** → 视为重连：注销旧 UDP 会话，重新生成 key/nonce

### 测试（至少 12 个）

用 `127.0.0.1:0` 起 broker + UDP，自己写一个极简设备客户端（可复用 T2 测试里的辅助代码）：

1. 完整 hello 握手：设备发 hello → 收到合规 hello 响应（key/nonce 均 16 字节、port 合法）
2. hello 响应的 `udp.server` 不是 0.0.0.0 / 127.0.0.1
3. hello 响应的 nonce[0] == 0x01
4. 未发 hello 就发 listen → 被忽略
5. listen start 后状态变 Recording，`on_recording_start` 被调用（用 mock strategy）
6. 发 UDP 音频帧 → `on_audio_frame` 被调用，帧内容正确
7. listen stop → 触发 `generate_response`，收到 `tts start`
8. 音频帧按预缓冲数量先连发（前 N 帧间隔 << 60ms）
9. 后续帧间隔约 60ms（断言 40..90ms 容差）
10. 下行 sequence 从 1 严格递增
11. 下行 timestamp 非 0 且按 60 递增
12. goodbye 后 UDP 会话被注销，后续该 session 的包被丢弃
13. 重复 hello 会重新生成 key/nonce

### 硬性约束

- **只改/新增**：`crates/haimen-xiaozhi/src/mqtt_udp.rs`、`crates/haimen-xiaozhi/src/lib.rs`
- 可以**读**但**不改** `ws.rs`（只复制节奏逻辑，顶部注释标注 `源自 ws.rs:xxx`）
- **不改** T1/T2/T3 的文件（除非发现 bug，需注释说明并最小修改）
- **不改** `protocol.rs` / `strategy.rs` / `types.rs` / `ota.rs`
- **不加新依赖**
- 库代码禁止 `unwrap()` / `expect()` / `panic!()`；错误手写 `Display` + `Error`
- 注释用中文，关键处说明「为什么」

### 自检

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace -- --test-threads=1
```


---

### T5 — OTA 改造 + 配置

（待 T4 完成后细化）

- `types.rs`：`OtaResponse.mqtt: Option<MqttInfo>`，`websocket` 改 `Option`
- `ota.rs`：`Json(_body)` → `Json(body)`，用 `body.mac_address` 生成 client_id / subscribe_topic
- `settings.rs`：新增 `[xiaozhi]` / `[xiaozhi.mqtt]` / `[xiaozhi.udp]`
- `lib.rs`：按配置挂载 MQTT 入口

---

### T6 — 设备模拟器 + 端到端脚本

（待 T5 完成后细化）

- `scripts/xiaozhi_sim/device_sim.py`
- `scripts/test-mqtt-udp.sh`
