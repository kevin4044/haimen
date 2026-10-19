#!/usr/bin/env python3
"""主动播报：让设备自己开口说话（走 tts 会话，老固件可用）

为什么不用 notify：设备固件 2.2.6 不支持 notify。
走 tts 会话：tts start → UDP 音频 → tts stop，设备播完会自动进入聆听。

前提（很重要）：
  1. 设备必须先被唤醒过一次（这样才有 UDP socket）
  2. 距上次对话不超过 120 秒（固件的 UDP 通道超时）
  3. 设备当前不在录音/播放状态

用法：
    # 1) 先把一段语音做成 Opus 帧（Ogg Opus 文件）
    python3 say.py --make-ogg "客厅有点热，要不要开空调？"
    # 2) 让设备开口
    python3 say.py --ogg /tmp/say.ogg

或一步到位（先生成再发）：
    python3 say.py --speak "客厅有点热，要不要开空调？"
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mqtt_link import MqttLink, admin_credentials
from opus_codec import extract_ogg_packets

DEFAULT_MAC = "84:c7:bb:59:93:64"
DEFAULT_BROKER = "192.168.110.35:1883"


def make_ogg(text: str, out: str) -> str:
    """用 macOS say + ffmpeg 生成单声道 Ogg Opus"""
    aiff = out.replace(".ogg", ".aiff")
    for cmd, desc in (
        (["say", "-v", "Tingting", text, "-o", aiff], "say"),
        ([
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
            "-i", aiff,
            "-c:a", "libopus", "-b:a", "24k",
            "-ar", "24000", "-ac", "1", "-vbr", "off", "-frame_duration", "60",
            out,
        ], "ffmpeg"),
    ):
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError(f"{desc} 失败: {proc.stderr.strip()}")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="主动播报（tts 会话 + UDP 音频）")
    ap.add_argument("--broker", default=DEFAULT_BROKER)
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--ogg", help="已生成的 Ogg Opus 文件")
    ap.add_argument("--speak", help="现场生成并播报的文本")
    ap.add_argument("--make-ogg", help="只生成 Ogg 文件，不发送")
    ap.add_argument("--out", default="/tmp/say.ogg")
    args = ap.parse_args()

    if args.make_ogg:
        make_ogg(args.make_ogg, args.out)
        print(f"已生成: {args.out}")
        return 0

    ogg = args.ogg
    if not ogg:
        if not args.speak:
            ap.error("需要 --ogg 或 --speak")
        print(f"生成语音: {args.speak}")
        ogg = make_ogg(args.speak, args.out)
        print(f"  → {ogg}")

    frames = extract_ogg_packets(ogg)
    print(f"Opus 帧数: {len(frames)}  （约 {len(frames) * 60} ms）")

    hex_frames = [f.data.hex() for f in frames]
    payload = {"type": "__say_audio", "frames_hex": hex_frames}

    safe = args.mac.replace(":", "_").lower()
    cid = f"GID_haimen@@@{safe}@@@{safe}"
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")

    # 消息体可能几十 KB，确认不超过 broker 上限（256 KiB）
    body = json.dumps(payload)
    print(f"消息体大小: {len(body)} 字节")

    _cid, _user, _pwd = admin_credentials()
    with MqttLink(host, int(port or 1883), _user, _pwd, client_id=_cid) as link:
        link.publish(ctl, payload, qos=0)
        time.sleep(0.5)

    print()
    print("已发送。设备应当：")
    print("  1. 立刻开始说这句话")
    print("  2. 说完自动进入聆听（屏幕显示「聆听中」）")
    print("  3. 这时你可以直接说话，它会回答 —— 这就是「主动开启对话」")
    print()
    print("若 haimen 日志出现「没有 UDP 会话」，说明：")
    print("  · 设备还没被唤醒过，或")
    print("  · 距上次对话超过 120 秒（UDP 通道已销毁）")
    print("  → 先对设备说一句话（唤醒），再 30 秒内重跑本脚本")
    return 0


if __name__ == "__main__":
    sys.exit(main())
