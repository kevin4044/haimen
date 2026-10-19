#!/usr/bin/env python3
"""等设备进入 Idle 后发 notify，并检测设备是否真的去下载音频

为什么需要等空闲：notify 只在设备 Idle 状态生效，其他状态静默丢弃且无 ACK。
固件 2.5.2 起支持 notify；2.2.6 及更早没有这个能力。

判定成功的依据（两个都要满足）：
  1. haimen 日志出现「控制面消息已转发给设备」
  2. HTTP 服务日志出现来自设备（非 127.0.0.1）的 GET 请求

用法：
    python3 wait_idle_then_notify.py --url http://192.168.110.35:8899/notify.ogg
    python3 wait_idle_then_notify.py --url ... --quiet-sec 12 --timeout 180
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mqtt_link import MqttLink

DEFAULT_MAC = "84:c7:bb:59:93:64"
DEFAULT_BROKER = "192.168.110.35:1883"
CONTAINER = "haimen-test"
ACTIVITY_MARKERS = ("流式 ASR", "VAD 检测到", "主动播报")


def haimen_log(since_sec: int) -> str:
    try:
        out = subprocess.run(
            ["docker", "logs", CONTAINER, "--since", f"{since_sec}s"],
            capture_output=True, text=True, timeout=10,
        )
        return (out.stdout or "") + (out.stderr or "")
    except Exception:
        return ""


def read_log(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return ""


def device_get(new_text: str) -> str | None:
    for line in new_text.splitlines():
        if "GET" not in line:
            continue
        if line.startswith("127.0.0.1") or line.startswith("::1"):
            continue
        return line.strip()
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--broker", default=DEFAULT_BROKER)
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--url", default="http://192.168.110.35:8899/notify.ogg")
    ap.add_argument("--quiet-sec", type=float, default=12.0,
                    help="需要连续安静这么多秒才认为设备空闲")
    ap.add_argument("--timeout", type=float, default=180.0)
    ap.add_argument("--log", default="/tmp/http8899.log")
    args = ap.parse_args()

    safe = args.mac.replace(":", "_").lower()
    cid = f"GID_haimen@@@{safe}@@@{safe}"
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")

    print(f"目标设备: {cid}")
    print(f"音频 URL: {args.url}")
    print(f"等设备空闲（连续 {args.quiet_sec:g}s 无对话活动），最多等 {args.timeout:g}s")
    print()

    deadline = time.monotonic() + args.timeout
    while True:
        busy = any(m in haimen_log(int(args.quiet_sec)) for m in ACTIVITY_MARKERS)
        if not busy:
            print(f"[{time.strftime('%H:%M:%S')}] 设备已空闲")
            break
        if time.monotonic() > deadline:
            print(f"[{time.strftime('%H:%M:%S')}] ⚠️ 等待超时，仍然发送（可能被丢弃）")
            break
        time.sleep(2)

    before = len(read_log(args.log))
    print(f"[{time.strftime('%H:%M:%S')}] 发送 notify …")

    with MqttLink(host, int(port or 1883), client_id="haimen-notify-idle") as link:
        link.publish(ctl, {"type": "notify", "audio_url": args.url,
                           "subtitles": [{"start_ms": 0, "text": "客厅有点热"}]}, qos=0)
        time.sleep(0.5)

    # 等设备来下载音频（最多 15 秒）
    found = None
    end = time.monotonic() + 15
    while time.monotonic() < end:
        time.sleep(0.5)
        hit = device_get(read_log(args.log)[before:])
        if hit:
            found = hit
            break

    print()
    fwd = "控制面消息已转发给设备" in haimen_log(20)
    print(f"  {'✅' if fwd else '❌'} 服务端转发成功")
    if found:
        print("  ✅ 设备已下载音频 —— 主动播报成功")
        print(f"     {found}")
    else:
        print("  ❌ 设备没有下载音频")

    print()
    if found:
        print("请确认：设备是否播放了「客厅有点热，要不要开空调？」（会先有一声提示音）")
        return 0
    print("排查：")
    print("  · 设备是否真的在 Idle（屏幕显示「待命」）")
    print("  · 固件是否确实是 2.5.2（OTA 日志里的 app_version）")
    print(f"  · 设备能否访问 {args.url}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
