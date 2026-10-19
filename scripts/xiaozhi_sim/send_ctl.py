#!/usr/bin/env python3
"""向设备下发任意控制消息（走 haimen 的 haimen/ctl/<client_id> 控制面）

用途：联调时不用改 haimen 代码，直接把任意 JSON 推给设备。

用法：
    # 测试投递：alert 会播提示音 + 屏幕显示文字（最直观）
    python3 send_ctl.py --alert "主动播报测试"

    # 测试投递：stt 只在屏幕显示文字，不改变设备状态
    python3 send_ctl.py --stt "这是一条测试文本"

    # 发任意 JSON
    python3 send_ctl.py --json '{"type":"llm","emotion":"happy"}'

    # 主动播报（notify）
    python3 send_ctl.py --notify http://192.168.110.35:8899/notify.ogg
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mqtt_link import MqttLink, admin_credentials

DEFAULT_MAC = "84:c7:bb:59:93:64"
DEFAULT_BROKER = "192.168.110.35:1883"


def main() -> int:
    ap = argparse.ArgumentParser(description="向设备下发任意控制消息")
    ap.add_argument("--broker", default=DEFAULT_BROKER)
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--json", help="直接给 JSON 字符串")
    ap.add_argument("--alert", help="发 alert（播声音 + 显示文字）")
    ap.add_argument("--stt", help="发 stt（只显示文字）")
    ap.add_argument("--notify", help="发 notify（audio_url）")
    ap.add_argument("--subtitles-text", help="notify 的字幕文本")
    args = ap.parse_args()

    if args.json:
        payload = json.loads(args.json)
    elif args.alert:
        payload = {
            "type": "alert",
            "status": "通知",
            "message": args.alert,
            "emotion": "neutral",
        }
    elif args.stt:
        payload = {"type": "stt", "text": args.stt}
    elif args.notify:
        payload = {"type": "notify", "audio_url": args.notify}
        if args.subtitles_text:
            payload["subtitles"] = [{"start_ms": 0, "text": args.subtitles_text}]
    else:
        ap.error("需要 --json / --alert / --stt / --notify 之一")

    safe = args.mac.replace(":", "_").lower()
    cid = f"GID_haimen@@@{safe}@@@{safe}"
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")

    print(f"目标: {cid}")
    print(f"主题: {ctl}")
    print(f"消息: {json.dumps(payload, ensure_ascii=False)}")

    _cid, _user, _pwd = admin_credentials()
    with MqttLink(host, int(port or 1883), _user, _pwd, client_id=_cid) as link:
        link.publish(ctl, payload, qos=0)
        time.sleep(0.4)

    print()
    print("已发送。观察设备：")
    print("  --alert  → 会有提示音 + 屏幕显示文字")
    print("  --stt    → 只在屏幕显示文字")
    print("  --notify → 会先有提示音，然后播放远端音频")
    print()
    print("若设备毫无反应，检查 haimen 日志：")
    print("  docker logs haimen-test --since 1m | grep 控制面消息")
    return 0


if __name__ == "__main__":
    sys.exit(main())
