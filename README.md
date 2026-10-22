# 海门 - Haimen（个人 fork）

<p align="center">
  <img src="docs/public/logo.svg" alt="haimen logo" width="260" />
</p>

<p align="center">
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <a href="rust-toolchain.toml"><img src="https://img.shields.io/badge/Rust-1.96-orange.svg" alt="Rust 1.96"></a>
</p>

> 本仓库是 **[shenjingnan/haimen](https://github.com/shenjingnan/haimen)** 的 fork。haimen 是一个 AI 网关基建 CLI——上游 README 中的架构、消息渠道、Agent 与 Web 控制台等基础功能与用法在这里同样适用（安装方式除外），**本文不再重复，只说明本仓库与上游的差异**。

本仓库在 `dev` 分支上围绕 **小智 AI 硬件** 做了一轮深度改造：新增可常连的 MQTT + UDP 传输层与服务端主动播报接口，补齐语音链路的延迟/稳定性调优，并引入 OpenAI 兼容 Agent、Docker 化开发工具链和一套 `specs/` 规格文档。默认传输协议仍是 WebSocket，原有 WebSocket 部署不受影响；但 OTA 端点新增了私网来源限制，公网部署需按[已知限制](#已知限制)调整。

## 仓库与分支

| 分支 | 用途 |
| --- | --- |
| `main` | 上游镜像，不含本仓库修改 |
| `dev` | 本仓库集成分支，功能分支合并到这里 |
| `feat/*` | 功能开发分支 |

- 本仓库当前**没有 GitHub Release/tag，也未发布 crates.io 包**，安装方式见[快速开始](#快速开始)。
- `cargo install haimen` 安装的是**上游** 0.12.1；`haimen upgrade` 的发布源也硬编码为上游仓库，**不要**在 fork 构建上使用（它会把二进制换成上游版本）。
- 基线：上游 `v0.12.1`（2026-08-08）。

## 与上游的主要区别

| 方向 | 上游 | 本仓库 |
| --- | --- | --- |
| 小智传输 | 仅 WebSocket（会话式） | 新增 MQTT + UDP 常连传输，可与 WebSocket 切换 |
| 主动播报 | 无 | `POST /api/v1/xiaozhi/say`，经 MQTT `notify` 下发 |
| OTA 安全 | 无来源限制 | 默认只接受私网/回环来源，可配 `ota_token` |
| 固件 OTA 升级 | 无服务端版本管理 | Web UI 查看设备版本、匹配 [78/xiaozhi-esp32](https://github.com/78/xiaozhi-esp32/releases)，支持指定版本刷写 |
| 自定义桌面主题 | 官方主题页（浏览器生成 assets.bin） | Web UI 生成/上传 assets.bin，经设备 MCP 下发 `set_download_url` + `reboot`，不经过 LLM |
| 语音链路 | 能量阈值 VAD、批处理 ASR | Silero VAD、ASR 2.0、判停/超时/首帧/下行节奏调优 |
| Agent | 4 个 provider（以 CLI 子进程为主） | 新增 OpenAI 兼容 HTTP provider（SSE 流式） |
| 工程化 | 本机 cargo + GitHub Actions | Docker 化开发/CI 工具链、设备模拟器与端到端脚本 |
| 设计文档 | `docs/plans/` | 新增 `specs/` 规格包、验收脚本与运维文档 |

### 1. 小智传输层：MQTT + UDP

上游小智只支持 WebSocket：服务端只能被动处理设备已建立的连接，没有常连控制通道，无法主动呼叫设备。本仓库新增了一套与 WebSocket 平级的传输层：

- **传输切换**：`[xiaozhi] protocol = "mqtt"` 时，OTA 响应改下发 `mqtt` 段；设备重启拉取 OTA 后切换到 MQTT。默认仍是 `websocket`。
- **内置 MQTT broker**：最小 MQTT 3.1.1 实现（上行支持 QoS1 并回 PUBACK、下行固定 QoS0、keepalive、同 `client_id` 顶号），不依赖外部 Mosquitto。设备固件不做 SUBSCRIBE，因此 broker 直接向对应连接写 PUBLISH。
- **UDP 音频**：Opus 60ms/帧，AES-128-CTR 加密（16 字节包头即 CTR IV），单包 payload ≤ 1484 字节；预缓冲 5 帧 + 60ms 发送时钟。
- **鉴权**：`auth_key` 非空时，设备密码由 `HMAC-SHA256(auth_key, client_id)` 派生；`auth_grace` 用于设备分批重启的迁移期，旧静态密码仅对设备身份放行。控制面客户端固定为 `haimen-admin`。
- **OTA 来源校验**：默认只接受私网/回环来源（`ota_allow_private_only = true`），可配 `ota_token`、`ota_trusted_proxies`。MQTT 模式下 OTA 会下发派生密码，务必限制其可达范围。
- **会话策略**：录音 30s 上限、一轮结束后 15s 空闲发 `goodbye`、插话打断保留 UDP 密钥、按 `session_key` 隔离多设备状态。

> ⚠️ broker 是**明文 MQTT（无 TLS）**，UDP 也无重传；只建议在局域网/受控网络使用，并配置 `auth_key`。

详细实现与协议见 [实现方案](specs/mqtt-udp/实现方案.md)、[协议规格](specs/mqtt-udp/协议规格.md)。

### 2. 主动播报：`POST /api/v1/xiaozhi/say`

上游没有任何主动下发通道。本仓库提供文本播报接口，链路为：

```text
文本 → TTS(24kHz PCM) → Opus 60ms 帧 → Ogg 封装 → 一次性 HTTP URL(60s) → MQTT notify → 设备拉流播放
```

- **仅 MQTT 模式可用**；WebSocket 模式下返回 `404 device_offline`。
- 鉴权：`[xiaozhi] say_token` 非空时要求 `Authorization: Bearer <say_token>`；空串 = 不校验（内网自用默认）。
- 目标设备：`device` 可填 MAC 或 MQTT `client_id`；不填时要求恰好一台在线设备。
- 状态码：`200` 已下发（**不保证已播放**，notify 无 ACK）、`400` 请求语法错误或文本为空/超长、`401` 鉴权失败、`404` 设备离线、`409` 设备忙或在线设备不止一台（不排队、不重试）、`415`/`422` Content-Type 或请求体字段不合法、`502` 合成失败。

```bash
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/say \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <say_token>' \
  -d '{"text":"客厅有点热，要不要开空调？"}'
```

Home Assistant 集成见 [HA 集成指南](specs/notify-channel/HA-集成指南.md)，示例配置：[rest_command](specs/notify-channel/ha/xiaozhi_notify.yaml) 与 [门铃自动化](specs/notify-channel/ha/automation-doorbell.yaml)。

### 多设备与房间

家里有多台小智时，在配置里登记「设备身份 → 名字/房间」（`~/.haimen/settings.toml`，或数据目录里的 `settings.toml`）：

```toml
[[xiaozhi.devices]]
device_id = "84:c7:bb:59:93:64"   # MAC 任意写法（冒号/下划线/12 位连写）或 MQTT client_id
name = "客厅小智"                   # 可选，人读的名字；缺省回退为 MAC
room = "客厅"                       # 房间名；空 = 未分配
```

| 能力 | 行为 |
| --- | --- |
| 房间上下文注入 | 该设备对话时，送给 LLM 的用户输入前会加一行 `【设备上下文】当前设备「客厅小智」位于「客厅」，用户就在这个房间和你说话；回答时可以结合这个房间的语境。`（每轮注入）；设备屏幕 ASR 文本与 agent 日志仍是原始识别文本 |
| 定向通知 | `POST /api/v1/xiaozhi/say` 的 `device` 可填 MAC / client_id / 配置里的 `name`；新增 `room`，只发给该房间在线且唯一的设备（多台 → `409 multiple_devices`，不广播） |
| 设备清单 | `GET /api/v1/xiaozhi/devices` 返回配置 + 在线 + 忙闲状态（鉴权同 `say_token`） |

- 未配置设备或 `room` 为空 → 不注入，行为与旧版本一致；旧 `settings.toml` 零迁移。
- `say` 不指定 `device`/`room` 时，恰好一台在线才下发，多台返回 `409`（不广播）；`device` 与 `room` 同时给时以 `device` 为准。
- 房间上下文在 MQTT 与 WebSocket 两条传输都生效；`say`/设备在线状态只反映 MQTT 通路。
- Web 控制台顶部「小智设备」页可维护设备与房间，保存后立即生效。

```bash
# 按房间播报（房间名来自配置里的 room）
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/say \
  -H 'Content-Type: application/json' \
  -d '{"text":"开饭了","room":"客厅"}'

# 按设备名播报（也可以填 MAC / client_id）
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/say \
  -H 'Content-Type: application/json' \
  -d '{"text":"晚安","device":"客厅小智"}'

# 看每台设备的房间与在线/忙闲状态
curl -s http://127.0.0.1:9527/api/v1/xiaozhi/devices | python3 -m json.tool
```

详细设计、API 契约与限制见 [multi-device-room/02-design.md](specs/multi-device-room/02-design.md)，验收脚本：[04-acceptance.sh](specs/multi-device-room/04-acceptance.sh)。

### 3. 语音链路调优

针对真机上的延迟、卡顿、识别率问题，本仓库对语音链路做了这些调整：

- **Silero 神经 VAD**：替换原先的能量阈值判据（16kHz / 512 样本 / 32ms，双阈值 + 去抖），能量阈值仅作兜底；判停仍以云端 VAD + 文本稳定为准。
- **火山 ASR 2.0**：默认 `resource_id = volc.seedasr.sauc.duration`（可配置覆盖），批处理 ASR 超时由 30s 收紧到 6s。
- **判停与兜底**：`end_window_size` 默认 500ms；即使服务端来不及发 final，也会用最后一个中间识别结果兜底，不会返回空结果。
- **首帧提前**：首句按宽标点（`，、,~`）切分并立即发送，减少首字延迟。
- **下行节奏（MQTT/UDP 路径）**：5 帧预缓冲、60ms 绝对发送时钟、`tts stop` 前等待 `(预缓冲 + 2) × 60ms` 尾帧，缓解掉帧；WebSocket 路径为 5 帧预缓冲 + 60ms 时钟，无尾帧等待。
- **每次 hello 开新会话**：MQTT 路径在设备 hello 时调用 Agent 的 `drop_session`，避免上一轮历史影响新对话（WebSocket 路径暂未接入）。
- **诊断日志**：记录 Agent 回复全文与首块长度，便于定位「只说半句」类事故。

相关设计：[ASR/VAD 修复](specs/asr-vad-fix/02-design.md)、[音频节奏](specs/audio-pacing/02-design.md)、[ASR resource id 调研](specs/asr-resource-id/)。

### 4. OpenAI 兼容 Agent Provider

上游 Agent 以 CLI 子进程为主（OpenClaw 有 embedded 回退），单轮存在进程启动开销。本仓库新增 `openai` provider，走 HTTP `POST {base_url}/chat/completions`，语音路径使用 SSE 流式增量：

```toml
[gateway]
active_provider = "openai"

[gateway.providers.openai]
base_url = "http://192.168.1.10:8642/v1"  # 必填：OpenAI 兼容服务根地址
model = "your-model"                        # 必填：模型名
# api_key = "sk-..."                      # 可选：为空则不发送 Authorization
```

```bash
haimen agent run --provider openai "你好"     # 单次调用
haimen agent chat --provider openai           # 交互式多轮
haimen serve --xiaozhi-llm-provider openai    # 小智语音路径（流式）
```

限制：无 system prompt、无 tool/function calling；多轮历史在进程内保存（默认保留最近 20 条消息，约 10 轮）。provider 启动/切换时会调用 `GET {base_url}/models` 做可用性检查，服务需实现该接口。需要工具调用或人设时，请让 OpenClaw gateway 或自建上游服务处理。设计见 [openai-provider](specs/openai-provider/02-design.md)。

### 5. Docker 化开发与构建

本仓库用 Docker 承载编译、测试与运行，本机不需要安装 Rust；运行镜像只嵌入已入库的 `web-ui/dist`，Docker 构建也不需要 Node。

| 命令 | 作用 |
| --- | --- |
| `./scripts/dev.sh init` | 构建开发镜像 `haimen-dev:rust`（Rust 1.96 + fmt/clippy） |
| `./scripts/dev.sh check` / `fmt` / `clippy` / `test` / `verify` | 容器内跑对应 cargo 命令 |
| `./scripts/dev.sh build` | release 构建（产物在 Docker 命名卷 `haimen-target`） |
| `./scripts/dev.sh image` | 构建运行镜像 `haimen:local` |
| `./scripts/dev.sh run` / `stop` / `logs` | 起停运行容器、跟日志 |

本地 CI 用 `./scripts/ci.sh`（fmt + clippy + workspace 测试）。另有 `scripts/status.sh`（环境体检）、`scripts/check-agent-health.py`（Agent 调用健康度）、`scripts/cd.sh`（作者环境的 Mac mini 原生部署，按需修改）。这些脚本的默认路径/主机名绑定作者环境，使用前请按注释调整。

### 6. 设备模拟器与端到端测试

没有真机也能验证传输层：

- [`scripts/test-mqtt-udp.sh`](scripts/test-mqtt-udp.sh)：端到端验收。前置：先 `./scripts/dev.sh init` 构建 `haimen-dev:rust`，并给 Python 装 `paho-mqtt`；脚本会起容器内 demo 服务，用设备模拟器走完 MQTT 握手、UDP 建链、上行/下行音频，断言帧序号 / timestamp / tts 事件（默认 Python 路径可能需用 `PY=/usr/bin/python3` 覆盖）。
- [`scripts/xiaozhi_sim/device_sim.py`](scripts/xiaozhi_sim/device_sim.py)：按固件流程实现的设备模拟器；配套 `send_ctl.py`、`say.py` 等联调脚本。
- `./scripts/dev.sh shell` 后执行 `cargo run -p haimen-xiaozhi --example mqtt_udp_demo`：不依赖 ASR/LLM/TTS 的最小 demo（EchoStrategy）；本机已装 Rust 时也可直接跑。

### 7. `specs/` 规格与文档

本仓库把设计、任务拆解、机械验收脚本和运维记录都放进了 `specs/`（上游没有这个目录）：

| 文档 | 内容 |
| --- | --- |
| [known-issues.md](specs/known-issues.md) | 真机已知问题（KI-001…），带日志证据 |
| [mqtt-udp/实现方案.md](specs/mqtt-udp/实现方案.md) | 为什么做 MQTT+UDP、总体方案与取舍 |
| [mqtt-udp/协议规格.md](specs/mqtt-udp/协议规格.md) | MQTT/UDP 协议逐字段规格 |
| [notify-channel/HA-集成指南.md](specs/notify-channel/HA-集成指南.md) | Home Assistant 端到端集成 |
| [multi-device-room/02-design.md](specs/multi-device-room/02-design.md) | 多设备/房间语义、房间上下文注入与定向通知 |
| [openai-provider/02-design.md](specs/openai-provider/02-design.md) | OpenAI provider 设计与验收 |
| [audio-pacing/02-design.md](specs/audio-pacing/02-design.md) | 下行音频尾帧收口 |
| [asr-vad-fix/02-design.md](specs/asr-vad-fix/02-design.md) | ASR 空结果与 VAD 日志修复 |
| [运维-迁移到Mac-mini.md](specs/运维-迁移到Mac-mini.md) | 从容器迁到 Mac mini 原生部署的原因 |
| [运维-鉴权启用步骤.md](specs/运维-鉴权启用步骤.md) | MQTT 鉴权启用与 grace 收尾 |

### 8. 固件 OTA 升级（UI 指定版本）

Web 控制台的「小智设备」页新增「固件升级（OTA）」面板：

1. 设备开机拉 OTA 时会把当前固件版本、板型（`board.type` / `board.name`）、芯片型号和 OTA 分区大小上报给 haimen，面板即时可见。
2. 服务端从 [78/xiaozhi-esp32 releases](https://github.com/78/xiaozhi-esp32/releases) 拉取版本列表，用板型匹配 `release-manifest.json`，判断「可升级 / 已是最新 / 无此板型」。
3. 用户选择版本（或手动输入 `v2.4.0` 这类 tag）点击升级；haimen 会下载对应 release zip，提取出 **app 分区镜像**（release 里的 `merged-binary.bin` 不能直接给设备 OTA），存到本地后设置升级目标。
4. **重启设备**（拔电/复位）后，设备开机拉 OTA 会拿到 `firmware.version` + haimen 的下载地址 + `force=1`，从 haimen 拉取固件并刷写；刷完重启上报新版本后，目标自动清除，不会反复刷机。

关键限制与说明：

- 升级由用户显式触发，**不会后台自动升级**；设备只在开机时检查 OTA，所以设置目标后必须手动重启设备。
- 只更新 **app 分区**；assets/bootloader/分区表不变（与上游 OTA 行为一致）。
- 固件来自上游 `78/xiaozhi-esp32`，刷入会覆盖设备当前固件（包括自行编译的版本）；自定义板型名若不在上游 manifest 里会提示「无此板型」。
- 默认取最新的非 draft、非 prerelease；旧版本可在升级弹窗里手动输入 tag。
- GitHub API 未认证限流 60 次/小时；服务端缓存 10 分钟，可用 `GITHUB_TOKEN` 或 `HAIMEN_GITHUB_TOKEN` 提高限额。
- 状态与固件缓存位于 `~/.haimen/xiaozhi-firmware/`（`state.json` + `bin/`）。Docker 下该路径在现有数据卷 `/root/.haimen` 内，`./scripts/dev.sh run` 或 `docker run -v <host>:/root/.haimen` 会自动持久化到宿主机；也可用 `HAIMEN_XIAOZHI_FIRMWARE_DIR` 指定其他宿主机挂载目录。
- 固件管理端点与现有 settings API 一样不做鉴权；请只把 Web 控制台暴露在内网。

```bash
# 查看设备当前版本 / 最新 release / 目标
curl -s http://127.0.0.1:9527/api/v1/xiaozhi/firmware | python3 -m json.tool

# 指定版本升级（device 支持 MAC / client_id / 设备名）
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/firmware/upgrade \
  -H 'Content-Type: application/json' \
  -d '{"device":"84:c7:bb:59:93:64","version":"v2.5.2"}'

# 清除升级目标
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/firmware/target/clear \
  -H 'Content-Type: application/json' \
  -d '{"device":"84:c7:bb:59:93:64"}'
```

### 9. 自定义桌面主题（assets）

Web 控制台的「小智设备」页新增「自定义主题」面板（不重编固件、不使用 iframe）：

1. 选择设备，读取屏幕分辨率和 assets 分区大小。
2. 配置浅色/深色背景（纯色或图片）、21 个情绪表情、隐藏字幕；可选上传预编译 CBin 字体。
3. 浏览器在本地生成 `assets.bin`（格式与官方 `78/xiaozhi-assets-generator` 一致：12 字节头 + 44 字节文件表 + `0x5A5A` 数据 + checksum）。
4. 上传到 haimen 后，haimen 通过设备 MQTT 直接发 MCP `self.assets.set_download_url` + `self.reboot`；设备重启后自己下载并写入 assets 分区。**全程不经过 LLM/Agent。**
5. apply 时 haimen 会检查包内是否带唤醒词 `srmodels.bin` 和 `common` 中文字体；缺失时自动从官方固件 release 里提取 factory assets 补齐（首次需要联网，之后缓存到 `~/.haimen/xiaozhi-firmware/base-assets/`），避免主题包把唤醒词/中文渲染覆盖掉。

```bash
# 上传 assets.bin（octet-stream）
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/assets \
  -H 'Content-Type: application/octet-stream' --data-binary @assets.bin

# 下发到设备（file 用上传返回的文件名）
curl -X POST http://127.0.0.1:9527/api/v1/xiaozhi/assets/apply \
  -H 'Content-Type: application/json' \
  -d '{"device":"84:c7:bb:59:93:64","file":"assets-0123456789abcdef.bin"}'
```

关键限制：

- assets 分区只有一份、整体覆盖，**没有 A/B 回滚**；换主题前建议先用 `esptool read_flash` 备份 assets 分区。
- 只换 UI 资源，不改字幕布局/字号/位置；`hide_subtitle` 对 WeChat 气泡风格固件可能不生效。
- 如果设备 hello 的 `text_font.charset` 是 `basic` 而不是 `common`，中文/字幕文字会渲染不出来；haimen 的自动补齐会避免这个问题。
- 自定义字体沿用官方生成器的 CBin 格式；当前页面不把 TTF 转 CBin。
- assets 文件存于 `~/.haimen/xiaozhi-firmware/assets/`（随现有数据卷持久化）；端点与 settings API 一样不做鉴权，只建议内网使用。
- 详细协议与官方链路调研见 [docs/research/xiaozhi-custom-theme-2026-10.md](docs/research/xiaozhi-custom-theme-2026-10.md)。

### 10. 助手表情（emoji → `llm`）

Hermes 侧约定每条回复开头带一个情绪 emoji；haimen 会在 TTS 前做两件事：

1. 把开头的 emoji 映射成设备认识的 21 个 emotion 名称（如 `🙂 → happy`、`😔 → sad`）；
2. 通过 MQTT/WS 下发 `{"type":"llm","emotion":"happy"}`，并把 emoji 从 TTS 文本里剥掉，避免朗读表情。

支持的名称与 assets 主题的 21 个表情槽一致：`neutral / happy / laughing / funny / sad / angry / crying / loving / embarrassed / surprised / shocked / thinking / winking / cool / relaxed / delicious / kissy / confident / sleepy / silly / confused`。
常见 emoji 变体（如 `😊`、`😀`、`😡`、`😢`）会归一到对应名称；正文中间出现的 emoji 不会触发表情。

## 快速开始

### 方式一：Docker（推荐，本机不需要 Rust）

前置：Docker（脚本会校验 `docker` 命令与 daemon 可用）。

```bash
git clone -b dev https://github.com/kevin4044/haimen.git
cd haimen

./scripts/dev.sh image   # 构建运行镜像 haimen:local（首次会编译全部依赖）
# 先把带 ASR/TTS 凭证的 settings.toml 放进 ~/.haimen-dev/（容器内映射到 /root/.haimen）
./scripts/dev.sh run     # 启动容器，Web 控制台 http://localhost:9527
```

`~/.haimen-dev/` 由脚本创建；没有配置文件时，容器内默认的 `haimen serve` 会因缺少 ASR 凭证退出。只想先看 Web 控制台，可手动用 Echo 模式启动：

```bash
docker run --rm -p 9527:9527 -v ~/.haimen-dev:/root/.haimen haimen:local haimen serve --xiaozhi-echo
```

`./scripts/dev.sh logs` 看日志，`./scripts/dev.sh stop` 停止。

> `./scripts/dev.sh run` 默认只映射 `9527/tcp`。启用 `protocol = "mqtt"` 时还需映射 broker 与 UDP 端口（如 `-p 1883:1883 -p 8888:8888/udp`），否则设备连不上。

二次开发：

```bash
./scripts/dev.sh init     # 首次构建开发镜像
./scripts/dev.sh verify   # fmt + clippy + 全 workspace 测试
```

### 方式二：本机构建

需要 Rust 1.96（`rust-toolchain.toml` 已固定，rustup 会自动安装）与本地 C 工具链（`cmake`、`pkg-config`、C 编译器；opus2 的 bundled libopus 与 openssl-sys 需要）。源码构建会尝试用 pnpm 重建前端，无 Node 时本地告警并复用已入库的 `web-ui/dist`（CI 环境会直接失败）。

```bash
git clone -b dev https://github.com/kevin4044/haimen.git
cd haimen
cargo build --release
# 首次无凭证也能起 Web 控制台（小智语音需配置 ASR 凭证，未配置时自动跳过）
./target/release/haimen start
# 配置好 ASR/TTS 后，也可只起 HTTP 服务：
./target/release/haimen serve   # Web 控制台 + 小智 + GitHub Webhook（protocol=mqtt 时同时起 MQTT/UDP）
```

默认配置路径为 `~/.haimen/settings.toml`；配置结构与上游兼容，但 ASR 默认参数有调整（见第 3 节）。ASR/TTS 凭证等字段说明见上游 README。

## 配置（本仓库新增项）

上游配置项不变，下面只列本仓库新增的配置。完整字段与默认值见 [types.rs](crates/haimen-xiaozhi/src/types.rs)。

```toml
[xiaozhi]
protocol = "mqtt"              # "websocket"（默认）或 "mqtt"
say_token = ""                 # 非空 = /api/v1/xiaozhi/say 需要 Authorization: Bearer
ota_token = ""                 # 非空 = OTA 端点需要 ?token= 或 x-haimen-ota-token
ota_allow_private_only = true  # 默认只接受私网/回环访问 OTA
ota_trusted_proxies = []       # 仅对列表内对端读取 X-Forwarded-For

[xiaozhi.mqtt]
endpoint = "192.168.1.10:1883" # 必填：设备可访问的 broker 地址（缺端口按 1883）
username = ""
password = ""
auth_key = ""                  # 非空 = 启用 HMAC-SHA256 派生密码鉴权
auth_grace = false             # 迁移期：true 时旧静态密码仍可连（仅设备身份）
keepalive = 240
publish_topic_template = "haimen/xz/{mac}/up"
downlink_topic_template = "haimen/xz/{mac}/down"

[xiaozhi.udp]
bind_addr = "0.0.0.0:8888"

[gateway.providers.openai]
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:7b"
# api_key = "sk-..."
```

> `[xiaozhi] protocol` 修改后需要**重启设备**（设备只在开机时拉取 OTA）。
>
> ⚠️ 配置里的 `${env.VAR}` 只在钉钉 connector 中解析，`[gateway.providers.*]` **不会**做环境变量替换。`api_key = "${env.OPENAI_API_KEY}"` 会把字面量当作 token 发送，请直接写值（或留空）。

## 已知限制

- MQTT 传输仍属实验性：真机上存在「聆听停止过快/识别率下降」「TTS 轻微掉帧」等问题，见 [known-issues.md](specs/known-issues.md)；多设备并发未充分验证。
- MQTT broker 明文无 TLS；`auth_key` 未配置时，局域网内任何客户端都可向 `haimen/ctl/<client_id>` 发布控制消息。仅建议内网使用。
- OTA 默认仅接受私网来源。公网/反向代理部署需配置 `ota_token`（或按需调整 `ota_allow_private_only`）；Docker 端口映射下对端是网桥地址，来源判断会失真。
- `notify` 无 ACK、不排队：设备忙时直接丢弃，调用方需自行重试；`200` 只代表已下发。
- 固件 OTA 升级只更新 app 分区，且必须手动重启设备才会检查；升级管理端点未鉴权，只建议内网使用。
- 本仓库无 Release；`haimen upgrade` 会从上游仓库下载，勿用于本仓库构建。
- `specs/soul/` 只是人设 prompt 草稿，尚未接入代码，不是可配置功能。

## 上游同步

```bash
git remote add upstream https://github.com/shenjingnan/haimen.git   # 已有 upstream 可跳过
git fetch upstream
git checkout dev
git merge upstream/main
```

上游的 `docs/plans/`、Web 前端、CI/release 工作流均未修改，合并时按常规冲突处理即可。

## 许可

[MIT](LICENSE)。上游版权归 shenjingnan 及贡献者所有；本仓库的修改同样以 MIT 发布。
