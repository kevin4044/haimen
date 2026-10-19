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

8. **配置**：`[xiaozhi] say_token`（见 §4.5）。
   - 字段**必须实现**，默认空串
   - 空串 → **不校验，直接放行**（当前实际行为），启动时打一条 warn 说明"鉴权未启用"
   - 非空 → 校验 `Authorization: Bearer <token>`，不匹配 → 401
   - **不要留 TODO**：校验逻辑现在就要能跑，只是默认不触发

### 约束

- 不改 ASR/VAD/策略链路
- notify 的无 ACK 特性如实反映在响应里（不假装成功保证）
- **设备忙直接 409，不做排队/重试**（用户决定，理由见 02-design §4）
- TTS/编码耗时不能阻塞整个 worker：接口内部并发即可，但要设超时（建议 15s）

### 完成标准

- `./scripts/ci.sh` 全绿
- 单测：设备忙碌时返回 409 且带 `state`
- 单测：多设备未指定 → 409 `multiple_devices`
- 单测：`say_token` 为空 → **不校验**，请求正常放行
- 单测：`say_token` 非空时，无 Bearer → 401；错误 token → 401；正确 token → 通过
- 单测：正常路径返回 200 且 `audio_url` 可被 GET（用 T1 的路由在同一 router 内验证）
- 单测：请求里多传 `voice` 字段应被忽略（不报错，也不改变合成音色）

---

### T3 — 校对 HA 集成指南 + 真机验证

### 背景

**`specs/notify-channel/HA-集成指南.md` 已由规格作者写好**（含 rest_command、
四种自动化示例、模板化人话、设备忙的三种处理方案、常见问题）。
本任务是**校对 + 用真实环境验证**，不是重写。

### 要做的事

1. 通读 `HA-集成指南.md`，逐条核对：
   - `rest_command` 的 yaml 语法在**当前 HA 版本**上能通过配置校验
   - 指南里的 IP/端口和实际部署一致（`192.168.50.33:9527`）
   - `response_variable` 那段要求 HA ≥ 2023.7 —— 确认用户的 HA 版本，
     不满足就在文档里改成"仅方案 A"

2. 在真实环境跑通一次：
   - 开发者工具 → 服务 → `rest_command.xiaozhi_say` → 设备出声
   - 记录实际耗时（从调用到出声），写进文档

3. 如果发现指南与实际不符（HA 版本差异、字段名不同），**直接改指南**，
   在提交信息里写清改了什么、为什么。

### 完成标准

- HA 配置校验通过（开发者工具 → YAML → 检查配置）
- 真机成功播报一次，耗时写进文档
- 文档里没有未经核实的断言

---

### T4 — 端到端验收脚本

### 目标

`04-acceptance.sh` 的 `--task T4` 跑通完整链路。

见脚本里的具体检查项。

---

### T5 —（用户自己配，本仓库不做）Hermes 侧接入

### 背景

**用户已确认：Hermes 侧可以配一个 skill 或单独写一个 MCP，由用户自行处理。**
所以本仓库不实现 Hermes 集成，只保证 HTTP 接口可用。

### 给用户的接口信息（写进交付说明）

```
POST http://192.168.50.33:9527/api/v1/xiaozhi/say
Content-Type: application/json
{"text": "要说的话"}                 # 只接受文本，音色由 haimen 决定

→ 200 {"ok":true,"audio_url":"...","duration_ms":4200}
→ 409 {"ok":false,"error":"device_busy","state":"listening"}
→ 404 {"ok":false,"error":"device_offline"}
→ 502 {"ok":false,"error":"tts_failed"}
```

**Hermes 侧只要包一层 —— 不管是 skill 里调 HTTP，还是 MCP 工具里 POST ——
都只依赖这一个接口。** 不需要 haimen 再做别的事。

### 建议（供用户参考）

如果用 MCP，工具签名建议：
```python
@mcp.tool()
async def notify_xiaozhi(text: str) -> str:
    """让小智音箱播报一句话。仅设备空闲时生效。
    用于主动提醒（门铃、定时、设备状态变化），不要用于回答用户问题。"""
```

**docstring 里写清「仅设备空闲时生效」很重要** —— 否则模型会在对话中用这个工具，
而对话中 notify 一定被丢弃，用户会觉得「说了没反应」。
