# 02 - 设计：MQTT + UDP 传输层（M2'）

> 本文是**实现依据**。所有协议细节来自两处交叉验证：
> - 固件源码 `/tmp/xz-src/main/protocols/mqtt_protocol.cc`、`ota.cc`、`protocols/protocol.h`
> - 社区服务端 `xiaozhi-esp32-server/main/xiaozhi-server/core/api/ota_handler.py`
>
> 凡标注「源码实证」的条目都有行号，可直接核对。

---

## 1. 目标与非目标

### 目标

在 haimen 里新增一个与 `ws.rs` 平级的 **MQTT + UDP 传输层**，让设备可以用 MQTT 长连接 + UDP 音频，
从而为后续「服务端主动播报 / 主动问询」打基础。

**M2' 阶段不切换设备**：`[xiaozhi] protocol` 默认仍是 `websocket`，
MQTT 传输层通过 `cargo test` + Python 模拟器验证，真机切换留到 M3'。

### 非目标

- 不改固件
- 不碰 `ws.rs`（现有唯一可用路径，必须零风险）
- 不做 notify 播报 / tts 问询（M4）
- 不做 HA 桥接的 MQTT Discovery（M1，用户已手工验证过，代码不在仓库）

---

## 2. 协议规格（实现依据）

### 2.1 OTA 响应里的 mqtt 段

**固件如何消费**（源码实证 `ota.cc:159-178`）：

```cpp
cJSON *mqtt = cJSON_GetObjectItem(root, "mqtt");
if (cJSON_IsObject(mqtt)) {                 // ← 只要是 JSON 对象，哪怕 {} 也会切 MQTT
    Settings settings("mqtt", true);
    cJSON_ArrayForEach(item, mqtt) {        // ← 所有 key-value 原样写入 settings
        if (cJSON_IsString(item))  settings.SetString(item->string, item->valuestring);
        else if (cJSON_IsNumber(item)) settings.SetInt(item->string, item->valueint);
    }
    has_mqtt_config_ = true;
}
```

**设备切 MQTT 的判定**（源码实证 `application.cc:551-557`）：

```cpp
if (ota_->HasMqttConfig())            protocol_ = make_unique<MqttProtocol>();   // MQTT 优先
else if (ota_->HasWebsocketConfig())  protocol_ = make_unique<WebsocketProtocol>();
else                                  protocol_ = make_unique<MqttProtocol>();   // 兜底也是 MQTT
```

**⚠️ 推论**：`mqtt` 段一旦存在（哪怕空对象），设备就**不再**用 WebSocket。
所以「同时下发 mqtt 和 websocket」不能实现运行时回退，只能靠 OTA 只下发其中一个。

**固件实际读取的 key**（源码实证 `mqtt_protocol.cc:80-86`）：

| key | 类型 | 默认 | 说明 |
|---|---|---|---|
| `endpoint` | string | 必填 | **必须带端口**：`host:port`。不带端口时默认 **8883**（源码 `mqtt_protocol.cc:151-152`） |
| `client_id` | string | - | MQTT client id |
| `username` | string | - | MQTT 用户名 |
| `password` | string | - | MQTT 密码 |
| `keepalive` | int | 240 | 心跳秒数 |
| `publish_topic` | string | - | 设备→服务器 的发布主题 |
| `subscribe_topic` | string | - | 服务器→设备 的下行主题（由 esp-ml307 的 Mqtt 类消费，固件应用层不直接读） |

**主题命名**（社区服务端实证 `ota_handler.py:245-279`）：

```python
mac_address_safe = device_id.replace(":", "_")          # 84_c7_bb_59_93_64
mqtt_client_id   = f"{group_id}@@@{mac}@@@{mac}"        # group_id = "GID_<设备型号>"（下划线化）
publish_topic    = "device-server"                       # ★ 所有设备共用一个主题
subscribe_topic  = f"devices/p2p/{mac_address_safe}"     # ★ 每设备一个
```

**关键推论**：
- 上行主题 `device-server` 是**共享的**，服务器必须靠 MQTT 连接的 `client_id` 反解出设备 MAC，
  再结合消息里的 `session_id` 区分会话
- 下行主题按 MAC 唯一，直接定向推送

### 2.2 MQTT 消息（JSON）

**设备 → 服务器**（发布到 `device-server`）：

```jsonc
// hello（连接建立后第一条，触发 UDP 通道协商）
{"type":"hello","version":3,"transport":"udp",
 "features":{"mcp":true,"glyph_push":true},
 "text_font":{"bundle":"noto-v1","charset":"common","size":20,"bpp":4},
 "audio_params":{"format":"opus","sample_rate":16000,"channels":1,"frame_duration":60}}

// listen
{"session_id":"xxx","type":"listen","state":"start","mode":"manual"}
// abort
{"session_id":"xxx","type":"abort","reason":"wake_word_detected"}
// mcp（JSON-RPC 响应）
{"session_id":"xxx","type":"mcp","payload":{"jsonrpc":"2.0","id":1,"result":{}}}
// goodbye
{"session_id":"xxx","type":"goodbye"}
```

**服务器 → 设备**（发布到 `devices/p2p/<mac>`）：

```jsonc
// hello 响应（★ 最关键：下发 UDP 通道与密钥）
{"type":"hello","transport":"udp","session_id":"xxx",
 "audio_params":{"format":"opus","sample_rate":24000,"channels":1,"frame_duration":60},
 "udp":{"server":"192.168.110.35","port":8888,
        "key":"<32 位 hex 字符串 = 16 字节>",
        "nonce":"<32 位 hex 字符串 = 16 字节>"}}

// goodbye（服务端主动关闭）
{"type":"goodbye","session_id":"xxx"}

// 其余类型与 WebSocket 协议一致：stt / llm / tts / mcp / system
```

**固件侧解析规则**（源码实证 `mqtt_protocol.cc:129-146`）：
```cpp
if (type == "hello")        ParseServerHello(root);
else if (type == "goodbye") { if (session_id == session_id_) CloseAudioChannel(false); }
else                        on_incoming_json_(root);   // 其它全部交给应用层（stt/tts/llm/mcp）
```

**⚠️ goodbye 的 ping-pong 防护**：设备收到 goodbye 后调 `CloseAudioChannel(false)`，
**不会回发 goodbye**（源码 `mqtt_protocol.cc:139-141`）。

### 2.3 UDP 音频包（★ 最容易写错的部分）

**包结构**（源码实证 `mqtt_protocol.cc:204-247` 发送、`305-326` 接收）：

```
偏移  长度  字段          说明
0     1    type          服务器在 nonce[0] 里设定，约定 0x01
1     1    flags         服务器在 nonce[1] 里设定，约定 0x00
2     2    payload_len   大端（htons）
4     4    ssrc          服务器在 nonce[4..8) 里设定
8     4    timestamp     大端（htonl），单位【毫秒】
12    4    sequence      大端（htonl），从 1 开始单调递增
16    N    payload       AES-128-CTR 加密后的 Opus 数据
```

**AES-CTR 的精确用法**（这是全协议最容易搞错的地方）：

```cpp
nonce = aes_nonce_;                                 // 服务器 hello 里下发的 16 字节
memcpy(nonce + 2,  &payload_len, 2);                // 覆盖 [2..4)
memcpy(nonce + 8,  &timestamp,   4);                // 覆盖 [8..12)
memcpy(nonce + 12, &sequence,    4);                // 覆盖 [12..16)
// [0..2) 和 [4..8) 保持服务器 nonce 的原值 —— 它们就是 type/flags 和 ssrc

encrypted = nonce(16 字节) || AES_CTR_encrypt(payload, counter_block = nonce);
```

**→ 核心结论：16 字节 AES-CTR 计数器块 = UDP 包头的 16 字节，随包传输。**
**→ 解密时必须用【每个包自己的前 16 字节】当计数器，不能用固定的 nonce。**
**→ 服务器生成 nonce 时应当让 `nonce[0]=0x01`、`nonce[1]=0x00`、`nonce[4..8)=ssrc`，其余字节留空（会被覆盖）。**

**参数**：
- 算法：AES-128-CTR（`psa_set_key_bits(&attributes, 128)`，源码 `mqtt_protocol.cc:485`）
- key：16 字节，以 **hex 字符串**（32 字符）经 `DecodeHexString` 解出（源码 `:459-464`）
- nonce：16 字节，同样 hex 字符串，**长度必须正好 16 字节**（源码 `:218` 校验）
- 加密范围：**仅 payload**，头部 16 字节明文（源码 `:234-243`）

**timestamp 单位**（源码实证 `protocols/protocol.h:23`）：
```cpp
uint32_t timestamp;     // Timestamp in milliseconds (used for server-side AEC)
```
→ **毫秒**。haimen 策略侧本来就是 `+60` 毫秒递增，可直接复用。

**接收侧校验**（源码实证 `mqtt_protocol.cc:326-363`）：
- `data.size() != 16 + payload_len` → 丢包并报错
- `sequence <= remote_sequence_` → 判定重复/过期，丢弃
- `sequence != remote_sequence_ + 1` → 只告警，**仍处理**
- 解密失败 → 丢包
- `local_sequence_ = 0` / `remote_sequence_ = 0` 在通道打开时重置（源码 `:499-500`）

**音频参数**：
| 方向 | 采样率 | 声道 | 帧长 |
|---|---|---|---|
| 上行（设备→服务器） | 16000 Hz | 1 | 60 ms（960 采样） |
| 下行（服务器→设备） | 24000 Hz | 1 | 60 ms（1440 采样） |

Opus 编码格式，payload 是**裸 Opus 包**（无 Ogg 封装）。

### 2.4 UDP 地址的坑

`udp.server` 必须是**局域网可达 IP**，不能是 `0.0.0.0` / `127.0.0.1`。
haimen 已有 `local-ip-address = "0.6"` 依赖，`src/web/mod.rs:156` 已在用 `local_ip_address::local_ip()`。

### 2.5 通道生命周期

```
MQTT 连上
  ↓
设备发 hello(transport:"udp")        ← 触发 OpenAudioChannel()
  ↓
服务器回 hello + udp{server,port,key,nonce}
  ↓
设备建 UDP socket，开始收发
  ↓
监听/说话/播放音频（全部走 UDP）
  ↓
任一方发 goodbye 或超时
  ↓
关闭通道；MQTT 保持连接
```

超时：`Protocol` 基类默认 120 秒无消息则标记不可用（源码 `mqtt-udp_zh.md:306-309`）；
MQTT 断线后 60 秒重连（`MQTT_RECONNECT_INTERVAL_MS 60000`，源码 `mqtt_protocol.h:22`）。

---

## 3. 架构设计

### 3.1 文件划分

```
crates/haimen-xiaozhi/src/
├── mqtt.rs          新增 ~200 行  MQTT 客户端封装（rumqttc → 事件流）
├── protocol_udp.rs  新增 ~250 行  UDP 包编解码 + AES-CTR（纯函数，易测）
├── udp_audio.rs     新增 ~300 行  UDP socket：单端口多设备、收发、序列号管理
├── mqtt_udp.rs      新增 ~500 行  传输层：会话状态机 + 对接 strategy + 帧节奏
├── ws.rs            不改          现有 WebSocket 传输层（保护）
├── ota.rs           改           读请求体 MAC；按配置决定下发 mqtt 还是 websocket
├── types.rs         改           OtaResponse 加 mqtt 段；websocket 改 Option
└── lib.rs           改           add_routes 里按配置挂载 mqtt 入口
```

### 3.2 为什么复制 ws.rs 的节奏逻辑而不是抽公共层

**决策：不重构 `ws.rs`，把帧节奏逻辑复制到 `mqtt_udp.rs`。**

理由：
1. `ws.rs` 是**当前唯一可用**的语音路径，重构风险全部落在生产路径上
2. **KI-001（语音卡顿）尚未定论**，节奏代码很可能还要改；抽公共层会导致改一处要验两条路径
3. 需要复制的部分约 200 行（`next_send_at` 累加、`wait_until_send_slot`、`snap_send_clock`、`PREBUFFER_COUNT`），复制成本低于重构风险

**等两条路径都稳定后再考虑抽 `transport.rs`。** 届时如果抽，用 strangler 模式。

### 3.3 帧节奏（与 ws.rs 保持一致）

```
① PREBUFFER_COUNT = 5：Tts::Start 后先连发 5 帧（300ms）填充设备 jitter buffer
② next_send_at 累加 60ms：next_send_at += 60ms（不是 now + 60ms，否则逐帧变慢）
③ wait_until_send_slot：绝对时刻对齐，避免累积抖动
④ snap_send_clock：断档（队列空）恢复后把时钟快进到 now，避免补偿式连发
```

⚠️ **UDP 下要额外注意**：UDP 不可靠，不能假设包必达。序列号必须严格递增，
丢包由设备侧容忍（设备只告警不丢整段）。**不要因为 send 返回成功就认为对方收到。**

### 3.4 配置

新增 `settings.toml` 段：

```toml
[xiaozhi]
# websocket（默认）| mqtt
protocol = "websocket"

[xiaozhi.mqtt]
# MQTT broker（设备与 haimen 都连这个）
endpoint = "192.168.110.34:1883"
username = "vdr_runtime"
password = "vdr_runtime"
keepalive = 240
# 上行：所有设备共享
publish_topic = "device-server"
# 下行模板：{mac} 会被替换成 84_c7_bb_59_93_64 这种形式
subscribe_topic_template = "devices/p2p/{mac}"

[xiaozhi.udp]
# UDP 音频监听端口（0 = 自动选一个空闲端口）
port = 8888
```

`AppConfig` 在 `src/settings.rs:831-860`，需要加对应 struct。

### 3.5 OTA 响应改造

`types.rs:184-192` 当前：
```rust
pub struct OtaResponse {
    pub websocket: WebsocketInfo,
    pub server_time: ServerTime,
    #[serde(skip_serializing_if = "Option::is_none")] pub firmware: Option<FirmwareInfo>,
    pub audio_params: AudioParams,
}
```

改为：
```rust
pub struct OtaResponse {
    /// 只有 protocol = websocket 时下发
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket: Option<WebsocketInfo>,
    /// 只有 protocol = mqtt 时下发（★ 存在即让设备切 MQTT）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttInfo>,
    pub server_time: ServerTime,
    #[serde(skip_serializing_if = "Option::is_none")] pub firmware: Option<FirmwareInfo>,
    pub audio_params: AudioParams,
}

#[derive(Debug, Serialize, Clone)]
pub struct MqttInfo {
    pub endpoint: String,        // host:port，必须带端口
    pub client_id: String,       // GID_xxx@@@mac@@@mac
    pub username: String,
    pub password: String,
    pub keepalive: u32,          // 240
    pub publish_topic: String,   // device-server
    pub subscribe_topic: String, // devices/p2p/<mac>
}
```

**⚠️ `handle_ota` 现在把请求体丢掉了**（`ota.rs:17` 是 `Json(_body)`）。
MQTT 的 `client_id` 需要 `body.mac_address`（`types.rs:168`），必须改成 `Json(body)` 并取用。
漏了会拿到全 `unknown` 的 client_id，导致多个设备撞 client_id 互踢。

---

## 4. 设备模拟器（第一交付物）

`scripts/xiaozhi_sim/` 下三个模块（**已完成 2/3，可直接复用**）：

| 文件 | 状态 | 说明 |
|---|---|---|
| `opus_codec.py` | ✅ 已验证 | WAV ⇄ Opus 帧（ffmpeg + 自实现 Ogg 封装/CRC） |
| `mqtt_link.py` | ✅ 已验证 | paho-mqtt 封装成同步队列；已连通 HA broker |
| `device_sim.py` | 待写 | 模拟设备：MQTT 握手 → hello → UDP 收发 → 发音频 → 收音频 |

**模拟器验收标准**：
1. 能连上 broker 并收到 haimen 的 hello 响应
2. 能解析 `udp.key` / `udp.nonce`（hex → 16 字节）
3. 能发出能被 haimen 正确解密的 UDP 音频包
4. 能收到 haimen 下发的 UDP 音频包并正确解密
5. 收包序列号连续、间隔稳定在 60ms（±20ms）

---

## 5. 测试策略

### 5.1 单元测试（不需要网络，进 CI）

`protocol_udp.rs`：
- 包头编解码 round-trip（16 字节，各字段字节序）
- AES-CTR 加解密 round-trip（固定 key/nonce/seq，断言密文可复原）
- 边界：payload_len > 65535 拒绝；包短于 16 字节拒绝；seq=0 拒绝
- **关键测试**：用与固件相同的构造方式生成包，断言前 16 字节 == 用于解密的计数器块

`udp_audio.rs`：
- 序列号管理：正常递增、乱序丢弃、跳号告警但接受
- 多设备：同一 socket 收到不同源地址的包能正确分流

### 5.2 集成测试（需要 broker，`#[ignore]`）

`crates/haimen-xiaozhi/tests/mqtt_udp_e2e.rs`：
- 起一个本地 MQTT broker（或用环境变量指定的真实 broker）
- 模拟设备走完整流程，断言 hello 响应结构正确

### 5.3 脚本测试（端到端，手动/CI 可选）

`scripts/test-mqtt-udp.sh`：
1. 检查 broker 可达
2. 启动 haimen（或复用已运行的实例）
3. 跑 `device_sim.py` 走完整流程
4. 断言：hello 往返成功、UDP 双向音频通、时序正确

---

## 6. 风险与规避

| 风险 | 影响 | 规避 |
|---|---|---|
| **AES-CTR 计数器块搞错** | 完全没声音 | 单元测试用固件同构方式生成包并断言计数器块；模拟器双向验证 |
| **timestamp 单位搞错** | 设备播放异常/AEC 失效 | 已确认毫秒（`protocol.h:23`）；模拟器断言 60ms 递增 |
| **endpoint 不带端口** | 设备去连 8883（TLS）失败 | 强制在 OTA 里输出 `host:port` |
| **client_id 撞车** | 设备互踢，连接反复断 | client_id 必须含 MAC；`handle_ota` 必须读请求体 |
| **UDP server 写成 0.0.0.0** | 设备连不上 | 用 `local_ip_address::local_ip()` |
| **`mqtt` 段下发后无法回退** | 设备不再用 WS | M2' 不真机切换；M3' 切换前先验证改配置能回退 |
| **复制节奏逻辑后两处漂移** | 后续改动要改两份 | 在 `mqtt_udp.rs` 顶部注释标注「源自 ws.rs:xxx，改动需同步」 |
| **MQTT 共享上行主题** | 多设备消息混淆 | 靠 client_id 反解 MAC + session_id 二级区分 |

---

## 6.5 协议更正（Agent A 深度源码核验，固件 2.5.3）

> 以下条目修正或补充了前文的描述，**以本节为准**。

### C1. timestamp 不是毫秒 —— 是不透明字段

```cpp
// protocols/protocol.h:23 的注释写着 ms，但实际行为不是
// 默认 sdkconfig CONFIG_USE_SERVER_AEC=n
```

- **上行**：设备 timestamp **恒为 0**
- **下行**：设备只在 `task.timestamp > 0` 时用它做服务端 AEC 参考（`audio_service.cc:361`），
  AEC 关闭时完全不参与逻辑
- **结论**：下行可以填单调递增的毫秒值，无害；但要保证**非 0**（见 C2）

### C2. ★ 上下行共用 key → 必须规避 CTR IV 复用

IV 就是包头 16 字节。上下行共用同一对 key/nonce，若某个上行包的包头与下行包的包头
完全相同，就会**keystream 重用**（可被异或还原明文）。

设备上行包头（设备保留我们 nonce 的 [0..2) 和 [4..8)，覆写其余）：
```
01 00 | payload_len | <session_id> | timestamp=0 | seq=设备seq
```
下行包头：
```
01 00 | payload_len | <session_id> | timestamp=我们填 | seq=我们seq
```

**规避方案（必须实现）**：下行 timestamp **固定填非 0**（如从 1 开始按 60 递增的毫秒），
这样上下行包头在 [8..12) 必然不同，**不可能碰撞**。

### C3. 下行包硬性约束

- `header[0]` 必须 `0x01`
- `16 + payload_len == datagram_size`（严格相等，否则设备丢包）
- `sequence` **首包 ≥ 1，严格递增**，每次 hello 设备会把 `remote_sequence_` 清零 → 我们要跟着重置
- **单包总长 ≤ 1500 字节**（组件 recv 缓冲 1500）
- 下行音频必须按 **~60ms/包实时**发送：设备解码队列只有 20 包（1.2s），满了**静默丢包**

### C4. hello 握手的硬性要求（任一不满足 = 静默 10 秒超时）

```cpp
// mqtt_protocol.cc:284-298
// 设备发 hello 后只等 10000ms，且没有任何 ACK
```

- 必须**先收到设备 hello 才能回 hello**（提前发会被清事件位，并且会重置 key/seq）
- 响应必填：`type=="hello"`、`transport=="udp"`、`udp` 是 object
- `udp.port` ∈ 1..65535
- `udp.key` / `udp.nonce` hex 解码后**正好 16 字节**
- 握手**无 ACK**，设备不回任何确认 → 我们只能靠"后续收到 UDP 包"判断成功

### C5. UDP 无握手，服务器后知设备地址

设备 `connect()` 是**本地操作，不发包**。所以：
- 服务器收到设备**第一个上行 UDP 包**之前，不知道设备地址
- 必须用 `udp.port` 指定的**同一个 socket、同一本地 IP** 回包
- 无法在设备开口前主动推音频

### C6. 下行 `frame_duration` 必须合法

`audio_params.frame_duration` ∈ {5, 10, 20, 40, 60, 80, 100, 120}。
我们固定用 **60**。

### C7. 会话识别只能靠 `nonce[4..8)`

设备保留我们 nonce 的 `[4..8)`（会话 ID）原样回传。
所以**一个 UDP 端口服务多设备时，用 `nonce[4..8)` 区分会话，不能只看源地址**
（源地址可能因 NAT/重连变化）。

### C8. 其他已知坑

| 坑 | 后果 | 对策 |
|---|---|---|
| `publish_topic` 为空 | 设备完全发不出，且不报错 | OTA 里非空校验 |
| 端口 8883 | **强制 TLS** + CA bundle 校验 | 明文必须用 1883 |
| MQTT 断线 | 只在 Idle 重连一次 | broker 侧保持连接稳定 |
| `Publish` 失败 | 污染 `error_occurred_`，后续全失败 | 不要在错误态下继续发 |
| 设备无 no-speech 自动回 Idle | 会话挂死 | 服务器必须驱动 `goodbye` |
| 设备不在 Speaking 时收到音频 | 应用层丢弃，但协议层 seq 已前进 | 必须等 `tts start` 后再发音频 |

### C9. 设备只认 `publish_topic`，且 OnMessage 忽略 topic 参数

（源码实证：`mqtt_protocol.cc:116` 的 lambda 接收 `topic` 但**从不使用**）

**推论：下行 topic 是什么都行**——设备根本不看。
但为了可读性和调试方便，仍按约定命名。

---

## 7. 交付顺序

```
T1  protocol_udp.rs + 单元测试          （纯函数，零风险，先钉死协议）
T2  mqtt.rs + udp_audio.rs              （I/O 层）
T3  mqtt_udp.rs 传输层                  （状态机 + 对接 strategy）
T4  OTA 改造 + 配置                     （protocol 开关、mqtt 段、读请求体）
T5  device_sim.py + test-mqtt-udp.sh    （端到端验证）
T6  部署验证                            （CI → 镜像 → 模拟器跑通）
```

每个 T 完成后必须 `./scripts/ci.sh` 全绿才能进下一个。
