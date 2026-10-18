#!/usr/bin/env python3
"""让设备主动播报（notify 消息）

用法：
    # 播放一个已有的 Ogg Opus 文件（推荐先用这个验证链路）
    python3 send_notify.py --audio-url http://192.168.110.35:8899/notify.ogg

    # 带字幕
    python3 send_notify.py --audio-url http://.../a.ogg --text "客厅有点热"

    # 先本地生成音频再播（需要 ffmpeg + macOS say）
    python3 send_notify.py --speak "客厅有点热，要不要开空调？"

# 设备行为（官方文档）
# - notify 只在设备处于 Idle 时生效，其他状态静默丢弃（没有 ACK）
# - audio_url 必须是单声道 Ogg Opus，2xx，不跟随重定向
# - 播完回到 Idle，不会自动进入聆听
# - 每次播报前设备会先放一个提示音

注意事项：
- 设备空闲时不会发任何消息，无法靠 MQTT 判断它是否在线；
  只能看 haimen 日志里「控制面消息已转发给设备」是否出现。
- 如果转发成功但设备没反应，多半是它不在 Idle（比如还停在聆听中）。
  让设备断电重启、保持安静，再发一次。
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


def client_id_for(mac: str) -> str:
    safe = mac.replace(":", "_").lower()
    return f"GID_haimen@@@{safe}@@@{safe}"


def build_ogg_with_say(text: str, out_path: str) -> str:
    """用 macOS say + ffmpeg 生成单声道 Ogg Opus"""
    aiff = out_path.replace(".ogg", ".aiff")
    for cmd, desc in (
        (["say", "-v", "Tingting", text, "-o", aiff], "say 生成语音"),
        ([
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
            "-i", aiff,
            "-c:a", "libopus", "-b:a", "24k",
            "-ar", "24000", "-ac", "1",
            "-vbr", "off", "-frame_duration", "60",
            out_path,
        ], "ffmpeg 转 Ogg Opus"),
    ):
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError(f"{desc} 失败:\n{proc.stderr.strip()}")
    return out_path


def main() -> int:
    ap = argparse.ArgumentParser(description="让设备主动播报（notify）")
    ap.add_argument("--broker", default="192.168.110.35:1883")
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--audio-url", help="Ogg Opus 的 HTTP URL")
    ap.add_argument("--speak", help="本地生成音频的文本（需要 ffmpeg + say）")
    ap.add_argument("--text", help="可选字幕文本")
    ap.add_argument("--serve-dir", default="/tmp",
                    help="生成音频后从这里起 HTTP 服务的目录")
    ap.add_argument("--serve-port", type=int, default=8899)
    ap.add_argument("--serve-ip", default="192.168.110.35",
                    help="设备可达的宿主机 IP")
    args = ap.parse_args()

    if not args.audio_url and not args.speak:
        ap.error("需要 --audio-url 或 --speak 之一")

    url = args.audio_url
    if not url:
        out = os.path.join(args.serve_dir, "notify.ogg")
        print(f"生成本地音频: {out}")
        build_ogg_with_say(args.speak, out)
        url = f"http://{args.serve_ip}:{args.serve_port}/{os.path.basename(out)}"
        print(f"  URL: {url}")
        print(f"  ⚠️ 确认该 URL 可访问（需要在本机起 HTTP 服务，见 --serve-dir）")

    msg = {"type": "notify", "audio_url": url}
    if args.text:
        msg["subtitles"] = [{"start_ms": 0, "text": args.text}]

    cid = client_id_for(args.mac)
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")

    print(f"目标设备 client_id: {cid}")
    print(f"控制主题:          {ctl}")
    print(f"消息:              {json.dumps(msg, ensure_ascii=False)}")

    with MqttLink(host, int(port or 1883), client_id="haimen-notify-sender") as link:
        link.publish(ctl, msg, qos=0)
        time.sleep(0.5)

    print()
    print("已发布。接着请检查：")
    print("  1. haimen 日志是否出现「控制面消息已转发给设备」")
    print(f"  2. HTTP 服务是否收到设备的 GET {url}")
    print("  3. 设备是否出声（会先有一个提示音）")
    print()
    print("若第 1 步成功但第 2/3 步没反应 → 设备不在 Idle 状态。")
    print("处理：设备断电重启，保持安静（别说唤醒词），再发一次。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
