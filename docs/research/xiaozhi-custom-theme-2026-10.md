# 官方自定义主题（assets.bin）调研报告（2026-10）

> 分支：`feature/xiaozhi-custom-theme`；调研日期：**2026-10-21**。
> 场景：haimen 想给已刷 78/xiaozhi-esp32 固件的设备换 UI（字体 / 表情 / 背景 / 字幕），
> 判断能否复用官方「主题配置」链路，以及不动服务端、不重编固件能做到什么。
>
> **结论分级**（全文统一）：
> - 【源码确认】= 直接读过对应开源仓库源码/文档（版本见文末）；
> - 【推断】= 由源码行为或公开资料推导，未在真机/线上验证；
> - 【未知】= 闭源服务端内部实现或未核对的分支，不做编造。
>
> 相关但不同源：`specs/known-issues.md`「已调研：官方自定义 UI / 生成 assets」一节已按本报告更新。

---

## 0. 结论摘要

1. **主题配置不需要重编固件，也不需要 LLM/Agent/ASR/TTS。** 它是设备控制面的一条普通 MCP 工具链：
   `self.assets.set_download_url` + `self.reboot`，重启后固件自己从 HTTP 拉取并写入 **assets 分区**。【源码确认】
2. **assets.bin 是浏览器端生成的**，不是官方服务端生成：官方开源的 `78/xiaozhi-assets-generator`（Vue SPA，MIT）
   在浏览器里用 JS/WASM 完成字体转换、GIF 缩放、SPIFFS 打包；官方闭源服务端只负责 **设备 token 鉴权 + MCP proxy + 文件 transfer 托管**。【源码确认 + 推断】
3. **官方网页也是直接 `POST /api/messaging/device/tools/call`**（Bearer token），服务端把它转成设备控制面的
   `{"type":"mcp","payload":{JSON-RPC}}`；haimen 已直接掌握设备 MQTT/WS 连接，可在同一通道上实现等价控制面（本次新增 `DeviceMcpHub`），**可以完全绕开官方服务端**。【源码确认（两侧协议）+ 推断（服务端转发实现）】
4. **haimen 现有 `src/agents/mcp_client.rs` 是反向依赖**：它用 `rmcp` 以子进程方式连接外部 MCP Server
   （`claude mcp serve` 一类），与设备侧 MCP Server 无关，**不能复用**。【源码确认】
5. 能改的：自定义文字字体（含 glyph push 能力声明）、21 个情绪表情图、浅/深色皮肤（文字色/背景色/背景图）、
   `hide_subtitle` 开关、唤醒词模型（`srmodels`）。**不能改**：字幕字号/位置/内边距、气泡样式、布局结构
   ——这些是编译期宏（`CONFIG_USE_WECHAT_MESSAGE_STYLE` / `CONFIG_USE_MULTILINE_CHAT_MESSAGE`）。【源码确认】
6. 最大工程风险：**设备只有一个 assets 分区、整体覆盖、没有 A/B 回滚**；下载中断会留下 header 与 payload
   不一致的分区（启动时 checksum 校验失败 → 主题失效）。【源码确认（行为）+ 推断（后果）】

---

## 1. 职责划分

| 层 | 组件 | 开源/闭源 | 实际职责 | 证据 |
|---|---|---|---|---|
| 服务端 | `api.tenclass.net`（官方） | **闭源** | 设备 token 签发/校验；`/api/messaging/device/tools/list`、`.../tools/call` 做 MCP proxy；`wss://api.tenclass.net/transfer/` 文件托管/中转 | 【源码确认：生成器调用地址】；内部实现【未知】 |
| 主题页 | `78/xiaozhi-assets-generator` | 开源 **MIT** | Vue 3 SPA：配主题、**浏览器端生成 assets.bin**、在线烧录（走 transfer）、下载 assets.bin | 【源码确认】 |
| 固件 | `78/xiaozhi-esp32` v2.5.2 | 开源 | 解析 MCP 工具、下载写 assets 分区、解析 `index.json`、应用主题/字体/表情/字幕 | 【源码确认】 |

补充事实：
- 生成器仓库 `web/public/static/` 静态资源 **73.58 MB**（树内 315 个 blob 求和；`fonts` 31.64MB、
  `wakenet_model` 23.42MB、`multinet_model` 12.31MB、`emojis` 6.08MB），浏览器本地生成因此无需后端 API。【源码确认】
- 生成器最新提交约 2026-02（`pushed_at`），线上主题页是否与该 `main` 完全同版本【未知】；
  DFRobot 文章证实线上入口在 **xiaozhi.me 设备管理 → 主题配置 → 自定义**，且需设备在线。【源码确认（文章）】
- 官方主题页 URL 带 `?token=`（`useDeviceStatus.js` / `HomePage.vue` 从 `window.location.search` 取），
  token 即 Bearer；token 格式/有效期由闭源服务端决定【未知】。

---

## 2. 端到端链路

### 2.1 打开主题页 → 读设备信息

1. 浏览器打开 `…/theme?token=<TOKEN>`（线上为 xiaozhi.me 设备管理里的「主题配置 → 自定义」）。【源码确认（参数名）+ 推断（具体路由）】
2. `useDeviceStatus.js` 先 `POST /api/messaging/device/tools/list` 判在线，再并发调用 3 个 MCP 工具：
   `self.get_system_info`、`self.get_device_status`、`self.screen.get_info`。【源码确认】
3. 用途：`get_system_info` 拿 `chip_model_name` / `board.name` / `application.version` / `flash_size` /
   **`partition_table` 中 `assets` 分区大小**；`screen.get_info` 拿宽高/单色；`get_device_status` 判在线。
   分区大小用于生成前校验 `assets.bin` 是否装得下（`GenerateModal.vue`：`fileSize > assetsPartitionSize` 直接 alert）。【源码确认】
4. 请求体就是 MCP 工具名+参数：`{"name":"self.get_system_info","arguments":{}}`，Header `Authorization: Bearer <token>`。【源码确认】

### 2.2 浏览器端生成 assets.bin

`AssetsBuilder.generateAssetsBin()`：预处理自定义字体（FreeType WASM → CBin）→ `preparePackageResources()`
→ 图片/GIF 处理（`WasmGifScaler`，gifsicle wasm）→ `SpiffsGenerator.generate()` 打包
→ 先加 `index.json`，再按扩展名排序（`ext` → 文件名）写入表项与数据。【源码确认】
字体转换全部在浏览器完成（`BrowserFontConverter.js`，支持 `ttf/woff/woff2/otf`）。【源码确认】

### 2.3 在线烧录：transfer 会话

`HomePage.handleStartFlash()`（`WebSocketTransfer.js`）顺序固定：【源码确认】

1. 先 `self.get_device_status` 确认在线。
2. `wss://api.tenclass.net/transfer/?token=<TOKEN>` 建 WebSocket。
3. 发 `{"type":"create_file","fileName":"assets.bin","fileSize":<size>}`。
4. 服务端回 `{"type":"file_created","url":"<下载 URL>"}` → 页面拿到 URL。
5. 页面调用 MCP `self.assets.set_download_url {url}`（把 URL 写进设备 NVS）。
6. 页面调用 MCP `self.reboot`（不等返回值，设备会立刻断线）。
7. 设备重启并 HTTP GET 该 URL；transfer 服务向浏览器发 `{"type":"transfer_started"}`，
   浏览器开始按 **64 KB** 分块发二进制帧；服务端每收一块回 `{"type":"ack","bytesSent":N}`；
   全部发完收 `{"type":"transfer_completed"}`。页面等 `transfer_started` 有 60s 超时。【源码确认（协议消息/分块/超时）】
8. 数据面关键点：**浏览器是数据源，设备从官方 transfer 服务下载**；服务端如何缓存/转发
   `transfer_started` 的触发时机【未知】，按源码只能确认页面在该事件后才开始发数据。

### 2.4 设备侧：下载并应用

`Application::CheckAssetsVersion()`（开机激活阶段调用一次）：【源码确认 `application.cc` L382-440】

1. `assets.partition_valid()` 为假（板子无 assets 分区）→ 打日志跳过。
2. 读 NVS：`Settings settings("assets", true)` 的 key `download_url`。
3. 有 URL 就 **先 `EraseKey("download_url")` 再下载**（失败不会自动重试，需重新下发 URL+reboot）。
4. 弹「发现新资源」提示，等音频空闲 3s，进入 `kDeviceStateUpgrading`，`assets.Download(url)`。
5. `Assets::Download`：`GET url`，要求 200、`content_length > 12` 且 **≤ assets 分区大小**；
   按 flash 扇区擦除/写入，payload 从偏移 12 开始，**最后才写 12 字节 header**，成功后
   `InitializePartition()` 重新 mmap 并校验 checksum。【源码确认 `assets.cc` L489-681】
6. 没有待下载 URL（或下载成功后），都会执行 `assets.Apply()`（默认 `refresh_display_theme=true`）：
   解析 `index.json` → srmodels / 字体 / 表情 / 皮肤 → `display->SetTheme(current_theme)` → `hide_subtitle`。【源码确认 `assets.cc` L258-432】

失败路径：下载失败 → Alert「下载资源失败」→ 回 `kDeviceStateActivating`；
由于 header 最后写、旧 header 还在，**分区可能处于「旧头 + 新数据」状态**，下次启动 checksum 校验失败 → 分区失效。【源码确认（写入顺序）+ 推断（失效后果）】

---

## 3. assets.bin 二进制格式

布局（小端）：【源码确认 `SpiffsGenerator.js` + `assets.cc` L22-28 / L149-256】

```
offset 0   uint32 total_files            // 文件数
offset 4   uint32 checksum               // combined 数据的字节和 & 0xFFFF
offset 8   uint32 combined_data_length   // 表 + 数据总长
offset 12  表项[total_files]             // 每项 44 字节
           数据区                        // 每文件前 2 字节 0x5A 0x5A
```

- **表项 44 字节**（不是 32）：`char name[32]`（UTF-8，超 32 字节截断/告警）+ `uint32 size` +
  `uint32 offset` + `uint16 width` + `uint16 height`。`offset` 是相对数据区起点的偏移。【源码确认】
  > 注：部分二手资料把表项写成「32 字节」（混淆了 `name[32]` 与表项总长）。实际
  > `32+4+4+2+2=44`，固件 `sizeof(mmap_assets_table)` 与生成器写入长度一致，均为 44。
- 固件定位数据：`offset = 12 + 44 * total_files + table_entry.offset`；数据首字节必须是 `'Z' 'Z'`（0x5A5A），
  `GetAssetData` 返回的是 **跳过 0x5A5A 后** 的指针与 `size`。【源码确认 `assets.cc` L219-256】
- `checksum` 计算范围 = 表 + 数据（即 header 之后全部），不是 header 本身。【源码确认】
- 表里 `width/height` 由生成器用浏览器解析图片尺寸写入（特殊格式 `.sjpg/.spng/.sqoi` 读偏移 14 处的宽高）；
  **但 v2.5.2 固件只把 size/offset 放进 map，并未使用 width/height**。【源码确认】
- 容量限制：启动 mmap 时 `stored_len > partition_size - 12` 即失败；还要受 MMU free pages 限制
  （代码注释提到 ESP32-C3 的空闲 MMU 数据页可能小于完整的 1MB assets 分区，即便 assets.bin 本身能装下）；
  下载时 `content_length > partition_size` 直接拒绝。【源码确认】

---

## 4. index.json 字段

生成器 `generateIndexJson()` 实际写出的字段（v2.5.2 固件读取行为对照）：【源码确认】

| 字段 | 生成器 | 固件 v2.5.2 |
|---|---|---|
| `version` | 恒为 `1` | 数字且 **> 1 直接拒绝**（提示升级固件）；缺省不检查 |
| `chip_model` | 写（`config.chip.model`，如 `esp32s3`） | **不校验**（元数据，供人/其他工具看） |
| `display_config` | 写 `{width,height,monochrome,color}` | **不校验**；真实屏幕信息走 `self.screen.get_info` |
| `srmodels` | 写唤醒词包文件名（如 `srmodels.bin`） | 读取 → `srmodel_load` → 交给 AudioService；失败仅打日志 |
| `multinet_model` | 仅自定义 MultiNet 时写 `{language,duration,threshold,commands[]}` | **`assets.cc` 未解析**；唤醒词实际仍由 `srmodels` 加载（官方服务端是否另外用该字段【未知】） |
| `text_font` | 字体文件名 | 读 → `LvglCBinFont` → `display->SetTextFont()` |
| `text_font_meta` | **当前 main 不写** | 读 `charset/size/bpp/bundle`，合法才重新开启 glyph push；缺失则 glyph push 关闭 |
| `emoji_collection` | `[{name,file}]`（21 项） | 数组；每项 `name/file`，若带 `eaf` 字段则跳过（旧扩展）；文件不存在仅告警 |
| `skin.light` / `skin.dark` | `{text_color,background_color,background_image?}` | 颜色 `#RRGGBB`；`background_color` 同时设置聊天区背景色；`background_image` → `LvglCBinImage` |
| `hide_subtitle` | bool（默认 false） | `cJSON_IsBool` 时 `display->SetHideSubtitle()` |
| 旧 `icon_collection` / `layout` | 不生成 | v2.5.2 无解析；v2.0.0~v2.1.0 也只有 `emoji_collection` → 判定为 **v2 之前的历史字段（细节【未知】）** |

坑：`LvglTheme::ParseColor` 只认 `#` 开头且长度 ≥7 的颜色，**`#fff` 这类短写会变成黑色**；非法值不报错。【源码确认】

---

## 5. 表情（emoji）

- 固定 **21 个情绪名**（LLM 的 `emotion` 字符串直接用这些 key）：`neutral, happy, laughing, funny, sad,
  angry, crying, loving, embarrassed, surprised, shocked, thinking, winking, cool, relaxed, delicious,
  kissy, confident, sleepy, silly, confused`。【源码确认（README + 生成器数组）】
- 预置包：**Twemoji 32×32 PNG / 64×64 PNG**、**Noto GIF 64×64 / 128×128**（GIF 文案明确标注需要 PSRAM）。【源码确认】
- 自定义：生成器 UI 当前只接受 **PNG/GIF**（`validFormats=['png','gif']`）；固件侧把原始图片字节直接交给
  `LvglRawImage`（不做格式转换），非 GIF 由 LVGL 解码器渲染，因此 JPG/WEBP 理论上可行但**取决于编译的 LVGL
  decoder**，未逐一核对 →【推断】。GIF 走 `IsGif()` 魔数判断 + `LvglGif`（`gifdec`，每帧 ARGB8888 canvas），
  内存开销 = `5×w×h` 量级，**无 PSRAM 的设备不现实** →【源码确认（分配）+ 推断（"必须有 PSRAM"）】。
- 打包方式：**原始图片文件原样进 assets.bin**，SPIFFS 表记录宽高；固件 `LvglRawImage` 只填 `data/data_size`，
  `header.w/h=0`，实际尺寸由解码器得出。【源码确认】
- `neutral` 必填（生成器 UI 强校验），其余可选；未提供的情绪在固件里的回退是
  ① 自定义图缺失 → ② `noto_emoji_get_utf8(emotion)` 内置 Noto emoji 字体 → ③ material symbols 大图标。【源码确认】
- 驱动源：`application.cc` 收到 `{"type":"llm","emotion":"happy"}` → `display->SetEmotion()`；设备状态切换
  （listening/speaking）也会调 `SetEmotion`。【源码确认】

---

## 6. 字体与 glyph push

- 预置 8 个 CBIN 字体（生成器 `web/public/static/fonts/`）：阿里巴巴普惠体 **14/16/20/30px**（14 用 1bpp，
  其余 4bpp）+ 思源黑体 **14/16/20/30px**（14 用 1bpp，其余 4bpp）；charset 分别对应
  `DeepSeek 7405` / `Qwen 18409`。【源码确认（`FontConfig.vue` + 中文 locale）】
- 自定义：**TTF / WOFF / WOFF2 / OTF**，浏览器 FreeType(WASM) 转 **CBin**；字号 **8–80px**，
  bpp **1 / 2 / 4**（UI 三档）；charset 可选 Qwen/DeepSeek/GB2312/Latin1/完整字符集。【源码确认】
- `text_font` 加载成功后固件**先 `DisableTextFontGlyphPush()`**，只有 `text_font_meta` 同时满足
  `charset ∈ {basic, common}`、`size` 为正整数、`bpp ∈ {1,4}` 且等于字体实际 bpp、`bundle` 非空且 ≤64 字符，
  才重新开启 glyph push。【源码确认 `assets.cc` L287-329】
- **当前开源生成器不写 `text_font_meta`**（`generateIndexJson()` 只有 `text_font`）→ 用官方主题页自定义字体后，
  设备 `hello` 里 `features.glyph_push=false` 且不发 `text_font`，缺字（生僻字/emoji 文本）由设备本地字体兜底，
  云端不会补 glyph。【源码确认（固件+生成器两侧）；官方线上版本是否另有补充【未知】】
- bpp=2 的字体结构有效、能显示，但**永远不满足 glyph push 条件**（固件只接受 1/4）。【源码确认】
- glyph push 协议（服务端 ↔ 设备，控制面内嵌在 `tts` 的 `sentence_start` / `stt` 消息里，不改变文本/音频）：
  能力协商字段、每消息 ≤64 glyph、解码位图 ≤64 KiB、PSRAM 缓存 256 glyph / 64 KiB、无 PSRAM 只保当前批次
  ——细节见 `docs/glyph-push_zh.md`。【源码确认】

---

## 7. 背景与主题（skin）

- `skin.light` / `skin.dark` 各三个字段：`text_color`、`background_color`、`background_image`（可选）。
  颜色同时影响 `chat_background_color`；`background_image` 加载成 `LvglCBinImage` 挂到 theme。【源码确认】
- 生成器把用户图片转成 **RGB565 原始像素 + 28 字节 LVGL `lv_image_dsc_t` 头**
  （`header.magic=0x19, cf=RGB565(0x12), flags; w/h; stride; data_size; data 指针占位; reserved×2`），
  文件命名 `background_light.raw` / `background_dark.raw`，**cover 裁剪到屏幕分辨率**（短边铺满、居中裁切）。【源码确认】
  > 生成器 README 写「带 64 字节 header」，与代码（28 字节）不一致；以代码为准 —— **README 过时**。
- 运行时切浅/深色有 MCP 工具 `self.screen.set_theme {theme:"light"|"dark"}`（查 `LvglThemeManager`，
  不存在返回 false）；但它只切内置注册的两套 theme，不改颜色/背景图。【源码确认】
- 资源应用路径：`Assets::Apply()` → 改 theme 对象字段 → `display->SetTheme(current_theme)` 立即重绘。【源码确认】

---

## 8. 字幕（hide_subtitle）

- **唯一开关**：`index.json` 的 `hide_subtitle`（bool）。没有 MCP 工具、没有 NVS 命令、没有编译选项。
  且它只在该次 `Apply(refresh_display_theme=true)` 时解析；`self.screen.set_theme` 等运行期调用不会重新读它。【源码确认】
- 默认 UI（v2.5.x 未开编译宏）：`bottom_bar_` + `chat_message_label_`，**底部单行、超长循环滚动**（`LV_LABEL_LONG_SCROLL_CIRCULAR`）。【源码确认 `lcd_display.cc` L1002-1034】
- `CONFIG_USE_MULTILINE_CHAT_MESSAGE=y`：同一个 bottom bar 改成自动高度、`LV_LABEL_LONG_WRAP` 多行。【源码确认】
- `CONFIG_USE_WECHAT_MESSAGE_STYLE=y`：气泡 UI，**没有 `bottom_bar_`**，`hide_subtitle` 实际不生效
  （`SetHideSubtitle` 只操作 `bottom_bar_`）；该分支下消息由动态气泡承载。【源码确认】
- 字幕内容来源（都写到同一个 label / 气泡）：【源码确认 `application.cc`】
  - TTS `{"type":"tts","state":"sentence_start","text":…}` → `SetChatMessage("assistant", text)`；
  - STT `{"type":"stt","text":…}` → `SetChatMessage("user", text)`；
  - 主动通知 `notify` 的 `subtitles[{start_ms,text}]` 由播放进度回调逐条 `SetChatMessage("assistant", text)`；
  - 系统提示（配网/升级/音量等）→ `SetChatMessage("system", …)`。
- 字体/颜色/背景**复用主题**（`text_font` 与 theme 的 text_color/background_color），
  **没有独立的字幕字体、位置、字号、行数配置**；要改这些只能重编固件。【源码确认】

---

## 9. 关键澄清：MCP 是控制面直连，不需要 LLM / Agent / ASR / TTS

- 设备协议层收到 `{"type":"mcp","payload":<JSON-RPC 2.0>}` 后直接交给 `McpServer::ParseMessage()`，
  支持 `initialize`、`tools/list`、`tools/call`。与语音链路完全解耦：**不经过 LLM、不触发 ASR/TTS、不需要 Agent**。【源码确认 `application.cc` L677-682 + `mcp_server.cc` L431-470】
- 官方网页做的就是：`POST /api/messaging/device/tools/call`（Bearer token）→ 服务端找到该 token 绑定的设备
  → 下发 `type:"mcp"` 的 `tools/call`。【源码确认（网页侧）】服务端转发细节【未知】。
- `self.get_system_info`、`self.reboot`、`self.assets.set_download_url` 都是 **user-only** 工具（`AddUserOnlyTools()`），
  但 `DoToolCall` 本身不再做权限判断（权限在服务端/token 层）；因此**通过 haimen 自己的控制面通道直发等价 JSON-RPC 即可**。【源码确认（设备侧）+ 推断（绕过官方服务端合法可行）】
- haimen 的 `src/agents/mcp_client.rs` 用 `rmcp::transport::child_process::TokioChildProcess` 连的是
  **本机外部 MCP Server**（Agent 侧），与设备无关，**复用它属于方向错误**。【源码确认】

对 haimen 的直接含义：做「一键换主题」不需要接入官方云；只要（a）拿到资产的 HTTP URL，
（b）通过现有控制面发 `self.assets.set_download_url` + `self.reboot`。官方服务端只在
「浏览器 ↔ 设备」之间腾挪文件这一环不可替代（若不自建静态托管）。

---

## 10. 复用官方生成器：没有 iframe，三种可行方式

官方生成器是 Vue 3 + Vite SPA，依赖 `window.location.search` 的 `token`、同源 `/api/messaging/...`、
`wss://api.tenclass.net/transfer/`，静态资源 73.6MB。iframe 不可行的原因：跨域 token 传递、同源 API、
73.6MB 需另行托管、且页面会尝试直连官方 wss/API（本地部署时同源后端不存在）。【推断】

三种**无 iframe**方案：

| 方案 | 做法 | 成本 | 评价 |
|---|---|---|---|
| A. 模块级复用 + React 壳（**Phase A 推荐**） | 把 `SpiffsGenerator.js`、`AssetsBuilder.js`（纯 JS class）、`font_conv/*`（FreeType WASM）、`WasmGifScaler` 搬进 `web-ui`，用 React 重写配置表单；只保留「生成 assets.bin」能力，烧录改走 haimen 自己的上传/控制面 | 中（字体/WASM/GIF 依赖与 worker 需适配 Vite） | **不搬 73.6MB 预置资源**（字体/唤醒词模型可按需从官方 release 下载或内置子集）；与 haimen 现有 React 19 + Vite 8 技术栈一致 |
| B. 独立 `/theme-studio` Vue SPA，新标签页打开 | 原样部署生成器（或子集），与主 UI 解耦，token 由 haimen 自己的会话替代 | 中（多一套前端构建/路由/静态托管） | 适合先跑通；但 Vue + React 双栈维护成本高 |
| C. 只复用 assets.bin 格式 | 按本报告第 3、4 节自己实现打包（TS），不复用其 UI/字体转换 | 高（字体 CBin 转换、GIF 处理要重写） | 仅当只需要「换内置主题/背景/表情」时划算 |

Phase A 的最小可行切片：主题 JSON（颜色/背景/hide_subtitle/21 表情）→ 浏览器打包 → 上传到 haimen 静态目录
→ 控制面 `self.assets.set_download_url` + `self.reboot`。字体与 GIF 放到 Phase B。

---

## 11. 风险与坑（换主题前必读）

1. **单分区整体覆盖、无回滚**：assets 分区只有一份，新包会替换掉厂包（可能含唤醒词 `srmodels`、字体、表情）。
   新包若缺 `srmodels`，唤醒词行为可能退化（旧包已不可恢复）。建议先允许「导出当前分区」或保留厂包 URL。【推断（后果）+ 源码确认（单分区/覆盖）】
2. **下载中断 → 分区失效**：header 最后写，失败时多半是「旧 header + 半新数据」，checksum 校验失败 →
   `partition_valid_=false`，主题/字体/表情全部回退内置，**且 `download_url` 已被清空，不会自动重试**。【源码确认】
3. **容量**：`assets.bin` 必须 ≤ 分区大小（常见 1MB / 4MB 级），且受 MMU 空闲页限制；生成器只在有
   `get_system_info` 数据时才前置校验。【源码确认】
4. **`hide_subtitle` 在气泡 UI 下无效**；改字幕字号/位置只能重编固件。【源码确认】
5. **自定义字体 = 失去 glyph push**（生成器不写 `text_font_meta`），生僻字只能靠本地字符集，建议字体 charset
   选大一点（Qwen 18409 / full）。【源码确认】
6. **颜色短写坑**：`#fff` → 黑色；必须 `#RRGGBB`。【源码确认】
7. **`multinet_model` 未被固件 `assets.cc` 消费**：自定义唤醒词是否真的生效取决于 `srmodels` 与固件版本，别只看 index.json。【源码确认】
8. **官方服务端不可控**：`api.tenclass.net` / transfer 是闭源且可能变更；haimen 若依赖它做中转，需容错与降级。【推断】
9. 生成器 README 与代码有出入（背景 header 64 vs 28 字节），以代码为准；线上部署版本与 `main` 是否一致【未知】。

---

## 12. 明确未知 / 待验证

- 官方服务端 MCP proxy 与 transfer 的内部实现、token 结构与权限模型。【未知】
- 线上主题页与开源仓库 `main` 的版本差异；官方是否在别的路径生成 `text_font_meta`。【未知】
- 设备固件对 JPG/WEBP 表情的实际解码支持（取决于 sdkconfig 的 LVGL decoder）。【未知】
- `icon_collection` / `layout` 旧字段的确切 schema（v2.0+ 已无解析）。【未知】
- 各板型 assets 分区实际大小、以及 `download_url` 下载超时/重试策略（固件内无重试）。【部分未知】
- 真机验证：自定义字体显示、GIF 表情内存、hide_subtitle 生效、错误注入（断网/断电）后的分区状态。
  **本报告未做真机实验。**

---

## 13. 证据链接

固件 `78/xiaozhi-esp32`（tag **v2.5.2**）：
- assets 格式/应用/下载：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/assets.cc>
- 字幕/表情/主题：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/display/lcd_display.cc>
- MCP 工具与服务端实现：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/mcp_server.cc>
- 控制面消息分发与 CheckAssetsVersion：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/application.cc>
- glyph push 协议：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/docs/glyph-push_zh.md>
- 协议 hello 能力声明：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/protocols/protocol.cc>
- GIF 解码（内存分配）：<https://github.com/78/xiaozhi-esp32/blob/v2.5.2/main/display/lvgl_display/gif/gifdec.c>

生成器 `78/xiaozhi-assets-generator`（`main`，MIT）：
- 仓库/README（字体、21 表情、格式说明）：<https://github.com/78/xiaozhi-assets-generator>
- assets.bin 打包：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/utils/SpiffsGenerator.js>
- index.json / 资源编排：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/utils/AssetsBuilder.js>
- 浏览器端烧录流程：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/views/HomePage.vue>
- transfer 协议：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/utils/WebSocketTransfer.js>
- 设备信息 MCP 调用：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/composables/useDeviceStatus.js>
- 字体配置页：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/components/tabs/FontConfig.vue>
- 背景转换（RGB565 + 28B header）：<https://github.com/78/xiaozhi-assets-generator/blob/main/web/src/components/tabs/BackgroundConfig.vue>

第三方/社区：
- DFRobot 教程（xiaozhi.me 主题配置入口、在线烧录 assets.bin 实测）：<https://mc.dfrobot.com.cn/article-385358.html>

---

## 14. 对 haimen 的下一步建议（不含实现）

1. **先别接官方云**：用现有控制面通道复刻 `self.assets.set_download_url` + `self.reboot`，配一个 haimen 侧
   静态文件 URL 即可端到端验证「换主题」。（控制面消息格式见 `specs/mqtt-udp/协议规格.md`）
2. **Phase A**：React 侧只做「生成 assets.bin（颜色/背景/hide_subtitle/表情）」+ 上传 + 触发，按本文第 3、4 节实现打包器；
   字体/glyph push/自定义唤醒词放 Phase B。
3. **必须先做**：备份/恢复路径（读回 assets 分区或保留厂包 URL）、失败注入测试、分区大小与 MMU 余量检查。
4. 若产品要「字幕字号/位置可调」，assets 方案做不到，需评估自行编译固件（`CONFIG_USE_WECHAT_MESSAGE_STYLE` /
   多行字幕 / 改 `chat_message_label_` 样式）的维护成本。
