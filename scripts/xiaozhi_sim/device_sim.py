#!/usr/bin/env python3
"""小智设备模拟器（MQTT + UDP）

用途：在没有真机的情况下验证 haimen 的 MQTT+UDP 传输层。
行为尽量贴近固件（源码依据见 specs/mqtt-udp/协议规格.md）。

模拟的固件行为：
1. 用 client_id 连 MQTT（明文 1883；8883 会强制 TLS）
2. 连上后**静默**，不主动发任何消息
3. 被"唤醒"时才发 hello（transport=udp），然后只等 10 秒
4. 收到 hello 响应后建 UDP（本地 connect()，不发包）
5. 发 listen start → 流式发 Opus 音频 → 发 listen stop
6. 接收下行：stt / tts / UDP 音频
7. 设备侧 timestamp 恒为 0（默认不启用服务端 AEC）

用法：
    python3 device_sim.py --broker 127.0.0.1:1883 --wav test.wav
    python3 device_sim.py --broker 127.0.0.1:1883 --duration 3 --verbose

退出码：0 = 全链路成功；非 0 = 失败（便于脚本判定）
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from aes_ctr import backend as aes_backend
from mqtt_link import MqttLink
from opus_codec import make_test_wav, wav_to_opus_frames
from udp_packet import (
    UdpCrypto,
    UdpPacketError,
    parse_hex16,
    validate_server_nonce,
)

DEVICE_MAC = "84:c7:bb:59:93:64"
FRAME_MS = 60


class DeviceSim:
    def __init__(self, broker: str, mac: str = DEVICE_MAC,
                 verbose: bool = False):
        host, _, port = broker.partition(":")
        self.broker_host = host
        self.broker_port = int(port or 1883)
        self.mac = mac
        self.mac_safe = mac.replace(":", "_")
        self.client_id = f"GID_haimen@@@{self.mac_safe}@@@{self.mac_safe}"
        self.publish_topic = f"haimen/xz/{self.mac_safe}/up"
        self.verbose = verbose

        self.mqtt: MqttLink | None = None
        self.sock: socket.socket | None = None
        self.crypto: UdpCrypto | None = None
        self.session_id: str | None = None

        # 下行统计
        self.rx_audio: list[tuple[int, int, int, float]] = []  # (seq, ts, len, wall)
        self.rx_json: list[dict] = []
        self._rx_lock = threading.Lock()
        self._rx_stop = threading.Event()

    # ── 日志 ──────────────────────────────────────────────

    def log(self, msg: str) -> None:
        print(f"  [sim] {msg}")

    def vlog(self, msg: str) -> None:
        if self.verbose:
            print(f"  [sim·v] {msg}")

    # ── MQTT ──────────────────────────────────────────────

    def connect_mqtt(self, timeout: float = 10.0) -> None:
        self.log(f"连接 broker {self.broker_host}:{self.broker_port} client_id={self.client_id}")
        self.mqtt = MqttLink(
            host=self.broker_host,
            port=self.broker_port,
            client_id=self.client_id,
            keepalive=240,
        )
        self.mqtt.connect(timeout=timeout)
        # 订阅一个宽主题：标准 broker 靠它路由；
        # 我们自建的 broker 是直接推送（设备本身不订阅），这里订阅只为兼容外部 broker。
        self.mqtt.subscribe("#", qos=0)
        self.log("MQTT 已连接")

    def send_json(self, obj: dict) -> None:
        assert self.mqtt is not None
        self.mqtt.publish(self.publish_topic, obj, qos=0)
        self.vlog(f"→ {json.dumps(obj, ensure_ascii=False)[:120]}")

    # ── hello 握手 ────────────────────────────────────────

    def send_hello(self) -> None:
        self.send_json({
            "type": "hello",
            "version": 3,
            "transport": "udp",
            "features": {"mcp": True},
            "audio_params": {
                "format": "opus",
                "sample_rate": 16000,
                "channels": 1,
                "frame_duration": FRAME_MS,
            },
        })
        self.log("已发送 hello，等待服务器响应（固件只等 10 秒）")

    def wait_hello_response(self, timeout: float = 10.0) -> dict:
        """等待服务器的 hello 响应，校验并建立 UDP 会话"""
        assert self.mqtt is not None
        deadline = time.monotonic() + timeout
        while True:
            remain = deadline - time.monotonic()
            if remain <= 0:
                raise TimeoutError(f"{timeout}s 内没收到 hello 响应")
            msg = self.mqtt.get(timeout=remain)
            if msg is None:
                continue
            try:
                obj = json.loads(msg.text())
            except Exception:
                continue
            self.vlog(f"← {msg.topic}: {msg.text()[:120]}")
            if obj.get("type") != "hello":
                # 抢跑到的其它消息先攒着
                with self._rx_lock:
                    self.rx_json.append(obj)
                continue
            self._validate_hello(obj)
            return obj

    def _validate_hello(self, obj: dict) -> None:
        """严格按固件的校验逻辑（任一不满足 = 设备侧静默超时）"""
        if obj.get("transport") != "udp":
            raise ValueError(f"hello.transport 必须是 'udp'，实际 {obj.get('transport')!r}")

        udp = obj.get("udp")
        if not isinstance(udp, dict):
            raise ValueError("hello.udp 必须是对象")

        port = udp.get("port")
        if not isinstance(port, int) or not (1 <= port <= 65535):
            raise ValueError(f"hello.udp.port 非法: {port!r}")

        server = udp.get("server")
        if not server or server in ("0.0.0.0", "127.0.0.1", "::"):
            raise ValueError(f"hello.udp.server 必须是局域网可达 IP，实际 {server!r}")

        key = parse_hex16(udp.get("key", ""), "udp.key")
        nonce = parse_hex16(udp.get("nonce", ""), "udp.nonce")
        validate_server_nonce(nonce)

        self.session_id = obj.get("session_id")
        self.crypto = UdpCrypto(key, nonce)
        self.log(
            f"hello 响应合规：udp={server}:{port} "
            f"session_id/ssrc=0x{self.crypto.ssrc:08x} key/nonce=16B"
        )

        ap = obj.get("audio_params") or {}
        self.log(
            f"下行音频参数: {ap.get('sample_rate')}Hz "
            f"{ap.get('format')} {ap.get('frame_duration')}ms"
        )
        self.downstream_rate = int(ap.get("sample_rate") or 24000)

    # ── UDP ───────────────────────────────────────────────

    def open_udp(self, hello: dict) -> None:
        udp = hello["udp"]
        server = udp["server"]
        port = int(udp["port"])
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(1.0)
        # 固件同样用 connect() —— 这是本地操作，不发包
        self.sock.connect((server, port))
        self.log(f"UDP 已 connect 到 {server}:{port}（本地操作，不发包）")

        t = threading.Thread(target=self._udp_rx_loop, daemon=True)
        t.start()

    def _udp_rx_loop(self) -> None:
        assert self.sock is not None and self.crypto is not None
        while not self._rx_stop.is_set():
            try:
                data = self.sock.recv(2048)
            except socket.timeout:
                continue
            except OSError:
                break
            if not data:
                continue
            try:
                header, payload = self.crypto.open(data)
            except UdpPacketError as exc:
                self.log(f"⚠️ 下行包解密/格式失败: {exc}")
                continue
            with self._rx_lock:
                self.rx_audio.append(
                    (header.sequence, header.timestamp, len(payload), time.monotonic())
                )
            self.vlog(
                f"← UDP seq={header.sequence} ts={header.timestamp} "
                f"len={len(payload)} type=0x{header.packet_type:02x}"
            )

    # ── 对话流程 ──────────────────────────────────────────

    def listen_start(self) -> None:
        self.send_json({
            "session_id": self.session_id or "",
            "type": "listen",
            "state": "start",
            "mode": "auto",
        })

    def listen_stop(self) -> None:
        self.send_json({
            "session_id": self.session_id or "",
            "type": "listen",
            "state": "stop",
            "mode": "auto",
        })

    def send_audio_frames(self, frames, realtime: bool = True) -> None:
        assert self.sock is not None and self.crypto is not None
        seq = 0
        base_ts = 0  # 固件默认上行 timestamp 恒为 0
        t0 = time.monotonic()
        for i, fr in enumerate(frames):
            seq += 1
            header = self.crypto.build_header(fr.size, base_ts, seq)
            pkt = self.crypto.seal(header, fr.data)
            # ★ 上行与下行共用 key，但下行 ts 非 0、上行 ts=0，IV 不会碰撞
            self.sock.send(pkt)
            if realtime:
                target = t0 + (i + 1) * FRAME_MS / 1000.0
                now = time.monotonic()
                if target > now:
                    time.sleep(target - now)
        self.log(f"已发送 {seq} 个上行音频帧（{seq * FRAME_MS}ms）")

    def drain_json(self, extra_wait: float = 2.0) -> list[dict]:
        """收集下行 JSON 消息（stt / tts 等）"""
        assert self.mqtt is not None
        deadline = time.monotonic() + extra_wait
        out = []
        with self._rx_lock:
            out.extend(self.rx_json)
            self.rx_json.clear()
        while True:
            remain = deadline - time.monotonic()
            if remain <= 0:
                break
            msg = self.mqtt.get(timeout=remain)
            if msg is None:
                break
            try:
                obj = json.loads(msg.text())
            except Exception:
                continue
            self.vlog(f"← {msg.topic}: {msg.text()[:140]}")
            out.append(obj)
        return out

    # ── 收尾 ──────────────────────────────────────────────

    def close(self) -> None:
        self._rx_stop.set()
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
        if self.mqtt is not None:
            self.mqtt.close()


def summarize(rx_json: list[dict], rx_audio) -> dict:
    """从下行消息与音频统计出可断言的指标"""
    stt = [o for o in rx_json if o.get("type") == "stt"]
    tts = [o for o in rx_json if o.get("type") == "tts"]
    tts_states = [o.get("state") for o in tts]

    seqs = [a[0] for a in rx_audio]
    ts = [a[1] for a in rx_audio]
    times = [a[3] for a in rx_audio]

    seq_ok = bool(seqs) and seqs == list(range(1, len(seqs) + 1))
    ts_nonzero = all(t > 0 for t in ts)
    ts_step = all(b - a == FRAME_MS for a, b in zip(ts, ts[1:]))

    gaps = [(b - a) * 1000 for a, b in zip(times, times[1:])]
    avg_gap = sum(gaps) / len(gaps) if gaps else 0

    return {
        "stt_count": len(stt),
        "stt_text": " ".join(str(o.get("text") or "") for o in stt),
        "tts_count": len(tts),
        "tts_states": tts_states,
        "has_tts_start": "start" in tts_states,
        "has_tts_stop": "stop" in tts_states,
        "audio_count": len(rx_audio),
        "audio_seq_ok": seq_ok,
        "audio_ts_nonzero": ts_nonzero,
        "audio_ts_step_60": ts_step,
        "avg_gap_ms": avg_gap,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="小智设备模拟器（MQTT+UDP）")
    ap.add_argument("--broker", default="127.0.0.1:1883", help="MQTT broker host:port")
    ap.add_argument("--mac", default=DEVICE_MAC, help="设备 MAC")
    ap.add_argument("--wav", help="上行音频 WAV（不给则生成 2 秒测试音）")
    ap.add_argument("--realtime", action="store_true", default=True,
                    help="按 60ms/帧实时发送（默认开）")
    ap.add_argument("--no-realtime", dest="realtime", action="store_false")
    ap.add_argument("--wait", type=float, default=8.0, help="发完后等待回复的秒数")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    print("=" * 60)
    print("小智设备模拟器（MQTT + UDP）")
    print(f"  AES 后端: {aes_backend()}")
    print("=" * 60)

    wav = args.wav
    if not wav:
        wav = "/tmp/xiaozhi_sim_uplink.wav"
        if not os.path.isfile(wav):
            make_test_wav(wav, text_duration_s=2.0)
        print(f"  使用测试音频: {wav}")

    frames = wav_to_opus_frames(wav, sample_rate=16000)
    print(f"  上行音频: {len(frames)} 帧 × {FRAME_MS}ms = {len(frames) * FRAME_MS}ms")

    sim = DeviceSim(args.broker, args.mac, verbose=args.verbose)
    ok = True
    try:
        sim.connect_mqtt()
        sim.send_hello()
        hello = sim.wait_hello_response(timeout=10.0)
        sim.open_udp(hello)

        sim.listen_start()
        time.sleep(0.2)
        sim.send_audio_frames(frames, realtime=args.realtime)
        sim.listen_stop()

        print(f"  等待服务器响应（最多 {args.wait}s）…")
        rx_json = sim.drain_json(extra_wait=args.wait)
        with sim._rx_lock:
            rx_audio = list(sim.rx_audio)

        s = summarize(rx_json, rx_audio)
        print()
        print("=" * 60)
        print("结果")
        print("=" * 60)
        print(f"  STT 消息:        {s['stt_count']} 条  {s['stt_text'][:50]!r}")
        print(f"  TTS 消息:        {s['tts_count']} 条  状态={s['tts_states']}")
        print(f"  下行音频:        {s['audio_count']} 帧")
        print(f"  序列号连续:      {'✅' if s['audio_seq_ok'] else '❌'}")
        print(f"  timestamp 非 0:  {'✅' if s['audio_ts_nonzero'] else '❌'}")
        print(f"  timestamp +60:   {'✅' if s['audio_ts_step_60'] else '❌'}")
        print(f"  平均帧间隔:      {s['avg_gap_ms']:.1f}ms（期望 ~60ms）")

        # 断言
        checks = [
            ("收到 tts start", s["has_tts_start"]),
            ("收到 tts stop", s["has_tts_stop"]),
            ("收到下行音频", s["audio_count"] > 0),
            ("序列号从 1 连续", s["audio_seq_ok"]),
            ("下行 timestamp 非 0", s["audio_ts_nonzero"]),
            ("下行 timestamp 按 60 递增", s["audio_ts_step_60"]),
        ]
        print()
        for name, passed in checks:
            print(f"  {'✅' if passed else '❌'} {name}")
            if not passed:
                ok = False
    except Exception as exc:
        print(f"\n  ❌ 失败: {type(exc).__name__}: {exc}")
        ok = False
    finally:
        sim.close()

    print()
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
