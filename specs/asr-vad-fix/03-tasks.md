# 03 - 任务单

> 规模：单个任务，预期改动约 30-50 行。
> 完成判定：`04-acceptance.sh` 退出码为 0。

---

### T1 — 修复 ASR 空结果（end_window_size 恢复 1500）+ 补齐 VAD 诊断日志

### 目标

上一个提交把豆包服务端 VAD 的 `end_window_size` 从 1500ms 改成了 400ms，
导致中文口语正常停顿就被判停，豆包返回空结果 + `code=45000081 等包超时`，
每句话都回退到批处理模式，延迟反而更差。

本任务：

1. `end_window_size` 恢复 **1500ms**
2. `force_to_speech_time` 从 200 调到 **500ms**
3. 关键 ASR 诊断日志从 `info` 升到 `warn`（info 被日志过滤吞掉了）
4. 新增每 50 帧打印一次的 **RMS 实测日志**（为后续调阈值提供数据）

### 先读这三处

1. `src/xiaozhi_asr_llm_tts.rs` 约 730-760 行 —— `create_streaming_asr_provider` 的 doubao 分支，
   看 `end_window_size` / `force_to_speech_time` 当前是怎么写的
2. `src/xiaozhi_asr_llm_tts.rs` 约 1550-1620 行 —— `on_audio_frame` 里的能量检测与 VAD 判定逻辑
3. `src/xiaozhi_asr_llm_tts.rs` 约 1280-1300 行、1470-1500 行、1520-1540 行 —— 三处初始化日志

### 要做的事

**A. 改两个默认值**

- `end_window_size` 的 `.unwrap_or(400)` → `.unwrap_or(1500)`
- `force_to_speech_time` 的 `.unwrap_or(200)` → `.unwrap_or(500)`

按 `02-design.md` 第 6 节「改动 1 / 改动 2」给出的注释替换现有注释，说明为什么不能短。

**B. 五处 `tracing::info!` 改为 `tracing::warn!`**

严格按 `02-design.md` 第 6 节「改动 3」的表格，**只改这 5 处**，
不要动文件里其他的 `tracing::info!`。

**C. 新增 RMS 诊断日志**

按 `02-design.md` 第 6 节「改动 4」，在 `compute_pcm_rms` 之后、VAD 判定之前插入。

⚠️ 如果原代码里已经存在 `if state.frame_count - state.last_log_frame >= 50 { ... }` 这个条件块，
**合并成一段**，不要保留两个相同条件的 `if`。

### 硬性约束

- **只改 `src/xiaozhi_asr_llm_tts.rs` 一个文件**
- 不要改 `MAX_SILENCE_FRAMES`（保持 20）
- 不要改 `SILENCE_RMS_THRESHOLD`（保持 2000.0）
- 不要改 `PREBUFFER_COUNT`（保持 5）
- 不要改 `src/xiaozhi_tts.rs`、`crates/haimen-openai/`、`crates/haimen-xiaozhi/`
- 不要引入新依赖
- 库代码里不要 `unwrap()` / `expect()` / `panic!()`
- 新增/修改的注释用中文

### 改动白名单

```
src/xiaozhi_asr_llm_tts.rs
specs/asr-vad-fix/
```

超出任何一条，验收脚本会判 FAIL。

### 自检命令

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```
