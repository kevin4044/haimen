#!/usr/bin/env python3
"""验证 UDP 音频通道在「固件 120 秒超时」之后是否仍然可用

背景：
- 固件 Protocol::IsTimeout() 是 120 秒，但超时【不会销毁 socket】
  （udp_ 只在 CloseAudioChannel() 里释放，而它只由 goodbye 触发）
- 我们传输层的 IDLE_LIMIT 是 240 秒，超过会清掉自己的会话记录

所以要在 [120s, 240s) 这个窗口里测：
  如果这时 tts 会话仍然能让设备播报并进入聆听
  → 说明固件超时根本不影响，主动问询随时可用，不需要任何保活

用法：
    python3 test_warm_channel.py                    # 说话后等 150 秒
    python3 test_warm_channel.py --wait 200         # 等更久
    python3 test_warm_channel.py --ogg /tmp/say.ogg

判定：
    ✅ 主动播报：开始 + 已发送 tts stop  → 服务端发出去了
    ✅ 之后出现「流式 ASR: 第一帧音频到达」→ 设备进入聆听（关键！）
"""

from __future__ import annotations

import argparse
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
TURN_MARKER = "策略侧 VAD 检测到语音结束"


def log_since(sec: int) -> str:
    try:
        out = subprocess.run(
            ["docker", "logs", CONTAINER, "--since", f"{sec}s"],
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
    ap.add_argument("--wait", type=float, default=150.0,
                    help="检测到对话结束后等多久再推（默认 150s，>120s 且 <240s）")
    ap.add_argument("--timeout", type=float, default=300.0,
                    help="最多等多久检测一轮对话")
    args = ap.parse_args()

    if not os.path.isfile(args.ogg):
        print(f"❌ 找不到音频 {args.ogg}（先 say ... 生成）")
        return 1

    frames = extract_ogg_packets(args.ogg)
    safe = args.mac.replace(":", "_").lower()
    cid = f"GID_haimen@@@{safe}@@@{safe}"
    ctl = f"haimen/ctl/{cid}"
    host, _, port = args.broker.partition(":")
    payload = {"type": "__say_audio", "frames_hex": [f.data.hex() for f in frames]}

    print(f"音频: {args.ogg}（{len(frames)} 帧 / 约 {len(frames) * 60}ms）")
    print(f"计划：检测到你说完后等 {args.wait:g} 秒再推 tts 会话")
    print(f"      这个窗口要 > 固件超时(120s) 且 < 我们清理(240s)")
    print()
    print("👉 现在对设备说一句话")
    print()

    # ① 等对话发生
    deadline = time.monotonic() + args.timeout
    found = False
    while time.monotonic() < deadline:
        if TURN_MARKER in log_since(6):
            found = True
            print(f"[{time.strftime('%H:%M:%S')}] 检测到你说完了")
            break
        time.sleep(1.5)

    if not found:
        print("❌ 超时：没检测到对话")
        return 1

    # ② 等固件超时窗口
    t0 = time.monotonic()
    print(f"等待 {args.wait:g} 秒（跨过固件 120s 超时）…")
    while time.monotonic() - t0 < args.wait:
        remain = args.wait - (time.monotonic() - t0)
        time.sleep(min(10, max(0.2, remain)))
    print(f"[{time.strftime('%H:%M:%S')}] 等待结束，推 tts 会话")

    # ③ 推 tts 会话
    mark = time.monotonic()
    _cid, _user, _pwd = admin_credentials()
    with MqttLink(host, int(port or 1883), _user, _pwd, client_id=_cid) as link:
        link.publish(ctl, payload, qos=0)
        time.sleep(0.5)

    # ④ 观察服务端日志
    sent_ok = False
    listening = False
    end = time.monotonic() + 20
    while time.monotonic() < end:
        time.sleep(1)
        log = log_since(int(time.monotonic() - mark) + 2)
        if "主动播报：开始" in log:
            sent_ok = True
        if "tts stop" in log and sent_ok:
            pass
        # tts stop 之后设备若进入聆听，会开始发麦克风音频
        if "流式 ASR: 第一帧音频到达" in log or "流式 ASR: 清空管道状态" in log:
            listening = True
            break

    print()
    print("=" * 56)
    print("结果")
    print("=" * 56)
    print(f"  {'✅' if sent_ok else '❌'} 服务端成功下发音频（主动播报：开始）")
    print(f"  {'✅' if listening else '❌'} 设备随后开始发麦克风音频（= 进入了聆听）")
    print()
    log = log_since(int(time.monotonic() - mark) + 2)
    for line in log.splitlines():
        if any(k in line for k in ("主动播报", "UDP 会话", "第一帧音频")):
            print(f"  [server] {line.strip()[:150]}")

    print()
    if sent_ok and listening:
        print("🎉 固件超时不影响 tts 会话 —— 主动问询随时可用，不需要任何保活")
        print("   请确认设备是否真的播报了那句话")
        return 0
    if sent_ok and not listening:
        print("⚠️ 播报发出去了，但设备没进入聆听")
        print("   → 可能需要先发一条消息刷新固件的 last_incoming_time_ 再走 tts")
        return 1
    print("❌ 服务端就没发出去（可能是我们自己的会话被清了）")
    return 1


if __name__ == "__main__":
    sys.exit(main())
