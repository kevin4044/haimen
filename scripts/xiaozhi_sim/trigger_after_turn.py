#!/usr/bin/env python3
"""等设备完成一轮对话后，自动触发主动播报（测试「主动开启对话」）

原理：
  1. 轮询 haimen 容器日志，等出现「策略侧 VAD 检测到语音结束」
     （说明用户刚说完一句，设备马上要回答）
  2. 等 12 秒让 LLM + TTS + 播放完成
  3. 发送 __say_audio，让设备主动开口
  4. 设备播完后会进入聆听 —— 测试者直接说话就能验证「主动开启对话」成功

为什么必须等一轮对话：固件只有被唤醒过才有 UDP socket，
且距上次对话超过 120 秒会销毁。所以只能在刚说完话后的窗口内触发。

用法：
    python3 trigger_after_turn.py --ogg /tmp/say.ogg
    python3 trigger_after_turn.py --ogg /tmp/say.ogg --wait-after 15
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
CONTAINER = "haimen-test"

# 这些日志出现说明一轮对话正在进行
TURN_MARKERS = (
    "策略侧 VAD 检测到语音结束",
    "🎤 [ASR 完成]",
    "🎤 [ASR 兜底]",
)


def recent_log(since_sec: int = 10) -> str:
    try:
        out = subprocess.run(
            ["docker", "logs", CONTAINER, "--since", f"{since_sec}s"],
            capture_output=True, text=True, timeout=10,
        )
        return (out.stdout or "") + (out.stderr or "")
    except Exception:
        return ""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--broker", default=DEFAULT_BROKER)
    ap.add_argument("--mac", default=DEFAULT_MAC)
    ap.add_argument("--ogg", default="/tmp/say.ogg")
    ap.add_argument("--wait-after", type=float, default=12.0,
                    help="检测到用户说完后，等多久再播报（留出回答时间）")
    ap.add_argument("--timeout", type=float, default=180.0,
                    help="最多等多久检测一轮对话")
    args = ap.parse_args()

    if not os.path.isfile(args.ogg):
        print(f"❌ 找不到音频: {args.ogg}（先用 say.py --make-ogg 生成）")
        return 1

    frames = extract_ogg_packets(args.ogg)
    safe = args.mac.replace(":", "_").lower()
    cid = f"GID_haimen@@@{safe}@@@{safe}"
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")
    payload = {"type": "__say_audio", "frames_hex": [f.data.hex() for f in frames]}

    print(f"音频: {args.ogg}  {len(frames)} 帧 / 约 {len(frames) * 60}ms")
    print(f"目标: {cid}")
    print()
    print("👉 现在对设备说一句话（比如「你好」），我会在它回答完后自动触发主动播报")
    print(f"   最多等 {args.timeout:g} 秒")
    print()

    deadline = time.monotonic() + args.timeout
    detected = False
    while time.monotonic() < deadline:
        log = recent_log(since_sec=5)
        if any(m in log for m in TURN_MARKERS):
            detected = True
            print(f"[{time.strftime('%H:%M:%S')}] 检测到对话轮次，"
                  f"等 {args.wait_after:g}s 让回答播完…")
            break
        time.sleep(1.5)

    if not detected:
        print("❌ 超时：没检测到对话（你可能没说，或者 ASR 没识别到）")
        return 1

    time.sleep(args.wait_after)

    print(f"[{time.strftime('%H:%M:%S')}] 发送主动播报…")
    _cid, _user, _pwd = admin_credentials()
    with MqttLink(host, int(port or 1883), _user, _pwd, client_id=_cid) as link:
        link.publish(ctl, payload, qos=0)
        time.sleep(0.5)

    print()
    print("=" * 56)
    print("已发送。请观察设备：")
    print("  1. 它应该主动说：「客厅有点热，要不要开空调？」")
    print("  2. 说完应自动进入「聆听中」")
    print("  3. 这时你直接说「开」 —— 如果它回答了，说明主动开启对话成功 🎉")
    print("=" * 56)
    print()

    # 打印服务端后续日志便于判断
    time.sleep(6)
    log = recent_log(since_sec=12)
    for line in log.splitlines():
        if any(k in line for k in ("主动播报", "tts stop", "UDP 会话", "主动")):
            print(f"  [server] {line.strip()[:160]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
