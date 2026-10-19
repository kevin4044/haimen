# 02 - 设计：让 HA 和 Hermes 能给 小智 发通知（notify 通路）

## 1. 问题

小智（ESP32 设备）支持 `notify` 消息 —— 服务端推一条消息，设备播放指定音频，播完回 Idle。
这是我们唯一能在**用户没说话时**主动开口的机制。

**现在没有任何实用通路能用它**。当前只有一条路：

```
MQTT 客户端 → haimen/ctl/<client_id> → haimen 转发给设备
```

要用它必须：
1. 持有 MQTT 管理端凭据（HMAC 派生密码）
2. 自己把文本做成 **Ogg Opus** 文件
3. 找一个设备能访问的 **HTTP 地址**托管这个文件
4. 手工拼 notify 消息

**→ HA 自动化和 Hermes 都没法这样用。**

## 2. 现状（已核实）

| 项 | 状态 |
|---|---|
| haimen 下发 notify | ❌ **没有实现**。`send_notify.py` 是绕过 haimen 直接发控制主题的 |
| haimen 的 HTTP 服务 | ✅ 在 9527，已有 `/api/v1/*` 路由体系 |
| 火山 TTS 合成 | ✅ `univoice` 已有，语音会话在用 |
| Opus 编码 | ✅ `opus2` 已在用（下行音频就是 Opus） |
| MQTT 控制主题 | ✅ `haimen/ctl/<client_id>`，仅管理端可发 |

## 3. 设备侧行为（已实测，是设计的硬约束）

来源：官方文档 + 本项目实测（2026-10-19 22:36 验证通过）

| 约束 | 说明 | 设计影响 |
|---|---|---|
| **只在 Idle 生效** | 其他状态（聆听/说话/连接中）**静默丢弃**，无错误返回 | 必须知道设备是不是 Idle，否则调了也没反应 |
| **无 ACK** | 设备不会告诉你收到没有 | 只能靠"设备来拉音频"这个 HTTP 请求判断 |
| **audio_url 必须是单声道 Ogg Opus** | 2xx，**不跟随重定向** | haimen 要自己托管音频，且 URL 必须直给 |
| **播完回 Idle** | 不会自动进入聆听 | 播完就结束，不会打断用户 |
| **先放一个提示音** | 设备固定行为，我们控制不了 | 用户会先听到"叮"，然后才是话 |
| **Idle 时设备不发任何消息** | 无法靠 MQTT 判断在线 | 在线状态不可靠，只能尽力而为 |

**★ 关键**：`notify` 的"静默丢弃"是最麻烦的 —— 调用方拿到 200 但设备没反应。
所以 haimen 必须尽量在**下发前**判断设备状态，并把结果如实返回。

## 4. 方案

### 4.1 对外接口（本仓库唯一新增的代码面）

```
POST /api/v1/xiaozhi/say
Content-Type: application/json
Authorization: Bearer <token>        // 可选，见 §4.5

{
  "text":   "客厅有点热，要不要把空调打开？",   // 必填，1..500 字
  "device": "84:c7:bb:59:93:64"                 // 可选，省略=唯一在线设备
}
```

**只接受文本，不接受音色参数。**

音色**必须和正常对话回复完全一致** —— 走同一条解析路径：

```
voice = voice_override（CLI --xiaozhi-tts-voice，若有）
        ?? 配置 [tts.providers.doubao].voice
```

理由：同一个人一会儿一个声音，用户会觉得"这不是小智在说话"，比不说还糟。
**所以实现时不要另读一次配置**，复用策略里已有的音色解析结果（含 override）。

响应：

```jsonc
// 200 已下发
{ "ok": true, "device": "84:c7:bb:59:93:64", "mode": "notify",
  "audio_url": "http://192.168.50.33:9527/xiaozhi/say/<uuid>.ogg",
  "duration_ms": 4200 }

// 409 设备不空闲（明确告诉调用方"发了也没用"）
{ "ok": false, "error": "device_busy", "state": "listening" }

// 409 有多个设备但没指定
{ "ok": false, "error": "multiple_devices" }

// 404 设备未知/离线
{ "ok": false, "error": "device_offline" }

// 502 TTS 合成失败
{ "ok": false, "error": "tts_failed", "detail": "..." }
```

**为什么用 409 而不是 200 + 静默**：调用方是自动化脚本和 agent，
必须能区分"发了"和"设备不在状态、没发"。

### 4.2 haimen 内部流程

```
POST /api/v1/xiaozhi/say
   ↓
① 解析设备（按 mac / client_id / 唯一在线设备）
   ↓
② 检查状态：MQTT 已连接 && 无 UDP 会话 && 不在播放
   ↓ 不满足 → 409 device_busy
③ 火山 TTS 合成 24kHz mono PCM
   音色 = 与正常回复同一条解析路径（voice_override ?? 配置 voice），不接受调用方指定
   ↓
④ 编码成 Ogg Opus（opus2，60ms/帧）
   ↓
⑤ 存进内存表 + 生成一次性 URL
   URL = http://<advertise_ip>:<port>/xiaozhi/say/<uuid>.ogg
   ↓
⑥ 下发 notify 消息（复用现有控制主题转发路径）
   ↓
⑦ 返回 200 + audio_url
   ↓
（设备来 GET → 返回音频 → 从内存表删除）
```

### 4.3 音频托管

haimen 已有 9527 HTTP 服务，加两个路由：

| 路由 | 作用 |
|---|---|
| `GET /xiaozhi/say/<uuid>.ogg` | 返回音频（`Content-Type: audio/ogg`，**不重定向**） |
| —— | 首次被 GET 后删除；另有 TTL（默认 60s）兜底，防止设备没来拉时内存泄漏 |

**为什么放内存而不是临时文件**：一条通知音频约 10~40KB，内存足够；
避免磁盘 IO 和清理逻辑；进程重启时自然丢弃（本来也没用了）。

### 4.4 HA 集成（不改代码，纯配置）

```yaml
# configuration.yaml
rest_command:
  xiaozhi_say:
    url: "http://192.168.50.33:9527/api/v1/xiaozhi/say"
    method: POST
    content_type: "application/json"
    payload: '{"text": {{ message | to_json }}}'
    timeout: 20
```

```yaml
# 自动化里
action:
  - service: rest_command.xiaozhi_say
    data:
      message: "洗衣机洗好了，记得晾衣服喔"
```

### 4.5 鉴权

haimen 的 HTTP 服务现在**没有鉴权**（内网自用）。这个接口一旦开放，
同网段任何人都能让设备说话。

**方案**：复用已有的 `ota_token` 思路，新增配置项 `[xiaozhi] say_token`：
- 为空 → 不校验（内网自用，与现状一致）
- 非空 → 要求 `Authorization: Bearer <token>`
- HA 的 `rest_command` 支持 `headers`，配置里带上即可

**不做**：不引入用户体系、不做限流（单设备家用，够用）。

## 5. Hermes 集成（**不在本仓库，需单独确认**）

Hermes 通过 MCP 工具控制 HA，当前工具来自容器里的 `home_lights.py`：

```
docker exec xiaozhi-mcp → /app/home_lights.py（34KB）
启动: mcp-calculator/mcp_pipe.py home_lights.py
```

**这份源码不在 haimen 仓库里**，容器由 `xiaozhi-mcp:latest` 镜像构建。
所以 Hermes 侧需要一个新 MCP 工具，例如：

```python
@mcp.tool()
async def notify_xiaozhi(text: str) -> str:
    """让小智音箱播报一句话（仅设备空闲时生效）"""
    # POST http://192.168.50.33:9527/api/v1/xiaozhi/say
```

**三个选项**（见 `03-tasks.md` T5 的讨论），本规格**只保证 HTTP 接口可用**，
Hermes 侧接入留待确定 `xiaozhi-mcp` 源码位置后再做。

## 6. 影响面

| 文件 | 改动 |
|---|---|
| `crates/haimen-xiaozhi/src/notify.rs` | **新增**：notify 消息构造、设备状态判断 |
| `crates/haimen-xiaozhi/src/say_audio.rs` | **新增**：内存音频表 + TTL 清理 + 路由处理 |
| `crates/haimen-xiaozhi/src/lib.rs` | 导出 + `add_routes` 挂新路由 |
| `crates/haimen-xiaozhi/src/types.rs` | 新增 `say_token` 配置 |
| `src/gateway/mod.rs` | 传递 `say_token` 与 `advertise_ip` |

**不改**：ASR/VAD/TTS 主链路、策略层、MQTT broker 协议实现。

## 7. 不做的事

- ❌ 不做"设备状态查询接口"（靠 409 的 `state` 字段够用）
- ❌ 不做消息队列/重试（notify 无 ACK，重试会重复播报）
- ❌ 不做多设备广播（先保证单设备可靠）
- ❌ 不改固件
- ❌ 不替换现有的 `haimen/ctl/` 控制主题（那是调试用的底层通道，保留）

## 8. 验收方式

见 `04-acceptance.sh`。核心是**端到端**：
1. 起一个模拟设备，确认它进入 Idle
2. 调 API，拿到 `audio_url`
3. **模拟设备 GET 那个 URL**，校验返回的是合法 Ogg Opus
4. 重放同一个 URL，确认已失效（一次性）
5. 设备忙时调 API，确认返回 409
