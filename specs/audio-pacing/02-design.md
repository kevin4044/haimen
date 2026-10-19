# 02 - 设计：下行音频尾帧收口（KI-004「TTS 轻微掉帧」）

## 1. 问题陈述

用户反馈：TTS 播放偶尔有"轻微掉帧"，尤其是句子尾部。

## 2. 已排除的猜测

### ❌ 队列空重锚缺失

对照参考实现（xiaozhi-esp32-server `core/utils/audioRateController.py:50-67`）
的"队列空后重新锚定播放时钟"，我们**已经有等价实现**：

```rust
// crates/haimen-xiaozhi/src/mqtt_udp.rs:1486
fn snap_send_clock(next_send_at: &mut Instant, now: Instant, frame: Duration) {
    if *next_send_at + frame < now {
        *next_send_at = now;
    }
}
```

判据是"落后超过一帧就锚到现在"，与参考实现的判据等价
（`elapsed_since_empty >= interval_ms`）。

### ❌ 发送突发打爆设备队列

发送是**逐帧** `wait_until_send_slot()` 的，只有起始预缓冲（默认 5 帧）
是连发。设备解码队列 20 包（1.2s），5 帧（300ms）远低于安全水位。
领先量由构造保证有界，不需要额外封顶。

## 3. 根因（待 A/B 验证）

```rust
// crates/haimen-xiaozhi/src/mqtt_udp.rs:1057
async fn run_playback(shared: Arc<Shared>, job: PlaybackJob) {
    drive_playback(&shared, &job, &mut tts_started).await;
    if tts_started && !cancelled(&job.cancel, &shared) {
        let _ = publish_tts(&shared, &job.client_id, TtsState::Stop, None).await;
        finish_ready(&shared, &job).await;
    }
}
```

`drive_playback` 发完**最后一帧**就返回，紧接着立刻发 `tts stop`。

而设备侧此刻还有 `prebuffer_count`（默认 5）帧滞留在解码队列/扬声器缓冲里
（≈300ms）。参考实现在这里**显式等待**：

```python
# xiaozhi-esp32-server/core/handle/sendAudioHandle.py:58-81
await rate_controller.queue_empty_event.wait()
playback_time = (PRE_BUFFER_COUNT + 2) * interval_ms / 1000.0   # (5+2)*60 = 420ms
await asyncio.sleep(playback_time)
# 然后才 send_tts_message(conn, "stop")
```

固件对 `tts stop` 的处理（`main/application.cc:637-641`）：

```cpp
} else if (strcmp(state->valuestring, "stop") == 0) {
    Schedule([this]() {
        if (GetDeviceState() == kDeviceStateSpeaking) {
            if (listening_mode_ == kListeningModeManualStop) {
                SetDeviceState(kDeviceStateIdle);
            } else {
                SetDeviceState(kDeviceStateListening);
            }
        }
    });
}
```

状态切出 `Speaking` 时音频管道会被回收，滞留在缓冲里的尾帧就被丢掉。

**★ 这一条是推断，必须用 T2 的埋点做 A/B 验证后再确认。**
如果埋点显示"最后一帧发送时刻 → tts stop 时刻"间隔本来就有几百毫秒
（比如 TTS 生成慢于播放），那根因另找。

## 4. 方案

### T1 — `tts stop` 前等尾帧播完

在 `run_playback` 里，`drive_playback` 返回后、发 `tts stop` 之前插入等待：

```
等待时长 = (prebuffer_count + 2) × frame_duration
默认 = (5 + 2) × 60ms = 420ms
```

`+2` 的来历：`prebuffer_count` 帧在设备队列里，再加 2 帧余量覆盖
UDP 传输抖动和固件调度延迟（照抄参考实现的公式，不要自创）。

**必须在 `cancel` 被触发时立即中断等待** —— 用户插话/abort 不能白等 420ms。

### T2 — 埋点，让"掉帧"可观测

在 `drive_playback` 结束时记录并输出：

- 本轮下发的总帧数
- 最后一帧的发送完成时刻
- 发送起始到结束的墙钟耗时
- 理论上应播放时长 = `总帧数 × frame_duration`
- **最后一帧发送时刻 → `tts stop` 时刻的间隔**（这就是 T1 要补的洞）

用 `warn!` 级（本项目的 INFO 会被过滤）。

用途：T1 上线前后各跑一段，对比"是否还有掉帧"。
若 T1 后仍掉帧，说明根因不在尾帧，需要查 UDP 丢包（`send_audio` 的错误路径）。

### T3 — `frame_step_ms` 可配（可选，低优先）

参考实现有 `tts_audio_send_delay` 旋钮（`config.yaml:78-82`）：
0 = 严格按帧率；>0 = 固定间隔（可略慢于 60ms，给设备缓冲留余量）。

在 `MqttUdpConfig` 加 `send_delay_ms: Option<u16>`，`frame_step_ms()` 优先取它。
默认 `None`（= 行为不变）。

## 5. 不做的事

- **不引入 jitter buffer / FEC / 重传**：参考实现也没有，真丢包只能靠固件
- **不改预缓冲数量**：5 帧已验证可用
- **不改 `snap_send_clock`**：它已经是对的
- **不动 ws.rs 的播放路径**（除非验收脚本要求一致性，否则只改 mqtt_udp）

## 6. 影响面

- `crates/haimen-xiaozhi/src/mqtt_udp.rs`：
  - `run_playback`（加尾帧等待）
  - `drive_playback`（加埋点）
  - `frame_step_ms` / `MqttUdpConfig`（T3，可选）
- `crates/haimen-xiaozhi/examples/mqtt_udp_demo.rs`（若加配置字段）
- `src/gateway/mod.rs`（若加配置字段）

**不改**：`strategy.rs`、`xiaozhi_asr_llm_tts.rs`、固件相关代码。

## 7. 实施结果与 A/B 验证（2026-10-19）

T1 + T2 已实现并部署到 Mac mini。

### 埋点实测（模拟器一轮完整对话）

```
下行播放统计  frames=167  elapsed_ms=9723  expected_ms=10020  tail_wait_ms=421
```

判读：

| 观测 | 结论 |
|---|---|
| `expected_ms = 167 × 60 = 10020` | 计算正确 |
| `elapsed_ms = 9723 = expected − 297` | 正好等于预缓冲 5 帧连发省下的时间（4×60=240ms + 抖动）→ **发送侧没有丢帧** |
| `tail_wait_ms = 421` | T1 生效（420ms + 1ms 调度抖动） |

**→ 发送侧一帧没丢，因此掉帧只可能发生在设备侧**，与 §3 的推断一致：
设备解码队列里滞留的尾帧被过早的 `tts stop` 回收。

### A/B 基线

- **T1 之前**：发完最后一帧立刻发 `tts stop`，`tail_wait_ms = 0`
- **T1 之后**：`tail_wait_ms ≈ 420`

若后续仍听到末尾掉帧，说明根因不只在尾帧，需要查 UDP 丢包
（`send_downlink_frame` 的失败路径）。

### 实现说明

`wait_tail_playback` 放在 `drive_playback` 内部而不是 `run_playback`，
因为 `tail_wait_ms` 要量实际等待时长（在函数外就量不到）。
`drive_playback` 返回 `bool` 表示是否已等过；中途异常退出的轮次由
`run_playback` 补等。取消路径行为不变：被取消的轮次不发 stop，
由 `handle_abort` 的 `Playing` 分支收口。
