# 03 - 任务单（T1..T5）

> 实现依据：`02-design.md`
> 每个任务完成后必须 `./scripts/ci.sh` 全绿。
> 验收：`bash specs/notify-channel/04-acceptance.sh --task T1`

---

### T1 — 内存音频表 + 一次性 URL 路由

### 目标

新增 `crates/haimen-xiaozhi/src/say_audio.rs`，实现"生成音频 → 托管 → 被拉取后失效"的闭环。
**不涉及 TTS，不涉及 notify**：本任务只做一个通用的内存媒体服务，可用固定字节测试。

### 要做的事

1. 进程内共享表：

```rust
/// 一次性的通知音频。
///
/// 为什么放内存：一条通知音频 10~40KB，够小；设备拉一次就不再需要，
/// 放磁盘要额外处理清理和并发。进程重启丢弃也无所谓（通知本身就过期了）。
pub struct SayAudioStore {
    items: Mutex<HashMap<String, SayAudioItem>>,
}

struct SayAudioItem {
    bytes: Vec<u8>,
    created_at: Instant,
    /// 单次拉取后就删；这个 TTL 只是兜底（设备可能因为不在 Idle 而没来拉）
    ttl: Duration,
}
```

2. 方法：
   - `insert(bytes: Vec<u8>, ttl: Duration) -> String`（返回 uuid）
   - `take(id: &str) -> Option<Vec<u8>>`（取出并删除）
   - `sweep()`（清掉过期的，由定时任务调）

3. axum 路由，挂在 `add_routes` 里：

```
GET /xiaozhi/say/{id}.ogg
```

要求：
- 命中 → `200` + `Content-Type: audio/ogg` + `Content-Length`，**并删除该条目**
- 未命中 → `404`，body 为纯文本说明
- **不得返回重定向**（设备的 HTTP 客户端不跟随 3xx）
- 不得加 `Cache-Control` 之类的缓存头影响设备

4. 路由需要的共享状态怎么传：`add_routes(r, strategy)` 现在只接 strategy。
   新增一个 `SayAudioStore` 的 `Arc`，**优先挂在已有的 strategy 或 shared 上**，
   避免改 `add_routes` 的签名（如果必须改签名，同步改 `src/web/mod.rs` 的调用点）。

5. 定时清理：在已有的 web 服务启动处起一个 `tokio::spawn` 的 30s 定时任务调 `sweep()`。

### 约束

- 不引入新依赖（uuid 已有）
- `take()` 必须是原子的（同一个 id 并发来拉只应有一个成功）
- 注释用中文，解释「为什么」

### 完成标准

- `./scripts/ci.sh` 全绿
- 单测：insert → take 拿到字节 → 再 take 返回 None
- 单测：过期条目被 sweep 清掉
- 单测：路由 200 返回正确 Content-Type / 404 不存在

---

### T2 — `POST /api/v1/xiaozhi/say`：合成 + 托管 + 下发

### 目标

新增接口，把「文本」变成「设备能播的 Ogg Opus」，托管起来，并下发 notify。

依赖 T1（用它的 `SayAudioStore`）。

### 接口

见 `02-design.md` §4.1 的完整请求/响应。

### 要做的事

1. **请求/响应类型**（`serde`），字段与 §4.1 一致；`text` 空或超 500 字 → 400。

2. **设备解析**：
   - `device` 给了 → 按 mac 或 client_id 精确匹配；找不到 → 404 `device_offline`
   - 没给 → 如果恰好一台在线设备就用它；0 台 → 404；多台 → 409 `multiple_devices`

3. **状态检查（关键）**：
   - MQTT 在线（broker 有该 client_id）
   - **没有 UDP 会话**（有 = 正在录音/对话中）
   - 不在播放（`SessionState::Playing`）
   - 不满足 → 409 `device_busy`，`state` 字段给出实际状态

4. **TTS 合成**：
   - **只吃文本，不接受音色参数**。
   - 音色走和正常回复**同一条解析路径**：`voice_override`（CLI `--xiaozhi-tts-voice`，若有）
     否则配置里的 `[tts.providers.doubao].voice`。**不要另读一份配置**，复用策略里
     已有的解析结果，否则 CLI 覆盖时会不一致。
   - 音色不一致会让用户觉得"这不是小智在说话"，比不说还糟。
   - 输出 24kHz mono PCM
   - 合成失败 → 502 `tts_failed`

5. **Opus 编码**：
   - 用 `opus2`，48000 容器 / 24kHz 输入参数，60ms/帧（与下行音频一致）
   - 封装成 **Ogg Opus 容器**（设备的 HTTP 客户端按 Ogg 解析）
   - 如果 `opus2` 不直接支持 Ogg mux，允许手写最小 Ogg 页封装（OpusHead / OpusTags / 数据页）

6. **托管**：调 T1 的 `insert()`，URL 基址取配置里的 `advertise_ip`（没有就用本机 LAN IP），
   端口取 HTTP 服务端口。

7. **下发 notify**：构造
```json
{"type":"notify","audio_url":"<url>","subtitles":[{"start_ms":0,"text":"<原文>"}]}
```
   走现有控制主题转发路径发给该设备。

8. **配置**：`[xiaozhi] say_token`（可选，见 §4.5）。非空时校验
   `Authorization: Bearer <token>`，不匹配 → 401。

### 约束

- 不改 ASR/VAD/策略链路
- notify 的无 ACK 特性如实反映在响应里（不假装成功保证）
- TTS/编码耗时不能阻塞整个 worker：接口内部并发即可，但要设超时（建议 15s）

### 完成标准

- `./scripts/ci.sh` 全绿
- 单测：设备忙碌时返回 409 且带 `state`
- 单测：多设备未指定 → 409 `multiple_devices`
- 单测：`say_token` 配置后，无 Bearer → 401
- 单测：正常路径返回 200 且 `audio_url` 可被 GET（用 T1 的路由在同一 router 内验证）
- 单测：请求里多传 `voice` 字段应被忽略（不报错，也不改变合成音色）

---

### T3 — 运维文档 + HA 配置样例

### 目标

让用户能照着配置就把 HA 接上。

### 要做的事

1. 新增 `specs/notify-channel/HA-配置样例.md`，包含：
   - `rest_command` 完整 yaml
   - 一个真实自动化示例（如「洗衣机洗好」）
   - 常见问题：设备回 409 怎么办、播报没反应怎么排查

2. 更新 `specs/ops/`（或现有运维文档）一节，说明：
   - 接口地址与鉴权
   - 如何用 curl 手测
   - notify 的三条硬约束（只在 Idle / 无 ACK / 先放提示音）

### 完成标准

- 照着文档能用 curl 打通一次真实播报（在真机或模拟器上验证）
- 文档里的 IP/端口与本机实际一致

---

### T4 — 端到端验收脚本

### 目标

`04-acceptance.sh` 的 `--task T4` 跑通完整链路。

见脚本里的具体检查项。

---

### T5 —（暂不实现）Hermes MCP 工具

### 背景

Hermes 当前的工具来自容器 `xiaozhi-mcp:latest` 里的 `/app/home_lights.py`。
**这份源码不在 haimen 仓库**，容器怎么构建的也不清楚。

### 三个选项（需用户先定）

| 选项 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| **A** | 在 `home_lights.py` 加 `notify_xiaozhi` 工具 | 复用现有 MCP 通道，改动最小 | 需要先拿到源码与构建方式 |
| **B** | haimen 自己起一个 MCP server（`rmcp` 已有 client 依赖，加 server feature） | 全在本仓库、可版本化 | Hermes 的 MCP 注册配置位置未确认 |
| **C** | 用 Hermes 的 `code_execution` 工具 curl | 零改动 | 依赖该工具可用，且把网络调用塞进执行环境，脆弱 |

**推荐 A**（前提是能拿到源码），其次 B。

**本任务在 T1..T4 完成、并确认源码位置之前不要动手。**
