# 迁移 haimen 到 Mac mini（准备中）

状态：**未部署**，仅完成了编译可行性验证。

## ✅ 已确认的事实

### 1. 设备 OTA 地址可以改 —— 不需要重刷固件

**用户确认（2026-10-19）：OTA 地址在设备配网阶段可以指定。**

所以"服务器换机器导致设备拉不到 OTA"不是阻塞项 —— 配网时把 OTA 地址改到新机器即可。
（此前我从固件源码推断 `ota_url` 只读不写、以为必须重刷，这个推断不完整，
配网阶段有设置入口。）

### 2. UDP 跨网段没问题

两个网段通过路由器 IP 层路由（非 NAT）。实测 110.35 ↔ 50.33 UDP 往返成功，
对端看到的是真实源 IP。

### 3. 交叉编译可行

项目固定的工具链 `1.96-aarch64-apple-darwin` 交叉编译时 proc-macro 全部
`can't find crate`（tokio_macros / thiserror_impl / zerofrom_derive）；
换标准工具链即可：

```bash
rustup toolchain install stable --profile minimal
rustup target add x86_64-apple-darwin --toolchain stable

cd ~/works/haimen
MACOSX_DEPLOYMENT_TARGET=15.0 \
  cargo +stable build --release --target x86_64-apple-darwin
```

产物验证（实测）：
- `target/x86_64-apple-darwin/release/haimen`，20 MB
- `Mach-O 64-bit executable x86_64`
- `minos 15.0`（Mac mini 跑 15.7.5，兼容）
- 无第三方动态库依赖（只依赖系统库）

**为什么必须设 `MACOSX_DEPLOYMENT_TARGET=15.0`**：
编译机 macOS 27 / SDK 27，不设的话产物 minos = 27，Mac mini 跑不起来。

### 4. 端口与防火墙

| 端口 | 用途 | Mac mini 状态 |
|---|---|---|
| 9527/tcp | HTTP + OTA | 空闲 |
| 1883/tcp | MQTT broker | 空闲 |
| 8888/**udp** | Opus 音频 | 空闲（8888 TCP 被 Docker 占，但我们用 UDP） |

防火墙 ALF 已关闭。

## 🔧 Mac mini 侧待办

1. 拷贝二进制 → `~/haimen/bin/haimen`
2. `settings.toml`：`[xiaozhi.mqtt] endpoint` 改成 mini 的 IP；
   `auth_key` **不用改**（派生密码只依赖 auth_key + client_id，设备侧无感）
3. launchd plist 自启
4. 设备配网时把 OTA 地址指到 mini
5. 验证 `curl http://<mini-ip>:9527/xiaozhi/ota/`

## ⚠️ Mac mini 资源

- Intel i5-8500B（6 核 3.0GHz），**8 GB 内存**
- 已跑：Hermes（gateway + dashboard）、Docker Desktop（10 个进程）
- 内存偏紧 → **用原生二进制，不要再用 Docker 跑 haimen**
