# 02 - 设计：多设备与房间（设备目录 + 房间上下文 + 定向通知）

> 目标：家里有多台小智时，LLM 知道「这句话从哪个房间说的」，
> 通知能按「哪台设备 / 哪个房间」定向送达，而不是只能填 MAC。

## 1. 问题

多台小智分房间部署（客厅、卧室、书房…）后，暴露出三个缺口：

1. **LLM 不知道房间。** 用户在客厅问「灯关了吗」，和在卧室问同一句话，
   期望的答案可能完全不同；但送给 LLM 的只有 ASR 文本，没有任何设备语义。
2. **通知只能按 MAC 寻址。** `POST /api/v1/xiaozhi/say` 的 `device` 只认
   MAC / `client_id`；写 HA 自动化的人得记住 `84:c7:bb:59:93:64` 这种串，
   也没法表达「只通知客厅」。
3. **多台设备时没有可观测面。** 不知道哪台在线、哪台在忙、每台配了什么
   名字/房间，排障只能翻日志。

前一轮「MQTT + UDP」已经解决了**多设备同时在线、互不串台**的传输层问题
（见 [`../mqtt-udp/实现方案.md`](../mqtt-udp/实现方案.md)）；本轮补的是
**人类语义**：名字、房间、以及基于它们的定向寻址。

## 2. 现状（已核实）

| 能力 | 状态 | 说明 |
|---|---|---|
| MQTT broker 按 `client_id` 顶号 | ✅ 已有 | 同一 `client_id` 重连顶掉旧连接，不同设备各自一条连接 |
| UDP 音频会话按设备隔离 | ✅ 已有 | 每设备独立 `udp_session_id`、密钥与发送时钟 |
| 每设备独立 ASR/VAD/LLM 会话 | ✅ 已有 | 会话键 MQTT 用 `client_id`、WS 用会话号，状态互不共享 |
| `say` 按 MAC / `client_id` 单播 | ✅ 已有 | 不指定时要求恰好一台在线；忙/多台返回 409 |
| 设备名字 / 房间语义 | ❌ 缺 | LLM 看不到房间；通知无法按名字/房间寻址 |
| 设备清单（在线 + 忙闲） | ❌ 缺 | 只能从 409 的 `state` / `devices` 间接猜 |
| 房间上下文注入 | ❌ 缺 | 送给 Agent 的文本与房间无关 |

**结论**：传输层已经能区分「哪台设备」，缺的是「这台设备是谁、在哪」的
静态目录，以及在对话/通知两个入口使用这份目录。

## 3. 方案

### 3.1 设备目录（DeviceDirectory）

在 `[xiaozhi]` 下新增设备表 `[[xiaozhi.devices]]`，启动时构建一份**进程内
静态目录**（`crates/haimen-xiaozhi/src/devices.rs`）：

```text
配置（settings.toml） ──启动──▶ DeviceDirectory（Vec<ConfiguredDevice>）
                                  │
                    ┌─────────────┴─────────────┐
                    ▼                           ▼
         对话链路：查 room 注入上下文      notify：按 name / room 寻址
```

- 每台设备存四项：配置里的原始 `device_id`（回显用）、归一化 MAC、
  `name`（空则回退 MAC）、`room`（空 = 未分配）。
- 身份相同（归一化后 MAC 相同）的重复配置**保留第一条**并打 warn，
  避免同一台设备出现两个房间。
- 目录是进程级全局槽，启动时安装一次；Web UI 保存后覆盖安装，立即生效。
- 目录**为空**（旧配置）时所有新行为自动退化为旧行为，见 §6。

**为什么用静态配置而不是自动发现**：MAC → 房间无法从协议里推断，必须由人
指定；家用规模最多十几台，静态表足够，且可预测、可版本化，不引入数据库。

### 3.2 身份归一化

外部身份有三种写法，统一收敛到「冒号小写 MAC」再比较：

| 输入 | 归一化结果 | 来源 |
|---|---|---|
| `84:C7:BB:59:93:64` | `84:c7:bb:59:93:64` | MAC（冒号） |
| `84_c7_bb_59_93_64` | `84:c7:bb:59:93:64` | MAC（下划线） |
| `84c7bb599364` | `84:c7:bb:59:93:64` | MAC（12 位连写） |
| `GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64` | `84:c7:bb:59:93:64` | MQTT `client_id`（取 `@@@` 之间的 MAC 段） |

- 不是 MAC 的输入（如 WS 的 `unknown`）原样小写返回，仍可做精确匹配。
- 配置查找统一大小写不敏感、忽略首尾空白。
- `device` 选择器额外支持配置里的 `name`（如「客厅小智」），方便人写自动化。

### 3.3 房间上下文注入

配置了 `room` 的设备，在**每轮**把用户文本交给 Agent 之前，前面加一行：

```text
【设备上下文】当前设备「客厅小智」位于「客厅」，用户就在这个房间和你说话；回答时可以结合这个房间的语境。
<原始 ASR 文本>
```

关键设计点：

1. **只影响送给 LLM 的文本。** 设备屏幕下发的 `stt` 事件、agent 日志的
   `input` 仍记录原始识别文本，排障时不会被前缀干扰。
2. **每轮都注入，不是只在首轮。** Agent 侧的会话历史可能被压缩/换代清空，
   只在首轮注入会丢；每轮多十几 token，换来「永远知道在哪个房间」。
3. **房间从哪来。** 传输层识别出设备身份后调用策略钩子
   `on_device_identified(session_key, device_id)`，把设备 MAC 存进会话状态；
   判断时再查设备目录：
   - MQTT：`hello` 成功后用 `client_id` 里的 MAC 调用；
   - WebSocket：HELLO 握手成功后用 `Device-Id` 请求头调用；
   - 兜底：MQTT 会话键本身就是 `client_id`，可直接从里面取 MAC。
4. **未配置 / `room` 为空 → 一字不加**，不产生额外 token，行为与旧版本一致。

### 3.4 notify 寻址优先级

`POST /api/v1/xiaozhi/say` 的目标解析改为三档，优先级从高到低：

```text
device 字段 ──命中──▶ 该设备（按 client_id 精确 / 归一化 MAC / 配置的 name）
    │ 不命中/未给
    ▼
room 字段 ──命中──▶ 该房间在线设备中恰好一台
    │ 不命中/未给
    ▼
恰好一台在线 ──▶ 用它
    └ 零台 → 404；多台 → 409 multiple_devices（不广播）
```

| 输入 | 行为 |
|---|---|
| 只给 `device` | 按 client_id / MAC / name 匹配在线设备；不存在或离线 → 404 `device_offline` |
| 只给 `room` | 只看配置了该 `room` 的在线设备：恰好一台 → 下发；零台 → 404（响应体回显 `"room"`）；多台 → 409 `multiple_devices`（带候选列表） |
| 同时给 `device` + `room` | **`device` 优先**，`room` 被忽略（不校验一致性） |
| 都不给 | 恰好一台在线才下发；多台 → 409 `multiple_devices`；零台 → 404 |

- **不广播**：房间多台、无参多台一律 409，让调用方显式选一台；避免「全家
  突然同时说话」这种不可控行为，也避免 notify 无 ACK 时的放大效应。
- 409 的响应体带候选列表（`client_id` / `device_id` / `name` / `room`），
  调用方可以直接挑一台重试，不用再查一次接口。
- 命中后仍先检查忙闲：非 idle → 409 `device_busy`（带 `state`），
  不排队、不重试（notify 只在 idle 生效，且无 ACK）。

### 3.5 设备清单接口

新增只读接口 `GET /api/v1/xiaozhi/devices`，把三份信息合并成一个视图：

1. `[xiaozhi.devices]` 里配置的设备（**已配置但离线也要列出**，否则 Web UI
   会把配置项「弄丢」）；
2. MQTT 当前在线的设备（可能还没配置名字/房间）；
3. 每台在线设备的忙闲状态（notify 只在 idle 生效）。

离线条目的 `client_id` 为 `null`、`online` 为 `false`、`state` 为 `null`；
未配置名字时 `name` 回退为 MAC。鉴权与 `say` 一致（`say_token`）。

### 3.6 Web UI 配置页

Web 控制台新增顶部「小智设备」页（`web-ui/src/pages/Settings/DeviceSettings.tsx`），
读写同一个设备目录，不再要求手改 `settings.toml`：

| 接口 | 作用 |
|---|---|
| `GET /api/v1/settings/xiaozhi/devices` | 已配置设备 + 当前在线设备的合并视图 |
| `PUT /api/v1/settings/xiaozhi/devices` | 全量替换设备表，写盘后重建目录并立即生效 |

GET 把「配置」和「在线」分成两个数组：`devices[]` 来自 `[[xiaozhi.devices]]`
（含已配置但离线的设备，`configured: true`），`online[]` 来自 MQTT hub
（带 `client_id` / `state` / `configured`）。PUT 会校验 `device_id` 非空、
归一化 MAC 不重复、最多 256 台；校验失败返回 `{"success":false,"error":"..."}`
且不写盘。

## 4. 配置格式

```toml
[xiaozhi]
protocol = "mqtt"

[[xiaozhi.devices]]
device_id = "84:c7:bb:59:93:64"   # 必填：MAC 任意写法或 MQTT client_id
name = "客厅小智"                   # 可选：人读的名字；空串回退为 MAC
room = "客厅"                       # 可选：房间名；空串 = 未分配

[[xiaozhi.devices]]
device_id = "aa:bb:cc:dd:ee:ff"
name = "卧室小智"
room = "卧室"
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `device_id` | 是 | MAC（冒号/下划线/12 位连写）或 MQTT `client_id`；TOML 里也接受别名 `mac` / `client_id` |
| `name` | 否 | 人读名字；`say` 的 `device` 可以直接填它 |
| `room` | 否 | 房间名；空 = 不注入上下文、不参与按房间寻址 |

- 配置文件位置：`~/.haimen/settings.toml`（或数据目录里的 `settings.toml`）。
- 直接改 TOML 需要重启进程生效；Web UI 保存走目录热替换，立即生效。
- 不写 `[[xiaozhi.devices]]` 就是空目录，旧配置零迁移。

## 5. API 契约

### 5.1 `POST /api/v1/xiaozhi/say`

鉴权：`[xiaozhi] say_token` 非空时要求 `Authorization: Bearer <say_token>`，
不匹配 → 401；空串 = 不校验（内网自用默认）。

请求：

```jsonc
{
  "text": "开饭了",
  "device": "客厅小智",   // 可选：MAC / client_id / 配置的 name
  "room": "客厅"           // 可选：配置里的 room；与 device 同时给时 device 优先
}
```

`text` 必填，trim 后非空、≤ 500 字（字符数）；`device` / `room` 至少一个或都不给。

成功响应（200）：

```json
{
  "ok": true,
  "device": "84:c7:bb:59:93:64",
  "client_id": "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64",
  "name": "客厅小智",
  "room": "客厅",
  "mode": "notify",
  "audio_url": "http://192.168.1.10:9527/xiaozhi/say/<uuid>.ogg",
  "duration_ms": 4200
}
```

`200` 只代表 **notify 已下发**，不保证设备真的播放（设备的 `notify` 无 ACK，
且只在 idle 生效）。新增回显 `client_id` / `name` / `room`，方便调用方确认
命中了哪台设备。

错误码：

| 状态码 | `error` | 触发条件 | 响应体要点 |
|---|---|---|---|
| `400` | `bad_request` | `text` 为空 / 超 500 字 | `detail` |
| `401` | `unauthorized` | `say_token` 非空且 Bearer 不匹配 | —— |
| `404` | `device_offline` | `device` 不存在或离线；`room` 内无在线设备；无参时零台在线；WS 模式下没有 notify 通路 | 按 `room` 查时回显 `"room": "厨房"` |
| `409` | `device_busy` | 目标设备在聆听/播放 | `"state": "listening" \| "playing"` |
| `409` | `multiple_devices` | `room` 内多台在线；或未指定目标且多台在线 | `"devices": [{client_id, device_id, name, room}]` 候选列表 |
| `502` | `tts_failed` | TTS 合成失败/超时 | `detail` |

补充说明：

- 请求不是合法 JSON 或缺必填字段时，由框架层返回 `415` / `422`（沿用旧接口行为）。
- `device` 与 `room` 同时给时以 `device` 为准；`device` 写错不会回退到 room。
- 409 的候选列表里的 `room` 是配置值（未配置为空串），`name` 未配置时回退 MAC。

### 5.2 `GET /api/v1/xiaozhi/devices`

鉴权同 `say_token`。成功响应（200）：

```json
{
  "ok": true,
  "devices": [
    {
      "device_id": "84:c7:bb:59:93:64",
      "client_id": "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64",
      "name": "客厅小智",
      "room": "客厅",
      "online": true,
      "state": "idle"
    },
    {
      "device_id": "aa:bb:cc:dd:ee:ff",
      "client_id": null,
      "name": "卧室小智",
      "room": "卧室",
      "online": false,
      "state": null
    }
  ]
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `device_id` | string | 归一化 MAC（在线/离线统一形态） |
| `client_id` | string \| null | MQTT 在线时才有；配置了但离线为 `null` |
| `name` | string | 配置的名字；未配置回退 MAC |
| `room` | string | 配置的房间；未配置为空串 |
| `online` | bool | 是否有 MQTT 连接 |
| `state` | string \| null | `"idle" \| "listening" \| "playing" \| null`；离线为 `null` |

只看 MQTT 通路（notify 的投递通路）：WebSocket 连接的设备不会出现在
`online: true` 里。离线设备、以及没有 MQTT hub（WebSocket 模式）时，
`state` 均为 `null`（未知）。

### 5.3 `GET/PUT /api/v1/settings/xiaozhi/devices`

Web UI「小智设备」页使用，读写设备目录。这是 Web 控制台的设置接口，
不走 `say_token` 鉴权（与其它 `/api/v1/settings/*` 一致）。

`GET` 响应（已配置设备 + 当前在线设备）：

```json
{
  "success": true,
  "data": {
    "devices": [
      { "device_id": "84:c7:bb:59:93:64", "name": "客厅小智", "room": "客厅", "configured": true }
    ],
    "online": [
      {
        "client_id": "GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64",
        "device_id": "84:c7:bb:59:93:64",
        "name": "客厅小智",
        "room": "客厅",
        "online": true,
        "state": "idle",
        "configured": true
      }
    ]
  }
}
```

`PUT` 请求体（**全量替换**；空数组 = 清空设备表）：

```json
{
  "devices": [
    { "device_id": "84:c7:bb:59:93:64", "name": "客厅小智", "room": "客厅" }
  ]
}
```

`PUT` 成功响应 `{"success": true, "data": {"devices": [...]}}`。

| 校验 | 行为 |
|---|---|
| `device_id` 去空白后为空 | 400，`{"success":false,"error":"设备 ID 不能为空（MAC 或 MQTT client_id）"}` |
| 归一化后 MAC 重复 | 400，`error` 含「设备 ID 重复：<mac>」 |
| 设备数量超过 256 台 | 400 |
| 保存 `settings.toml` 失败 | 500，`error` 含「保存配置失败」 |

- 保存后重建设备目录并覆盖安装：正在进行的对话不受影响，下一轮对话与下一次
  `say` 立即按新目录工作。
- 校验失败不写盘、不改目录。

## 6. 多设备安全与兼容性

**旧配置零迁移**

- 不写 `[[xiaozhi.devices]]` → 目录为空：对话不注入前缀、`say` 只能按
  MAC / `client_id` 寻址、无参时仍要求恰好一台在线 —— 与旧版本完全一致。
- `say` 原有请求字段（`text` / `device`）语义不变；新字段 `room` 可选，
  老调用方不受影响。
- `devices` 配置项有默认值（空表），老 `settings.toml` 反序列化不会失败。
- MQTT/WS 的传输隔离、鉴权、OTA 行为均未改动。

**多设备安全**

- `say` 不广播：房间多台/无参数多台都要求显式指定，不会出现「一句话全家一起
  响」；409 让调用方看到候选，而不是静默丢弃。
- `say_token` 保护 `say` 与 `devices` 两个接口；未配置时仍是内网自用语义。
  409 候选列表会暴露设备 `client_id` / 名字 / 房间，只应发给可信调用方。
- `GET/PUT /api/v1/settings/xiaozhi/devices` 与其它 Web 设置接口一致、不带鉴权；
  只应让 Web 控制台/可信内网访问（能改设备表 = 能改 notify 的寻址目标与房间上下文）。
- 设备目录只是静态映射，不改变 MQTT broker 的连接鉴权，也不引入新监听端口。
- 房间名不是安全边界：能调 `say` 的人本来就能指定任意 MAC；房间只是寻址便利。

**并发与生效时机**

- 目录用 `RwLock<Option<Arc<DeviceDirectory>>>` 全局槽持有，读多写少；
  每轮对话/每次 `say` 读一次，开销可忽略。
- Web UI 保存是覆盖式安装；同时有请求在跑时，最多各自用「旧/新」目录之一，
  不会 panic 或半更新。

## 7. 限制（明确不做）

| 限制 | 说明 |
|---|---|
| notify 无 ACK | `200` 只表示已下发；设备是否播了无法确认 |
| 仅 idle 生效 | 设备聆听/播放中 → 409 `device_busy`；不排队、不重试 |
| 不广播 | 房间多台 / 无参多台 → 409，必须指定设备 |
| 房间是精确匹配 | 大小写不敏感 + 去首尾空白，不做别名、模糊匹配或层级（「客厅」≠「客厅灯」） |
| 房间多台需指定设备 | 同一房间放多台时，按房间通知会 409，只能按 `device`/`name` 指定 |
| 设备在线状态只反映 MQTT | notify 是 MQTT 通路；WS 模式没有 `say`，清单也只列配置项 |
| 目录由配置驱动 | 保存到 `settings.toml`；不做自动发现，MAC → 房间必须人工指定 |
| TOML 手改需重启 | Web UI 保存才立即生效 |
| 不改固件 | 设备侧协议行为不变 |

## 8. 验收

机械验收脚本：[`04-acceptance.sh`](./04-acceptance.sh)（只读检查为主，
可选按设备/房间实测 say）。任务拆解与勾选状态见 [`03-tasks.md`](./03-tasks.md)。
