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

### T2 — `mqtt.rs` + `udp_audio.rs`：I/O 层

（待 T1 完成后细化）

- `mqtt.rs`：rumqttc 封装，连接/订阅/发布 + 事件流（`mpsc`）
- `udp_audio.rs`：单 UDP socket 多设备复用，按源地址分流，收发队列，序列号管理

---

### T3 — `mqtt_udp.rs`：传输层

（待 T2 完成后细化）

- 会话状态机（`AwaitingHello / Ready / Recording / Playing`）
- hello 握手、UDP 建链、对接 `strategy.rs`
- 帧节奏（复制 `ws.rs` 的 `next_send_at` / `wait_until_send_slot` / `snap_send_clock`，顶部注释标注来源）

---

### T4 — OTA 改造 + 配置

（待 T3 完成后细化）

- `types.rs`：`OtaResponse.mqtt: Option<MqttInfo>`，`websocket` 改 `Option`
- `ota.rs`：`Json(_body)` → `Json(body)`，用 `body.mac_address` 生成 client_id / subscribe_topic
- `settings.rs`：新增 `[xiaozhi]` / `[xiaozhi.mqtt]` / `[xiaozhi.udp]`
- `lib.rs`：按配置挂载 MQTT 入口

---

### T5 — 设备模拟器 + 端到端脚本

（待 T4 完成后细化）

- `scripts/xiaozhi_sim/device_sim.py`
- `scripts/test-mqtt-udp.sh`
