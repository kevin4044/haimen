# xiaozhi-esp32-server v0.9.7 协议/传输层深读，与 haimen（Rust）逐项对比

> 调研对象：`/Users/kevin/Documents/works/xiaozhi-esp32-server`（tag `v0.9.7`，关键文件与 tag 一致；`receiveAudioHandle.py` 在 tag 后仅多 4 行，本文行号按 v0.9.7 校准）
> 对照实现：`/Users/kevin/works/haimen`（commit `41b2ef6`）
> 固件参考：`/Users/kevin/Documents/works/xiaozhi-esp32`（当前 main；`notify` 在 2.5.2 实测可用，2.2.6 不支持）
> 网关参考：`xinnan-tech/xiaozhi-mqtt-gateway` main（2026-10-19 抓取；它不在 v0.9.7 仓库内，是独立 Node 服务）

代码位置约定：

- `X/` = `/Users/kevin/Documents/works/xiaozhi-esp32-server/`
- `F/` = `/Users/kevin/Documents/works/xiaozhi-esp32/`
- `H/` = `/Users/kevin/works/haimen/`
- `G/` = `https://github.com/xinnan-tech/xiaozhi-mqtt-gateway/blob/main/`（下文写 `G/app.js:410`）

---

## 0. 结论先行

1. **v0.9.7 的 Python 服务端没有 MQTT/UDP 实现**。它把 MQTT+UDP 外包给独立的 Node 网关；网关**自己手写了一个极简 MQTT 3.1.1 broker**，不是 mosquitto/emqx，也**不维护订阅表**，而是对着设备那条 TCP 连接直接写 PUBLISH。所谓“解决固件不订阅”，本质是“双方都放弃订阅语义”。
2. **我们的 UDP nonce 字节结构是对的**，和网关同构；但 `[4..8)` 不是 hello JSON 里那个 UUID `session_id`，而是 32 位 UDP 路由 id（固件叫 ssrc，网关叫 connectionId，我们叫 `udp_session_id`）。设备只保留 `[0..2)` 和 `[4..8)`，其余每个包覆盖。
3. **主动能力我们领先服务端栈**：`notify` 是固件 2.5.2 才有的单向播报，xiaozhi-server/manager 没有把它产品化；`tts` 会话我们能在“先唤醒过”的前提下主动问询。但两者都没有绕过“设备必须先 hello 拿 UDP 密钥”的硬限制；网关的 `self.remote_wakeup` 依赖非官方固件。
4. **对比暴露出我们自己四个必须修的洞**：MQTT 完全没有 keepalive；MQTT 完全没有鉴权且控制主题可被任意客户端注入；策略层是全局单例，多设备会串音/串收口；全局 `run_loop` 会被长播报/慢设备阻塞。
5. **最值得抄的不是功能，而是四条工程习惯**：每连接隔离会话状态、设备级 MQTT 签名、按入站流量判活、播完最后一帧再发 `tts stop`。

---

## 1. 架构图

### 1.1 它：三层，MQTT/UDP 不在 Python 里

```
  ESP32 ──MQTT/TCP:1883──┐              ┌── UDP:8884 (AES-128-CTR) ──┐
        控制面 JSON       │              │  Opus 音频                  │
                          ▼              ▼                            │
                 ┌─────────────────────────────────┐                  │
                 │ xiaozhi-mqtt-gateway (Node.js)  │──────────────────┘
                 │  ├ 手写 MQTT 3.1.1 broker       │
                 │  ├ 单 dgram socket + 连接表     │
                 │  └ WebSocket client             │
                 └───────────────┬─────────────────┘
                                 │ ws://.../xiaozhi/v1/?from=mqtt_gateway
                                 ▼
                 ┌─────────────────────────────────┐
                 │ xiaozhi-server (Python asyncio) │
                 │  每个 WS 一个 ConnectionHandler │
                 │  VAD → ASR → LLM → TTS          │
                 └─────────────────────────────────┘

  OTA(8003) ──下发 mqtt{endpoint, client_id, username, password, topics}──> ESP32
  manager-api ──HTTP /api/commands, /api/call, /api/devices/status──> gateway:8007
```

- Python 只认 WebSocket；`?from=mqtt_gateway` 是它和网关的私有约定（`X/main/xiaozhi-server/core/connection.py:228`）。
- 网关是独立的 Node 进程，自己监听 1883/TCP、8884/UDP、8007/HTTP；MQTT 和 UDP 都不经过 Python。
- 网关和 Python 之间还有一套**私有 16 字节帧头**（`G/app.js:147-153`，`X/main/xiaozhi-server/core/handle/sendAudioHandle.py:108-112`），它和固件 UDP 头不是同一个东西。

### 1.2 我们：单进程，协议头端到端

```
  ESP32 ──MQTT/TCP:1883──┐
        控制面 JSON       │              ┌── UDP:8884 (AES-128-CTR) ──┐
                          ▼              ▼                            │
                 ┌─────────────────────────────────┐                  │
                 │ haimen (Rust, 单进程)           │──────────────────┘
                 │  mqtt_broker.rs   手写 broker   │
                 │  mqtt_udp.rs      会话/节奏     │
                 │  udp_audio.rs     单 socket     │
                 │  protocol_udp.rs  AES-CTR 编解码│
                 │  strategy / ASR / LLM / TTS     │
                 └─────────────────────────────────┘
```

我们的 16 字节头从服务端一直用到设备，中间没有第二次封装；这是比它少一层转换、少一类 bug 的地方。

### 1.3 MQTT+UDP 握手时序（两边合并后的真实流程）

```
Device                         Gateway                              Python
  │ CONNECT clientId=GID@@@mac@@@mac │                                  │
  │ PUBLISH device-server hello v3   │                                  │
  │─────────────────────────────────>│                                  │
  │                                  │ WS hello v2 transport=websocket  │
  │                                  │─────────────────────────────────>│ handleHelloMessage
  │                                  │<─────────────────────────────────│ hello{session_id=uuid,audio_params}
  │<─────────────────────────────────│ hello v3 transport=udp           │
  │   {session_id, udp{server,port,key,nonce}}                          │
  │                                  │                                  │
  │ UDP packet: type|flags|len|ssrc|timestamp|sequence|AES(payload)     │
  │─────────────────────────────────>│ 按 ssrc=[4..8) 找连接             │
  │                                  │ 解密后加 16B 私有头 → WS binary  │
  │                                  │─────────────────────────────────>│ 按 timestamp 做 AEC/ASR
```

关键点：**key/nonce 是网关生成的，不是 Python 生成的**。Python 的 `handleHelloMessage` 只回 `session_id` 和 `audio_params`（`X/main/xiaozhi-server/core/handle/helloHandle.py:42-65`），没有 `udp` 字段。

---

## 2. Q1：它怎么解决“固件不订阅”？

### 2.1 先纠正一个前提

`main/xiaozhi-server/core/providers/` 和 `core/handle/` 下**没有任何 MQTT 相关实现**：

- `core/providers/` 只有 ASR/LLM/TTS/VAD/Memory/Intent/VLLM 等，没有 MQTT provider。
- `core/handle/` 的消息类型只有 `hello/abort/listen/iot/mcp/server/ping`（`X/main/xiaozhi-server/core/handle/textMessageType.py:1-12`），没有 mqtt handler。
- 全 Python 服务端里 MQTT 只出现在 4 处：OTA 下发配置、`?from=mqtt_gateway` 标记、二进制 16B 头解析、下行 16B 头封装（`X/main/xiaozhi-server/core/api/ota_handler.py:235-280`、`connection.py:193/228/375-415`、`sendAudioHandle.py:84-117/270-275`）。

所以，“它怎么解决”这个问题的准确答案是：**Python 层不解决，它把问题外包给了 Node 网关；网关的解法是“根本不按订阅路由”。**

### 2.2 网关是自建 broker，不是 mosquitto/emqx

`G/app.js:5-16` 的 require 只有 `net`、`crypto`、`dgram`、`ws`、`express`，没有 `mqtt`/`aedes`/`mosca`；`G/mqtt-protocol.js` 自己实现 `CONNECT/CONNACK/PUBLISH/SUBSCRIBE/SUBACK/PINGREQ/PINGRESP/DISCONNECT`（`G/mqtt-protocol.js:5-14`）。它监听裸 TCP：

```js
this.mqttServer = net.createServer((socket) => {
  const connectionId = this.generateNewConnectionId();
  new MQTTConnection(socket, connectionId, this);
});
this.mqttServer.listen(this.mqttPort, ...)   // G/app.js:738-746
```

**为什么不用 mosquitto/emqx？** 因为标准 MQTT broker 只给“已订阅的连接”投递；固件不发 SUBSCRIBE，标准 broker 就不会把下行给它。想用现成 broker，要么改 broker，要么替设备伪造订阅，反而更绕。它选择自建，直接对连接写。

### 2.3 “主动 push”到底怎么发生

1. 设备 CONNECT 时，网关从 `client_id` 拆出 MAC，算出固定下行主题：`replyTo = devices/p2p/<mac>`（`G/app.js:287-310`）。
2. 网关的 `handleSubscribe` **只回 SUBACK，不记录任何 topic**（`G/app.js:314-325`；`G/mqtt-protocol.js:317-326` 也只解析第一个 topic）。
3. 所有下行（hello/stt/tts/mcp/goodbye）都走 `sendMqttMessage`，它不查订阅表，直接对 `replyTo` 发 QoS0 PUBLISH（`G/app.js:410-413`；`G/mqtt-protocol.js:386-435`）。
4. Python 的 JSON 通过 WS 到网关，网关 `sendMqttMessage` 发给设备（`G/app.js:155-165`）。

所以链路是：

```
Python 业务代码 → WebSocket → 网关 → 对设备 TCP 直接写 PUBLISH → 设备
```

不是“broker 路由”，是“网关知道自己拿着哪条连接”。

### 2.4 固件侧的证据：它真的不订阅，而且不看 topic

- 当前固件 `MqttProtocol::StartMqttClient` 只读 `publish_topic`，没有 `subscribe_topic`，也没有 `mqtt_->Subscribe(...)`（`F/main/protocols/mqtt_protocol.cc:85-98`）。
- 历史上确实订阅过，2025-01 的提交 `e2c5578` 删掉了 `mqtt_->Subscribe(subscribe_topic_, 2)`（`git show e2c5578 -- main/protocols/mqtt_protocol.cc`）。
- `OnMessage` 的回调签名是 `(const std::string& topic, const std::string& payload)`，但 `topic` 参数在函数体里**一次都没用**，直接按 `type` 解析 payload（`F/main/protocols/mqtt_protocol.cc:116-148`）。

结论：topic 在 MQTT+UDP 路径里只是“兼容 OTA 的标签”。网关写 `devices/p2p/<mac>`，设备即使收到别的 topic 也照样处理；订阅语义被双方共同放弃。**这正是我们能自己写 broker 直接 push 的根据。**

### 2.5 代价

- 设备离线时下行直接丢，没有 QoS1/retained/离线队列（`G/mqtt-protocol.js:386-435` 只写 socket）。
- 没有 per-topic 路由，多设备只能按 `client_id`/MAC 直连。
- 我们的 broker 同样“不按订阅路由”，但我们额外实现了 SUBACK/UNSUBACK/QoS1 PUBACK（`H/crates/haimen-xiaozhi/src/mqtt_broker.rs:421-436`、`708-736`），兼容性更好；这是我们可以比它做得更标准的地方。

---

## 3. Q2：hello 怎么生成 UDP key/nonce/session_id？和我们一致吗？

### 3.1 谁生成什么

| 字段 | 它 | 我们 |
|---|---|---|
| `session_id`（协议会话号，UUID 文本） | Python `uuid.uuid4()`，`X/main/xiaozhi-server/core/connection.py:92`；网关原样带回设备 `G/app.js:485-497` | `protocol_session_id = Uuid::new_v4()`，`H/crates/haimen-xiaozhi/src/mqtt_udp.rs:644` |
| UDP `key` | 网关 `crypto.randomBytes(16)`，每次 hello 重新生成，`G/app.js:437-441` | `random_bytes()`（UUIDv4 字节）+ 每次 hello 重新生成，`H/.../mqtt_udp.rs:629-636`、`1455-1457` |
| UDP `nonce` | 网关 `generateUdpHeader(0,0,0)`，`G/app.js:427-435`、`441` | `nonce_template(udp_session_id)`，`H/.../mqtt_udp.rs:1447-1453` |
| UDP 路由 id（`[4..8)`） | 网关 `connectionId`，32 位随机且连接内唯一，`G/app.js:728-735` | `udp_session_id`，32 位随机、非 0、在 `udp_index` 内唯一，`H/.../mqtt_udp.rs:1459-1472` |
| `encryption` | 固定 `aes-128-ctr` | 固定 AES-128-CTR（`H/.../protocol_udp.rs:245-257`） |

### 3.2 字节级对照

固件 UDP 头：

```
偏移 0      1      2..4        4..8                  8..12        12..16
     type   flags  payload_len ssrc                  timestamp    sequence
     0x01   0x00   大端        大端（路由 id）        大端         大端
     明文   明文   明文        明文                   明文         明文
     ↑      ↑      ↑           ↑                      ↑            ↑
     设备保留       设备每包覆盖 设备保留               设备每包覆盖  设备每包覆盖
```

固件发送时用 hello 下发的 nonce 当模板，只覆盖 `[2..4)`、`[8..12)`、`[12..16)`：

```cpp
nonce = aes_nonce_;
memcpy(nonce.data() + 2,  &payload_len, ...);
memcpy(nonce.data() + 8,  &timestamp, ...);
memcpy(nonce.data() + 12, &sequence, ...);
// 用整个 16 字节 nonce 当 AES-CTR 计数器块
```
（`F/main/protocols/mqtt_protocol.cc:208-240`；接收侧同样用入站包头当计数器，`302-366`）

xiaozhi 网关的 nonce 实际是：

```
01 00 00 00 | <connectionId 4B BE> | 00 00 00 00 | 00 00 00 00
```

我们的 nonce 实际是：

```
01 00 00 00 | <udp_session_id 4B BE> | 00 00 00 00 | 00 00 00 00
```

**所以字节结构完全一致**。你说的模板 `01 00 00 00 | session_id(4B BE) | 0 | 0` 也一致，只是把 `[4..8)` 叫成了 `session_id`；它应该是 **UDP 路由 id**，不是 hello JSON 里的 UUID `session_id`。我们的代码里这两个是分开的：`protocol_session_id` 是 UUID，`udp_session_id` 才是 nonce 里的 4 字节（`H/.../mqtt_udp.rs:148-152`）。

### 3.3 一致性和差异

**一致：**

- 每次 hello 新 key；nonce 只提供 type/flags/ssrc 模板；每个包覆盖长度/时间戳/序列号。
- 设备保留 `[0..2)` 和 `[4..8)`，我们和网关都满足。
- 上下行共用同一把 key，AES-CTR 计数器块就是包头。

**差异：**

1. **网关的 `connectionId` 是“每条 MQTT TCP 连接一个”，同一连接重复 hello 时 nonce 的 `[4..8)` 不变，只换 key**（`G/app.js:437-441` 用的是同一个 `this.connectionId`）。我们是“每次 hello 换一个 `udp_session_id`，并注销旧的”（`H/.../mqtt_udp.rs:669-698`）。两种都能工作；我们的隔离更彻底，旧 key 的包会被 UDP 层直接丢。
2. **网关 hello 带 `version: 3`**（`G/app.js:485-497`），我们的 hello 只有 `type/transport/session_id/audio_params/udp`，没有 `version`（`H/.../mqtt_udp.rs:646-657`）。当前固件 `ParseServerHello` 不校验 version（`F/main/protocols/mqtt_protocol.cc:413-417`），所以能工作；但补上 `"version": 3` 更稳、更接近官方。
3. **key 熵**：网关 16 字节纯随机（128 bit）；我们 UUIDv4 字节（122 bit 随机）。都足够，不必改。
4. **会话号语义**：网关的 `udp.session_id` 就是 Python 的 UUID；它的 `connectionId` 只是路由。我们的 `protocol_session_id`/`udp_session_id` 分离，和它同构。

### 3.4 一个安全提醒

两边的 key/nonce 都是在 MQTT 明文里下发的（OTA 也是明文 1883）。AES-CTR 只保护 UDP 音频，不认证、不防重放；谁拿到 MQTT 连接/日志，谁就能解密音频。设备级 MQTT 鉴权（下一节）是必要的，但仍不是强认证（静态口令可被嗅探重放）。

---

## 4. Q3：会话生命周期——goodbye 谁发、什么时候发？有没有空闲收口？设备侧超时呢？

### 4.1 谁发 goodbye

| 发送方 | 触发 | 证据 |
|---|---|---|
| 设备 | 设备主动结束（按键退出、重启、升级、切 AEC 等）→ `CloseAudioChannel(true)`，先销毁 UDP，再发 `{"session_id","type":"goodbye"}` | `F/main/protocols/mqtt_protocol.cc:249-267`；`application.cc:831` 等 |
| 网关 | ① 到 Python 的 WS bridge 关闭（正常结束/异常）② 收到非 hello 消息但当前没有 bridge ③ 通话对端离开 ④ pending 通话超时 | `G/app.js:456-460`、`519-523`、`348`、`783-785` |
| Python | **不发**。它只关 WS；goodbye 是网关看到 WS 关闭后补发的 | `X/main/xiaozhi-server/core/connection.py:1543-1660`；全文无 `"type":"goodbye"` 下行 |
| 我们 | 空闲 15s、识别到告别语且回复播完、设备主动 goodbye 时清理 | `H/crates/haimen-xiaozhi/src/mqtt_udp.rs:914-935`、`1172-1180`、`564-572` |

固件收到服务端 goodbye 时，只有 `session_id` 与当前一致才执行 `CloseAudioChannel(false)`，并且**不回 goodbye**，避免 ping-pong（`F/main/protocols/mqtt_protocol.cc:131-143`）。这一点我们做对了：`close_idle_conversation` 明确带 `protocol_session_id`（`H/.../mqtt_udp.rs:919-934`）。

### 4.2 空闲收口

**xiaozhi：两层，但都不是协议层 goodbye**

1. 软收口：`close_connection_no_voice_time` 默认 120s。每帧音频检查“多久没语音”，超过就构造 `end_prompt`（默认“时间过得真快…”）走一遍 LLM，生成告别语，`close_after_chat=True`；TTS 最后一帧发完后 `conn.close()`（`X/main/xiaozhi-server/core/handle/receiveAudioHandle.py:106-130`、`config.yaml:61`、`226`、`sendAudioHandle.py:51-54`）。
2. 硬看门狗：`timeout_seconds = close_connection_no_voice_time + 60 = 180s`，每 10s 检查一次，超时直接 `close()`（`X/main/xiaozhi-server/core/connection.py:184-186`、`1720-1733`）。
3. Python 关闭 WS 后，网关 bridge 关闭，网关再给设备发 goodbye（`G/app.js:456-460`）。所以设备最终回 Idle，但**goodbye 不是 Python 发的**。

**我们：一层显式 goodbye + 一层录音上限**

- 一轮对话结束（`tts stop` 后进入 `Ready`），`last_speech_at` 超过 15s 就发 goodbye 并注销 UDP（`H/.../mqtt_udp.rs:33-38`、`872-935`）。`last_speech_at` 以“真正下发音频”为基准，不是设备持续推流的时间（`H/.../mqtt_udp.rs:161-169`、`1130-1133`）。
- 录音超过 30s 没收到 `listen stop`，强制生成回复（`RECORDING_LIMIT`，`H/.../mqtt_udp.rs:30-31`、`850-870`）。
- 告别语识别：回复播完立即发 goodbye（`end_of_conversation`，`H/.../mqtt_udp.rs:1172-1180`）。

**评价：**

- 15s 比 120s 激进得多。好处是设备更快回 Idle，停止推麦克风、停止烧 ASR；坏处是用户“说完一句想一下再补一句”可能被收掉。建议**做成配置**（例如 15/30/60s），并保留“告别语立即收口”的快路径。
- xiaozhi 的“先让 LLM 说一句告别再关”体验更好；我们目前是直接 goodbye（或策略层 goodbye_frames，但 MQTT 路径没接）。可以把 `goodbye_frames` 接进 MQTT 收口，先播完再 goodbye。
- xiaozhi 的 120s 会让设备在 Listening 状态持续推流 2 分钟，隐私/成本都不好；这一点我们更优。

### 4.3 设备侧超时

- 固件 `Protocol::IsTimeout()`：距离最后一次收到 MQTT 消息或 UDP 音频超过 **120s** 就算超时（`F/main/protocols/protocol.cc:108-117`）。
- 但**没有定时器主动关通道**；只在 `IsAudioChannelOpened()` 被调用时检查（`F/main/protocols/mqtt_protocol.cc:566-568`）。所以超时后设备只是“认为通道不可用”，下一次要开麦时会重新 hello，并不会自己回 Idle。
- 因此**服务端主动 goodbye 仍然是最可靠的收口手段**；如果服务端不发、MQTT 又断了，设备可能长时间停在 Listening。我们的 15s 收口在正常链路上比它可靠；但前提是 MQTT 还活着（见 Q6）。

### 4.4 一个我们自己的 bug

`say_raw_frames`（主动播报）不修改 `session.state`，只会在结束时重置 `last_speech_at`（`H/.../mqtt_udp.rs:428-493`）。如果一次主动播报超过 15s，`prune_sessions` 会看到 `state == Ready` 且 `last_speech_at` 很旧，从而在播报中途发 goodbye、注销 UDP（`H/.../mqtt_udp.rs:896-911`）。修法：主动播报期间置 `Playing` 或每帧更新 `last_speech_at`，并与正常回放共用一把 per-session 发送锁。

---

## 5. Q4：主动播报/主动唤醒——它到底能不能让设备先说话？

### 5.1 能力矩阵

| 能力 | xiaozhi-server | 网关 | 固件 | 我们 |
|---|---|---|---|---|
| `notify`（HTTP URL 拉音频，单向播报） | ❌ 没有实现，`core/api` 只有 OTA/vision；全文无 `audio_url` | ❌ `/api/commands` 只接受 `type:"mcp"`，其他类型直接报“指令类型无效”（`G/app.js:1022-1048`） | ✅ 2.5.2 支持；仅 Idle；Ogg Opus；无 ACK（`F/main/application.cc:599-631`、`1182-1226`；`notify_player.cc:184-252`） | ✅ 通过 `haimen/ctl/<client_id>` 透传，2.5.2 实测可用（`H/scripts/xiaozhi_sim/send_notify.py`、`H/specs/known-issues.md`） |
| `tts` 主动问询 | ❌ 只在用户说话后响应 | ❌ 没有主动 TTS | ⚠️ `tts start` 可把 Idle 切到 Speaking，但没有 UDP 通道就没有音频；`tts stop` 在 auto 模式会进 Listening（`F/main/application.cc:642-651`） | ⚠️ `say_raw_frames` 走 tts 会话，**要求设备先唤醒过、UDP 会话还在**（`H/.../mqtt_udp.rs:420-427`） |
| 远程唤醒 | ❌ | ⚠️ `/api/call/request` 发 MCP `tools/call` `self.remote_wakeup`（`G/app.js:1085-1148`） | ❌ stock 固件没有这个工具（`F/main/mcp_server.cc` 全量工具里 0 处 `remote_wakeup`） | ❌ 没有 |
| 唤醒词问候 | ✅ 有缓存 TTS（`X/.../helloHandle.py:68-126`） | — | 设备发 `listen detect`（`F/main/protocols/protocol.cc:61-77`） | ⚠️ `listen detect` 被忽略（`H/.../mqtt_udp.rs:723-725`），`wake_greeting` 没接到 MQTT |

### 5.2 它有没有绕过“设备必须先唤醒”？

**单向播报：绕过了。** `notify` 不需要 hello、不需要 UDP、不需要唤醒词；只要设备处于 Idle 且 notify player 空闲，服务端发 `{"type":"notify","audio_url":"http://.../x.ogg"}`，设备自己 HTTP GET 播放，播完回 Idle。这是固件层的能力，不是 xiaozhi-server 的。

**双向对话：没有绕过。** 设备要上行音频、要下行 TTS 音频，必须先发 hello 拿到 UDP key/nonce/session，而 hello 是设备在 `OpenAudioChannel()` 时主动发的（唤醒/按键后）。服务端没有“命令设备发 hello”的标准消息。网关的 `self.remote_wakeup` 是给**定制固件**用的 MCP 工具；stock 固件没有，所以对普通设备无效。

**我们的现状：**

- `notify` 走控制面透传，方向对，也验证过。但它目前**不是类型化 API**：没有 HTTP 接口、没有 Idle 状态门控、没有音频格式校验、没有 ACK。设备不处于 Idle 时会静默丢弃（`F/main/application.cc:1183-1184`），调用方只看得到“MQTT 转发成功”。
- `say_raw_frames` 的 tts 会话能在“先唤醒过”的设备上主动问询，并且 `tts stop` 后 auto 模式会进 Listening，形成“问完等回答”。这是它能做到、xiaozhi-server 做不到的。但它有“播报期间不置 Playing，15s 空闲可能中途收口”的 bug（见 4.4）；另外下行 timestamp 固定从 1 开始，跨方向首包有理论碰撞（不是同向 IV 复用——sequence 单调递增，顺带澄清）。

### 5.3 如果要产品化 notify，固件契约是这些

来自固件代码，不是猜测：

- `audio_url` 必须是 `http://` 或 `https://` 的**单声道 Ogg Opus**；固件用 `OggDemuxer` 解，不是裸 Opus/MP3/WAV（`F/main/notify/notify_player.cc:184-252`）。
- HTTP 必须 2xx；`Accept: audio/ogg, application/ogg`；`Accept-Encoding: identity`；读超时 5s（`notify_player.cc:186-193`）。
- 每包 5–120ms；拒绝 stereo、2.5ms、坏 Ogg 结构、不完整流（`notify_player.cc` 的 `OnPacket`/`Finish`）。
- 只在 `kDeviceStateIdle` 且 `notify_player_.IsBusy()==false` 时接受；播放前会放一声 popup 音，播完回 Idle（`F/main/application.cc:1182-1226`）。
- **没有 ACK**；服务端无法从协议层知道成功。可行的“隐式 ACK”是看自己的 HTTP 访问日志有没有设备的 GET（`H/specs/known-issues.md` 的实测就是这么做的）。

### 5.4 如果要“主动开启对话”，唯一干净的路

给固件加一个 MCP 工具（例如 `self.start_listening`），内部调用 `Application::StartListening` / `ContinueOpenAudioChannel`，服务端用标准 MCP `tools/call` 触发。网关的 `self.remote_wakeup` 就是这个思路，但 stock 固件没有实现（`F/main/mcp_server.cc` 无此工具）。不改固件，就只能：

1. 先用 `notify` 播一句“请说‘小智小智’唤醒我”（体验差）；
2. 或等用户自己唤醒后，再用 tts 会话问询。

---

## 6. Q5：多设备/并发模型——asyncio 怎么组织？有没有池、限流、背压？

### 6.1 xiaozhi Python

- 单进程单事件循环；`websockets.serve` 每来一个连接创建一个 `ConnectionHandler`（`X/main/xiaozhi-server/core/websocket_server.py:76-131`）。
- 每个连接：
  - 一个 `ThreadPoolExecutor(max_workers=5)`，用于 LLM/工具等阻塞调用（`X/.../connection.py:124`、`1353`）；
  - TTS provider 每个连接一套，内部再起 2 个线程：文本优先级线程 + 音频播放线程（`X/.../providers/tts/base.py:316-324`）；
  - 1 个上报线程（`X/.../connection.py:740`）；
  - 合计每设备约 8 个线程。
- 没有连接池、没有全局连接上限、没有 per-IP 限流、没有信号量。`websockets.serve` 也没设 `max_queue`/`max_size`（`websocket_server.py:76-82`）。
- 背压：
  - `tts_text_queue`/`tts_audio_queue` 都是无界 `queue.Queue()`（`base.py:43-44`），TTS 产得比播得快就涨；
  - 真正节流靠 `AudioRateController` 按 60ms 绝对播放时钟发包（`X/.../utils/audioRateController.py:1-187`）；
  - 打断时 `clear_queues()` 清空（`connection.py:1662-1690`）。
- 唯一的“限流”是业务层每日输出字数 `max_output_size`（`X/.../utils/output_counter.py:1-60`），不是网络限流。

### 6.2 网关 Node

- 单线程事件循环；`MQTTConnection` 每个 TCP 一个对象，`MQTTServer.connections`/`clientIdMap` 两张表（`G/app.js:217-220`、`713-720`）。
- 只有一个 UDP socket，所有设备共用；按包头 `[4..8)` 的 `connectionId` 查表（`G/app.js:750-752`、`854-878`）。
- 全局 1s 定时器遍历所有连接做 keepalive、通话超时清理（`G/app.js:773-795`）。
- 没有连接池/上限/限流；`max_mqtt_payload_size` 默认 8192（`G/mqtt-protocol.js:56-72`）。
- 背压：`socket.write(packet)` 的返回值不检查，没有 `drain`（`G/mqtt-protocol.js:433`）；设备慢/死时靠内核缓冲，队列会涨。
- 相同 `clientId` 重连会把旧连接踢掉（`G/app.js:808-820`）。

### 6.3 我们 Rust

- Tokio 多任务：1 个 broker accept loop、1 个 UDP recv loop、1 个全局 `run_loop`、每个回放一个 playback task（`H/.../mqtt_udp.rs:240-278`、`314-382`、`937-970`）。
- 每设备状态在 `Tables { sessions, udp_index }`，一把 `tokio::Mutex`（`H/.../mqtt_udp.rs:195-209`）。
- 队列：
  - broker 上行 `mpsc(256)`（`H/.../mqtt_broker.rs:24-25`）；
  - broker 下行写队列 `mpsc(32)`（`H/.../mqtt_broker.rs:24-25`、`257-260`）；
  - UDP 上行 `mpsc(64)`，满了直接丢（`H/.../udp_audio.rs:81`、`275-277`）。
- 没有连接上限、没有 per-device 限流；`online_device_count` 只是诊断（`H/.../mqtt_udp.rs:290-293`）。

**评价：**

- 资源效率我们明显更好：Tokio 任务 vs 每设备 8 线程。
- 隔离性它明显更好：Python 是 per-connection 状态；我们的 `ResponseStrategy` 是**全局单例**，`AsrLlmTtsStrategy` 的 `streaming_state`、`vad_notify`、`no_speech_notify`、`end_of_conversation`、`silence_closed` 都是全局的（`H/src/xiaozhi_asr_llm_tts.rs:531-551`）。多设备同时录音会共用同一条 ASR 管道；`vad_completion` 一触发，`finish_recording_by_vad` 会把**所有** Recording 会话一起推进（`H/.../mqtt_udp.rs:825-848`）；一个设备说“再见”置位的 `end_of_conversation` 可能被另一个设备的 `finish_ready` 消费（`H/.../mqtt_udp.rs:1172-1180` + `H/src/xiaozhi_asr_llm_tts.rs:1578-1579`）。代码自己也注明这是单设备假设（`mqtt_udp.rs:832-833`）。
- 我们的全局 `run_loop` 会 `await handle_udp`，而 `handle_udp` 会 `await strategy.on_audio_frame`，后者会 `pcm_tx.send(...).await`（`H/.../mqtt_udp.rs:818-822`；`H/src/xiaozhi_asr_llm_tts.rs:1774-1777`）。ASR 管道一堵，所有设备的 MQTT/UDP 都跟着停。这是比“每连接任务”更严重的头阻塞。
- 主动播报 `say_raw_frames` 也是在全局 `run_loop` 里同步 `await` 的（`H/.../mqtt_udp.rs:504-509/428-493`），里面按 60ms 逐帧 `sleep`。**一次 10 秒的主动播报，会让所有设备 10 秒收不到 MQTT/UDP。** 这比“单个设备慢”更严重，必须在做多设备前拆出去。
- 两边都没有连接数上限/接入限流；如果要对公网，必须补。

---

## 7. Q6：错误处理与重连——它怎么判死？我们的 `publish_to` 为什么不可信？

### 7.1 它的 keepalive

- MQTT CONNECT 里读 `keepAlive`，设置 `keepAliveInterval = keepAlive * 1000 * 1.5`（`G/mqtt-protocol.js:249`）。
- 全局 1s 定时器对每个连接 `checkKeepAlive()`：`now - lastActivity > keepAliveInterval` 就 `close()`（`G/app.js:360-374`、`773-795`）。
- 收包会刷新 `lastActivity`（`G/mqtt-protocol.js:38`），发 PINGRESP/PUBLISH/SUBACK 也会刷新（`:433`、`:453`、`:468`）。

**它有一个和我们的 bug 同源的缺陷：** `sendPublish` 在 `:433` 刷新 `lastActivity`，所以**下行 JSON（stt、tts `sentence_start`、goodbye 等）会刷新保活**；如果设备 TCP 已经死了，但服务端还在持续发这些控制消息，keepalive 会被推迟，直到 `socket.write` 报错或内核缓冲塞满。**音频走 UDP，不刷新 MQTT 保活**，所以这个缺陷没有“每 60ms 一帧”那么严重，但长回复/多句 TTS 仍会掩盖死连接。它的 `close()` 也不保证发 goodbye（bridge 不在了就只关 socket）。

### 7.2 它的在线状态 API 是错的

`/api/devices/status` 返回 `connection.isAlive()`（`G/app.js:1056-1079`），而 `isAlive()` 要求 `this.bridge && this.bridge.isAlive()`（`G/app.js:621-623`）。也就是说：**设备 MQTT 连着但没在对话（没有 bridge）时，会被判成离线**。manager-api 的在线列就是用它（`X/main/manager-api/.../DeviceServiceImpl.java:161-180`）。这是一个明确的“不如我们”的点。

### 7.3 它的重连

- 设备 MQTT 断线后 60s 重连（`F/main/protocols/mqtt_protocol.h:22`、`mqtt_protocol.cc:104-106`）。
- 同 `clientId` 新连接进来，网关把旧连接踢掉（`G/app.js:808-820`）。
- 新 hello 会关闭旧 bridge；旧 bridge 的 close 回调会给设备发旧 `session_id` 的 goodbye（`G/app.js:437-460`）。如果设备已经进入新会话，固件会因为 `session_id` 不匹配而忽略这条 goodbye（`F/main/protocols/mqtt_protocol.cc:131-143`），不会误杀新会话。
- 这个“旧会话 goodbye + 设备按 session_id 过滤”的配合是值得学的；我们的 repeated hello 只是注销旧 UDP，没有给旧会话发 goodbye（`H/.../mqtt_udp.rs:669-698`）。同一设备其实不需要，但多连接/多实例时要小心。

### 7.4 我们的问题（你怀疑的那条是真的）

`publish_to` 的语义是“把帧放进写队列”，不是“已经写到 socket”：

```rust
match tx.send(frame).await {
    Ok(()) => Ok(true),   // 只是入队成功
    ...
}
```
（`H/crates/haimen-xiaozhi/src/mqtt_broker.rs:173-200`；写队列 `WRITE_QUEUE=32`，`:24-25`）

而 broker **完全没有 keepalive 检测**，代码里明确写着“keepalive 已记录但未做空闲超时”：

```rust
// 超过 1.5 倍无包应断开，M2' 只记录、不检测。
let keepalive_secs = connect.keepalive_secs;
```
（`H/.../mqtt_broker.rs:270-279`）

所以：

- 设备拔网/半开连接时，`read_packet` 可能长时间不返回；`write_loop` 的 `write_all` 可能阻塞；`tx.send` 仍可能成功（队列没满），`publish_to` 返回 `true`。
- `prune_sessions` 依赖 `broker.connected_clients()`（`H/.../mqtt_udp.rs:872-893`），而 registry 只在读/写任务退出时摘除（`H/.../mqtt_broker.rs:306`、`333-335`、`373-381`）。半开连接不退出，会话/UDP key 就一直是“僵尸”。
- 更糟的是：如果某个设备的写队列满了，`publish_to` 会 `await` 在 `tx.send` 上；而 `handle_hello`、`close_idle_conversation` 都在全局 `run_loop` 里调用 `publish_body`（`H/.../mqtt_udp.rs:699`、`931`、`1334-1365`）。**一个坏设备可以把整个 MQTT/UDP 主循环卡死，所有设备一起断。**
- 同样，`say_raw_frames` 也在 `handle_mqtt` 里被 `await`（`H/.../mqtt_udp.rs:504-509`），整段播报期间全局 loop 不处理任何其他消息。修法：主动播报 spawn 成独立 task，并用 per-session 锁/代次防止与正常回放并发。

### 7.5 推荐修法（按重要性）

1. `ConnEntry` 增加 `last_rx: Instant` 和 `keepalive: Duration`；**只在收到入站报文时**刷新（PINGREQ/PUBLISH/CONNECT），不要被下行 PUBLISH 刷新（避免复刻网关 `:433` 的 bug）。
2. `conn_loop` 用 `tokio::time::timeout(keepalive * 1.5, read_packet(...))`，或单独 1s tick 检查；超时立即 `cancel`/关闭并摘 registry。
3. `publish_to` 增加超时（例如 1s）并区分“入队成功/写失败/离线”；非关键控制消息可以 `try_send` + 丢旧帧，关键消息 fail-fast。注意音频走 UDP，不经过这个写队列；别把“音频丢帧”策略混进来。不要让全局 loop `await` 一个慢设备的写队列。
4. `connected_clients()` 改成返回“registry + last_rx 未过期”的集合，或者在 broker 内维护在线标志；`prune_sessions` 才能可靠清 UDP key。
5. 可选：给 `TcpStream` 开 `SO_KEEPALIVE`（用 `socket2`），作为 MQTT keepalive 的补充；但 MQTT 层判活仍是主手段。

---

## 8. 逐项对比表

> “更好”列是**针对我们这种单机/单租户、设备是 stock 小智固件**的场景；换场景结论会变。

| # | 维度 | 它怎么做 | 我们怎么做 | 谁更好 | 关键依据 |
|---|---|---|---|---|---|
| 1 | 进程/部署 | Python + Node 网关（+ Java 管理端），4 个配置点 | 单 Rust 进程，OTA 一处配置 | 我们（单机） | `X/docs/mqtt-gateway-integration.md:120-140`；`H/.../ota.rs:147-157` |
| 2 | MQTT broker | Node 手写极简 broker，QoS0，8KB，无 UNSUBACK | Rust 手写，QoS1/SUBACK/UNSUBACK/256KB | 我们 | `G/mqtt-protocol.js:5-14/56-72`；`H/.../mqtt_broker.rs:421-436` |
| 3 | 不订阅 push | 不建订阅表，直接写 `devices/p2p/<mac>` | 不建订阅表，按 client_id 写连接 | 平手（我们更显式） | `G/app.js:310/317-325/410-413`；`H/.../mqtt_broker.rs:3-5/173-200` |
| 4 | MQTT 鉴权 | 设备级 HMAC 密码，但 2 段 clientId 可绕过；日志打印密码 | **无鉴权**，OTA 下发的是静态配置值（测试为 `haimen/haimen`） | 它（但仍不够强） | `G/utils/mqtt_config_v2.js:20-33`；`G/app.js:287-315`；`H/.../mqtt_broker.rs:680-692`；`H/.../ota.rs:147-157` |
| 5 | MQTT keepalive | 1.5×keepAlive，1s 检查；**下行 JSON 会刷新 lastActivity** | **完全没有** | 它（有机制但不完整） | `G/mqtt-protocol.js:249/433`；`G/app.js:360-374`；`H/.../mqtt_broker.rs:270-279` |
| 6 | 写背压/失败 | `socket.write` 不检查返回值，无 drain | 有界队列，但全局 await 可被坏设备卡死 | 它略好（非阻塞） | `G/mqtt-protocol.js:433`；`H/.../mqtt_broker.rs:177-200/359-371`；`H/.../mqtt_udp.rs:1334-1365` |
| 7 | 重连/旧连接 | 同 clientId 踢旧；旧 bridge 关闭发旧 session goodbye；设备按 session_id 过滤 | 同 clientId 踢旧，conn_id 防误删；重复 hello 换 key | 平手（它文档化更好） | `G/app.js:808-820/437-460`；`H/.../mqtt_broker.rs:283-306/373-381`；`H/.../mqtt_udp.rs:669-698` |
| 8 | hello 合并 | 网关生成 key/nonce，把 Python session_id/audio_params 合并后发 v3 | 单进程直接生成 hello | 平手 | `G/app.js:437-497`；`H/.../mqtt_udp.rs:579-703` |
| 9 | nonce/密钥 | key 16B 随机；nonce `[4..8)=connectionId` | key UUIDv4 字节；nonce `[4..8)=udp_session_id` | 平手（字节级一致） | `G/app.js:427-441`；`H/.../mqtt_udp.rs:1447-1472` |
| 10 | UDP 复用/分流 | 单 dgram socket，按 `[4..8)` 查表 | 单 `Arc<UdpSocket>`，按 ssrc 查表 | 平手 | `G/app.js:750-752/854-878`；`H/.../udp_audio.rs:58-64/239-278` |
| 11 | UDP 校验 | 不限制单包大小；时间戳用 Python 墙钟 | 1500 字节上限、严格长度/hex、timestamp≠0 | 我们 | `G/app.js:415-424`；`H/.../udp_audio.rs:16-18/145-150`；`H/.../protocol_udp.rs:101-146` |
| 12 | 上行重放/乱序 | 只拒绝 `sequence < remoteSequence` | **完全不检查** | 它略好（固件是严格 `<=`） | `G/app.js:585-590`；`F/main/protocols/mqtt_protocol.cc:333-340`；`H/.../udp_audio.rs:255-264` |
| 13 | 下行节奏 | 5 包预缓冲 + 60ms；时间戳用墙钟 ms | 5 包预缓冲 + 60ms；时间戳单调 +60 | 我们（时间戳更稳） | `X/.../sendAudioHandle.py:16-18/238-274`；`H/.../mqtt_udp.rs:1042-1105/1144-1154` |
| 14 | 16B 头 | Python↔网关一套私有头 + 网关↔设备一套固件头 | 固件头端到端 | 我们（少一层） | `X/.../sendAudioHandle.py:108-112`；`G/app.js:147-153`；`H/.../protocol_udp.rs:33-41` |
| 15 | goodbye | Python 不发；网关在 bridge 关闭/无 bridge 时发 | 服务端主动发（空闲/告别），设备 goodbye 也处理 | 我们 | `X/.../connection.py:1543-1660`；`G/app.js:456-460/519-523`；`H/.../mqtt_udp.rs:914-935` |
| 16 | 空闲收口 | 120s 无语音 → LLM 告别 → 关 WS；180s 硬超时 | 15s 无新语音（以最后一次下发音频计）→ goodbye；30s 录音上限 | 各有取舍：它 UX 好，我们隐私/成本好 | `X/.../receiveAudioHandle.py:106-130`；`X/.../connection.py:184-186`；`H/.../mqtt_udp.rs:30-38/872-935` |
| 17 | 设备侧超时 | 120s 无入站算超时，但只在开通道时检查 | 依赖服务端 goodbye；无设备超时兜底 | 平手 | `F/main/protocols/protocol.cc:108-117`；`F/main/protocols/mqtt_protocol.cc:566-568` |
| 18 | notify 单向播报 | 服务端栈没有；固件 2.5.2 支持 | 控制面透传可用；未产品化 | 我们（产品化前也是我们更接近） | `G/app.js:1022-1048`；`F/main/application.cc:599-631/1182-1226`；`H/.../mqtt_udp.rs:384-395/496-533` |
| 19 | 主动问询/唤醒 | 网关 `self.remote_wakeup` 需定制固件；stock 无 | tts 会话可在“先唤醒过”后问询；不能从 Idle 开通道 | 我们（stock）/它（定制固件） | `G/app.js:1085-1148`；`F/main/mcp_server.cc`（无 remote_wakeup）；`H/.../mqtt_udp.rs:420-427` |
| 20 | 多设备隔离 | 每连接一套 handler/线程/状态 | 传输层按 session 分流，但策略/ASR 是全局单例 | 它 | `X/.../connection.py:121-131`；`H/src/xiaozhi_asr_llm_tts.rs:531-551`；`H/.../mqtt_udp.rs:825-848` |
| 21 | 资源效率 | 每设备约 8 线程 | Tokio 任务 | 我们 | `X/.../connection.py:124`；`X/.../tts/base.py:316-324`；`H/.../mqtt_udp.rs:240-278` |
| 22 | 连接数/限流 | 无上限、无限流 | 无上限、无限流 | 都没有 | `X/.../websocket_server.py:76-82`；`G/app.js:713-720`；`H/.../mqtt_udp.rs:290-293` |
| 23 | 测试/可观测 | 网关无测试；有连接数/通话时长日志 | 协议/broker/传输都有单测；日志偏 warn，无指标 | 我们（测试）/它（运维日志） | `H/.../protocol_udp.rs:259+`、`mqtt_broker.rs:800+`、`udp_audio.rs:330+`；`G/app.js:455-458/789-794` |

---

## 9. 值得借鉴的具体做法（带文件行号）

### 9.1 设备级 MQTT 签名（P0）

它把 `client_id`、`username`、`password` 一起下发：

- OTA 生成 `client_id = GID_<model>@@@<mac>@@@<mac>`，`username = base64({"ip":...})`，`password = Base64(HMAC-SHA256(client_id + "|" + username, signature_key))`（`X/main/xiaozhi-server/core/api/ota_handler.py:246-279`）。
- 网关用同一个 key 重算并比较（`G/utils/mqtt_config_v2.js:20-33`），还用真实 socket IP 覆盖 username 里的 IP（`:63-68`）。

**值得学：** 每设备不同口令、OTA 与 broker 共享密钥、密码不写死。**不要学：**

- 2 段 `clientId` 直接跳过密码校验（`G/app.js:299-309`）；
- 把 password 打进 debug 日志（`G/app.js:281-288`）；
- 静态 HMAC 可被嗅探重放，不是挑战应答。

我们目前的 OTA 下发的是静态配置的 username/password（测试用例里是 `haimen/haimen`，默认空，`H/crates/haimen-xiaozhi/src/ota.rs:147-157`），broker 解析了 username/password 但完全不校验（`H/.../mqtt_broker.rs:680-692`）。这是最该补的一项。

### 9.2 按“入站流量”判活，而不是按“有没有在发”（P0）

它的骨架是对的：CONNECT 读 keepAlive，存 1.5 倍，1s 定时检查（`G/mqtt-protocol.js:249`、`G/app.js:360-374/773-795`）。**但 `sendPublish` 也刷新 `lastActivity`（`G/mqtt-protocol.js:433`），这是 bug。**

我们直接抄骨架，修掉这个 bug：

- `ConnEntry { last_rx, keepalive }`；只在 `read_packet` 成功时更新 `last_rx`；
- `timeout = keepalive * 1.5`；`tokio::time::timeout` 或 1s tick；
- 超时关连接 + 摘 registry；上层 `prune_sessions` 自然清 UDP key。

落点：`H/crates/haimen-xiaozhi/src/mqtt_broker.rs:270-279`（现在只记录不检测）、`:359-381`（写循环/摘除）。

### 9.3 每连接隔离会话状态（P1，多设备前提）

Python 的做法很朴素但正确：每个 WS 一个 `ConnectionHandler`，所有可变状态（`client_abort`、`client_is_speaking`、`dialogue`、`asr_audio`、`sentence_id`、`close_after_chat`）都是实例字段（`X/main/xiaozhi-server/core/connection.py:80-201`）；`websocket_server` 每次 new 一个（`:121-131`）。

我们的 `MqttUdpTransport` 传输层是 per-session 的，但策略层不是：`AsrLlmTtsStrategy` 的 `streaming_state`/`vad_notify`/`no_speech_notify`/`end_of_conversation`/`silence_closed` 都是单例字段（`H/src/xiaozhi_asr_llm_tts.rs:531-551`）。多设备会串音、串收口。

**最省事的修法：** 传输层为每个 `session_id` 建一个 `Arc<AsrLlmTtsStrategy>`（共享 `agent`/`asr_config`/`tts_config`，但会话状态独立）；或者把上述字段改成 `HashMap<String, ...>`，所有 hook 都带 `session_id`。后者改动大但更省内存。

### 9.4 播完最后一帧再发 `tts stop`（P1，立刻改善尾音）

xiaozhi 在 `send_tts_message("stop")` 前先 `_wait_for_audio_completion`：等队列空，再等 `(PRE_BUFFER_COUNT + 2) * interval_ms`（默认 7×60=420ms）让设备 jitter buffer 播完（`X/.../sendAudioHandle.py:58-80`、`293-310`）。

我们的 `run_playback` 在 `drive_playback` 返回后**立刻**发 `tts stop`（`H/.../mqtt_udp.rs:972-979`），没有等待。设备可能还在播最后几帧，就被切到 Listening/Idle，尾音被砍。`say_raw_frames` 只等了 `step*3=180ms`（`:476-478`），也不够。

**修法：** 正常回放和主动播报都在最后一帧后 sleep `(prebuffer_count + 2) * frame_duration_ms`（默认 420ms），再发 `tts stop`；或者像 xiaozhi 一样等一个“队列空 + 播放位置”事件。

**开头也要等：** 固件收到 `tts start` 后是 `Schedule` 异步切到 `kDeviceStateSpeaking`（`F/main/application.cc:637-641`），而 `OnIncomingAudio` 只在 `Speaking` 时把包放进解码队列（`F/main/application.cc:567-571`）。我们 `drive_playback` / `say_raw_frames` 都是 `tts start` 后立刻发首帧（`H/.../mqtt_udp.rs:1024-1050`、`454-466`），如果首帧到得比状态切换早，会被静默丢掉（协议层 `remote_sequence_` 已经前进，不重传）。**建议 `tts start` 后 sleep 20–100ms 再发第一帧。**

### 9.5 三层退出 + 硬看门狗（P1，UX 取舍）

xiaozhi：

1. 精确退出词 `exit_commands: ["退出","关闭"]`，去标点后 `==`（`X/.../intentHandler.py:53-64`、`config.yaml:84-86`）；
2. LLM 意图 → `handle_exit_intent` 工具 → `close_after_chat`（`X/.../plugins_func/functions/handle_exit_intent.py:33-45`）；
3. 120s 无语音 → LLM 生成告别语 → 播完关；180s 硬超时（`X/.../receiveAudioHandle.py:106-130`、`connection.py:184-186/1720-1733`）。

我们：

- 告别语是 `contains` 匹配（`H/src/xiaozhi_asr_llm_tts.rs:1556-1572`），"再见面" 会命中 "再见"；建议改精确匹配 + 词表配置，或交给 LLM 意图；
- 15s 收口可以保留，但建议配置化，并加“硬看门狗”（例如 `idle + 60s` 必关），防止回放任务卡住导致永不收口；
- 可选：收口前用 `goodbye_frames` 播一句再见，再发 goodbye（ws.rs 已有这个模式，`H/crates/haimen-xiaozhi/src/ws.rs:190-200`）。

### 9.6 唤醒回复缓存（P2）

xiaozhi 对唤醒词回复做预生成 + 缓存：按音色缓存 wav，首次用默认短音频，后台用 TTS 重新生成，全局锁防止并发（`X/.../helloHandle.py:68-126`）。这样唤醒后不用等 LLM/TTS，直接播。

我们的 `ResponseStrategy::wake_greeting` 已经有钩子（`H/crates/haimen-xiaozhi/src/strategy.rs:176-183`），`ws.rs` 也用了（`:311`），但 MQTT 传输层把 `listen detect` 直接忽略（`H/.../mqtt_udp.rs:723-725`）。如果设备在 MQTT 模式下也会发 `listen detect`（固件确实会，`F/main/protocols/protocol.cc:61-77`），可以接上：`detect` → `wake_greeting(session_id)` → 走 tts 会话播报。注意 `detect` 可能早于 hello，需等 hello/Ready 再播。

### 9.7 网关的 `self.remote_wakeup` 机制（P2，需改固件）

`/api/call/request` 给被叫发：

```json
{"type":"mcp","payload":{"jsonrpc":"2.0","id":9999,"method":"tools/call",
 "params":{"name":"self.remote_wakeup","arguments":{"reason":"[device_call]...","action":"listen"}}}}
```
（`G/app.js:1130-1148`）

这是“服务端让设备开麦”的唯一干净机制，但 stock 固件没有 `self.remote_wakeup`（`F/main/mcp_server.cc` 全量工具 0 处）。如果我们要做真正的主动对话，需要给固件加一个 `self.start_listening` 工具，内部调用 `Application::StartListening`/`ContinueOpenAudioChannel`，再用 MCP `tools/call` 触发。不要指望 `tts start` 能从 Idle 直接开 UDP。

### 9.8 状态/时长日志（P2）

网关会记录通话开始/结束和时长（`G/app.js:455-458`）、连接数/活跃数（`:789-794`）。我们目前大量用 `warn!` 打关键路径（为了绕过日志过滤），但没有结构化指标。建议加：

- `mqtt_connections_total`、`mqtt_keepalive_timeouts_total`；
- `udp_sessions_active`、`udp_rx_dropped_total`、`udp_seq_gap_total`；
- `playback_tail_wait_ms`、`idle_close_total`、`goodbye_sent_total`；
- per-device `last_rx_age`、`publish_queue_depth`。

---

## 10. 它做得不如我们的地方

1. **不是原生实现，部署/配置复杂。** Python + Node 两个必选进程（管理端还要 Java），要同步 `server.mqtt_gateway`、`server.mqtt_signature_key`、`server.udp_gateway`、`server.mqtt_manager_api` 四个配置（`X/docs/mqtt-gateway-integration.md:120-140`）。我们单进程、OTA 一处配置。
2. **MQTT broker 太“够用主义”。** 只支持 QoS0，QoS>0 直接关连接（`G/app.js:380-385`）；最大包 8KB（`G/mqtt-protocol.js:56-72`）；没有 UNSUBACK；`socket.write` 不处理背压（`:433`）；没有测试。我们的 broker 支持 QoS1/SUBACK/UNSUBACK、256KB、TCP_NODELAY、同 clientId 踢旧、conn_id 防误删（`H/.../mqtt_broker.rs:421-436/283-306`）。
3. **在线状态 API 是错的。** 空闲设备（MQTT 连着、没有 bridge）被判离线（`G/app.js:621-623/1056-1079`）。我们的 `online_device_count` 至少是按会话表统计（虽然修 keepalive 前也会有僵尸）。
4. **服务端栈没有 notify/主动 TTS。** 固件 2.5.2 已经支持 `notify`，但网关 `/api/commands` 只接受 MCP（`G/app.js:1022-1048`），manager-api 也没有 `audio_url` 调用；白白浪费了固件能力。我们的控制面透传至少能用。
5. **Python 每设备约 8 线程、队列无界。** 5 线程池 + 2 个 TTS 线程 + 1 个上报线程（`X/.../connection.py:124`、`tts/base.py:316-324`）；`queue.Queue()` 无上限（`base.py:43-44`）；没有连接数上限/限流。设备一多，线程和内存都不划算。
6. **空闲 120s + Python 不发 goodbye。** 正常链路靠网关把 WS close 翻译成 goodbye；如果网关没部署/挂了，纯 WS 设备没有协议层 goodbye。120s 也让设备多推 2 分钟麦克风。我们的 15s 主动 goodbye 更干净（但需 keepalive 兜底）。
7. **下行时间戳是墙钟。** Python 每个包 `int(time.time()*1000) % 2**32`（`X/.../sendAudioHandle.py:270-274`），会抖、可能回退；我们用 `1 + 60*n` 单调递增（`H/.../mqtt_udp.rs:1144-1154`）。
8. **多一层私有 16B 头。** Python↔网关要封一套 `type/len/seq/timestamp/opus_len`（`X/.../sendAudioHandle.py:108-112`），网关再解一次（`G/app.js:147-153`），然后才生成固件 UDP 头。我们端到端只有固件头，少一层协议、少一类错位。
9. **测试覆盖。** 网关没有测试；Python 的 MQTT 相关只有桥接，没有协议单测。我们的 `protocol_udp`/`mqtt_broker`/`udp_audio`/`mqtt_udp` 都有单测（含已知 AES-CTR 向量、QoS1、预缓冲节奏、goodbye 注销 UDP 等），这是明显优势。

**它比我们强、但不属于“协议层”的一项：** 服务端 AEC。它用 `features.aec` + 16B 头里的 timestamp 缓存下行 PCM，再做互相关/谱减（`X/.../connection.py:417-530`）。我们 MQTT 路径没有等价实现；如果设备回声严重，这是它更成熟的地方。

---

## 11. 我们可以直接抄的方案（按优先级）

> “抄”分三种：抄协议/流程、抄代码结构、抄运维策略。下面每条都给出落点，不要求照搬它的技术栈。

### P0（正确性/安全，建议先做）

| # | 抄什么 | 它的依据 | 我们的落点 | 验收标准 |
|---|---|---|---|---|
| P0-1 | **MQTT keepalive + 入站判活** | `G/mqtt-protocol.js:249`、`G/app.js:360-374`（骨架），但**不要抄 `:433` 的出站刷新** | `H/.../mqtt_broker.rs:270-279` 增加 `last_rx`/`timeout`；`conn_loop` 超时关闭；`publish_to` 加超时/离线语义 | 拔网 1.5×keepAlive 内 registry 摘除、`prune_sessions` 清 UDP key；`publish_to` 不再对死连接返回 true |
| P0-2 | **设备级 MQTT 鉴权 + 控制主题授权** | `X/.../ota_handler.py:260-279`、`G/utils/mqtt_config_v2.js:20-33`；但只允许 3 段 clientId，拒绝 2 段绕过 | OTA 生成 `password = HMAC(server_secret, client_id + username)`；broker 常量时间比较；`haimen/ctl/` 只允许带共享密钥/白名单的 client 发布 | 错误密码 CONNACK 拒绝；伪造 clientId 不能顶掉真设备；未授权客户端不能发 `haimen/ctl/...` |
| P0-3 | **不要让全局 `run_loop` await 慢设备/长播报** | 它的 Node `socket.write` 是非阻塞的（虽然没处理背压） | `publish_body` 加 1s timeout；非关键控制消息 `try_send` + 丢旧帧；`say_raw_frames` spawn 成独立 task；或把每个 session 的 I/O 拆到独立 task | 一个设备写队列塞满 / 一次 10s 播报时，其他设备仍能收发；`run_loop` 延迟 P99 不受影响 |

### P1（多设备/稳定性，建议这一轮做）

| # | 抄什么 | 它的依据 | 我们的落点 | 验收标准 |
|---|---|---|---|---|
| P1-1 | **每会话独立的策略状态** | Python 每连接一个 `ConnectionHandler`（`X/.../connection.py:80-201`） | 把 `streaming_state`/`vad_notify`/`no_speech_notify`/`end_of_conversation` 改成按 `session_id` 索引；或每 session 一个策略实例 | 两台设备同时说话，ASR 文本/回复不串；一台说“再见”不影响另一台 |
| P1-2 | **`tts start` 后等 20–100ms 再发首帧；播完最后一帧再发 `tts stop`** | `X/.../sendAudioHandle.py:58-80`（420ms）；固件状态切换是异步的（`F/main/application.cc:637-641/567-571`） | `H/.../mqtt_udp.rs:1024-1050` 加首帧延迟；`:972-979` 加尾部等待；`say_raw_frames:454-478` 两处都补 | 首帧不被丢；长句尾音不被砍；设备 `tts stop` 后回 Listening/Idle 的时机正确 |
| P1-3 | **上行序列号/重放窗口** | 固件严格 `sequence <= last` 丢弃（`F/main/protocols/mqtt_protocol.cc:333-340`）；网关只做了 `<`（`G/app.js:585-590`） | `H/.../udp_audio.rs:50-56` 的 `SessionEntry` 增加 `last_remote_seq`；`255-264` 收包时拒绝 `<=`，记录 gap | 重放同一 UDP 包不会再次进 ASR；乱序包按策略丢弃/缓存 |
| P1-4 | **notify 产品化 + Idle 门控** | 固件契约（`F/main/application.cc:1182-1226`、`notify_player.cc:184-252`）；我们的透传（`H/.../mqtt_udp.rs:384-395/496-533`） | 增加 `/notify` 音频服务（Ogg Opus 单声道）+ HTTP API；发送前检查 `session.state == Ready` 且无 UDP 活动；记录设备 GET 作为隐式 ACK | 播报只在 Idle 生效；非 Idle 时返回明确错误；HTTP 日志能看到设备请求 |
| P1-5 | **收口策略配置化 + 硬看门狗** | `X/.../receiveAudioHandle.py:106-130` + `connection.py:184-186` | `IDLE_CLOSE_LIMIT` 改成配置（15/30/60）；增加 `idle + 60s` 必关；可选先播 `goodbye_frames` 再 goodbye | 配置生效；回放任务卡死时硬超时仍能收口 |

### P2（体验/运维，按需）

| # | 抄什么 | 它的依据 | 我们的落点 |
|---|---|---|---|
| P2-1 | hello 补 `"version": 3` | `G/app.js:485-497` | `H/.../mqtt_udp.rs:646-657` |
| P2-2 | `listen detect` → 唤醒问候 | `X/.../helloHandle.py:68-126`、`F/main/protocols/protocol.cc:61-77` | `H/.../mqtt_udp.rs:723-725` 不再忽略；等 Ready 后 `wake_greeting` |
| P2-3 | 精确退出词 + LLM 意图 | `X/.../intentHandler.py:53-64`、`config.yaml:84-86` | `H/src/xiaozhi_asr_llm_tts.rs:1556-1572` 改精确匹配/词表 |
| P2-4 | 状态/指标 | `G/app.js:455-458/789-794` | 增加连接数、keepalive 超时、UDP 丢包、seq gap、idle close 计数 |
| P2-5 | 控制面 HTTP API 的签名 | `X/main/manager-api/.../MqttGatewayAuthorization.java:47-80`（每日 token） | 如果暴露 `/notify`/`/say` HTTP 接口，用同样的 HMAC/时间戳 |
| P2-6 | 下行 timestamp 随机基址 + 跨方向审计 | 网关用墙钟 ms，天然避开首包碰撞；我们固定从 1 开始（`H/.../mqtt_udp.rs:1144-1154`） | 每次播放/每个会话随机一个 32 位起始 timestamp，单调递增；`udp_audio` 记录 `last_tx_ts`，异常回退打日志 |
| P2-7 | MCP 工具缓存（可选） | `G/app.js:634-700` | 如果我们要支持设备 MCP，连接时缓存 `initialize`/`tools/list`，避免每次透传 |

---

## 12. 附：几个容易踩错的细节

1. **`notify` 不是 `tts`。** 它不走 UDP、不需要 hello、不需要唤醒；但只支持 Ogg Opus，且只在 Idle 生效。把 WAV/MP3 URL 发过去，设备会静默失败。
2. **`tts stop` 不是“停止播放”开关，而是状态机事件。** 设备只在当前 `Speaking` 时才处理；auto 模式进 `Listening`，manual 模式进 `Idle`（`F/main/application.cc:642-651`）。所以“主动问询”依赖 `tts stop` 把设备带到 Listening，但前提是它真的进过 Speaking 并且有 UDP 音频。
3. **`tts start` 和首帧之间要留 20–100ms。** 设备收到 `tts start` 后通过 `Schedule` 异步切 `Speaking`（`F/main/application.cc:637-641`），而音频只在 `Speaking` 时入解码队列（`:567-571`）；发太早首帧会被丢，且 UDP 序列号已经前进，不会重传。
4. **`goodbye` 的 `session_id` 必须匹配。** 设备只在 `session_id_ == 当前` 时关通道（`F/main/protocols/mqtt_protocol.cc:131-143`）；不带或带错，设备会继续停在 Listening 推流。我们的 `close_idle_conversation` 已经带对（`H/.../mqtt_udp.rs:930`）。
5. **`server` 地址必须是设备可达的地址。** 容器里的 `local_ip()` 可能是容器 IP；网关用 `PUBLIC_IP`，我们用 `advertise_ip` 显式覆盖（`H/.../mqtt_udp.rs:70-78/617-627`）。这一点两边都踩过。
6. **`publish_to` 的 `true` 只是“入队成功”。** 任何把它当“已送达”的调用方都会在设备半开连接时误判；要么改语义，要么在调用方加 `is_online`/超时。
7. **AES-CTR 没有认证。** 翻转密文字节不会报错，只会解出坏 Opus；不要用“解密成功”当“包合法”。如果要防篡改，得在应用层加 MAC（固件不改做不到）。
8. **上下行共用 key。** 下行 timestamp 从 1 开始并不天然安全；如果上行首包的 timestamp/len/seq 恰好相同，计数器块会撞。最稳的是给下行一个随机 32 位起始 timestamp，再单调递增；这比“从 1 开始”更安全。
9. **`notify` 没有 ACK。** 不要用“MQTT publish 成功”判断设备播了；看 HTTP GET 日志，或者接受“尽力而为”。

---

## 13. 一句话总结

**它的协议层是“Python 只做业务、Node 网关手写 broker 直接 push、设备不订阅也不看 topic”；我们的协议层是同一套字节协议的单进程原生实现，方向正确且多处更强。真正要补的不是再抄一个功能，而是把我们的 MQTT 判活/鉴权、UDP 重放与时间戳、多设备策略隔离和全局循环背压这四件事做扎实——这四件不做，主动播报和多设备都会在真实网络里翻车。**
