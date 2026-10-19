# 对比：xiaozhi-esp32-server (v0.9.7) vs 我们的 haimen MQTT+UDP 实现

调研日期：2026-10-19
对象：`/Users/kevin/Documents/works/xiaozhi-esp32-server`（xinnan-tech，Python 30.9k 行 + Java 管理端 339 文件）

---

## 一、架构差异（最重要的发现）

### 它没有 MQTT+UDP 实现

全仓库搜 `mqtt` 只有 3 个文件：
- `docs/mqtt-gateway-integration.md`（部署文档）
- Java `MqttGatewayAuthorization.java`（只做鉴权）

**它的 MQTT+UDP 靠外部 Node.js 网关翻译：**

```
设备 ──MQTT(1883)+UDP(8884)──> xiaozhi-mqtt-gateway（虾哥开源，Node.js，独立进程）
                                        │
                                        └──WebSocket──> xiaozhi-server（Python，只认 WS）
```

**我们是原生实现：**

```
设备 ──MQTT+UDP──> haimen（Rust，单进程）
                     ├── mqtt_broker.rs   自建 broker（关键：固件从不发 SUBSCRIBE）
                     ├── udp_audio.rs     单 socket 多设备复用
                     ├── protocol_udp.rs  AES-CTR + 16 字节头
                     └── mqtt_udp.rs      传输层
```

**结论：这块我们是领先的。** 他们多一个 Node.js 进程 + 一个管理 API 端口(8007)，
配置要同步 `server.mqtt_gateway` / `server.udp_gateway` / `server.mqtt_manager_api` /
`server.mqtt_signature_key` 四处。我们是单进程，OTA 直接下发。

### 它比我们强的地方：MQTT 认证

它的 `mqtt.username` = base64(`{"ip":"192.168.1.15"}`)，
`mqtt.password` = HMAC 签名（用 `MQTT_SIGNATURE_KEY`）。

我们目前是明文 `haimen`/`haimen`。局域网自用可接受，但可以补。

---

## 二、它没有的能力（我们的优势）

### 1. 完全没有「服务端主动说话」

搜遍 Python 端：
- 无 HTTP API（无 FastAPI/APIRouter）
- 无推送、无定时触发、无 notify
- 纯被动：设备连上 → 用户说话 → 服务器回应

**我们的 `notify` 主动播报 + `tts` 会话主动问询是它没有的。**

### 2. 无「主动开启对话」

同原因：需要服务端主动推送音频，它做不到。

---

## 三、它做得比我们好的地方（值得抄）

### 【高优先级】1. 三层退出机制 + 硬看门狗

`core/handle/intentHandler.py:53` + `core/handle/receiveAudioHandle.py:110` + `core/connection.py:184`

```python
# ① 精确关键词退出（可配置）
exit_commands: ["退出", "关闭"]
async def check_direct_exit(conn, text):
    for cmd in conn.cmd_exit:
        if text == cmd:              # 去标点后精确匹配
            await conn.close()

# ② LLM 意图识别 → handle_exit_intent 工具 → close_after_chat=True
# ③ 120 秒无语音 → 生成告别语 → 播完再关
close_connection_no_voice_time: 120

# ④ 硬看门狗：在①的基础上 +60 秒强制关闭
self.timeout_seconds = close_connection_no_voice_time + 60
```

**对比我们：**
- 我们是**包含匹配**（`contains`），他们说**精确匹配** → 我们误判风险更高
- 我们 15 秒收口，他们 120 秒 → **我们太激进**
- 我们静默关闭，他们**先让设备说一句告别语再关**（`end_prompt`，如"时间过得真快…"）→ 体验更好
- 我们没有硬看门狗兜底

**建议：**
1. 关键词改精确匹配（去标点后 `==`），并把词表配置化
2. `IDLE_CLOSE_LIMIT` 15s → 120s（可配置）
3. 关闭前走一遍 LLM 生成告别语再发 goodbye
4. 加一道 `idle + 60s` 的硬看门狗

### 【高优先级】2. Silero VAD（解决 KI-003）

`config.yaml:605`

```yaml
VAD:
  SileroVAD:
    threshold: 0.5                  # 人声开始
    threshold_low: 0.3              # 人声结束（双阈值迟滞）
    min_silence_duration_ms: 200    # 200ms 静音判停
```

**我们：能量 RMS 阈值 3000 + 连续静音帧。**
实测环境噪音 RMS 1600~2200、人声 4000~15000 —— 阈值太接近，必然误判。

**这是架构级差距。** 神经网络 VAD 能区分"环境噪音"和"人声尾音"，能量阈值不能。

**建议：** 接 Silero VAD（ONNX，Rust 可用 `ort` crate），
前置在 ASR 之前，替代本地能量 VAD。ASR 的 `end_window_size` 退回宽松值。

### 【中优先级】3. 唤醒后 2 秒忽略 VAD

`core/handle/receiveAudioHandle.py:31`

```python
if conn.just_woken_up:
    have_voice = False
    # 2 秒后恢复 VAD 检测
```

**这可能正是 KI-003「聆听停止过快」的一个原因**：设备刚唤醒时
自己的提示音/回声会被 VAD 当成用户说话，导致状态错乱。

### 【中优先级】4. AudioRateController（解决 KI-004）

`core/utils/audioRateController.py`（187 行）

四个设计点：
1. **绝对播放时钟**（`start_timestamp` + `play_position`），非 `now + interval` → 无累积漂移
   （我们已做 ✅ `next_send_at += 60ms`）
2. **队列空后恢复时重置 start_timestamp**，保持播放位置连续
   → 避免"暂停后突然连发"撑爆设备缓冲（我们做得不完整 ⚠️）
3. **message 和 audio 混在同一队列按序发**
   → 保证 `tts sentence_start` 与音频帧严格先后（我们可能有竞态 ⚠️）
4. **`tts_audio_send_delay` 可配**：0 = 精确帧率；>0 = 固定间隔（可略慢于 60ms）
   → 这就是 KI-004 掉帧的调参旋钮

参数与我们一致：`AUDIO_FRAME_DURATION=60`、`PRE_BUFFER_COUNT=5`。

播放完成判定：`playback_time = (PRE_BUFFER_COUNT + 2) * interval_ms`
（我们现在是 `sleep(step * 3)`，可以用这个更精确的公式）

### 【低优先级】5. LLM 工具调用的退出意图

`plugins_func/functions/handle_exit_intent.py`

把"结束对话"做成一个 LLM 工具，`say_goodbye` 作为参数由 LLM 生成。
比我们的关键词匹配智能，但依赖 LLM 配合。

**建议：** 保留关键词作为快速路径，另外给 agent 的 SOUL 加一条：
识别到告别意图时调用工具/输出标记 → 传输层收口。两者互补。

---

## 四、看着很炫但单人自用没必要

| 功能 | 它有什么 | 为什么我们不需要 |
|---|---|---|
| 管理后台 | Java+Vue，设备/用户/参数管理 | 单用户，配置文件够用 |
| 多租户 | 用户隔离、绑定设备、短信验证 | 单人 |
| 声纹识别 | `voiceprint-integration.md`，区分家庭成员 | 除非要做多人个性化 |
| 知识库/RAG | `ragflow-integration.md` | Hermes 已有记忆能力 |
| 数字人 | `all-in-one-digital-human-setup.md` | 用不上 |
| 通话 | `call_device` 插件，设备间互打 | 用不上 |
| OTA 管理 | 固件升级管理 | 我们手动刷更可控 |
| 性能压测 | `performance_tester/` | 单设备 |

---

## 五、可直接借鉴的清单（按 ROI 排序）

| # | 事项 | 收益 | 成本 |
|---|---|---|---|
| 1 | 收口超时 15s→120s + 关闭前说告别语 + 硬看门狗 | 体验立刻变好 | 半天 |
| 2 | 关键词退出改精确匹配 + 词表配置化 | 减少误判 | 1 小时 |
| 3 | Silero VAD 替代能量 VAD | 解决 KI-003 | 2-3 天 |
| 4 | 唤醒后 2 秒内忽略 VAD | 可能缓解 KI-003 | 1 小时 |
| 5 | AudioRateController 的队列连续性 + message 混排 | 缓解 KI-004 | 1 天 |
| 6 | `tts_audio_send_delay` 可配 | KI-004 调参旋钮 | 半天 |
| 7 | MQTT 连接签名认证 | 安全性 | 半天 |

---

## 六、结论

1. **MQTT+UDP 和主动播报这两块，我们领先**：他们是外部网关 + 纯被动，我们是原生单进程 + 主动能力。
2. **音频前端（VAD）和对话收口这两块，他们成熟得多**：Silero 双阈值 VAD、三层退出 + 看门狗、带告别语的收口。
3. **他们的功能生态（管理台/多租户/知识库/声纹）对单人自用是负资产**，不要把复杂度抄过来。
4. **最有价值的两个借鉴**：`Silero VAD`（解决 KI-003）和 `对话收口分层设计`（体验）。

---

# 附录：Agent 层深读结论（subagent 报告摘要）

## 它的架构：薄编排 + 厚配置

```
意图识别：默认【没有】—— Intent: function_call 的 provider 恒返回 continue_chat，
          真正路由全交给 LLM 原生 tool calling
          （core/providers/intent/function_call/function_call.py:9-22）
intent_llm：可选前置分类器，官方注释自认「增加处理时间」，默认关闭
记忆：短期=会话内列表不裁剪；长期=会话末总结成一段文本（≤1800 字）覆盖写回
知识库：RAGFlow 外挂，包装成一个普通 function tool
声纹：voiceprint-api 外挂，与 ASR 并行，结果只进 prompt
MCP：只做客户端（外部 server / 设备 / mcp-endpoint 三种），不实现 MCP server
主动能力：完全没有定时/播报调度，只有超时告别、唤醒问候、来电播报
```

## 我们落后它的三件事（值得抄）

### P0-1 长期记忆（唯一质变级功能）

```python
# 会话结束守护线程异步总结，下次连接注入 <memory>
# mem_local_short.py:135-198 + connection.py:304-325
# 隔离键只有 device_id，不按人
```
**它的坑我们不要复刻：**
- 覆盖式单字段 `summaryMemory` → 总结跑偏就永久丢历史 → 我们要 append + 版本
- 每轮同步查询阻塞主链路（`connection.py:1113-1119`）→ 我们预取/异步
- few-shot 的 `is_temporary` 没过滤，会污染记忆；`Assistant: None` 也会写进去

### P0-2 `direct_answer` 虚拟工具（省一次 LLM 往返）

```python
# connection.py:58-76
# 把「不调工具」也包装成一次工具调用
# → 把「调不调」的二分类变成「调哪个」的多分类
# → response 参数流式抠出来直接送 TTS，避免「工具轮 + 文本轮」两次往返
```
**这是最聪明的低延迟技巧，和我们 0.9s 的目标一致。**

### P1 上下文注入（减少工具调用）

```
把 时间/天气/HA 实体状态 注入 system prompt 的 <context> 段
→ 「客厅灯开着吗」「现在几点」不调工具直接答，每轮省 300ms~1s
（prompt_manager.py:249-312 + context_provider.py:15-64）
```

## 它踩过的坑（我们别复刻）

| 坑 | 位置 | 教训 |
|---|---|---|
| 长期记忆同步查询阻塞主链路 | `connection.py:1113-1119` | 连接时预取 + 本地缓存 |
| 总结覆盖式单字段 | `AgentEntity.java:76-78` | append-only + 时间戳 |
| few-shot 污染记忆 | `mem_local_short.py:150-167` | 写记忆前过滤临时消息 |
| 超时告别伪造 user 消息 | `receiveAudioHandle.py:133` | 主动播报走独立「系统发起」通道 |
| 动态工具描述改全局注册表会串台 | `plugin_executor.py:116-123` | 工具描述 per-session 拷贝 |
| 声纹健康检查同步阻塞连接初始化 | `voiceprint_provider.py:88-138` | 外部检查异步/缓存 |
| 短期上下文无 token 预算 | `dialogue.py:94-161` | 要预算 + 滚动窗口 |
| 工具全量下发无预筛 | `connection.py:1103-1107` | 按场景裁剪 |

## 结论：我们的差异化方向

> **它把主动能力做得很薄（只有超时告别/唤醒问候），我们已经领先。**
> **下一步不该去补它的 Java/Vue 中台，而是把「主动能力 + 长期记忆」接起来
> ——「它记得昨天你说过什么，并在合适的时候主动提起来」。**

---

# 附录：协议层深读结论（subagent 报告摘要）

## 它的 MQTT+UDP 真相

```
它不用 mosquitto/emqx，而是虾哥手写的极简 MQTT 3.1.1 broker
  （xiaozhi-mqtt-gateway，Node.js，app.js:5-16、mqtt-protocol.js:5-14）

"固件不订阅"不是被解决，而是【双方放弃订阅语义】：
  · 网关 handleSubscribe 只回 SUBACK、不记 topic（app.js:317-325）
  · 所有下行直接写 devices/p2p/<mac>（app.js:310/410-413）
  · 固件历史上删了 Subscribe（commit e2c5578），OnMessage 的 topic 完全不用
  （mqtt_protocol.cc:116-148）

→ 结论：任何标准 broker 都做不到，只能自建。我们的方案是这个约束下的正解。
```

## 好消息：nonce 我们和它**字节级一致**

```
01 00 00 00 | UDP路由id(4B BE) | 0 | 0
```
我们叫 `udp_session_id`，它叫 `connectionId`。**实现正确。**
唯一差一点：hello 里可以补 `"version": 3`。

## 好消息：之前的 IV 复用担心是多余的

`say_raw_frames` 每次 `ts=1` **不构成同向 AES-CTR IV 复用**，
因为 `udp_audio` 的 `next_sequence` 单调递增，计数器块仍不同。
真正的风险是**上下行共用 key 时跨方向首包可能撞**（device seq=1/ts=1 vs server ts=1/seq=1）。
→ 建议给下行 random 32-bit timestamp 基址，而不是当成已确认的 bug。

## ⚠️ 我们必须修的四个洞（优先级最高）

### P0-1 MQTT 完全没有 keepalive

```
mqtt_broker.rs:270-279  明写「只记录、不检测」
mqtt_broker.rs:173-200  publish_to 只是入队，不代表送达
```
**后果**：半开连接变僵尸，`publish_to` 返回成功但设备收不到；
`prune_sessions` 也清不掉（因为 broker 的 connected_clients 仍有它）。
**这正是之前「控制面消息已转发给设备」但设备毫无反应的那类现象的可能来源。**

修法：按**入站**判活（收到任何包刷新），超过 1.5× keepalive 就断开清理。
**不要**抄它的出站刷新（`mqtt-protocol.js:433`）——那样僵尸永远清不掉。

### P0-2 MQTT 完全没有鉴权（安全问题）

```
mqtt_broker.rs:680-692  解析了 username/password 但【不校验】
```
**后果**：
- 局域网内**任何人**可以发 `haimen/ctl/<client_id>` 注入 `notify` / `__say_audio`
  → 让你的音箱说出任意内容
- 还能用真设备的 clientId 连上来，把真设备顶掉

修法：设备级 HMAC（它用 `username=base64({"ip":...})` + `password=HMAC`），
并对控制主题做授权（只接受 server 自身或已认证来源）。

### P1-3 全局 run_loop 会被阻塞

```
mqtt_udp.rs:818-822  handle_udp 里 await on_audio_frame(...)
mqtt_udp.rs:504-509  say_raw_frames 直接在 handle_mqtt 里同步 sleep
```
**后果**：一次 10 秒播报会让整个传输层停 10 秒 ——
期间收不到设备的 listen/abort/上行音频，多设备是全体停摆。
单设备也会让「播报期间用户插话」失效。

修法：把 say / ASR 调用 / 写队列从一个全局 loop 拆出去（spawn 独立 task）。

### P1-4 策略层是全局单例（多设备架构债）

```
src/xiaozhi_asr_llm_tts.rs:531-551  streaming_state / vad_notify / end_of_conversation 都是 self 级
mqtt_udp.rs:825-848  vad_completion 一触发会把所有 Recording 会话一起推进
```
**后果**：两个设备同时说话会串 ASR、串收口。
单设备暂时无感，但接第二台设备前必须按 session_id 隔离。

## 它做得比我们好的（补充）

| 项 | 它 | 我们 |
|---|---|---|
| 每连接会话隔离 | ✅ | ❌ 全局单例 |
| `tts start` 后等状态切换再发首帧 | ✅ 20-100ms | ❓ |
| `tts stop` 前等 `(prebuffer+2)*60ms` 尾帧 | ✅ | ❓ |
| 收口 | 120s 无语音 + LLM 告别 + 180s 硬超时 | 15s 直接 goodbye |
| 退出词 | **精确匹配** | contains（易误判） |
| MQTT 鉴权 | 有（但有漏洞） | **无** |
| keepalive | 有（但判活方式错） | **无** |

## 它做得不如我们的（补充）

- 三进程/四配置面（网关 + Python + Java 管理台 + 4 个地址配置）
- broker 只 QoS0、8KB 上限、无 UNSUBACK、无背压
- 在线状态 API 把空闲设备判离线（`app.js:621-623`）
- Python 服务端没有 notify/主动 TTS
- Python 线程重、队列无界
- Python 从不发 goodbye（靠网关 bridge 关闭时补发）
- 我们 broker 支持 QoS1/SUBACK/UNSUBACK、codec 更严格、下行时间戳单调、单测覆盖更全

## 最终 P0 清单（跨三份报告 + 今天的事故）

| # | 事项 | 风险 |
|---|---|---|
| 1 | **Hermes LLM 调用加超时** | 今天整机哑掉就是它 |
| 2 | **MQTT 鉴权** | 局域网可注入任意播报 / 顶掉设备 |
| 3 | **MQTT keepalive** | 僵尸连接，消息假成功 |
| 4 | **把 say/ASR 从全局 loop 拆出去** | 播报期间整条链路冻结 |
| 5 | 收口 120s + 告别语、退出词改精确匹配 | 体验 |
| 6 | 策略层按 session_id 隔离 | 接第二台设备前必做 |
