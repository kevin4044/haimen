# 已知问题清单（Known Issues）

> 记录真机测试中发现、但尚未修复的问题。
> 每条都附有日志证据，便于后续定位。
> 最后更新：2026-10-18

---

## KI-001 语音播放卡顿（断断续续 / 跳字）

**状态**：待排查（用户明确说过「卡顿先不管」，暂缓）
**严重度**：中高（直接影响体验，但不影响功能正确性）

### 现象

用户反馈：语音回复「卡卡的」「断断续续」。

### 已有的线索

**① 帧节奏相关的上游修复已经在代码里**

`crates/haimen-xiaozhi/src/ws.rs` 已包含：
- `next_send_at` 累加 60ms 的发送时钟（不是每次重置为当前时刻）
- `wait_until_send_slot` 绝对时刻对齐
- `record_frame_arrival_gap` 诊断（帧到达间隔超 300ms 打 WARN）

`src/xiaozhi_tts.rs` 已包含：
- **未启用 DTX**（注释明确说明 DTX 静音帧对 ESP32 固件播放器不友好）

**② 日志里没有「断档」警告**

`record_frame_arrival_gap` 未触发 → 说明帧到达是平滑的，
TTS 合成没有断供。

**③ 所以卡顿不来自"合成断供"，也不来自"发送节奏"**

### 待排查方向

```
① 预缓冲帧数（当前 5 帧 = 300ms）
   一次性连发 5 帧是否冲击固件 jitter buffer？

② 设备端解码/播放
   Opus 24kHz 解码能力、播放缓冲大小

③ WiFi 链路质量
   设备 192.168.110.59 → 服务器 192.168.110.35
   丢包/抖动？可用 ping -i 0.06 观察

④ TTS 输出的 PCM 是否有异常
   火山 TTS 返回的 PCM 采样率是否真是 24kHz？
   如果实际是 16kHz 而按 24kHz 解码 → 会变调+卡顿

⑤ Opus 编码参数
   VBR + complexity 10 是否适合 esp32 解码
```

### 复现方式

说一句较长的指令（比如「说一句长一点的话，随便说」），听回复是否连贯。

---

## KI-002 「再打开」不调工具，只嘴上说「开了」

**状态**：已做一次修复（SOUL.md 加铁律 0），但**未完全生效**
**严重度**：高（功能错误：说做了但没做）

### 现象

```
用户：把我书房的灯关掉呗     → ✅ 真的关了
用户：再打开。              → ❌ 回答"开了。"，但灯没开
```

### 证据

**haimen agent log**

```json
{
  "input": "把我书房的灯关掉呗。",
  "output": "关了。",
  "latency_ms": 3261,          ← 有工具调用（含一次参数校验失败重试）
  "status": "success"
}
{
  "input": "再打开。",
  "output": "开了。",
  "latency_ms": 1422,          ← 明显更短，没有工具调用
  "status": "success"
}
```

**Hermes gateway 日志**

```
关灯那次：Turn ended: api_calls=3        ← 有 3 次 LLM 调用（含工具轮）
       + WARNING: tool_call to 'ha_call_service' failed argument validation
再打开：  Turn ended: api_calls=1        ← 只有 1 次，完全没调工具
```

### 已尝试的修复

改 `~/.hermes/profiles/u5bb6-u5ead-u7ba1-u5bb6/SOUL.md`：

**① 插入铁律 0**（压倒所有格式规则）

```markdown
**0. 说「开了 / 关了 / 调好了」之前，必须先真的调工具执行。这条压倒上面所有格式规则。**

- **没调工具就说「开了」「关了」，是这条最严重的违规。**
- 工具返回错误或没成功 → 说「没做成」，不许假装做了。
- 「再打开」「关了吧」这类要靠上文的指令，先去上文里找是哪台设备，然后**照常执行**。

> 下面「对照表」里的「关灯成功 → 关了」是**执行成功之后**的回答，
> 不是「听到"关灯"就回"关了"」的模板。顺序永远不能颠倒。
```

**② 修正能力范围**（原来说有电视/音箱，实际没有）

**③ 对照表加「前提」行**

### 效果验证

**部分生效**：

```
问："你能控制电视吗？"
答："这我做不了"              ✅ 能力边界修正生效
```

**但「再打开」这类依赖上文的指令仍然不执行** ⚠️

### 可能原因

```
① deepseek-flash 模型能力不足
   它的 function calling 判断力弱，对上文指代（"再打开"指什么）不敏感

② Hermes 的 tool schema 里 ha_call_service 参数复杂
   需要 domain + service + data{area_id 或 entity_id}
   模型生成参数时容易出错（关灯那次就失败了 1 次）

③ prompt 里同时有「简短」和「必须执行」两个要求
   模型优先满足了「简短」（回"开了"最省事）

④ 缺少"自检"环节
   没有机制让模型在回答前确认"我真的做了吗"
```

### 待排查方向

```
① 看 Hermes agent.log 里「再打开」那轮的完整 prompt
   → 确认 SOUL.md 的新铁律 0 是否真的进了 prompt

② 看工具 schema 的复杂度
   → 如果 ha_call_service 参数太复杂，考虑换我们自己的 MCP（7 个语义化工具）

③ 换更强的模型试一次
   → 比如 deepseek-v4 非 flash 版，看是否解决
   → 如果解决了，说明是模型能力问题

④ 在 SOUL.md 里加更具体的例子
   → "再打开 → 找到上文说的设备 → 调工具 → 才回开了"
```

---

## 附：其他已知但非阻塞的问题

| 编号 | 问题 | 状态 |
|---|---|---|
| — | 本地能量 VAD 从不触发（环境噪音 + 直流偏置） | 已修 DC 偏置，仍在噪音下失效；判停靠服务端 500ms，够用 |
| — | 火山 ASR 的 `is_final` 极少到达 | 已用 `last_nonempty_text` 兜底 |
| — | 长回复（106 字） | SOUL.md 已加 20 字上限，效果待观察 |

---

## KI-003 聆听停止过快 + 识别率下降

**状态**：待排查（MQTT 模式实测反馈，怀疑与此前延迟调优有关）
**严重度**：高（打断用户说话，且识别错）

### 现象

- 话还没说完就被判定「说完了」，开始返回回复
- 整体识别率比之前下降

### 高度怀疑的改动

本轮为降延迟，动过这几个参数（全部在 `src/xiaozhi_asr_llm_tts.rs`）：

| 参数 | 原值 | 现值 | 影响 |
|---|---|---|---|
| `end_window_size` | 1500ms | **500ms** | 豆包服务端判停窗口，**最可疑** |
| `force_to_speech_time` | 1000ms | **500ms** | 最短可用语音时长 |
| `MAX_SILENCE_FRAMES` | 40 帧(2400ms) | **15 帧(900ms)** | 本地能量 VAD |
| `SILENCE_RMS_THRESHOLD` | 1500 | **3000** | 静音判定阈值 |
| `PREBUFFER_COUNT`（ws.rs / mqtt_udp.rs） | 10 | **5** | 与判停无关 |

**`end_window_size = 500ms` 是最可能的元凶**：中文口语中间停顿常常 400-600ms，
500ms 窗口会在用户思考/换气时直接判停。历史记录里 400ms 曾导致严重的空结果问题，
500ms 虽然靠 `last_nonempty_text` 兜底不至于失败，但会**过早截断**。

识别率下降可能同样来自过早截断——话被切掉一半，识别结果自然差。

### 待做

1. 用日志统计「ASR 完成」到「用户实际停止」的时间差，确认是否 < 用户语速
2. 把 `end_window_size` 逐步回调：500 → 800 → 1000，找到不打断的边界
3. 考虑**区分场景**：短指令（"关灯"）可以激进，长句需要保守
4. 确认 `last_nonempty_text` 兜底是否会因为截断而只拿到半句

### 注意

本轮延迟已经从 2.45s 降到接近理想，**回调参数会牺牲一部分延迟**。
需要权衡：宁可慢 200ms，也不要打断用户。

---

## KI-004 TTS 轻微掉帧

**状态**：待排查
**严重度**：中（影响听感，不致命）

### 现象

语音播放有轻微掉帧（卡顿感），比切 MQTT 之前更明显一些。

### 可能的差异点（MQTT vs WebSocket）

```
WebSocket：音频帧走 TCP，天然可靠有序
MQTT+UDP：音频帧走 UDP，不可靠、可能乱序/丢包
```

**UDP 丢包的后果**：
- 设备侧解码队列只有 20 包（1.2s），发太快会静默丢包
- 网络抖动导致包乱序时，设备只告警不重传（协议无重传机制）

### 待排查方向

1. **发送节奏**：当前 `avg_gap ≈ 57ms`（期望 60ms），预缓冲 5 帧连发拉低均值；
   实测连发段之后是否稳定在 60ms
2. **UDP 丢包率**：容器/宿主能看到 UDP 收发计数差异吗？加计数器
3. **本地网络**：Mac Studio ↔ 设备在同一网段，丢包应该很少；可用 ping 大包压测
4. **单包大小**：`opus` VBR 导致包长波动，是否触发设备侧缓冲异常
5. **对比实验**：同一段文本，WS 模式 vs MQTT 模式各播一次，对比掉帧情况

---

## KI-005 「再见」不会结束聆听

**状态**：未实现
**严重度**：低（体验缺失，非 bug）

### 现象

对小智说「再见」，它只是回一句「再见」，但不会结束对话/进入待机。
官方云端在小智说再见后会主动结束会话。

### 固件侧事实（来自协议调研）

- 设备**没有** no-speech 自动回 Idle 的机制，会话结束必须由服务器驱动
- 服务端发 `{"type":"goodbye","session_id":"..."}` 且 `session_id` 匹配时，
  设备会 `CloseAudioChannel(false)`（源码 `mqtt_protocol.cc:131-143`），
  即关闭音频通道回到 Idle，且**不会回发 goodbye**（防 ping-pong）

### 实现思路

1. 在 SOUL.md 里让模型识别「告别意图」并输出一个可检测的标记
   （不能简单匹配"再见"二字，否则"跟奶奶说再见"也会误判）
2. 策略层（`AsrLlmTtsStrategy`）检测到告别意图后，走完 TTS 播报，
   再让传输层发 `goodbye`
3. 注意：`goodbye` 必须在音频播完之后发，否则会把回复掐断

### 依赖

需要一个「这句话说完后要结束会话」的信号，从策略传到传输层。
可参考现有的 `no_speech_completion()` / `goodbye_frames()` 钩子设计。

---

## 里程碑：固件升级 2.2.6 → 2.5.2（2026-10-19）

**设备**：`bread-compact-wifi-lcd`（ESP32-S3，LCD ST7789 240x320）
**固件**：`v2.5.2_bread-compact-wifi-lcd.zip`
（SHA256 `0523dfcac6129718707048d41982d4cbdbcbadcd197ecd4783133aa218f1ff72`）

### 刷机方式与结果

**无需 erase，直接刷 merged-binary.bin**：
```
esptool.py --chip esp32s3 -p <port> -b 460800 write_flash 0x0 merged-binary.bin
```

依据（三条实证）：
1. 官方发布包只有 `merged-binary.bin`，设计就是从 0x0 直刷
2. 2.2.6 与 2.5.2 的分区表 `partitions/v2/16m.csv` **逐字节相同**
   （nvs 0x9000/0x4000、otadata 0xd000、ota_0 0x20000、assets 0x800000）
3. `idf.py merge-bin` 不含 nvs 分区 → NVS 里的 WiFi 凭据与 `ota_url` 都保留

实测：刷完设备仍拉取我们的 OTA（`ota_url` 保住），OTA 日志确认
`app_version=2.5.2`、`board=bread-compact-wifi-lcd`、MAC 未变。

### notify 能力验证通过 ✅

固件 2.5.2 支持 `notify`（2.2.6 不支持，当时完全无反应）。

实测流程：
```
1. 起 HTTP 服务提供单声道 Ogg Opus（ffmpeg 从 say 生成）
2. 通过控制面 haimen/ctl/<client_id> 发
   {"type":"notify","audio_url":"http://192.168.110.35:8899/notify.ogg"}
3. haimen 日志：控制面消息已转发给设备
4. HTTP 日志：192.168.110.59 - "GET /notify.ogg" 200    ← 设备真身来取
5. 设备播放完成，自动回到 Idle
```

**两种主动能力的定位（都已实测）**：

| 能力 | 路径 | 前提 | 播完状态 | 适用 |
|---|---|---|---|---|
| 纯播报 | `notify` | 仅需 Idle | Idle | 洗衣机好了、门铃、温度告警 |
| 主动问询 | `tts` 会话 | 先唤醒过 + 120s 内 | Listening | "要开空调吗？"→ 等回答 |

`notify` 突破了 `tts` 会话的两个硬限制：不需要先唤醒、不依赖 UDP 通道超时。

### 待办：把 notify 做成正式功能

当前还是外部脚本 + 临时 HTTP 服务，正式化需要：
1. haimen 内置静态音频服务（或复用现有 HTTP server 的 `/notify` 路由）
2. 内置 TTS：文本 → Ogg Opus（需 Ogg 容器封装；`opus2` 只能编 Opus 包）
3. HTTP API：`POST /api/v1/xiaozhi/notify {"text": "...", "then_listen": false}`
4. 之后 HA 自动化可直接调用

---

## 待调研：官方自定义 UI / 生成 assets 固件

**状态**：已知入口，暂不深究（2026-10-19 记录）

用户提及官网有「自定义界面」的地方，可以生成 asset 固件。

**为什么重要**：`hide_subtitle`（是否隐藏底部对话字幕）**只从 assets 的
主题配置读取**，代码里没有任何运行时开关，也没有 MCP 工具：

```cpp
// main/assets.cc:428-433
cJSON* hide_subtitle = cJSON_GetObjectItem(root.get(), "hide_subtitle");
if (cJSON_IsBool(hide_subtitle)) {
    display->SetHideSubtitle(cJSON_IsTrue(hide_subtitle));
}
```

而 2.5.x 的默认显示风格把对话文字从「中部气泡区（保留多轮）」
改成了「底部单行字幕」（`bottom_bar_` + `chat_message_label_`），
且只在 `CONFIG_USE_WECHAT_MESSAGE_STYLE` 下才是气泡 UI。

**如果以后要改 UI / 显示文字**，三条路径：
1. 官方自定义 UI 生成 assets（用户提到的入口，待调研）
2. 自己编译固件，加 `CONFIG_USE_WECHAT_MESSAGE_STYLE=y`
   （恢复气泡 UI）或 `CONFIG_USE_MULTILINE_CHAT_MESSAGE=y`（底部多行）
3. 用 MCP `self.assets.set_download_url` 让设备下载自定义 assets
