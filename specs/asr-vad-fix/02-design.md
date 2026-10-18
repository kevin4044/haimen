# 02 - 设计：修复 ASR 空结果 + 补齐 VAD 诊断日志

## 1. 背景

上一个提交（`c20eaad`）为了降低延迟，把豆包服务端 VAD 参数从 1500ms 改成了 400ms。
结果引入了一个**功能性 bug**：

```
用户说话 → 豆包服务端 400ms 静音就判停 → 此时可能还没识别出任何文字
        → 返回空结果 + 错误码 45000081「等包超时」
        → haimen 判定"ASR 识别结果为空"
        → 回退到批处理模式（延迟反而更高）
        → 每句话都这样，所有延迟优化全部失效
```

日志证据（容器时间 UTC）：

```
11:37:17  ERROR 流式 ASR 任务异常退出: ASR 识别结果为空
11:39:36  WARN  🎤 [ASR 错误] ASR error: code=45000081, message=等包超时
11:39:36  ERROR 流式 ASR 任务异常退出: ASR 识别结果为空
11:39:40  ERROR 流式 ASR 任务异常退出: ASR 识别结果为空
11:43:38  WARN  🎤 [ASR 错误] ASR error: code=45000081, message=等包超时
...
```

这些报错的时间点（11:33 UTC = 19:33 本地）与容器重建时间完全吻合，可确认是本次改动引入。

## 2. 根因分析（为什么 400ms 是错的）

对比社区复刻版 `xinnan-tech/xiaozhi-esp32-server` 后，发现两边**架构不同**：

```
小智复刻版的架构：
  豆包服务端 VAD:  end_window_size = 200ms   ← 只是兜底
  本地 Silero VAD: 1000ms（神经网络）        ← 真正的判停主力
  → 本地 VAD 准确、不受噪音影响，所以服务端 VAD 可以很短

我们的架构：
  豆包服务端 VAD:  ← 唯一的"权威判停"手段
  本地能量 VAD:    ← 只在【已识别出非空文字后】才允许触发
  → 本地 VAD 是"加速器"，不是"替代品"
  → 所以服务端 VAD 必须保留足够长的窗口
```

**本地能量 VAD 的触发条件**（`src/xiaozhi_asr_llm_tts.rs` 约 1600 行）：

```rust
if state.silence_count >= MAX_SILENCE_FRAMES
    && self.asr_received_text.load(Ordering::Acquire)   // ← 必须已经识别出文字
{
    // 才允许关闭 ASR 流
}
```

**结论**：`end_window_size` 是**兜底**，不能短；真正的延迟收益应该来自
**本地 VAD 的 `MAX_SILENCE_FRAMES`**（已从 40 降到 20，即 2400ms → 1200ms，这个是对的，保留）。

## 3. 第二个问题：诊断日志被过滤，无法定位

容器近 40 分钟日志里 **INFO 条数为 0**，即使配置是 `debug = true` / `log_level = "debug"`。

导致我们看不到这些关键信息：

```rust
tracing::info!("流式 ASR: 第一帧音频到达，惰性初始化管道");
tracing::info!("流式 ASR: 管道已就绪（惰性初始化）");
tracing::info!("流式 ASR: 已接收 {} 帧", ...);
tracing::info!("无语音超时: ...");
tracing::info!("🎤 [ASR 完成] 共收到 {} 个结果块");
```

**结果是只能靠猜**。需要把这些日志升到 `warn` 级（`warn` 确实能打印出来，现有 `等包超时` 的 WARN 就看到了）。

同时补充**最关键的实测数据**：每帧的 RMS 能量值。有了它才能判断
`SILENCE_RMS_THRESHOLD = 2000` 对当前麦克风是否过高。

## 4. 目标

1. **修复 ASR 空结果 bug**：`end_window_size` 恢复 1500ms
2. **`force_to_speech_time` 从 200 调到 500**（200 太激进）
3. **让 ASR 诊断日志可见**：关键 INFO 升到 WARN
4. **输出 RMS 实测值**：为后续调 `SILENCE_RMS_THRESHOLD` 提供数据

## 5. 非目标

- 不改 `SILENCE_RMS_THRESHOLD`（保持 2000，等拿到 RMS 数据再定）
- 不改 `MAX_SILENCE_FRAMES`（保持 20）
- 不改 `PREBUFFER_COUNT`（保持 5）
- 不改 TTS / 播放 / 音频卡顿相关代码
- 不引入 Silero VAD

## 6. 具体改动

### 改动 1：`end_window_size` 默认值 400 → 1500

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**位置**：`create_streaming_asr_provider` 的 `doubao` 分支（约 740-755 行）

```rust
// 当前
end_window_size: Some(
    cfg.get_credential("end_window_size")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(400),          // ← 改这个
),

// 改成
// 服务端 VAD 判停窗口。这是我们唯一的"权威判停"手段，
// 不能短：中文口语停顿 400ms 很常见，窗口太短会在用户话没说完时判停，
// 导致豆包返回空结果 + code=45000081「等包超时」。
// 真正的延迟优化来自本地能量 VAD（MAX_SILENCE_FRAMES），见下方说明。
end_window_size: Some(
    cfg.get_credential("end_window_size")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1500),
),
```

### 改动 2：`force_to_speech_time` 默认值 200 → 500

**文件**：`src/xiaozhi_asr_llm_tts.rs`，同一个代码块

```rust
// 当前
force_to_speech_time: Some(
    cfg.get_credential("force_to_speech_time")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(200),          // ← 改这个
),

// 改成
// 至少累积这么多音频后才允许判停。200ms 太激进（大约只有 3 帧），
// 环境噪音抖动就可能触发误判；500ms 是折中值。
force_to_speech_time: Some(
    cfg.get_credential("force_to_speech_time")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(500),
),
```

### 改动 3：关键诊断日志 info → warn

**文件**：`src/xiaozhi_asr_llm_tts.rs`

以下 5 处 `tracing::info!` 改为 `tracing::warn!`（**只改这 5 处，不要动其他 info**）：

| 行号（约） | 内容 |
|---|---|
| 1296 | `"流式 ASR: 管道已就绪（惰性初始化）"` |
| 1487 | `"流式 ASR: 清空管道状态（惰性初始化）"` |
| 1530 | `"流式 ASR: 第一帧音频到达，惰性初始化管道"` |
| 1590 附近 | `"无语音超时: 录音开始后 {} 帧 ({}ms) 无有效语音，触发告别并关闭连接"` |
| 1240 附近 | `"🎤 [ASR 完成] 共收到 {} 个结果块，完整文本长度: {}"` |

同时在每处加上简短注释，说明为什么用 warn：

```rust
// 用 warn 而非 info：haimen 的日志过滤会吞掉 info，
// 这行是判断 ASR 链路是否正常工作的关键证据，必须可见。
tracing::warn!("流式 ASR: 第一帧音频到达，惰性初始化管道");
```

### 改动 4：新增 RMS 实测日志（临时诊断）

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**位置**：`on_audio_frame` 里，约 1568 行的 `let rms = compute_pcm_rms(&pcm_bytes);` 之后、
VAD 判定之前

**当前代码**（约 1560-1580 行）：

```rust
// ── 本地 PCM 能量检测 ──
let rms = compute_pcm_rms(&pcm_bytes);
if rms >= SILENCE_RMS_THRESHOLD {
```

**改成**：

```rust
// ── 本地 PCM 能量检测 ──
let rms = compute_pcm_rms(&pcm_bytes);

// 临时诊断（找到合适阈值后可删除）：
// 用 warn 级绕开 info 被过滤的问题，每 50 帧（约 3 秒）打印一次实测能量，
// 用于判断 SILENCE_RMS_THRESHOLD 对当前麦克风是否过高。
if state.frame_count - state.last_log_frame >= 50 {
    state.last_log_frame = state.frame_count;
    tracing::warn!(
        "ASR 诊断: 第 {} 帧, RMS={:.0}, 阈值={:.0}, speech_detected={}, 已识别文字={}, 静音帧计数={}, 无语音帧={}",
        state.frame_count,
        rms,
        SILENCE_RMS_THRESHOLD,
        state.speech_detected,
        self.asr_received_text.load(Ordering::Acquire),
        state.silence_count,
        state.no_speech_frames,
    );
}

if rms >= SILENCE_RMS_THRESHOLD {
```

⚠️ **注意**：原来代码里可能已经有一段 `if state.frame_count - state.last_log_frame >= 50` 的日志（打印"已接收 N 帧"）。
如果有，就**合并到这一段里**，不要出现两个相同条件的 `if`。合并后保留 RMS 等字段即可。

## 7. 测试要求

- 不要新增测试（本次是参数与日志改动，无可测逻辑）
- 确保现有测试全部通过，特别是 `crates/haimen-xiaozhi/src/ws.rs` 里与预缓冲相关的那个（上个提交已同步过）

## 8. 明确不做

- 不要改 `src/xiaozhi_tts.rs`
- 不要改 `crates/haimen-openai/`
- 不要改 `crates/haimen-xiaozhi/`
- 不要动 `MAX_SILENCE_FRAMES` / `SILENCE_RMS_THRESHOLD` / `PREBUFFER_COUNT`
- 不要引入新依赖
