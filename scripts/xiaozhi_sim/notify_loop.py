#!/usr/bin/env python3
"""持续发送 notify，直到设备真的来下载音频（用来配合设备重启时的测试）

用法：
    python3 notify_loop.py                 # 每 6 秒一次，最多 15 次
    python3 notify_loop.py --interval 4 --times 30

用途：设备断电重启后需要 20-30 秒才进入 Idle。这个循环会在设备一进入
Idle 时立刻让它出声，避免人工掐时间。

判定成功的依据：HTTP 服务日志里出现**非 127.0.0.1** 的 GET 请求
（127.0.0.1 是本机自测，不算）。
"""

from __future__ import annotations

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mqtt_link import MqttLink, admin_credentials

DEFAULT_MAC = "84:c7:bb:59:93:64"
DEFAULT_URL = "http://192.168.110.35:8899/notify.ogg"


def ctl_topic(mac: str) -> str:
    safe = mac.replace(":", "_").lower()
    return f"haimen/ctl/GID_haimen@@@{safe}@@@{safe}"


def read_log(path: str) -> str:
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return ""


def device_request_in(new_text: str) -> str | None:
    """找出非本机来源的 GET 请求"""
    for line in new_text.splitlines():
        if "GET" not in line:
            continue
        if line.startswith("127.0.0.1") or line.startswith("::1"):
            continue
        return line.strip()
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--broker", default="192.168.110.35:1883")
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--interval", type=float, default=6.0)
    ap.add_argument("--times", type=int, default=15)
    ap.add_argument("--log", default="/tmp/http8899.log")
    args = ap.parse_args()

    before = read_log(args.log)
    base_len = len(before)
    topic = ctl_topic(args.mac)
    host, _, port = args.broker.partition(":")

    print(f"控制主题: {topic}")
    print(f"音频 URL: {args.url}")
    print(f"频率: 每 {args.interval:g}s，最多 {args.times} 次")
    print()
    print("👉 请现在给设备断电重启，然后保持安静（不要说唤醒词）")
    print()

    found = None
    _cid, _user, _pwd = admin_credentials()
    with MqttLink(host, int(port or 1883), _user, _pwd, client_id=_cid) as link:
        for i in range(args.times):
            link.publish(topic, {"type": "notify", "audio_url": args.url}, qos=0)
            print(f"[{time.strftime('%H:%M:%S')}] 第 {i + 1} 次发送", flush=True)

            deadline = time.monotonic() + args.interval
            while time.monotonic() < deadline:
                time.sleep(0.5)
                now = read_log(args.log)
                hit = device_request_in(now[base_len:])
                if hit:
                    found = hit
                    break
            if found:
                break

    print()
    if found:
        print("✅ 设备已下载音频 —— 主动播报链路打通")
        print(f"   {found}")
        return 0

    print("❌ 超时：设备没有来下载音频")
    print()
    print("排查方向：")
    print("  1. 设备是否重启完成（开机 + 联网约 20-30 秒）")
    print("  2. 设备是否处于 Idle（开机后若自动进了聆听就不生效）")
    print(f"  3. 设备能否访问 {args.url}（同网段？防火墙？）")
    print("  4. haimen 日志是否有「控制面消息已转发给设备」")
    return 1


if __name__ == "__main__":
    sys.exit(main())
