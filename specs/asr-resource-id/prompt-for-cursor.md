# 任务：修复火山 ASR 因 resource_id 过期导致的 403，并让 ASR 任务错误可见

## 背景

haimen 作为小智硬件的自建后端，语音链路是：
设备 → WebSocket → ASR(火山豆包) → LLM(Hermes gateway) → TTS(火山豆包) → 设备

**当前故障**：设备连上后一直卡在"聆听中"，日志里每 60ms 刷一条：

```
WARN haimen_xiaozhi::ws: Streaming ASR on_audio_frame 失败，将继续缓冲音频
     device_id=84:c7:bb:59:93:64
     error=ASR 管道已关闭
```

## 根因（已实测确认）

火山引擎**已下线 1.0 版语音识别**，旧资源号 `volc.bigasr.sauc.duration` 返回 **403 Forbidden**。

而 `univoice` crate 的 `DoubaoAsr` 在未指定 `resource_id` 时，默认值就是这个已下线的 1.0 资源号：

```rust
// univoice-0.1.6/src/asr/types.rs:17
pub const DEFAULT_RESOURCE_ID: &str = "volc.bigasr.sauc.duration";   // ← 已下线
```

haimen 创建 ASR provider 时没有传 `resource_id`，于是走了这个默认值 → 握手 403。

### 正确的资源号（火山当前控制台）

| 产品 | 资源号 |
|---|---|
| **豆包流式语音识别 2.0（小时版）** | `volc.seedasr.sauc.duration` ← 用这个 |
| 豆包流式语音识别 2.0（并发版） | `volc.seedasr.sauc.concurrent` |
| 豆包语音合成 2.0 | `seed-tts-2.0`（**haimen 的 TTS 已经默认用这个，无需改动**） |

### 实测证据

用 `volc.seedasr.sauc.duration` 单独调用 `univoice::asr::DoubaoAsr::listen_stream()`，
用一段 16kHz mono PCM（内容是"关掉书房的灯"）测试，结果：

```
--- listen_stream OK, 握手成功 ---
[chunk 5]  text: "关掉"           confidence 0.8
[chunk 12] text: "关掉书房的"      confidence 0.8
[chunk 16] text: "关掉书房的灯。"  is_final: true, confidence 1.0
--- 流结束，共 16 个 chunk ---
```

改用旧资源号 `volc.bigasr.sauc.duration` 则直接 `WebSocket error: HTTP error: 403 Forbidden`。

## 为什么错误被吞掉了（第二个问题）

`src/xiaozhi_asr_llm_tts.rs` 的 `init_asr_pipeline` 里：

```rust
let asr_handle: tokio::task::JoinHandle<Result<String, String>> = tokio::spawn(
    async move {
        let mut stream = asr
            .listen_stream(audio_stream)
            .await
            .map_err(|e| format!("流式 ASR 启动失败: {}", e))?;   // ← 在这里 return Err
        ...
    },
);
// asr_handle 存进 AsrPipelineState，但没有任何地方 await 它
// → 这个 Err 永远不会被打印 ⚠️
```

任务一退出，`audio_stream` 被 drop → `pcm_tx.send()` 开始失败 → 每帧报"ASR 管道已关闭"。
真正的 403 原因完全看不到。

---

## 要做的改动

### 改动 1：`create_streaming_asr_provider` 的 doubao 分支

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**函数**：`create_streaming_asr_provider`（约 660-700 行，`match cfg.active_provider.as_str()` 里的 `_ =>` 分支）

现在是这样：

```rust
let api_key = cfg
    .get_credential("api_key")
    .ok_or_else(|| "ASR API Key 未配置（当前提供商: doubao）".to_string())?;
Ok(Box::new(DoubaoAsr::new(DoubaoAsrOption {
    base: BaseProviderOption {
        language: Some("zh-CN".into()),
        ..Default::default()
    },
    api_key: Some(api_key),
    mode: DoubaoAsrMode::Streaming,
    ..Default::default()
})))
```

改成（**照抄 `src/tts_factory.rs` 里 `create_tts_provider` 的做法**，保持风格一致）：

```rust
let api_key = cfg
    .get_credential("api_key")
    .ok_or_else(|| "ASR API Key 未配置（当前提供商: doubao）".to_string())?;
// 火山已下线 1.0 语音识别（volc.bigasr.sauc.duration 返回 403），
// 默认改用 2.0 小时版；允许通过配置覆盖（并发版为 volc.seedasr.sauc.concurrent）。
let resource_id = cfg
    .get_credential("resource_id")
    .unwrap_or_else(|| "volc.seedasr.sauc.duration".to_string());
Ok(Box::new(DoubaoAsr::new(DoubaoAsrOption {
    base: BaseProviderOption {
        language: Some("zh-CN".into()),
        ..Default::default()
    },
    api_key: Some(api_key),
    resource_id: Some(resource_id),
    mode: DoubaoAsrMode::Streaming,
    ..Default::default()
})))
```

### 改动 2：`create_asr_provider` 的 doubao 分支

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**函数**：`create_asr_provider`（约 583-666 行，同样的 `_ =>` 分支）

做**完全相同**的改动（加 `resource_id` 读取 + 传给 `DoubaoAsrOption`）。

> ⚠️ 两个函数都要改：`create_asr_provider` 是给非流式用的，`create_streaming_asr_provider` 是给流式用的，两者都在这个文件里，doubao 分支各有一份 `DoubaoAsrOption` 构造。

### 改动 3：让 ASR 任务的错误可见

**文件**：`src/xiaozhi_asr_llm_tts.rs`
**函数**：`init_asr_pipeline`（约 1080-1280 行）

把 `tokio::spawn` 的闭包改成先在内部捕获结果、打日志、再返回。

现在的结构大致是：

```rust
let asr_handle = tokio::spawn(async move {
    let mut stream = asr.listen_stream(audio_stream).await.map_err(...)?;
    // ... 大量逻辑 ...
    if full_text.is_empty() { Err("...") } else { Ok(full_text) }
});
```

改成（保持原有逻辑不变，只包一层错误日志）：

```rust
let asr_handle = tokio::spawn(async move {
    let result: Result<String, String> = async {
        let mut stream = asr.listen_stream(audio_stream).await.map_err(...)?;
        // ... 原有逻辑原样放在这里，最后返回 Ok(full_text) / Err(...) ...
        Ok(full_text)
    }
    .await;

    // 任务异常退出时把原因打出来：调用方从不 await 这个 handle，
    // 不打日志的话错误（如 403、鉴权失败、网络错误）会彻底静默。
    if let Err(err) = &result {
        tracing::error!("流式 ASR 任务异常退出: {}", err);
    }
    result
});
```

**注意**：里面的 `?` 现在作用于内层 `async {}`，外层 `.await` 拿到 `Result` 之后再判断并记录日志，最后把 `result` 返回，`JoinHandle<Result<String,String>>` 的类型保持不变。

### 改动 4：配置

**不需要改 `src/config/settings.rs`** —— `AsrConfig.providers` 是 `HashMap<String, HashMap<String, String>>`，字段自由。

配置文件（`~/.haimen/settings.toml`）里加一行即可：

```toml
[asr.providers.doubao]
api_key = "<你的火山 API Key>"
resource_id = "volc.seedasr.sauc.duration"
```

**这一步是给用户配置的示例，不需要写进代码仓库**；代码只需保证 `resource_id` 缺省时 fallback 到 `volc.seedasr.sauc.duration`。

---

## 硬性约束

- **不要改 `crates/` 下任何 crate**，特别是不要动 `crates/haimen-openai/`（那里有另一处独立改动在进行中，避免冲突）
- **不要改 `src/tts_factory.rs`** —— TTS 已经正确（默认 `seed-tts-2.0`，且已支持配置覆盖）
- **不要改其他 ASR provider**（qwen / glm / mimo / xfyun / xiaomi）的行为
- **不要引入新依赖**
- 新增代码要有中文注释，解释为什么默认值是 `volc.seedasr.sauc.duration`
- 库代码里不要 `unwrap()` / `expect()` / `panic!()`（测试代码除外）

## 自检

改完依次跑，全绿再交付：

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```

## 验收标准

1. `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test` 全绿
2. 代码里搜 `DoubaoAsrOption` 应该有 **两处** 都设了 `resource_id`
3. `resource_id` 缺省时 fallback 是 `volc.seedasr.sauc.duration`（不是旧值）
4. `init_asr_pipeline` 的 spawn 里在 `Err` 分支有 `tracing::error!`
