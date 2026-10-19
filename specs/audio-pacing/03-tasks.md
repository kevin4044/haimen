# 03 - 任务单（T1 尾帧等待 / T2 埋点 / T3 可配节拍）

> 实现依据：`02-design.md`
> 每个任务完成后必须 `./scripts/ci.sh` 全绿。
> 验收：`bash specs/audio-pacing/04-acceptance.sh --task T1`

---

### T1 — `tts stop` 前等尾帧播完

### 目标

在 `crates/haimen-xiaozhi/src/mqtt_udp.rs` 的 `run_playback` 里，
`drive_playback` 返回之后、`publish_tts(Stop)` 之前，插入"让设备把最后几帧播完"的等待。

### 当前代码（改前）

```rust
async fn run_playback(shared: Arc<Shared>, job: PlaybackJob) {
    let mut tts_started = false;
    drive_playback(&shared, &job, &mut tts_started).await;
    if tts_started && !cancelled(&job.cancel, &shared) {
        // 设备要求 start → 句子/音频 → stop。stop 放在所有帧之后。
        let _ = publish_tts(&shared, &job.client_id, TtsState::Stop, None).await;
        finish_ready(&shared, &job).await;
    }
}
```

### 要做的事

1. 新增一个可取消的等待辅助（放本文件内，靠近 `wait_until_send_slot`）：

```rust
/// 发完最后一帧后，等设备把缓冲里的尾帧播完再发 `tts stop`。
///
/// 为什么需要：`drive_playback` 只保证帧「发出去」，设备解码队列里还压着
/// `prebuffer_count` 帧（默认 5 ≈ 300ms）。紧接着发 stop 会让设备切出
/// Speaking 状态、回收音频管道，尾帧被丢掉 —— 表现为句子末尾掉帧。
///
/// 公式照抄参考实现（sendAudioHandle.py:58-81）：
///   (prebuffer_count + 2) × frame_duration
/// 多出的 2 帧覆盖 UDP 抖动与固件调度延迟。
///
/// 取消信号触发时必须立刻返回，不能白等。
async fn wait_tail_playback(
    shared: &Shared,
    cancel: &mut watch::Receiver<bool>,
) -> bool
```

行为：
- 时长 = `(cfg.prebuffer_count + 2) * frame_step_ms(&cfg)` 毫秒
- 用 `tokio::select!` 同时等 `sleep(时长)` 和 `cancel.changed()`
- `cancel` 已触发或提前触发时返回 `false`；正常等完返回 `true`

2. 在 `run_playback` 里用它：

```rust
if tts_started && !cancelled(&job.cancel, &shared) {
    // 先让设备把尾帧播完，再通知它收口
    let mut cancel = job.cancel.clone();
    let _ = wait_tail_playback(&shared, &mut cancel).await;
    let _ = publish_tts(&shared, &job.client_id, TtsState::Stop, None).await;
    finish_ready(&shared, &job).await;
}
```

注意：等待结束后**无论是否被取消都要照常发 stop**（设备侧不能停在 Speaking）。
`wait_tail_playback` 的返回值可以只用于日志，不要用它决定是否发 stop。

3. 配置：`MqttUdpConfig` 新增字段

```rust
/// 发 `tts stop` 前额外等待的帧数（乘 frame_duration）。
/// 默认 2（= 参考实现的 `prebuffer + 2` 里的那个 2）。
#[serde(default = "default_tail_extra_frames")]
pub tail_extra_frames: u32,
```

默认值 2。所有构造 `MqttUdpConfig` 的地方（`src/gateway/mod.rs`、
`examples/mqtt_udp_demo.rs`、测试里的 `Env::boot`）都要补这个字段。

### 约束

- 不要改 `snap_send_clock` / `wait_until_send_slot`（它们已经是对的）
- 不要改预缓冲数量
- 不要动 `ws.rs`
- 等待必须可取消（用户插话时不能卡 420ms）
- 所有新增注释用中文，解释「为什么」而不是「做什么」

### 完成标准

- `./scripts/ci.sh` 全绿
- `bash specs/audio-pacing/04-acceptance.sh --task T1` 通过

---

### T2 — 埋点：让「掉帧」可观测

### 目标

在 `drive_playback` 里记录本轮下发的帧数与时间，结束时用 `warn!` 输出。
用途：T1 上线前后对比，客观判断尾帧是否被砍。

### 要做的事

1. `drive_playback` 内维护计数（帧数、首帧发送时刻、末帧发送时刻）
2. 函数结束前输出一条 `warn!`，字段至少包含：

```
client_id, frames（下发帧数）,
elapsed_ms（首帧到末帧的墙钟耗时）,
expected_ms（frames × frame_step_ms，理论应播时长）,
tail_wait_ms（T1 实际等待了多久）
```

3. 如果 `frames == 0` 就不要打（避免噪音）

### 约束

- 只用 `tracing::warn!`（本项目 INFO 会被过滤）
- 不要引入新的依赖
- 不要在热路径里做字符串拼接（只在结束时打一次）

### 完成标准

- `./scripts/ci.sh` 全绿
- `bash specs/audio-pacing/04-acceptance.sh --task T2` 通过
- 真机跑一轮对话，日志里能看到这条统计

---

### T3 — `frame_step_ms` 可配（低优先）

### 目标

给下行帧间隔加一个可配置覆盖，用于「设备缓冲小 / 网络抖动大」时降速。

### 要做的事

1. `MqttUdpConfig` 新增：

```rust
/// 覆盖下行帧间隔（毫秒）。`None` = 用 `frame_duration_ms`（默认 60）。
/// 设为 65~75 可给设备缓冲留余量，代价是整体播放略慢于实时。
#[serde(default)]
pub send_delay_ms: Option<u16>,
```

2. `frame_step_ms()` 优先返回它：

```rust
fn frame_step_ms(cfg: &MqttUdpConfig) -> u32 {
    cfg.send_delay_ms
        .map(u32::from)
        .filter(|ms| *ms > 0)
        .unwrap_or(cfg.frame_duration_ms.max(1))
}
```

3. 所有 `MqttUdpConfig` 构造点补字段（默认 `None`）

### 约束

- 默认 `None` 时行为必须与现在**完全一致**
- 不要改预缓冲逻辑

### 完成标准

- `./scripts/ci.sh` 全绿
- `bash specs/audio-pacing/04-acceptance.sh --task T3` 通过
