# HA 集成指南：让 Home Assistant 通知 小智

> 目标：HA 里任何事件（门铃响、洗衣机洗完、有人开门、定时提醒…）
> 都能让 小智 主动开口说一句话。

## ✅ 已在本环境部署并验证（2026-10-20）

本文档描述的配置**已经在用户的 HA 上真实生效**，端到端验证通过：
HA 自动化 → `rest_command.xiaozhi_say` → haimen → 小智播报，用户确认听到。

**本环境实测信息：**

| 项 | 值 |
|---|---|
| HA 版本 | 2026.10.1 |
| HA 地址 | `http://192.168.110.34:80`（注意不是 8123） |
| 配置路径 | `/config`（软链到 `/homeassistant`） |
| 门铃实体 | `event.loock_cn_1215944731_401_doorbell_ring_e_7_1`（鹿客智能门锁自带的可视门铃） |
| `rest_command` | 追加在 `/config/configuration.yaml` 末尾（原本没有 packages） |
| 自动化 | `automation.men_ling_xiang_liao_rang_xiao_zhi_bo_bao`，写在 `/config/automations.yaml` |

**改动的备份：**
```
/config/configuration.yaml.bak-20261020-112323
/config/automations.yaml.bak-20261020-112631
```

**回滚：**
```bash
sudo cp /config/configuration.yaml.bak-20261020-112323 /config/configuration.yaml
sudo cp /config/automations.yaml.bak-20261020-112631 /config/automations.yaml
# 然后重启 HA 核心
```

**⚠️ 一个坑**：鹿客门锁把同一个物理门铃暴露成两个 event 实体
（`..._e_5_1006` 是「门锁 门铃」，`..._e_7_1` 是「门锁 可视门铃」，
两者状态相差约 1 秒）。**只用一个**，两个都配会让小智说两遍。

## 一句话结论

**用 HA 自带的 `rest_command` + 自动化，不需要写自定义集成。**

```
HA 事件（门铃/传感器/定时）
   ↓
HA 自动化 action → rest_command.xiaozhi_say
   ↓ HTTP POST
haimen POST /api/v1/xiaozhi/say
   ↓ 合成 + 编码 + 托管 + 下发 notify
小智「叮」一声，然后说话
```

---

## 第一步：在 `configuration.yaml` 里定义 rest_command

```yaml
rest_command:
  xiaozhi_say:
    url: "http://192.168.50.33:9527/api/v1/xiaozhi/say"
    method: POST
    content_type: "application/json"
    # {{ message }} 由自动化传入；to_json 负责转义引号/换行，别自己拼字符串
    payload: '{"text": {{ message | to_json }}}'
    timeout: 20
    # 如果 haimen 配了 say_token，取消下面两行注释
    # headers:
    #   Authorization: "Bearer <你的 say_token>"
```

**改完要重启 HA 核心**：开发者工具 → YAML → 「重启」。
（`rest_command` 不支持热重载，改 `configuration.yaml` 必须重启。）

### 想指定设备（多设备时）

```yaml
  xiaozhi_say_to:
    url: "http://192.168.50.33:9527/api/v1/xiaozhi/say"
    method: POST
    content_type: "application/json"
    payload: '{"text": {{ message | to_json }}, "device": {{ device | to_json }}}'
    timeout: 20
```
`device` 传 MAC（`84:c7:bb:59:93:64`）或 haimen 的 client_id 都行。

### 按房间/按设备名通知

在 haimen 的 `[xiaozhi.devices]` 里给设备配好 `name` / `room`（Web 控制台顶部「小智设备」页也能改，保存后立即生效）：

```toml
[[xiaozhi.devices]]
device_id = "84:c7:bb:59:93:64"
name = "客厅小智"
room = "客厅"
```

HA 侧加两个 `rest_command`：

```yaml
  xiaozhi_say_to_room:
    url: "http://192.168.50.33:9527/api/v1/xiaozhi/say"
    method: POST
    content_type: "application/json"
    payload: '{"text": {{ message | to_json }}, "room": {{ room | to_json }}}'
    timeout: 20

  xiaozhi_say_to_device:
    url: "http://192.168.50.33:9527/api/v1/xiaozhi/say"
    method: POST
    content_type: "application/json"
    payload: '{"text": {{ message | to_json }}, "device": {{ device | to_json }}}'
    timeout: 20
```

- `device` 可以传 MAC、client_id，或配置里的 `name`（如「客厅小智」）。
- `room` 传配置里的房间名（如「客厅」）。
- 两个字段同时给时以 `device` 为准。

门铃只响客厅（其他房间的小智不播）：

```yaml
alias: 门铃只响客厅小智
trigger:
  - platform: state
    entity_id: binary_sensor.doorbell          # ← 换成你的门铃实体
    to: "on"
action:
  - service: rest_command.xiaozhi_say_to_room
    data:
      message: "门铃响了喔，有人在门口等。"
      room: "客厅"
mode: single
```

**注意**：房间语义是「不广播」的 —— `room` 只在该房间**在线且唯一**的设备上播。
房间内多台在线设备会返回 409，不会猜一台。

排查 404 / 409：

| 返回 | 含义 | 排查方式 |
|---|---|---|
| `404 device_offline`（响应体带 `"room":"厨房"`） | 该房间没有在线设备 | 房间名写错？设备离线？先看下面设备清单的 `room` / `online` |
| `409 multiple_devices` | 房间内有多台在线设备 | 响应体 `devices` 里挑一台，用 `xiaozhi_say_to_device` 按 MAC/client_id/name 重发 |
| `409 device_busy` | 目标设备正在聆听/播放中 | 同前文「处理设备正忙」：HA 侧重试或丢弃 |

设备清单 / 房间名核对（`say_token` 非空时加 `-H "Authorization: Bearer <token>"`）：

```bash
curl -s http://192.168.50.33:9527/api/v1/xiaozhi/devices | python3 -m json.tool
```

---

## 第二步：在开发者工具里手测

**开发者工具 → 服务（Services）** → 选 `rest_command.xiaozhi_say` →
YAML 模式填：

```yaml
message: "这是一条测试通知，听到说明链路通了"
```

点「调用服务」。**几秒内小智会「叮」一声然后说话。**

没反应的排查顺序：
1. HA 日志（设置 → 系统 → 日志）里有没有 `rest_command` 报错
2. 用 `curl` 直接打接口，看返回码（见下）
3. 设备是不是**不在 Idle**（在聆听/说话中）→ 接口会返回 409

```bash
curl -s -X POST http://192.168.50.33:9527/api/v1/xiaozhi/say \
  -H 'Content-Type: application/json' \
  -d '{"text":"curl 测试"}' | python3 -m json.tool
```

---

## 第三步：写自动化

### 例 1：门铃响了通知小智

```yaml
alias: 门铃响了让小智说话
trigger:
  - platform: state
    entity_id: binary_sensor.doorbell          # ← 换成你的门铃实体
    to: "on"
action:
  - service: rest_command.xiaozhi_say
    data:
      message: >-
        门铃响了喔，有人在门口等。
mode: single
```

**注意 `mode: single`**：门铃可能连响几次，不限制会同时发好几条，
小智会连续「叮」好几声。

### 例 2：洗衣机洗好了

```yaml
alias: 洗衣机洗完通知
trigger:
  - platform: state
    entity_id: sensor.washer_status            # ← 换成你的实体
    to: "finished"
action:
  - service: rest_command.xiaozhi_say
    data:
      message: "洗衣机洗好了，记得晾衣服喔。"
mode: single
```

### 例 3：定时提醒（带条件，只在有人时播）

```yaml
alias: 早上提醒带伞
trigger:
  - platform: time
    at: "07:30:00"
condition:
  - condition: state
    entity_id: person.wangjunlong
    state: "home"
action:
  - service: rest_command.xiaozhi_say
    data:
      message: "外头好像会下雨，出门记得带伞喔。"
mode: single
```

### 例 4：模板化内容（把传感器数值念出来）

```yaml
action:
  - service: rest_command.xiaozhi_say
    data:
      message: >-
        室内现在 {{ states('sensor.living_room_temperature') }} 度，
        {% if states('sensor.living_room_humidity') | float > 70 %}有点闷，要不要开一下除湿？{% else %}还挺舒服的喔。{% endif %}
mode: single
```

**⚠️ 模板里不要出现工程词汇**（entity_id、亮度百分比、K 值）——
这些会**被逐字念出来**。要变成人话，见下一节。

---

## 第四步：需要「人话」就用模板转一层

HA 的 `states()` 会给出原始值。直接念 `light.strip` / `30` / `5000K` 很难听。

**办法：用 `template` 实体做转换**，自动化里念转换后的值。

```yaml
# configuration.yaml
template:
  - sensor:
      - name: "客厅灯状态说人话"
        state: >-
          {% set b = state_attr('light.living_room_strip','brightness') %}
          {% if is_state('light.living_room_strip','on') %}
            {% if b is none %}开着{% elif b < 60 %}开得很暗{% elif b < 160 %}开到一半{% else %}开得很亮{% endif %}
          {% else %}关着{% endif %}
```

然后自动化里 `message: "客厅灯带{{ states('sensor.客厅灯状态说人话') }}"`。

---

## 第五步：处理「设备正忙」

`notify` **只在设备 Idle 时生效**，其他状态会被静默丢弃。所以接口会明确返回：

| 返回 | 含义 | 建议处理 |
|---|---|---|
| `200` | 已下发 | —— |
| `409 device_busy` | 设备在聆听/说话中 | 见下面三个方案 |
| `404 device_offline` | 设备离线 | 静默丢弃或换渠道（手机推送） |
| `502 tts_failed` | 语音合成失败 | 记日志，别重试（TTS 故障连发会更糟） |

### 方案 A：直接丢弃（最简单）

门铃响的时候用户大概率就在家、能听到门铃本身，丢一条通知不致命。

### 方案 B：延迟重试（推荐给门铃这类「错过了可惜」的事件）

```yaml
alias: 门铃（设备忙时等 30 秒再试）
trigger:
  - platform: state
    entity_id: binary_sensor.doorbell
    to: "on"
action:
  - service: rest_command.xiaozhi_say
    data:
      message: "门铃响了喔，有人在门口等。"
    response_variable: say_result
    continue_on_error: true
  - if:
      - condition: template
        value_template: "{{ say_result.status != 200 }}"
    then:
      - delay: "00:00:30"
      - service: rest_command.xiaozhi_say
        data:
          message: "刚刚有人按门铃喔。"
mode: single
```

**`response_variable` 需要 HA 2023.7+**（`rest_command` 支持返回响应体）。
不确定版本的话，先用方案 A；等确认了再加这段。

**更简单的替代**（不需要 `response_variable`）：
不管成功失败都延迟补一条，代价是设备空闲时会说两遍。一般不建议。

### 方案 C：排队模式 —— **本期不做**

在接口上加 `wait_secs`：设备忙时挂起请求，等 Idle 再下发。

**用户已决定：本期不做。** 理由是现阶段通知丢了就丢了，
后续再考虑在 haimen 侧加消息队列。

所以现在**只有方案 A 和 B 可选**。想要「错过了补一条」的效果，
就用方案 B 在 HA 侧自己重试。

---

## 常见问题

**Q：调用返回 200 但设备没出声？**
1. 设备不是 Idle（刚说完话还在聆听）→ 等它回待命再试
2. `audio_url` 里的 IP 设备访问不到 → haimen 的 `advertise_ip` 配错
3. 设备音量 0 或静音

**Q：和小智正在对话时门铃通知会不会打断它？**
不会。notify 在非 Idle 状态被**静默丢弃**，不会打断对话。
所以「设备忙」时必须靠上面的重试方案，否则通知就没了。

**Q：能不能一次通知多个设备？**
当前规格只做单设备（不带 `device` 时选唯一在线的那台）。多设备广播留待下一轮。

**Q：HA 挂了会不会影响小智？**
不会。HA → haimen 是单向 HTTP 调用，HA 不调用时完全没有依赖。

---

## 一句话总结

```
configuration.yaml 加一段 rest_command
   + 自动化里一条 service: rest_command.xiaozhi_say
   + data: message: "要说的话"
```
就完事了。**不需要 MQTT、不需要自定义集成、不需要改 HA 核心。**
