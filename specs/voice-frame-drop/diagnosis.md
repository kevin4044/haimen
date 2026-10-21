# 小智语音回复「经常掉帧」根因诊断（独立验证）

> 任务：`task-2`（独立验证「tts start 竞态导致预缓冲丢失」根因）
> 仓库/分支：`haimen` @ `fix/voice-frame-drop`，HEAD `63ab8a1`（分析时工作区另有**未提交**的 settle 修复 WIP，见 §7，本文不把 WIP 当已定稿）
> 固件参考：`/Users/kevin/Documents/works/xiaozhi-esp32` @ `1718809`（真机日志显示固件 `app_version="2.5.2"`，board `bread-compact-wifi-lcd`）
> 日志：`/tmp/haimen-mini/clean.log`（3473 行，2026-10-19 09:51 ~ 2026-10-21 08:12，共 86 轮下行播放）
> 方法：逐行核对代码与固件源码 + 日志定量统计 + 网络实测。**未修改任何 Rust 代码**，唯一写入为本文件。
> 标记约定：**【已验证】** = 直接读代码/源码/日志可确认；**【推断】** = 由已验证事实推出，但缺设备侧直接观测；**【反驳】** = 证据与假设相反。

---

## 0. 结论摘要（TL;DR）

1. **初始假设的机制成立（高置信，机制层面已验证）**：MQTT 控制面的 `publish_tts(Start)` 只把报文放进 per-client mpsc 写队列就返回（`mqtt_broker.rs:208-231`，`write_loop` 在另一个任务里写 socket：`mqtt_broker.rs:288-291, 482-494`）；`drive_playback_inner` / `say_raw_frames` 紧接着**连发 5 帧预缓冲**、中间没有任何等待（`mqtt_udp.rs:1694-1721`、`:712-732`）。固件侧 `OnIncomingAudio` 只在 `Speaking` 才入解码队列（`application.cc:567-571`），而 `tts start` 要经过 `Schedule` 异步切状态（`application.cc:637-641`、`Schedule` 实现 `:1248-1253`、主任务循环 `:270-276`）。→ 预缓冲可以（且通常整批 5 帧）赶在状态切换前到达并被静默丢弃，协议层 `remote_sequence_` 已前进、不重传。
2. **但「WS 路径无此竞态」的说法需要修正**：WS 只是消除了**跨链路乱序**（start 与音频在同一有序 TCP 流上，`ws.rs:814-850`），固件 `Schedule` 的异步窗口在 WS 上同样存在。差别是 MQTT 多两层不确定性（broker 写队列异步 + 独立 UDP 通道）且 5 帧在 <1ms 内整批发出，所以「整段预缓冲」被丢的概率大得多。这让「切 MQTT 后更明显」（KI-004）能自洽，但不能说 WS 完全无竞态。
3. **日志无法直接证明设备丢帧**（证据里没有设备串口日志）。86 轮 `下行播放统计` 只能证明**发送侧**无丢帧、无停顿、无 MQTT 超时。因此「竞态就是用户症状的（唯一）主因」是**【推断】，置信度中高（~0.7）**，需要 settle 0/100 的 A/B 真机听感 +（可选）固件调试日志来确证。
4. **新发现（独立缺陷）**：`ContinuityPump 收尾超时，已中止` 是**假中止**——5s 超时后 `PumpGuard::finish` 里的 `JoinHandle` 已被 `take()` 进 future，超时丢弃 future 只是 **detach**，pump 并没有被 abort。26 次超时中可以对齐到统计行的 24 次里，回放在超时后仍继续 2.1–17.9s（中位 7.3s）才发 `tts stop`。它不是掉帧根因，但会拖长「设备还在说话」的窗口、延迟收口，且日志内容具有误导性。
5. **网络：Lead 的「跨网段 RTT 抖动」描述不准确**。我从与设备同网段（192.168.110.35）实测 ping 设备：0% 丢包，RTT 在 3ms 与 500–640ms 之间交替（典型的设备 WiFi 省电/DTIM 睡眠），而不是路由段问题（本机 ping mac mini 192.168.50.33 仅 4ms）。播放期间固件在 `OnAudioChannelOpened` 里请求 `PERFORMANCE` 省电档（`application.cc:573-574`）——**若板级实现生效，则播放期不会睡眠**（WifiManager 组件源码不在本地，标【推断】）。所以网络不是首要根因，但必须保留为残留风险。

---

## 1. haimen 代码路径核对

### 1.1 `publish_to` 是否等 socket 写完？→ 否【已验证】

| 事实 | 证据 |
| --- | --- |
| 下行报文只进 per-client mpsc（容量 32），`tx.send(frame).await` 成功即返回 | `crates/haimen-xiaozhi/src/mqtt_broker.rs:25`（`WRITE_QUEUE: usize = 32`）、`:208-231` |
| 真正的写 socket 在独立任务里：`handle_connection` 先 `tokio::spawn(write_loop)`，`write_loop` 里 `write_all` + `flush` | `mqtt_broker.rs:288-291`、`:482-494` |
| broker 没有暴露「已落 socket」的回执/屏障 | 全文无 flush/ack API |
| 写队列满时 `publish_to` 会 await，但 `publish_body` 套了 3s 超时 | `mqtt_udp.rs:46`（`MQTT_WRITE_TIMEOUT = 3s`）、`:2036-2077` |

→ `publish_tts(Start).await` 返回只代表「入队成功」，不代表设备已收到/已处理。

### 1.2 从 publish start 到首帧 UDP 之间有没有等待？→ 没有【已验证】

- `drive_playback_inner`（HEAD `crates/haimen-xiaozhi/src/mqtt_udp.rs:1694-1721`）：
  ```text
  1695  publish_tts(Start)              // 入队即返回
  1699-1710  publish_tts(SentenceStart, ...) × N
  1712-1721  for audio in prebuffer { send_downlink_frame(...) }   // 5 帧连发，无 sleep
  1723-1731  之后才建立 60ms 发送时钟
  ```
- `say_raw_frames`（`mqtt_udp.rs:686-751`）同构：
  ```text
  712  publish_tts(Start)
  719-732  for (i, frame) 逐帧 send_audio；前 prebuffer 帧之间不 sleep（i+1>=prebuffer 才开始 sleep）
  ```
- 预缓冲默认 5 帧：`mqtt_udp.rs:117`（`prebuffer_count: 5`）；网关装配也硬编码 5：`src/gateway/mod.rs:531`。
- 若预缓冲帧被设备丢弃，**整个 300ms lead 归零**；之后发送严格按 60ms 实时节奏（`:1723-1731`、`:1748-1774`），不会自行重建 lead。

### 1.3 WS 路径是否「同链有序」？→ 传输层有序，但固件窗口仍在【部分已验证 / 需修正】

- WS 的 `tts start` 与预缓冲音频走**同一个 WebSocket/TCP 连接**，`send_json(Start)` 返回后再发二进制帧：`crates/haimen-xiaozhi/src/ws.rs:814-830`（start）、`:831-839`（flush 句子文本）、`:841-850`（5 帧预缓冲连发）。→ **运输层字节顺序有保证**，start 一定先于音频到达设备（已验证）。
- 但固件解析 start 后仍是 `Schedule(... SetDeviceState(Speaking))`（`application.cc:637-641`），音频回调仍检查 `Speaking`（`:567-571`）。WS 的 JSON 与二进制在**同一个接收任务**里顺序回调（`websocket_protocol.cc:108-151`），但状态切换在主任务——所以固件层面的窗口**依然存在**，只是比 MQTT 小（没有 broker 排队 + 没有独立 UDP 通道乱序）。
- 结论：**不能写「WS 路径无此竞态」**，应写成「WS 只有运输层有序性优势，固件异步窗口未消除；按历史听感 WS 更顺是幅度差异，不是有无差异」。如果未来再启用 WS 路径，建议同样加一个小 settle（20–50ms 即可）。

---

## 2. 固件证据核对（本地源码，行号可直接复查）

| 断言 | 固件证据 | 判定 |
| --- | --- | --- |
| 音频只在 `Speaking` 时入解码队列 | `main/application.cc:567-571`（`OnIncomingAudio` 回调里 `if (GetDeviceState() == kDeviceStateSpeaking) PushPacketToDecodeQueue`），否则静默丢弃；`application.cc:569` 忽略返回值 | 【已验证】 |
| `tts start` 经 `Schedule` 异步切状态 | `application.cc:637-641`；`Schedule` 把回调塞进 `main_tasks_` 并 `xEventGroupSetBits(MAIN_EVENT_SCHEDULE)`：`:1248-1253`；主任务循环里才执行：`:270-276`；主任务优先级 10：`:181-183` | 【已验证】 |
| UDP 音频与 MQTT 控制面是两条接收路径 | `main/protocols/mqtt_protocol.cc:301`（`network->CreateUdp(2)`）、`:363`（`remote_sequence_ = sequence` 在应用回调**之前**推进）、`:367`（`on_incoming_audio_` 由 UDP 接收回调直接调用） | 【已验证】 |
| 被应用丢弃的帧无法重传 | `mqtt_protocol.cc:334-344` 只对 `seq <= remote_sequence_` 丢弃、对跳号告警后**接受**；`remote_sequence_` 已前进 | 【已验证】 |
| 解码队列 20 包，满即静默丢 | `main/audio/audio_service.h:43`（`MAX_DECODE_PACKETS_IN_QUEUE = 1200/60 = 20`）；`audio_service.cc:609-629`（`wait=false` 时返回 false）；`application.cc:569` 忽略返回值 | 【已验证】 |
| 播放队列空时输出任务会**阻塞等待**（underrun 表现为停顿/断续） | `audio_service.cc:326-343`（`AudioOutputTask` 等 `audio_playback_queue_` 非空） | 【已验证】 |
| 播放期 WiFi 省电档切 PERFORMANCE、通道关闭切回 LOW_POWER | `application.cc:573-574`、`:583-584`；板级 `WifiBoard::SetPowerSaveLevel` 转发到 `WifiManager`：`main/boards/common/wifi_board.cc:282-296`；`WifiManager` 来自 `78/esp-wifi-connect` 组件，本地无源码 | 状态调用【已验证】；PERFORMANCE == `WIFI_PS_NONE` 为【推断】 |
| 服务端声明下行 24kHz、设备上行 16kHz | haimen 真实策略 `AsrLlmTtsStrategy::hello_audio_params` 声明 `sample_rate: 24000`（`src/xiaozhi_asr_llm_tts.rs:1903-1910`）；设备 hello 固定 16000（`mqtt_protocol.cc:GetHelloMessage`） | 【已验证】参数匹配，不是掉帧原因 |

### 2.1 规范/前序调研核对（含小节号修正）

- `specs/mqtt-udp/协议规格.md`
  - §4.5.5（`:576-578`）：`tts start` 与首帧之间建议留 **20–100ms**；
  - §7.7（`:1186-1190`）是 sequence 语义；**与本次竞态直接相关的是 §7.9（`:1200-1204`「tts start 必须先于音频且留一点时间」）和 §7.10（`:1206-1210` 60ms 实时节奏/解码队列 20 包）**。任务描述里写的「§7.7/§7.11」建议改为「§7.9/§7.10」。
- `specs/对比-xiaozhi-协议层详细报告.md` §9.4（`:485-493`）与 P1-2（`:571`、`:594`）：明确建议「`tts start` 后 sleep 20–100ms 再发第一帧」，并点名 `drive_playback` / `say_raw_frames` 两处。**当前 HEAD 代码没有实现**（见 §1.2）→ P1-2 状态仍是「未落地」。
- `specs/audio-pacing/02-design.md`：上一轮把根因归到「尾帧被过早 tts stop 回收」。该结论**证据不足**：T2 埋点只证明了发送侧一帧没丢（本轮日志复核一致），但「设备侧丢帧」并不只有尾帧一个入口，起点竞态被漏掉了；而且 T1 上线后（tail_wait≈420ms）用户仍反馈掉帧，本次任务的存在本身即反证。KI-004 里「比切 MQTT 之前更明显」与 §0 第 2 条一致。

---

## 3. 为什么「5 帧整批」大概率全丢（时序分析）

【推断，但有代码结构支撑】

1. haimen 侧：`publish_tts(Start)` → mpsc 入队 → 返回（µs 级）；紧接着 5 次 `send_audio`（每次锁 + `send_to`，合计 <1ms）。**5 个 UDP 数据报在 <1ms 内全部出网。**
2. broker 写任务被 `tx.send` 唤醒后才是 `write_all + flush`，它在**另一个 tokio 任务**里；它相对上面 5 次 UDP 发送没有顺序保证（多线程 runtime 甚至可能并发）。即使它抢在 UDP 前把 start 写进 TCP，设备侧还要：
3. 设备 MQTT 任务收 TCP、解析 JSON → 调 `Application::OnIncomingJson` → `Schedule` 入主任务队列（此时状态**还没变**）；
4. 主任务被唤醒、执行 `SetDeviceState(Speaking)`（最坏要等 1 个 FreeRTOS tick，ESP32-S3 默认 `CONFIG_FREERTOS_HZ=100` ≈ 10ms，或等主任务当前任务让出）；
5. 而设备 UDP 接收任务在 1–2 个网络 RTT 内就能把 5 个包全部处理完（解密 + seq 校验 + 回调 + 状态判断）。
→ **「设备处理完 start 并切状态」通常比「5 帧 UDP 全部到达」慢至少一个数量级**，所以整批预缓冲被丢是符合预期的，而不是偶发。

补充一个容易被忽略的点：**这些预缓冲帧实际上几乎都是 ContinuityPump 的静音帧**，不是开头的字音。日志实测「VAD 结束 → 首帧发出」中位 **0.24s**（见 §4.3，正好是 pump 以 60ms 产 5 帧的时间），而「VAD 结束 → TTS 首个文本块」中位 **0.94s**（真正音频还要更晚）。所以：
- 被丢的是「静音 + 300ms lead」，听感**不是每次开头缺字**，而是**整个回复期间设备缓冲没有余量**——任何 UDP 抖动/设备调度抖动都会在语音中造成断续。这与用户「经常掉帧」而非「开头缺字」的描述吻合。
- 也解释了为什么 tail 修复（只补尾）不能解决该症状。

---

## 4. 日志统计（`/tmp/haimen-mini/clean.log`，86 轮）

### 4.1 `下行播放统计`（86 条）——发送侧完全正常【已验证】

| 指标 | 结果 |
| --- | --- |
| 轮次数 | 86 |
| `frames` | min 31 / 中位 131.5 / max 1465 |
| `elapsed_ms − expected_ms` | 全部落在 **[−300, −230]**；79/86 在 **[−299, −297]**；分布 `{-300:4, -299:36, -298:23, -297:20, -296:1, -291:1, -230:1}` |
| 判读 | `expected = frames×60`；`elapsed ≈ (frames−5)×60` 正好等于「5 帧连发省下 300ms」→ 证明 (a) 预缓冲确实是 5 帧连发；(b) 全程按 60ms 时钟发送，**没有任何一帧周期（60ms）级别的停顿**（否则 delta 会明显大于 −300）；(c) 累计误差仅 0–3ms |
| `tail_wait_ms` | 420–423 × **73** 轮；0 × **13** 轮 |
| 发送失败 | `下行音频发送失败` **0** 条；`下发 MQTT 超时` **0** 条；`下发 MQTT 失败` **0** 条；`设备已掉线` **0** 条 |

- 13 轮 `tail_wait_ms=0` **不是异常**：逐条核对后续 1–3 行，全部是「被下一轮打断/设备重连/进程重启」（紧跟新 `流式 ASR: 清空管道状态`、`新 hello` 或 `小智 MQTT+UDP 传输层已启动`）。属于 barge-in 或断连的轮次，不构成掉帧证据。【已验证】
- 结论：**发送侧没有丢帧、没有阻塞**；问题只能在「设备接收侧」或「设备播放侧」。

### 4.2 `ContinuityPump 收尾超时`（26 次）——「已中止」是假的【已验证机制，影响为量化推断】

**机制（代码语义）**：
- `src/xiaozhi_asr_llm_tts.rs:2786`：`tokio::time::timeout(5s, pump_guard.finish())`；
- `PumpGuard::finish(mut self)`（`:3725-3735`）在**第一次 poll 时**就 `self.0.take()` 把 `JoinHandle` 移入自身状态，然后 `handle.await`；
- 超时发生时 `timeout` **drop 掉内层 future** → 其中的 `JoinHandle` 被 drop = **detach**（tokio 语义：drop JoinHandle 不 abort 任务）；future 里的 `self`（PumpGuard）此时 `self.0 == None`，`Drop`（`:3737-3743`）也不会 abort。
→ 日志「已中止」与代码注释「超时则 abort pump 防泄漏」都不成立；pump 继续运行直到 `pcm_rx` 关闭。
（仅当 timeout 第一次 poll 前就到期才会 abort，实际不可能；5s 远大于首 poll。）

**观测影响**（把每个超时对齐到其后第一条 `下行播放统计`，用 `统计时刻 − tail_wait` 估计回放结束时刻；24/26 可对齐，另 2 次所在轮次无帧、未打统计）：

| 超时后回放仍持续的时长 | 结果 |
| --- | --- |
| min / 中位 / mean / max | **2.08s / 7.3s / 7.1s / 17.9s** |
| 超过回放通道积压理论上限（frame_tx 容量 32 × 60ms = 1.92s）的轮次 | **24/24 全部超过**（其中 21/24 还 >2.3s） |

→ 超时后 pump 仍在以 60ms 继续产出帧（主要是静音），回放继续、`tts stop` 被推迟 2–18s。**这不是掉帧的根因**，但会造成「回复结束后设备还停在 Speaking/长时间静默」的错误观感，并拖长下一轮唤醒前的不可打断窗口；建议单独修（见 §6.6）。

补充：日志里那两条 1465 帧 / 87.6s 的超长轮次也是同一族问题——ASR/生成侧挂起（`生成语音回复失败 err=ASR 识别超时 (30s)`、`断档提示: TTS 合成超时`），pump 用静音持续供帧，把「生成等待」变成了「设备持续 Speaking 播静音」。与丢帧无关，但会污染听感判断。【已验证日志，机制为推断】

### 4.3 VAD→首帧 ≈ 0.24s：证明预缓冲是静音、且 start→首帧没有 settle【已验证】

| 指标 | 结果（n=75 能配到 VAD 前驱的轮次） |
| --- | --- |
| VAD 结束 → 首帧 UDP 发出（由 `统计时刻 − tail_wait − elapsed` 反推） | min 0.24 / 中位 **0.24** / 72 轮 ≤0.48s；3 个离群 44.5 / 64.6 / 65.8s（生成挂起轮） |
| VAD 结束 → `TTS 首块`（文本，n=57） | 中位 **0.94s** / mean 1.64 / max 5.73s |

→ 首帧总是比 TTS 首个文本块早约 0.7s，说明首帧是 pump 静音；也说明当前 `tts start → 首帧` 的间隔只有「publish 入队 + 5 次 send_to」的量级（≈0），**settle 确实不存在**。

### 4.4 其它日志信号（不解释下行掉帧，但影响判断/体验）

| 信号 | 次数 | 说明 |
| --- | --- | --- |
| `on_audio_frame 失败，帧已留在缓冲里 err=ASR 管道已关闭` | 642 | **上行**音频在 ASR 管道关闭后丢弃 → ASR 质量/稳定性问题 |
| `喂入 ASR 超时（下游积压），本帧丢弃` | 25 | 上行喂 ASR 超时 |
| `🎤 [ASR 完成]…完整文本长度: 0` / 兜底 / ASR 空错误 | 71+79 | 上行识别稳定性 |
| `生成语音回复失败` | 17 | 生成阶段失败轮次（其中 ASR 超时/批处理超时） |
| `MQTT 连接已断开` / `设备 hello` | 14 / 55 | 设备重连频繁（回待命/重启）；都发生在轮次边界，未观察到播放中途断连 |
| `MQTT 空闲超时` | 2 | 均在 goodbye 后 10+ 分钟，与播放无关 |

→ 上下行问题的症状容易混在一起（用户可能把「ASR 听不懂/生成失败/长时间静音」也感知为「掉帧」）。建议诊断时以「回复音频是否连贯」为唯一判据，并单独跟踪上行问题。

---

## 5. 替代假设评估（逐条支持/反驳）

| # | 假设 | 判定 | 依据 |
| --- | --- | --- | --- |
| 1 | **网络 UDP 丢包/乱序（跨网段 RTT 抖动）是主因** | **部分支持，但不作为首要根因**；Lead 的归因需修正 | ① 我从与设备**同网段**的 192.168.110.35 实测 `ping -c20 -i0.5 192.168.110.59`：**0% 丢包**，RTT **min 3.1ms / avg 312.6ms / max 639.2ms / stddev 254.6ms**，呈 3ms ↔ 500–640ms 交替、慢值逐轮 +18ms——这是**设备 WiFi 省电睡眠/DTIM** 的特征，不是跨网段路由问题（本机 ping mini 192.168.50.33 仅 4ms）。② 播放期固件 `OnAudioChannelOpened` 会请求 `PERFORMANCE`（`application.cc:573-574`），**若生效则播放时几乎无此抖动**（WifiManager 映射未在本地源码核实，【推断】）。③ 服务端 0 次 UDP 发送失败；真丢包因无重传会在**随机位置**造成断续，与「每轮起点固定丢整批」不同。**残留风险仍高**：若 PERFORMANCE 未生效或 AP 侧抖动 >300ms，即使修好竞态仍会丢帧。 |
| 2 | **设备解码队列溢出（20 包/1.2s）** | **【反驳】** | 服务端每轮只连发 5 帧（300ms，仅为队列容量 1.2s 的 25%），之后严格 60ms 实时；`snap_send_clock` 保证落后只重锚不补发（`audio-pacing/02-design.md` §2 已核对）。除非一次突发 >20 包，否则不可能溢出；日志帧节奏证明没有突发。 |
| 3 | **播放循环被 MQTT publish 阻塞** | **【反驳】** | 86 轮 `elapsed−expected` 全部 ≈ −298ms（仅 1 轮 −230），没有任何 ≥60ms 的累计停顿；`下发 MQTT 超时` 0 次；每轮控制面报文只有几十条、写队列 32，远未拥塞。 |
| 4 | **TTS PCM 断供导致 underrun** | **【反驳（作为 underrun 原因）】**，但会以另一种形式影响听感 | ContinuityPump 以 60ms tick 持续喂静音/音频（`xiaozhi_asr_llm_tts.rs:3579` `tick_ms=60`、`:3604`），发送侧永不断供；日志中 `断档提示: TTS 合成超时` 仅 1 次。若 TTS 合成慢于实时，pump 会**插入静音**（听感为句中停顿，而非跳帧）。属另一类问题，需 TTS 首音时间埋点确认，不影响本次根因判定。 |
| 5 | **prebuffer 从 10 降到 5 的回归** | **部分成立，但只影响 WS 路径；MQTT 一直是 5** | `git log -S "PREBUFFER_COUNT: usize = 10" -- ws.rs` → `18c4019` 引入 10，`c20eaad`（2026-10-18 19:50）改成 5；MQTT 传输层 `2ab0895`（同日 22:57）引入时 `prebuffer_count` 就是 5。所以用户感知「切 MQTT 后更明显」不能归因于 10→5（MQTT 从未有过 10），而应归因于 MQTT 把 5 帧**整批**丢掉（WS 至少大概率能保住一部分/全部，见 §1.3）。 |
| 6 | 设备 Opus 解码性能不足 / TTS PCM 采样率异常 | **未验证，低概率** | haimen 下行声明 24kHz（`src/xiaozhi_asr_llm_tts.rs:1903-1910`），设备支持 24k 解码并重采样；若采样率错配会表现为变调/加速，而非断续。无设备串口证据，暂列残留。 |

---

## 6. 结论与建议

### 6.1 置信度

| 结论 | 置信度 | 依据类型 |
| --- | --- | --- |
| 代码/固件层面确实存在「start 未生效时预缓冲整批被丢」的竞态 | **~0.95（已验证）** | 逐行代码 + 固件源码 |
| 该竞态是用户「经常掉帧」的**主因** | **~0.7（推断）** | 只能靠 A/B 确证；且网络残留风险未排除 |
| 发送侧正常、无 MQTT 阻塞、无解码队列溢出、尾帧修复有效 | **~0.95（已验证）** | 86 轮统计 |
| 「WS 路径无竞态」不成立；应表述为「无跨链路乱序，固件窗口更小」 | **~0.9（代码结构）** | 代码 + 固件 |
| PumpGuard 超时未真正 abort，导致 2–18s 延迟收口 | **~0.9（机制已验证，影响量化推断）** | 代码语义 + 日志时序 |

### 6.2 settle 建议值：**默认 100ms，可配置（0–300ms）**

延迟预算（播放期，按健康路径估算）：

| 环节 | 典型 | 坏情况 |
| --- | --- | --- |
| broker 入队 → write_loop 写 socket | <1ms | 运行时繁忙 ~5–10ms |
| TCP 到设备（WiFi PERFORMANCE） | 1–5ms | 设备唤醒/AP 抖动 20–50ms |
| 设备 MQTT 解析 + `Schedule` | <1ms | 解析/任务排队 ~1ms |
| 主任务执行状态切换 | ≤1 tick（10ms） | 主任务队列积压 10–20ms |
| 合计 | **~15ms** | **~80ms** |

→ **100ms 有 5 倍以上余量**，且因为预缓冲帧实际是静音、真正语音要等 TTS（VAD→首块文本中位 0.94s，音频更晚），**这 100ms 不增加用户听到首字的延迟**，只把静音 lead 推迟 100ms。不建议默认 >200ms（收益无证据，且会推迟设备进入 Speaking/显示状态）。
- 必须 **cancel-aware**（barge-in 时立即放弃），并保证 `tts start` 已发出时取消路径仍能正确发 `tts stop`（或由 abort 路径收口）。
- 建议把 settle 值传导到配置（`config.toml`），以便不改代码做 A/B；同时在 `下行播放统计` 里增补 `settle_ms` / `tts_start→首帧` 实际间隔，便于验证。

### 6.3 prebuffer 是否需要调？→ **本次先不动（保持 5）**

- 5 帧被丢 → lead=0，是**数量之外的问题**：把 prebuffer 加到 8/10，只要竞态还在，整批照样被丢，等于没加。
- settle 修好后 5 帧 = 300ms lead 大概率够用（播放期 WiFi 应为 PERFORMANCE）。**先做 settle 的 A/B**；若仍偶发 underrun，再单独把 prebuffer 调到 8（上限建议 ≤10：设备解码队列 20 包，留余量给抖动、避免一次连发占满）。不要一次改两个变量，否则无法归因。

### 6.4 验证方案（必须做，否则「主因」无法升级为已验证）

1. **A/B 真机听感**：同分支分别用 `tts_start_settle_ms = 0 / 100`（同文本、同网络、各 ≥10 轮），记录「句中卡顿次数/主观评分」。若 100ms 明显改善 → 主因确认。
2. **设备侧直接证据（可选但最硬）**：接 UART 看设备串口日志；或临时给固件 `OnIncomingAudio` 的非 Speaking 分支加一行告警，统计每轮开头被丢的帧数（预期 settle=0 时每轮丢约 5 帧、settle=100 时 0 帧）。注意现有 `wrong sequence` 告警**看不到**这种丢弃（协议层 seq 已连续推进，见 §2）。
3. **回归**：确认 settle 期间 barge-in、`tts stop` 时机、`say`/notify 主动播报不受影响；跑 `cargo test -p haimen-xiaozhi`（WIP 已加多个 settle 相关测试，见 §7）。
4. **网络对照**：A/B 期间同时 `ping -i 0.2 设备` 记录 RTT；若 settle=100 后仍有卡顿且 ping 慢值同步出现，则残留网络因素占主导，转查 WiFi 省电/AP。

### 6.5 剩余风险

1. MQTT `tts start` 的 TCP 包若发生重传（RTO ≥200ms）或 broker 写队列拥塞超过 settle，预缓冲仍会被丢 → 建议后续在 broker 上加「写完成屏障」（见 §6.6），把 settle 降到 20–50ms 即可覆盖，且不受写任务调度影响。
2. 播放期若 WiFi 省电未被真正关闭（`WifiManager` 行为未在本地核实），300ms lead 可能仍不够 → 残留网络型掉帧。
3. 读日志/复现期间工作区已有 settle WIP（§7），**不要把 WIP 与 HEAD 行号混用**；WIP 定稿后建议更新本文的代码引用。
4. `ContinuityPump` detach 缺陷与上行 ASR 稳定性问题会继续干扰听感与线上判断，建议单独立项修复，不要与本次 settle 混在一个 PR。

### 6.6 建议的后续修复（按优先级）

1. **P1：settle 落地**（WIP 已按此方向实现，见 §7）。
2. **P1：修 `PumpGuard` 超时语义**：超时应真正 `abort()`。最小改法：把 `JoinHandle` 留在 guard 上，用 `tokio::time::timeout(5s, &mut handle)`，超时分支显式 `handle.abort().await` 后再返回；或改成 `timeout(5s, pump_handle)` + `pump_handle.abort()`。顺带修正日志/注释（现在「已中止/防泄漏」与事实不符）。
3. **P2：broker 写完成屏障**：`write_loop` 每写完一帧后 `notify` 一个 `watch`/`oneshot`，`publish_tts` 可选择性等待「已写 socket」，再开始 settle 计时。这能消除 §6.5-1 的调度不确定性，并把 settle 需求降到 20–50ms。
4. **P2：WS 路径一致性**：若还要保留 WS 回放，给 `playback_frames_stream` 同样加一个 20–50ms 的 start 后等待（它也有固件 Schedule 窗口，只是更小）。
5. **P3：埋点**：`下行播放统计` 增加 `settle_ms`、`tts_start→首帧实际间隔`；`ContinuityPump 收尾超时` 改为带 `sent_frames`/`pending_pcm` 的 warn，方便区分「detach 后继续喂帧」与「通道积压」。

---

## 7. 对工作区未提交 WIP 修复的核验（仅供 Lead 参考，非最终结论）

分析时工作区（`git status`）有 3 个文件未提交：`mqtt_udp.rs`（约 +260 行，仍在变动）、`gateway/mod.rs`、`examples/mqtt_udp_demo.rs`，方向与 §6.2 一致（以下为分析时的快照，定稿请以最终 diff 为准）：

- 新增 `MqttUdpConfig.tts_start_settle_ms`，默认 `default_tts_start_settle_ms() = 100`；网关与 demo 显式传 100；
- 新增 cancel-aware `wait_tts_start_settle`，在 `drive_playback_inner`（`publish_tts(Start)`、`*tts_started = true` 之后，句子/预缓冲之前）与 `say_raw_frames`（start 之后、首帧之前）各调用一次；
- 新增多个 settle 测试（快照中可见 `tts_start_settle_delays_first_downlink_frame`、`abort_during_tts_start_settle_skips_audio_and_recovers`、`wait_tts_start_settle_is_cancellable` 等）。

核验意见（不阻塞）：
1. 方向、插入点、可取消性都正确；`settle=0` 时保持旧行为。
2. `paced_downlink()` 等时序测试经 `Env::boot` 使用默认 100ms settle，会让每轮测试固定多等 100ms（不影响断言，因为所有帧整体后移）。建议非 settle 测试显式用 `boot_with_settle(..., 0)`，保持测试快且只测发送节奏本身。
3. 建议把 settle 值做成配置项（供 §6.4 的不改码 A/B），并在统计行加 `settle_ms`。
4. WIP 在分析过程中仍在变化（本文件记录的是快照），定稿后请以最终 diff 为准。

---

## 附：复现/复核命令

```bash
# 1) 发送侧统计（86 轮；delta = elapsed-expected ≈ -300 证明 5 帧连发、无停顿）
grep "下行播放统计" /tmp/haimen-mini/clean.log

# 2) 收尾超时
grep -c "ContinuityPump 收尾超时" /tmp/haimen-mini/clean.log   # 26

# 3) 发送失败 / MQTT 超时（均为 0）
grep -c "下行音频发送失败\|下发 MQTT 超时\|下发 MQTT 失败" /tmp/haimen-mini/clean.log

# 4) 设备网络（本机 192.168.110.35，与设备同网段；0% 丢包，RTT 3ms ↔ 500-640ms 交替）
ping -c 20 -i 0.5 192.168.110.59
ping -c 10 -i 1   192.168.50.33      # 对照：到 mac mini 4ms

# 5) 固件/代码行号复核（相对各仓库根）
sed -n '567,571p;637,641p;1248,1253p;270,276p' /Users/kevin/Documents/works/xiaozhi-esp32/main/application.cc
sed -n '208,231p;482,494p' crates/haimen-xiaozhi/src/mqtt_broker.rs
sed -n '1694,1731p'        crates/haimen-xiaozhi/src/mqtt_udp.rs   # 注意：WIP 合入后行号会变
```

---

## 8. 修复定稿（Lead 补充，2026-10-21）

> §7 记录的是分析期间的工作区快照；以本节为准。

### 8.1 落地内容（分支 `fix/voice-frame-drop`）

| 位置 | 改动 |
| --- | --- |
| `crates/haimen-xiaozhi/src/mqtt_udp.rs` | `MqttUdpConfig.tts_start_settle_ms`（默认 `crate::types::DEFAULT_TTS_START_SETTLE_MS`，复核定稿为 150） |
| 同上 | `wait_tts_start_settle(settle_ms, &mut watch::Receiver<bool>)`：`select!` 等 sleep 或 cancel；0 时立即返回 true；只收毫秒数，便于直接单测 |
| 同上 | `drive_playback_inner`：`publish_tts(Start)` 之后、`sentence_start`/预缓冲之前调用；取消即 `return false`；settle 返回后补一次 cancel 复查（避免 timer/cancel 同时就绪时补发 stale sentence_start） |
| 同上 | `say_raw_frames`：start 之后、首帧之前 sleep settle（主动播报路径无 cancel watch，沿用既有设计） |
| 同上 | `下行播放统计` 增加 `settle_ms` 字段，真机 A/B 时可直接确认配置生效 |
| `crates/haimen-xiaozhi/src/types.rs` | `[xiaozhi.udp] tts_start_settle_ms`（serde 默认 150，0 = 旧行为） |
| `src/gateway/mod.rs` | `build_mqtt_udp_config` 用 `x.udp.tts_start_settle_ms`，不再硬编码 |
| `src/web/api/xiaozhi_settings.rs` | GET/PUT 暴露 `udp.tts_start_settle_ms`；上限 1000（推荐 0–300） |

### 8.2 测试

- `tts_start_settle_delays_first_downlink_frame`：settle=150 时断言「设备收到 tts start → 首帧 UDP」≥120ms（变异测试：删掉 settle 调用后失败，实测只隔 5.7µs）。
- `abort_during_tts_start_settle_skips_audio_and_recovers`：settle 期间 barge-in 不发音频，下一轮可恢复。
- `wait_tts_start_settle_is_cancellable`：直接覆盖「已取消 / 等待中取消 / 正常等完 / 0 立即返回」。
- `tts_start_settle_defaults_to_150ms`：锁默认值，防止漂移。
- `firmware_model_drops_prebuffer_without_settle_but_keeps_it_with_settle`：用固件行为模型（start 后 50ms 内到达的帧判丢）验证 settle=0 时整批 3/3 被丢、settle=200 时 0 丢。
- `cargo test -p haimen-xiaozhi`：148 passed / 0 failed。

### 8.3 A/B 操作（真机）

```toml
[xiaozhi.udp]
tts_start_settle_ms = 100   # 0 = 复现旧行为；100 = 修复
```

每档各 10 轮；`grep 下行播放统计` 确认 `settle_ms`；对比「句中卡顿次数」。
若 100ms 后仍偶发：先按 §6.5 排除网络/固件，再单独把 prebuffer 5→8 做第二组
A/B（不要与 settle 同时改）。

### 8.4 未纳入本分支的后续项（按 §6.6）

`PumpGuard` 超时假 abort（P1）、broker 「已写 socket」屏障（P2）、WS 路径 settle
（P2）、统计行 `tts_start→首帧实际间隔`（P3）。

---

## 9. 第二轮复审（2026-10-21，独立固件审计 + 对抗性 review）

> 触发：用户反馈「这个 bug 修复了好几次了，确认这次修改有效」。
> 方法：独立子代理逐行核对本地固件源码（`xiaozhi-esp32` @ 1718809 = v2.5.2+16）、
> 复核线上日志、对 commit 做有效性/失效条件评审。

### 9.1 结论

| 问题 | 结论 |
| --- | --- |
| 代码改动是否正确且在 MQTT 主路径生效 | **是**（审查确认 strategy/请求路径/配置/取消语义/回退均正确；无回归） |
| 固件丢帧机制是否真实 | **是，高置信**；且确认有**两个**窗口：① 非 Speaking 静默丢；② 状态已置位但主任务还没跑完 Speaking handler 时，`ResetDecoder()` 会清空解码队列（`audio_service.cc:802-817`） |
| 丢帧量是否必然为「整批 5 帧」 | **不是**。UDP 接收任务优先级低于 MQTT/主任务（`CreateUdp(2)`，推断），start 先被处理时部分帧可能在 ResetDecoder 之后入队。丢 0–5 帧都可能，呈双峰。 |
| 100ms 是否够 | 典型足够（2–10ms，尾 30–60ms），但为双峰失败留余量，**默认上调到 150ms** |
| 参考实现「tts start 后 sleep 20–100ms」 | **说法不成立**（见 §9.2），旧文档已修正 |
| 是否一定解决用户症状 | **不能仅凭代码确认**：设备侧无观测、提交后无真机数据。机制置信度高，但「是症状主因」仍是推断 |

**为什么这次不同**：前几次修复分别针对「尾帧」（T1）、「服务端供给断档」（ContinuityPump）、
「发送节奏累积漂移」（WS）。这次针对的是 **start 边界**：ContinuityPump 让 `tts start`
与首帧之间的天然间隔从「TTS 合成时延（数百 ms）」变成 ~0，把固件异步切状态的窗口暴露了出来。
这是唯一能同时解释「为什么 pump 之后更明显」「为什么尾帧修复无效」的机制。

### 9.2 更正：参考实现并没有显式等待

`specs/对比-xiaozhi-esp32-server.md:355` 的「它 ✅ 20–100ms」是报告作者的**建议措辞**，
不是参考代码事实。xiaozhi-esp32-server 的 `sendAudioHandle.py` 首帧是立即发的；它之所以
不暴露该竞态，是因为它在 LLM/TTS 产出音频**之前**就发了 `tts start`，start → 首帧之间
天然有数百 ms。本项目的 ContinuityPump 为了消除服务端断档，把这段天然间隔填成了静音。

### 9.3 本轮加固

1. 默认 settle **100ms → 150ms**（`types::DEFAULT_TTS_START_SETTLE_MS`）；失败是双峰的，
   宁可在 start 边界多等 50ms（流式路径只推迟静音，不影响首字）。
2. `build_mqtt_udp_config` 对直接手改 `settings.toml` 的路径补 `clamp(0..=1000)`
   （Web API 已校验，但 TOML 直写绕过）。
3. 新增**固件行为模型测试**：模拟「start 后 50ms 内到达的帧被丢」，
   断言 settle=0 时整批 3/3 被丢、settle=200 时 0 丢（旧行为可复现、修复可证）。
4. 复审仍保留的缺口：WS 路径未加 settle（当前 `protocol=mqtt` 不受影响）；settle 从
   `publish_to` **入队返回**起算而非 socket 写完（见 §9.4）。

### 9.4 失效条件（按概率）

1. **中**：设备真实切换时延 > settle（显示锁/PSRAM/CPU 抢占；显示锁尾部可达 30–60ms，
   已用 150ms 覆盖 2.5–5 倍余量）。
2. **中**：broker 写队列/ TCP 重传吃掉 settle —— settle 从「入队」起算，start 的 TCP
   包若重传（RTO 200ms+），预缓冲仍会抢跑。彻底修法：broker 加「已写 socket」屏障后再
   起计时（§6.6 P2，本次未做）。
3. **低–中**：播放期 WiFi 若仍在省电（PERFORMANCE 是否等于 `WIFI_PS_NONE` 未核实），
   300ms lead 可能被 DTIM/唤醒抖动击穿 —— settle 无线。
4. **低**：稳态网络丢包/乱序；解码队列 20 包在 >1.2s 停摆时溢出；`tts start` 落在
   非法状态（Connecting 等）。

### 9.5 决定性验证（不需要烧固件，且不依赖主观听感）

现有部署（**旧版、settle=0**）即可先验证根因，不必先部署修复：

1. 用控制面测试通道 `__say_audio` 向设备注入一段 **前 300ms 为 440Hz 提示音、之后静音**
   的 20 帧 Opus（`scripts/xiaozhi_sim/send_ctl.py` → `haimen/ctl/<client_id>`）。
2. 设备需先唤醒（说「你好小智」，UDP 会话存在）。
3. 若旧版听到提示音 → 预缓冲没被丢，**根因假设错误**，不要指望 settle 修复；
   若旧版**听不到**提示音 → 预缓冲确实被固件丢弃，根因确认。
4. 部署修复后重复：`settle=0` 听不到、`settle=150` 能听到完整提示音 → 修复有效。
   同时 `下行播放统计` 的 `settle_ms` 字段可确认配置生效。

补充取证（可选，更硬）：设备 UART/USB 串口（本机已看到 Espressif USB CDC 设备）
可抓固件日志——状态切换（`State: listening -> speaking`）、`wrong sequence` /
`duplicate/old audio sequence` 警告；或在固件 `OnIncomingAudio` 的 else 分支加一行
丢弃计数（需自编译固件），这是唯一能定量统计每轮丢帧数的办法。

---

## 10. 第三轮：真机探针定位到「服务端内容供给」，最终修复（2026-10-21 18:07 验证通过）

### 10.1 探针实验（决定性排除法）

用 `__say_audio` 控制通道（与正常回复完全相同的 `tts start` → 预缓冲 → UDP 60ms
路径），分别播放：

| 探针 | 内容 | 用户听感 |
| --- | --- | --- |
| 6s 连续 440Hz 提示音 | 无任何断档 | **零卡顿** |
| 8s 真实回复的 TTS 录音（未经泵） | 含真实语调/停顿 | **整段很顺** |
| 实时语音回复（经 ContinuityPump） | 同上内容 | 开头 2–3 秒断断续续 |

→ 排除设备接收/解码/播放、网络、TTS 内容本身。差异只剩 **ContinuityPump 的实时
供数处理**。

### 10.2 根因

ContinuityPump 为了「窗口内 0 断档」，只要手上暂时没有新 PCM 就补一帧静音。
但它**不知道下发给传输层的管道里还排着多少内容**：供数抖动时补的静音帧会排在
稍后到达的真实内容**前面**被播放出来——短抖动也会变成可听断档。开头的 TTS
流式供数最不稳定，所以症状集中在开头；之后 TTS 跟上，管道里常有内容垫着，就顺了。

600ms 一次性预缓冲无效（用户实测「老样子」）恰好验证了这一点：预缓冲只是把内容
堆进管道，并不阻止泵继续插静音。

### 10.3 修复（`src/xiaozhi_asr_llm_tts.rs`）

1. **开头内容预缓冲**：首段真实 PCM 先攒 `PUMP_CONTENT_PREBUFFER_MS = 400ms` 再开播
   （最多等 `900ms`，供数太慢不会无限拖）；期间继续发原有 lead 静音。
2. **插静音前看管道余量**：`frame_tx` 里还有 ≥ `PUMP_SILENCE_SKIP_LEAD_FRAMES = 5`
   帧（300ms）未播时，本轮 tick 不补静音，让队列先吸收抖动；余量见底才补。

### 10.4 真机结果

- 部署后 3 轮响应日志中 **`插入静音（内容中途）` 为 0 条**；
- 每轮有 1 条 `内容预缓冲释放`（`buffered_ms≈775`，`waited_ms≈12`）；
- 用户确认：**全程都不卡了**，首字延迟无明显变化。

### 10.5 后续回归观测

- `ContinuityPump 内容预缓冲释放`：每轮一条，确认预缓冲生效。
- `ContinuityPump 插入静音（内容中途）`：只应在真的供数断档时出现，带
  `offset_ms` / `dur_ms` / `silence_frames`；若某轮出现，说明该轮供数断档超过
  了队列余量（可据此再调大预缓冲或队列阈值）。
- 若未来出现「说完了但设备还停在 Speaking 很久」：那是上游 LLM 流不结束
  （agent 日志 latency 异常长）被泵的静音填充放大，与本修复无关，用户决定暂不处理。
