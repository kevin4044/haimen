# 03 - 任务单：多设备与房间

> 实现依据：[`02-design.md`](./02-design.md)
> 验收脚本：[`04-acceptance.sh`](./04-acceptance.sh)
> 当前状态：**后端核心已完成并单测通过**；Web UI 配置页与 settings API 已由并行任务合入（前端构建由 Lead 统一验证）；文档与验收脚本即本规格包。

## 状态总览

| 任务 | 内容 | 状态 |
|---|---|---|
| [T1](#t1--设备目录与身份归一化) | 设备目录 `[[xiaozhi.devices]]` + 身份归一化 + 启动安装 | ✅ 已完成 |
| [T2](#t2--房间上下文注入mqtt--websocket) | 房间上下文注入 LLM 输入（MQTT / WS） | ✅ 已完成 |
| [T3](#t3--say-定向寻址设备名--房间) | `say` 支持 `device`=名字、新增 `room`、409/404 错误体 | ✅ 已完成 |
| [T4](#t4--设备清单接口) | `GET /api/v1/xiaozhi/devices` | ✅ 已完成 |
| [T5](#t5--web-ui小智设备配置页--settings-api) | Web UI「小智设备」页 + `GET/PUT /api/v1/settings/xiaozhi/devices` | ✅ 已合入（并行任务） |
| [T6](#t6--文档与验收脚本) | README / 设计文档 / 任务单 / 验收脚本 / HA 指南 | ✅ 已完成（即本规格包） |

---

### T1 — 设备目录与身份归一化

### 目标

新增一份「设备身份 → 名字/房间」的静态目录，供对话链路和 notify 共用；
旧配置（没有设备表）行为完全不变。

### 完成内容（✅）

- [x] `crates/haimen-xiaozhi/src/devices.rs`：`ConfiguredDevice` / `DeviceDirectory` /
      全局槽 `install_device_directory` / `install_devices_from_config`。
- [x] `XiaozhiDeviceConfig { device_id, name, room }`，TOML 别名 `mac` / `client_id`。
- [x] 身份归一化 `normalize_mac` / `device_id_from_client_id` / `normalize_device_id`：
      冒号、下划线、12 位连写、MQTT `client_id`（`@@@` 取 MAC 段）统一成冒号小写。
- [x] 重复设备（归一化后 MAC 相同）保留第一条并 warn；`name` 为空回退 MAC；
      `room` 为空不参与房间语义。
- [x] 启动安装：`src/cli.rs`（`haimen serve`）与 `src/gateway/mod.rs`（`start_all`）
      在构建策略/传输层之前调用 `install_devices_from_config`。
- [x] 单测：`normalize_mac_accepts_common_spellings`、
      `device_id_from_client_id_extracts_mac`、
      `directory_resolves_by_mac_client_id_and_name`、
      `directory_handles_rooms_and_context`、`duplicate_mac_keeps_first_entry`。
- [x] 配置反序列化单测：`test_xiaozhi_devices_parse_with_aliases`、
      `test_xiaozhi_devices_default_to_empty`（空表/旧 TOML 可加载）。

### 验收点

- [x] `[xiaozhi.devices]` 三种身份写法都能解析。
- [x] 不配置设备表时目录为空，不报错。

---

### T2 — 房间上下文注入（MQTT / WebSocket）

### 目标

配置了 `room` 的设备，在把用户文本交给 Agent 之前注入一行房间上下文；
只影响 LLM 输入，不影响设备屏幕 ASR 文本与日志。

### 完成内容（✅）

- [x] `ResponseStrategy` 新增钩子 `on_device_identified(session_key, device_id)`（默认 no-op，
      Echo 等既有策略不受影响）。
- [x] MQTT：`hello` 成功后用 `client_id` 里的 MAC 调用（`mqtt_udp.rs`）。
- [x] WebSocket：HELLO 握手成功后用 `Device-Id` 请求头调用（`ws.rs`）。
- [x] `AsrLlmTtsStrategy::contextualize_user_text`：查目录拿到「设备名 + 房间」，
      生成前缀 `【设备上下文】当前设备「客厅小智」位于「客厅」，…`；
      会话状态里没记录 MAC 时回退从会话键（MQTT 的 `client_id`）取。
- [x] 流式与非流式两条回复路径都改用 `agent_input`；`stt` 事件与日志仍用原始 `user_text`。
- [x] 单测：`room_context_injection_follows_device_directory`（注入 / 兜底 / 空房间不注入 /
      未配置设备不注入）。

### 验收点

- [x] 配了房间 → LLM 输入带前缀，且原始文本保留在末尾。
- [x] 未配置设备或 `room` 为空 → 文本逐字不变（与旧版本一致）。

---

### T3 — `say` 定向寻址（设备名 / 房间）

### 目标

`POST /api/v1/xiaozhi/say` 支持按配置的名字和房间寻址，明确 404/409 语义，
不广播；200 回显命中设备的 `client_id` / `name` / `room`。

### 完成内容（✅）

- [x] `device` 支持：`client_id` 精确、归一化 MAC、配置的 `name`（大小写不敏感）。
- [x] 新增可选字段 `room`：只匹配该房间**在线且唯一**的设备；多台 → 409。
- [x] 优先级 `device` > `room` > 唯一在线设备；`device` 与 `room` 同时给时 `device` 优先。
- [x] 404 `device_offline`：按 `room` 查不到时响应体回显 `"room": "厨房"`。
- [x] 409 `multiple_devices`：带候选列表 `[{client_id, device_id, name, room}]`。
- [x] 409 `device_busy`：带 `"state": "listening" | "playing"`（沿用旧逻辑）。
- [x] 200 响应新增 `client_id` / `name` / `room`；`device` 仍为归一化 MAC。
- [x] 单测：`device_selector_matches_device_name`、
      `room_selector_targets_the_only_online_device_in_room`、
      `room_selecting_multiple_devices_returns_409_with_candidates`、
      `unknown_room_returns_404_with_room_echo`。
- [x] 既有用例（忙 → 409、多台不指定 → 409）保持通过，未改变旧语义。

### 验收点

- [x] 指定设备名只下发给那台设备。
- [x] 房间内唯一设备时只下发给它；多台时报 409 且不下发。
- [x] 未知/离线房间报 404 并回显房间名。

---

### T4 — 设备清单接口

### 目标

`GET /api/v1/xiaozhi/devices`：配置 + 在线 + 忙闲状态合并视图，鉴权同 `say_token`。

### 完成内容（✅）

- [x] `notify::handle_devices` + 路由注册（`crates/haimen-xiaozhi/src/lib.rs`）。
- [x] 在线设备：来自 MQTT hub，`device_id` 归一化 MAC、`client_id`、`name`、`room`、
      `online=true`、`state`（idle/listening/playing）。
- [x] 已配置但离线：`client_id=null`、`online=false`、`state=null`，名字/房间取自目录。
- [x] 未配置的在线设备也列出（`name` 回退 MAC、`room` 空串）。
- [x] 单测：`devices_endpoint_merges_configured_and_online`。

### 验收点

- [x] `ok=true` 且 `devices` 是数组。
- [x] 在线/离线两类条目字段形状一致；离线 `client_id` 为 `null`。
- [x] `say_token` 非空时无 Bearer → 401。

---

### T5 — Web UI「小智设备」配置页 + settings API

### 目标

Web 控制台新增顶部「小智设备」页，维护设备与房间，保存后立即生效。

### 完成内容（✅ 已由并行任务合入）

- [x] `src/web/api/xiaozhi_devices.rs`：
  - `GET /api/v1/settings/xiaozhi/devices` 返回 `data.devices`（已配置，含离线）
    + `data.online`（MQTT 在线，带 `state` / `configured`）。
  - `PUT /api/v1/settings/xiaozhi/devices` 全量替换 `{"devices":[...]}`，
    校验后写 `settings.toml`，并立即 `install_devices_from_config` 重装目录。
- [x] 路由注册：`src/web/mod.rs`、`src/web/api/mod.rs`。
- [x] Web UI：`web-ui/src/pages/Settings/DeviceSettings.tsx`（顶部「小智设备」页）、
      `web-ui/src/api/xiaozhi.ts`、`web-ui/src/types/index.ts`、`App.tsx` 导航。
- [x] 参数校验：`device_id` 非空、归一化 MAC 不重复、≤ 256 台；非法输入 400
      且不写盘。
- [x] 单测：`put_persists_config_and_installs_directory`、`put_rejects_empty_device_id`、
      `put_rejects_duplicate_normalized_mac`、`get_returns_configured_devices_with_online_array`、
      `put_accepts_client_id_alias_and_empty_device_list`。
- [x] 前端构建/lint（`pnpm check` / `pnpm build`）—— 由并行任务在 Lead 机器上实跑通过，
      产物已更新到 `web-ui/dist/`。

### 验收点

- [x] 保存后设备目录立即更新（单测断言全局目录已重装）。
- [x] 校验失败不写盘（单测断言 `settings.toml` 不存在/未变）。
- [x] 保存后 `GET /api/v1/xiaozhi/devices` 立即反映新房间：已用临时 HOME + 真实二进制
      在 19527 端口联调（PUT 三台设备 → GET 立即看到新名字/房间，`settings.toml` 同步落盘）。
- [ ] 保存后按新 `room` 调 `say` 能命中：解析逻辑由 `notify::tests` 单测覆盖；
      websocket 模式没有 MQTT 设备，需真机/模拟设备联调。

---

### T6 — 文档与验收脚本

### 完成内容（✅）

- [x] `README.md`：主动播报小节后新增「多设备与房间」，文档索引加本规格包。
- [x] `specs/multi-device-room/02-design.md`：问题/现状/方案/配置/API 契约/兼容性/限制。
- [x] `specs/multi-device-room/03-tasks.md`：本文件。
- [x] `specs/multi-device-room/04-acceptance.sh`：可执行验收脚本（`HAIMEN_URL` /
      `SAY_TOKEN` / `TARGET_DEVICE` / `TARGET_ROOM`）。
- [x] `specs/notify-channel/HA-集成指南.md`：追加「按房间/按设备名通知」及 404/409 排查。

### 验收点

- [x] `bash -n specs/multi-device-room/04-acceptance.sh` 语法通过。
- [x] 脚本无参数时只做只读检查（`/devices` + `/health`）。
- [x] HA 指南原有内容未改动，只新增小节。

---

## 整体验收点

| # | 验收点 | 方式 |
|---|---|---|
| A1 | 旧配置零迁移：无 `[[xiaozhi.devices]]` 时一切照旧 | 单测 `test_xiaozhi_devices_default_to_empty`；空目录时 say 仍按 MAC 工作 |
| A2 | 房间注入只影响 LLM 输入 | 单测 `room_context_injection_follows_device_directory` |
| A3 | 按设备名定向 | 单测 `device_selector_matches_device_name` |
| A4 | 按房间定向（唯一设备） | 单测 `room_selector_targets_the_only_online_device_in_room` |
| A5 | 房间多台 → 409 候选列表，且不下发 | 单测 `room_selecting_multiple_devices_returns_409_with_candidates` |
| A6 | 房间无在线设备 → 404 回显房间 | 单测 `unknown_room_returns_404_with_room_echo` |
| A7 | 设备清单合并配置与在线状态 | 单测 `devices_endpoint_merges_configured_and_online` |
| A8 | 真实部署链路 | `HAIMEN_URL=http://127.0.0.1:9527 bash specs/multi-device-room/04-acceptance.sh`；可选 `TARGET_DEVICE=... TARGET_ROOM=...` |

后端单测运行方式（由 Lead/CI 统一执行，文档作者未在本机跑 cargo）：

```bash
cargo test -p haimen-xiaozhi --lib devices
cargo test -p haimen-xiaozhi --lib notify
cargo test -p haimen --lib room_context_injection_follows_device_directory
```

> 工具链：本仓库用 Docker 跑 Rust（`./scripts/dev.sh test`），本机不需要 Rust。
