#!/usr/bin/env python3
"""监控设备与 haimen 的连接活动（用于排查设备连不上的问题）

每 0.5 秒采样一次容器内的 TCP 连接表，报告：
- 来自非 OrbStack 网段（192.168.215.x）的连接 → 就是真实设备
- 9527（OTA/HTTP）与 1883（MQTT）两个端口的连接数

用法：
    python3 watch_device_conn.py            # 监控 30 秒
    python3 watch_device_conn.py --seconds 60
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time

CONTAINER = "haimen-test"


def sample() -> list[tuple[str, int, str]]:
    """返回 [(远端IP, 远端端口, 本地端口)]，只保留 ESTABLISHED(01)"""
    try:
        out = subprocess.run(
            ["docker", "exec", CONTAINER, "cat", "/proc/net/tcp"],
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return []

    rows = []
    for line in out.splitlines()[1:]:
        parts = line.split()
        if len(parts) < 4:
            continue
        local, remote, state = parts[1], parts[2], parts[3]
        if state != "01":  # 只看 ESTABLISHED
            continue
        try:
            lip, lport = local.split(":")
            rip, rport = remote.split(":")
            ip = ".".join(
                str(int(rip[i:i + 2], 16)) for i in (6, 4, 2, 0)
            )
            rows.append((ip, int(rport, 16), int(lport, 16)))
        except Exception:
            continue
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--interval", type=float, default=0.5)
    args = ap.parse_args()

    print(f"监控 {CONTAINER} 的 TCP 连接，共 {args.seconds:g}s")
    print("  真实设备会在 192.168.110.x 出现；192.168.215.x 是 OrbStack 转发\n")

    seen: set[str] = set()
    deadline = time.monotonic() + args.seconds
    while time.monotonic() < deadline:
        for ip, rport, lport in sample():
            if ip.startswith("192.168.215."):
                continue  # OrbStack 端口转发
            key = f"{ip}:{rport}->{lport}"
            if key not in seen:
                seen.add(key)
                print(f"[{time.strftime('%H:%M:%S')}] 发现真实连接 {key}", flush=True)
        time.sleep(args.interval)

    print()
    if seen:
        print(f"✅ 共发现 {len(seen)} 条非转发连接：")
        for k in sorted(seen):
            print(f"   {k}")
        return 0

    print("❌ 监控期内没有任何来自真实设备的 TCP 连接")
    print()
    print("说明设备既没连 MQTT(1883)，也没连 HTTP(9527)")
    print("→ 大概率它连的不是这台机器，或 OTA/网络有问题")
    print()
    print("建议确认：")
    print("  · 设备屏幕是否显示 WiFi 已连、是否有报错")
    print("  · 设备的 OTA 地址是否仍是 http://192.168.110.35:9527/xiaozhi/ota/")
    print("  · 设备与本机是否在同一网段（192.168.110.x）")
    return 1


if __name__ == "__main__":
    sys.exit(main())
