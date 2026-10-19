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
