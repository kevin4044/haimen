# 任务：降低语音链路端到端延迟（对齐小智云端的判停策略）

## 背景

haimen 作为小智硬件的自建后端，语音链路是：
设备 → WebSocket → ASR(火山豆包) → LLM(Hermes gateway) → TTS(火山豆包) → 设备

**当前问题**：用户说完话到听到回复，明显比小智云慢。

**根因（已通过对比社区复刻版 [xinnan-tech/xiaozhi-esp32-server](https://github.com/xinnan-tech/xiaozhi-esp32-server) 确认）**：
我们的判停（VAD）参数过于保守，比小智云端长了 1.3 秒以上。

### 实测对比数据

| 参数 | 小智复刻版 | 我们当前 | 差距 |
|---|---|---|---|
| **豆包 `end_window_size`** | **200 ms** | **1500 ms** | **−1300 ms** |
| **`force_to_speech_time`** | 未设置（默认） | **1000 ms** | 短指令多等 |
| **本地静音帧数 `MAX_SILENCE_FRAMES`** | Silero 1000 ms | **40 帧 = 2400 ms** | −1400 ms |
| **预缓冲 `PREBUFFER_COUNT`** | **5 帧 = 300 ms** | **10 帧 = 600 ms** | **−300 ms** |

实际判停取"谁先触发"：
```
我们: min(豆包 1500, 本地 2400) = 1500 ms
小智: min(豆包 200,  Silero 1000) = 200 ms
```

### 本次不做的事

- **不动音频卡顿问题**（用户明确说先不管）
- **不引入 Silero 神经 VAD**（那是下一阶段，见文末）
- **不改架构**

本次只调参数，**目标是把判停从 1500ms 降到 400–500ms，预缓冲从 600ms 降到 300ms**。

---

## 改动 1：豆包 VAD 参数改为可配置，默认值大幅缩短

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**位置**：`create_streaming_asr_provider` 函数的 `doubao` 分支（约 730-745 行）

### 当前代码

```rust
api_key: Some(api_key),
mode: DoubaoAsrMode::Streaming,
sample_rate: 16000,
bits: 16,
channel: 1,
// VAD 端点检测：1500ms 静音强制判停（与本地能量 VAD 的 2.4s 配合，
// 给中文口语停顿留出余地，避免说话中途被截断）
end_window_size: Some(1500),
// 至少 1s 音频后才允许判停（避免极短音频误判）
force_to_speech_time: Some(1000),
..Default::default()
```

### 改成

```rust
api_key: Some(api_key),
mode: DoubaoAsrMode::Streaming,
sample_rate: 16000,
bits: 16,
channel: 1,
// 服务端 VAD 判停窗口：静音超过这么久就判定用户说完了。
// 云端复刻版用 200ms，这里默认 400ms（更保守），可通过配置覆盖：
//   [asr.providers.doubao]
//   end_window_size = "400"
// 注意：这是【服务端】判停；本地能量 VAD（MAX_SILENCE_FRAMES）是兜底，
// 两者取先触发者。
end_window_size: Some(cfg.get_credential_parsed("end_window_size").unwrap_or(400)),
// 至少累积这么多音频后才允许判停，避免"嗯"之类极短音频误判。
// 原值 1000ms 对"开灯"这类短指令是浪费，降到 200ms。
force_to_speech_time: Some(cfg.get_credential_parsed("force_to_speech_time").unwrap_or(200)),
..Default::default()
```

**说明**：需要给 `AsrConfig`（或 `TtsConfig` 同名方法所在的 trait/impl）加一个辅助方法：

```rust
/// 从配置读取数值型凭证字段，缺失或解析失败时返回 None
pub fn get_credential_parsed<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
    self.get_credential(key).and_then(|v| v.trim().parse::<T>().ok())
}
```

如果你不想加 trait/方法，也可以就地写：
```rust
let end_window_size: u32 = cfg
    .get_credential("end_window_size")
    .and_then(|v| v.trim().parse().ok())
    .unwrap_or(400);
let force_to_speech_time: u32 = cfg
    .get_credential("force_to_speech_time")
    .and_then(|v| v.trim().parse().ok())
    .unwrap_or(200);
```
**优先选这种就地写法**，改动面更小。

⚠️ 注意：`create_asr_provider`（非流式）的 doubao 分支**不需要改** —— 它不走 VAD。

---

## 改动 2：本地能量 VAD 兜底缩短

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**位置**：约 553-557 行的常量定义

### 当前

```rust
/// 本地能量检测：RMS 低于此值视为静音
const SILENCE_RMS_THRESHOLD: f64 = 1500.0;
/// 本地能量检测：连续静音帧数（60ms/帧），>= MAX_SILENCE_FRAMES 时触发 VAD
const MAX_SILENCE_FRAMES: u64 = 40;
```

### 改成

```rust
/// 本地能量检测：RMS 低于此值视为静音。
/// 1500 在空调/风扇噪音下容易误判"还在说话"导致空等，提到 2000。
const SILENCE_RMS_THRESHOLD: f64 = 2000.0;
/// 本地能量检测：连续静音帧数（60ms/帧），>= MAX_SILENCE_FRAMES 时触发 VAD。
/// 40 帧 = 2400ms 太长（小智云端用 Silero VAD 只要 1000ms）。
/// 降到 20 帧 = 1200ms，作为服务端 VAD 的兜底。
const MAX_SILENCE_FRAMES: u64 = 20;
```

---

## 改动 3：预缓冲帧数减半

**文件**：`crates/haimen-xiaozhi/src/ws.rs`
**位置**：约 707-717 行，`playback_frames_stream` 函数内

### 当前

```rust
// 10 帧 × 60ms = 600ms 预缓冲：实测 TTS 合成速率 ~7-8x 实时，
// 600ms 已足以覆盖网络抖动。预缓冲不宜过大——Tts::Start 后预缓冲帧
// 是一次性连发的，若超出固件 jitter buffer 容量会溢出丢帧
// （同样表现为"半个字/语速压缩"）。
const PREBUFFER_COUNT: usize = 10;
```

### 改成

```rust
// 5 帧 × 60ms = 300ms 预缓冲。
// 云端复刻版用 5 帧（300ms），实测 TTS 合成速率 ~7-8x 实时，300ms 已够。
// 预缓冲直接决定"用户说完到开始出声"的延迟，越小体验越好；
// 但过小会在合成慢时断供。从 10 帧降到 5 帧，首句快 300ms。
const PREBUFFER_COUNT: usize = 5;
```

**同时更新**：
- 约 851 行的注释 `预缓冲帧已发出，设备有约 600ms 的音频可播放。` → 改成 `约 300ms`
- 约 1534 行的注释 `Tts::Start → 10 预缓冲帧 → [断档] → 10 恢复帧` → 改成 `5 预缓冲帧`

---

## 改动 4：更新受影响的单元测试

**文件**：`crates/haimen-xiaozhi/src/ws.rs`
**位置**：约 1565 行

### 当前

```rust
assert_eq!(frame_times.len(), 20, "应收到 10 预缓冲帧 + 10 断档恢复帧");

// 恢复帧 = 索引 10..20
let first_resume = frame_times[10].1;
let last_resume = frame_times[19].1;
```

### 改成（不要硬编码，从 PREBUFFER_COUNT 推导）

```rust
// 预缓冲帧数 + 断档恢复的 10 帧
const RESUME_FRAMES: usize = 10;
assert_eq!(
    frame_times.len(),
    PREBUFFER_COUNT + RESUME_FRAMES,
    "应收到 {PREBUFFER_COUNT} 预缓冲帧 + {RESUME_FRAMES} 断档恢复帧"
);

// 恢复帧 = 索引 PREBUFFER_COUNT..PREBUFFER_COUNT+RESUME_FRAMES
let first_resume = frame_times[PREBUFFER_COUNT].1;
let last_resume = frame_times[PREBUFFER_COUNT + RESUME_FRAMES - 1].1;
```

**注意**：`PREBUFFER_COUNT` 是函数内的局部 const，如果测试在另一个函数/模块里，需要把它提到模块级（`const PREBUFFER_COUNT: usize = 5;` 放在文件顶部或 impl 外），或者测试里直接写 `5` 并加注释说明。

---

## 硬性约束

- **不要改 `src/xiaozhi_tts.rs`**（TTS 编码逻辑与本次无关）
- **不要改 `crates/haimen-openai/`**
- **不要引入新依赖**
- **不要动 `TEXT_STABLE_MS`**（当前 1500ms，本次不改 —— 它只在已识别到文本时生效，影响面需要单独评估）
- 所有新增/修改的注释用中文，说明"为什么是这个值"以及对应的云端参考值
- 库代码不要 `unwrap()` / `expect()` / `panic!()`（测试代码除外）

## 自检

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```

全绿再交付。

## 验收标准

1. `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test` 全绿
2. `end_window_size` 缺省值是 **400**（不是 1500），且支持配置覆盖
3. `force_to_speech_time` 缺省值是 **200**（不是 1000），且支持配置覆盖
4. `MAX_SILENCE_FRAMES` = **20**，`SILENCE_RMS_THRESHOLD` = **2000.0**
5. `PREBUFFER_COUNT` = **5**，相关注释与测试同步更新
6. 配置文件里加下面这段能生效（**不需要写进仓库，仅作验证说明**）：

```toml
[asr.providers.doubao]
api_key = "..."
resource_id = "volc.seedasr.sauc.duration"
end_window_size = "400"          # 可选，不写默认 400
force_to_speech_time = "200"     # 可选，不写默认 200
```

---

## 附：下一阶段（本次不做，仅记录）

如果 400ms 实测仍偏慢且不误截断，可以继续降到 200ms 对齐小智云端。
再往下（<200ms）就需要引入 **Silero 神经 VAD** 替换当前的纯能量 VAD，
否则环境噪音下会频繁误判。参考实现：
`xiaozhi-esp32-server/main/xiaozhi-server/core/providers/vad/silero.py`
