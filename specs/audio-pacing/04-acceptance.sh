#!/usr/bin/env bash
#
# 04 - 下行音频尾帧收口的机械验收
#
# 用法：
#   bash specs/audio-pacing/04-acceptance.sh --task T1
#   bash specs/audio-pacing/04-acceptance.sh            # 默认 T1
#
# 全部检查在 Docker 开发容器里跑（本机不装 Rust）。
# 退出码 0 = 通过；非 0 = 失败。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TASK="T1"
while [ $# -gt 0 ]; do
    case "$1" in
        --task) TASK="${2:-T1}"; shift 2 ;;
        *) shift ;;
    esac
done

FAILURES=0
fail() { echo "FAIL $*"; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS $*"; }
info() { echo "INFO $*"; }

DEV="./scripts/dev.sh"
has() { grep -q -- "$1" "$2" 2>/dev/null; }

# ─────────────────────────────────────────────
# 0. 环境
# ─────────────────────────────────────────────
if ! docker info >/dev/null 2>&1; then
    fail "Docker 不可用（请先启动 OrbStack）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
if [ ! -x "$DEV" ]; then
    fail "缺少 $DEV"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
info "环境检查通过（Docker 可用）"

# ─────────────────────────────────────────────
# 0b. 改动白名单：只允许这些文件被改
# ─────────────────────────────────────────────
ALLOWED_CHANGED_RE='^(crates/haimen-xiaozhi/src/mqtt_udp\.rs|crates/haimen-xiaozhi/examples/mqtt_udp_demo\.rs|src/gateway/mod\.rs|specs/audio-pacing/)'
CHANGED="$(git -c core.quotePath=false status --porcelain | awk '{print $2}' | sed 's/^"//; s/"$//')"
if [ -n "$CHANGED" ]; then
    OUTSIDE="$(echo "$CHANGED" | grep -Ev "$ALLOWED_CHANGED_RE" || true)"
    if [ -n "$OUTSIDE" ]; then
        fail "改动了白名单外的文件："
        echo "$OUTSIDE" | sed 's/^/     /'
    else
        pass "改动都在白名单内"
    fi
fi

# ─────────────────────────────────────────────
# 1. 编译 + 测试（T1..T3 都要）
# ─────────────────────────────────────────────
info "cargo check + test（容器内）"
if docker run --rm -v "$PWD":/src -w /src \
      -v "haimen-cargo-registry":/usr/local/cargo/registry \
      -v "haimen-target":/target -e CARGO_TARGET_DIR=/target \
      -e HTTP_PROXY= -e HTTPS_PROXY= \
      haimen-dev:rust sh -c 'cargo check --workspace && cargo test -p haimen-xiaozhi --lib' \
      >/tmp/pacing-ci.log 2>&1; then
    pass "cargo check + 测试通过"
else
    fail "cargo check / 测试失败（见 /tmp/pacing-ci.log）"
    tail -20 /tmp/pacing-ci.log | sed 's/^/     /'
fi

# ─────────────────────────────────────────────
# 2. 按任务做静态检查
# ─────────────────────────────────────────────
case "$TASK" in
T1)
    # 尾帧等待函数存在
    if has "fn wait_tail_playback" crates/haimen-xiaozhi/src/mqtt_udp.rs; then
        pass "存在 wait_tail_playback"
    else
        fail "缺少 wait_tail_playback"
    fi

    # 可取消：函数体里同时出现 cancel 与 sleep
    BODY="$(awk '/fn wait_tail_playback/,/^}/' crates/haimen-xiaozhi/src/mqtt_udp.rs 2>/dev/null)"
    echo "$BODY" | grep -q "cancel" || fail "wait_tail_playback 没有检查取消信号"
    echo "$BODY" | grep -q "sleep" || fail "wait_tail_playback 没有 sleep"
    echo "$BODY" | grep -q "cancel" && echo "$BODY" | grep -q "sleep" && pass "wait_tail_playback 可取消"

    # 公式：prebuffer + tail_extra_frames
    echo "$BODY" | grep -q "prebuffer_count" || fail "等待时长没有基于 prebuffer_count"
    echo "$BODY" | grep -q "tail_extra_frames" || fail "等待时长没有用 tail_extra_frames"
    echo "$BODY" | grep -q "prebuffer_count" && echo "$BODY" | grep -q "tail_extra_frames" \
        && pass "等待时长按 (prebuffer + extra) × frame_duration 计算"

    # run_playback 里调用在 publish_tts(Stop) 之前
    if awk '/async fn run_playback/,/^}/' crates/haimen-xiaozhi/src/mqtt_udp.rs 2>/dev/null \
        | grep -q "wait_tail_playback"; then
        pass "run_playback 调用了 wait_tail_playback"
    else
        fail "run_playback 没有调用 wait_tail_playback"
    fi
    if awk '/async fn run_playback/,/^}/' crates/haimen-xiaozhi/src/mqtt_udp.rs 2>/dev/null \
        | grep -n "wait_tail_playback\|TtsState::Stop" | head -2 \
        | grep -q "wait_tail_playback"; then
        pass "wait_tail_playback 出现在 Stop 之前"
    else
        fail "wait_tail_playback 位置不对（应在 publish_tts(Stop) 之前）"
    fi

    # 配置字段
    has "tail_extra_frames" crates/haimen-xiaozhi/src/mqtt_udp.rs \
        && pass "MqttUdpConfig 有 tail_extra_frames" \
        || fail "MqttUdpConfig 缺少 tail_extra_frames"

    # 不要动重锚函数
    has "fn snap_send_clock" crates/haimen-xiaozhi/src/mqtt_udp.rs \
        && pass "snap_send_clock 仍在（不应删除）" \
        || fail "snap_send_clock 被删除了（不应该动它）"
    ;;
T2)
    BODY="$(awk '/async fn drive_playback/,/^}/' crates/haimen-xiaozhi/src/mqtt_udp.rs 2>/dev/null)"
    echo "$BODY" | grep -q "frames" || fail "drive_playback 没有帧计数"
    echo "$BODY" | grep -q "expected_ms\|expected" || fail "没有理论播放时长"
    echo "$BODY" | grep -q "warn!" || fail "没有用 warn! 输出统计"
    [ "$FAILURES" -eq 0 ] && pass "T2 埋点字段齐全"

    # 只在结束打一次：warn! 出现次数应很少
    COUNT=$(echo "$BODY" | grep -c "tracing::warn!")
    if [ "$COUNT" -ge 1 ] && [ "$COUNT" -le 3 ]; then
        pass "埋点 warn! 次数合理（$COUNT 次）"
    else
        fail "埋点 warn! 次数异常（$COUNT 次，期望 1~3）"
    fi
    ;;
T3)
    has "send_delay_ms" crates/haimen-xiaozhi/src/mqtt_udp.rs \
        && pass "MqttUdpConfig 有 send_delay_ms" \
        || fail "MqttUdpConfig 缺少 send_delay_ms"

    BODY="$(awk '/fn frame_step_ms/,/^}/' crates/haimen-xiaozhi/src/mqtt_udp.rs 2>/dev/null)"
    echo "$BODY" | grep -q "send_delay_ms" || fail "frame_step_ms 没有优先取 send_delay_ms"
    echo "$BODY" | grep -q "frame_duration_ms" || fail "frame_step_ms 回退值应为 frame_duration_ms"
    echo "$BODY" | grep -q "send_delay_ms" && echo "$BODY" | grep -q "frame_duration_ms" \
        && pass "frame_step_ms 覆盖 + 回退都正确"
    ;;
*)
    fail "未知任务 $TASK"
    ;;
esac

# ─────────────────────────────────────────────
# 3. 端到端：跑一次模拟器全链路
# ─────────────────────────────────────────────
info "端到端脚本（模拟器）"
if [ -x ./scripts/test-mqtt-udp.sh ]; then
    if ./scripts/test-mqtt-udp.sh >/tmp/pacing-e2e.log 2>&1; then
        pass "端到端脚本 ALL CHECKS PASSED"
    else
        fail "端到端脚本失败（见 /tmp/pacing-e2e.log）"
        tail -20 /tmp/pacing-e2e.log | sed 's/^/     /'
    fi
else
    info "跳过：scripts/test-mqtt-udp.sh 不可执行"
fi

# ─────────────────────────────────────────────
echo
if [ "$FAILURES" -eq 0 ]; then
    echo "验收通过（任务 ${TASK}）"
    exit 0
else
    echo "验收失败：$FAILURES 项（任务 ${TASK}）"
    exit 1
fi
