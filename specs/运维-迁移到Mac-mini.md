# 迁移 haimen 到 Mac mini（已完成）

状态：**haimen 已部署并端到端验证通过**（2026-10-19）。Silero VAD 未完成，见文末。

## 最终方案：原生二进制（不用 Docker）

### 为什么放弃 Docker

**Colima 不转发 UDP 端口** —— 实测：

```
mini 宿主机: lsof -nP -iUDP:8888  → 无任何监听（docker-proxy 也没起）
从 110 网段测: Connection refused (ICMP port unreachable)
对照 OrbStack: ✅ 正常
```

容器跑起来 `-p 8888:8888/udp` 后，宿主端口没有监听，UDP 包直接被拒。
**音频通道依赖 UDP，所以 Colima 方案不可用。**

（TCP 的 9527/1883 转发正常，OTA 和 MQTT 能连，唯独 UDP 不行。）

### 原生二进制部署

编译（在 Mac Studio，M4 Max）：

```bash
cd ~/works/haimen
MACOSX_DEPLOYMENT_TARGET=15.0 \
  cargo +stable build --release --target x86_64-apple-darwin
```

**两个关键点**：
1. **必须 `+stable`**：项目固定的 `1.96-aarch64-apple-darwin` 工具链交叉编译时
   proc-macro 全部 `can't find crate`（tokio_macros / thiserror_impl / zerofrom_derive）。
2. **必须设 `MACOSX_DEPLOYMENT_TARGET=15.0`**：编译机 macOS 27 / SDK 27，
   不设的话产物 minos=27，mini（15.7.5）跑不起来。

产物：20 MB，`Mach-O 64-bit executable x86_64`，`minos 15.0`，无第三方动态库依赖。

部署位置（mini）：

```
~/haimen/bin/haimen                       二进制
~/haimen/logs/haimen.log                  标准输出
~/.haimen/settings.toml                   配置
~/Library/LaunchAgents/ai.haimen.serve.plist   launchd 自启
```

launchd 用 `KeepAlive=true`，崩溃自动拉起。

## 配置差异（mini 版）

```toml
[xiaozhi.mqtt]
endpoint = "192.168.50.33:1883"   # 指向 mini 自己
auth_key = "<与 Studio 相同>"      # 派生密码只依赖 auth_key + client_id

[gateway.providers.openai]
base_url = "http://192.168.50.33:8642/p/u5bb6-u5ead-u7ba1-u5bb6/v1"  # Hermes 同机
```

## 端到端验证（真实语音）

用 macOS `say` 生成 16kHz 单声道 WAV，模拟设备连 mini 的 broker：

```
STT:        '你好，现在家里有什么灯开着吗？'
TTS:        start / sentence_start / sentence_start / stop
下行音频:    206 帧
序列号连续:   ✅
timestamp:   非 0、按 60 递增 ✅
平均帧间隔:   58.8ms（期望 60ms）
→ PASS
```

**整链（MQTT + UDP + 火山 ASR + Hermes + 火山 TTS）在 mini 上原生跑通。**

## 工具改动

`scripts/xiaozhi_sim/mqtt_link.py` 新增：
- `device_credentials(client_id)` —— 设备侧派生密码 `HMAC(auth_key, client_id)`
- `admin_credentials()` —— 控制面 `haimen-admin` 凭据

`device_sim.py` 连接时自动带上设备凭据（否则被 CONNACK 0x04 拒绝）。

## ⏳ Silero VAD：未完成，卡在 ONNX Runtime 获取

尝试过 `voice_activity_detector 0.2.1`（封装 Silero V5 + ort），失败：

```
ort-sys build.rs: Failed to download from
  https://cdn.pyke.io/0/pyke:ort-rs/ms@1.22.0/aarch64-apple-darwin.tgz
  timeout: global
```

实测该 CDN **能连但只有 7 KB/s**，包 19 MB → 约 45 分钟，`ort` 的全局超时先触发。

### 三条可选路径

| 方案 | 做法 | 成本 | 风险 |
|---|---|---|---|
| **A. tract-onnx（推荐）** | 纯 Rust 推理，只从 crates.io 取依赖；模型文件本地已有（`silero_vad.onnx` 2.2MB） | 自行实现模型 I/O（input `[1,576]` = 64 context + 512 chunk，state `[2,1,128]`，sr）约 200 行 | tract 对 LSTM 算子支持需验证 |
| B. 慢速下载 ONNX Runtime | 手动 curl 下载 19MB×2 平台（各约 45 分钟），`ORT_SKIP_DOWNLOAD=1` + `ORT_LIB_LOCATION` | 一次性等待 | 跨平台各要一份 |
| C. brew onnxruntime | `brew install onnxruntime`（1.31.0，下载快） | 低 | ort rc.10 期望 1.22，版本不匹配可能链接/运行失败 |

**建议走 A**：不依赖外部二进制，模型已在本机，纯 crates.io 依赖。

### 顺带确认：KI-003 已经被别的修复覆盖

Silero 要解决的"噪音误判"问题，`6bb434e` 已经通过**拆掉本地 VAD 抢判停权 + 不关 ASR 流**
解决了主要症状。Silero 现在的价值是：
1. 无语音超时更准（不会因噪音误判"用户说了"）
2. 更可解释的诊断
3. **barge-in（打断）的基础设施** —— 这是我们目前完全没有的能力

## 切换清单（等用户决定）

当前状态：**Studio 和 mini 各跑一个 haimen，设备仍连 Studio**。两个实例互不影响。

切换步骤：
1. 设备配网阶段把 OTA 地址改到 `http://192.168.50.33:9527/xiaozhi/ota/`
   （用户确认：OTA 地址在配网阶段可指定）
2. 设备重启后连到 mini
3. 验证通过后停掉 Studio 的容器：`docker rm -f haimen-test`
4. 回滚：配网改回 `http://192.168.110.35:9527/xiaozhi/ota/` + 重新起 Studio 容器

**注意**：两台机器的 `auth_key` 必须相同（否则设备派生密码对不上）。
