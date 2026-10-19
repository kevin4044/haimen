#!/usr/bin/env python3
"""统计 Agent（LLM）调用健康度：频率、延迟分布、超时/错误

用途：判断「Hermes 卡住」这类故障到底多频繁，决定要不要去改 Hermes。

数据源：~/works/haimen-data/agent-logs/YYYY-MM-DD.jsonl
每行一条记录，字段：timestamp/source/agent/input/output/status/error/latency_ms

用法：
    python3 scripts/check-agent-health.py            # 今天
    python3 scripts/check-agent-health.py --days 3   # 最近 3 天
    python3 scripts/check-agent-health.py --slow 5   # 列出最慢的 5 次
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter
from datetime import date, timedelta

DEFAULT_LOG_DIR = os.path.expanduser("~/works/haimen-data/agent-logs")


def load(path: str) -> list[dict]:
    rows = []
    if not os.path.isfile(path):
        return rows
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return rows


def pct(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, int(round((len(ordered) - 1) * p)))
    return ordered[idx]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=1, help="统计最近几天")
    ap.add_argument("--slow", type=int, default=5, help="列出最慢的几次")
    ap.add_argument("--dir", default=DEFAULT_LOG_DIR)
    args = ap.parse_args()

    today = date.today()
    days = [today - timedelta(days=i) for i in range(args.days)]
    rows: list[dict] = []
    for d in days:
        rows.extend(load(os.path.join(args.dir, f"{d.isoformat()}.jsonl")))

    if not rows:
        print(f"❌ {args.dir} 里没有最近 {args.days} 天的记录")
        return 1

    status = Counter(r.get("status", "?") for r in rows)
    latencies = [r.get("latency_ms", 0) for r in rows if r.get("latency_ms")]
    xs = [r for r in rows if r.get("source") == "xiaozhi"]

    print("=" * 62)
    print(f"Agent 调用健康度（最近 {args.days} 天，共 {len(rows)} 次）")
    print("=" * 62)
    print(f"  语音来源(xiaozhi): {len(xs)} 次")
    print()
    print("  ── 状态分布 ──")
    for st, n in status.most_common():
        flag = "  ⚠️" if st in ("timeout", "error") else ""
        print(f"    {st:<10} {n:>4} 次{flag}")
    print()

    if latencies:
        print("  ── 延迟分布（全部来源）──")
        print(f"    P50   {pct(latencies, 0.50):>8.0f} ms")
        print(f"    P90   {pct(latencies, 0.90):>8.0f} ms")
        print(f"    P99   {pct(latencies, 0.99):>8.0f} ms")
        print(f"    max   {max(latencies):>8.0f} ms")
        print()

    slow = sorted(rows, key=lambda r: r.get("latency_ms", 0), reverse=True)[: args.slow]
    if slow:
        print(f"  ── 最慢的 {len(slow)} 次 ──")
        for r in slow:
            ts = (r.get("timestamp") or "")[:19].replace("T", " ")
            st = r.get("status", "?")
            ms = r.get("latency_ms", 0)
            text = (r.get("input") or "")[:24].replace("\n", " ")
            print(f"    {ts}  {ms:>7} ms  {st:<8} {text}")
        print()

    timeouts = status.get("timeout", 0)
    errors = status.get("error", 0)
    bad = timeouts + errors
    rate = bad * 100.0 / len(rows)
    print("  ── 结论 ──")
    if bad == 0:
        print("    ✅ 没有超时/错误 —— 不需要动 Hermes")
    else:
        print(f"    ⚠️ 异常 {bad} 次（超时 {timeouts} / 错误 {errors}），占比 {rate:.1f}%")
        if bad >= 3:
            print("    → 频率偏高，建议给 Hermes 的 DeepSeek 流式调用加 read timeout")
        else:
            print("    → 频率很低，先继续观察")
    print()
    print("  提示：haimen 侧超时会打固定日志，可交叉验证：")
    print("    docker logs haimen-test 2>&1 | grep -c 'Agent 首字节超时'")
    return 0


if __name__ == "__main__":
    sys.exit(main())
