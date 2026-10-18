#!/usr/bin/env bash
#
# 04 - 机械验收：ASR VAD 修复 + 诊断日志
#
# 全部检查都在 Docker 开发容器里跑（本机不装 Rust）。
# 退出码 0 = 通过；非 0 = 失败。
# 失败时按行输出以 "FAIL" 开头的行（handoff.mjs 会收集它们）。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

FAILURES=0
fail() { echo "FAIL $*"; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS $*"; }
info() { echo "INFO $*"; }

DEV="./scripts/dev.sh"
has() { grep -q -- "$1" "$2" 2>/dev/null; }
count_matches() { grep -c -- "$1" "$2" 2>/dev/null || echo 0; }

TARGET="src/xiaozhi_asr_llm_tts.rs"

# ─────────────────────────────────────────────
# 0. 环境
# ─────────────────────────────────────────────
if ! docker info >/dev/null 2>&1; then
    fail "Docker 不可用（请先启动 OrbStack）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
if [ ! -x "$DEV" ]; then
    fail "缺少可执行脚本 $DEV"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
if [ ! -f "$TARGET" ]; then
    fail "缺少目标文件 $TARGET"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi

# ─────────────────────────────────────────────
# 1. 改动白名单
# ─────────────────────────────────────────────
ALLOWED_FILES=(
    "src/xiaozhi_asr_llm_tts.rs"
)
ALLOWED_PREFIXES=(
    "specs/asr-vad-fix/"
)

is_allowed() {
    local path="$1" f p
    for f in "${ALLOWED_FILES[@]}"; do
        [ "$path" = "$f" ] && return 0
    done
    for p in "${ALLOWED_PREFIXES[@]}"; do
        case "$path" in "$p"*) return 0 ;; esac
    done
    return 1
}

WHITELIST_OK=1
while IFS= read -r line; do
    [ -z "$line" ] && continue
    path="${line:3}"
    path="${path##* -> }"
    if ! is_allowed "$path"; then
        fail "改动超出白名单: $path"
        WHITELIST_OK=0
    fi
done < <(git status --porcelain)

[ "$WHITELIST_OK" = "1" ] && pass "改动都在白名单内"

# ─────────────────────────────────────────────
# 2. end_window_size 默认值必须是 1500
# ─────────────────────────────────────────────
if has 'unwrap_or(1500)' "$TARGET"; then
    pass "end_window_size 默认值 1500"
else
    fail "end_window_size 默认值不是 1500（应把 .unwrap_or(400) 改成 .unwrap_or(1500)）"
fi

if has 'unwrap_or(400)' "$TARGET"; then
    fail "仍存在 .unwrap_or(400)，必须改掉"
else
    pass "旧的 400 默认值已移除"
fi

# ─────────────────────────────────────────────
# 3. force_to_speech_time 默认值必须是 500
# ─────────────────────────────────────────────
if has 'unwrap_or(500)' "$TARGET"; then
    pass "force_to_speech_time 默认值 500"
else
    fail "force_to_speech_time 默认值不是 500（应把 .unwrap_or(200) 改成 .unwrap_or(500)）"
fi

# ─────────────────────────────────────────────
# 4. 诊断日志升到 warn
# ─────────────────────────────────────────────
declare -a WARN_MSGS=(
    "流式 ASR: 管道已就绪（惰性初始化）"
    "流式 ASR: 清空管道状态（惰性初始化）"
    "流式 ASR: 第一帧音频到达，惰性初始化管道"
    "无语音超时"
    "ASR 完成"
)

for msg in "${WARN_MSGS[@]}"; do
    # 该文案必须出现在某个 warn! 里
    if grep -A2 'tracing::warn!' "$TARGET" 2>/dev/null | grep -q "$msg"; then
        pass "「$msg」已升到 warn 级"
    elif grep -B2 "$msg" "$TARGET" 2>/dev/null | grep -q 'tracing::info!'; then
        fail "「$msg」仍是 info 级，需改为 warn!"
    else
        fail "找不到「$msg」对应的 warn! 日志"
    fi
done

# ─────────────────────────────────────────────
# 5. RMS 诊断日志
# ─────────────────────────────────────────────
if has "RMS=" "$TARGET"; then
    pass "已新增 RMS 实测日志"
else
    fail "缺少 RMS 实测日志（需在 compute_pcm_rms 之后插入 warn! 打印 rms 值）"
fi

if has "SILENCE_RMS_THRESHOLD" "$TARGET"; then
    pass "RMS 日志引用了阈值常量"
fi

# 不能出现两个相同条件的 if（避免日志重复）
FRAME_LOG_IFS=$(count_matches 'frame_count - state.last_log_frame >= 50' "$TARGET")
if [ "${FRAME_LOG_IFS:-0}" -ge 1 ] && [ "${FRAME_LOG_IFS:-0}" -le 1 ]; then
    pass "帧计数日志条件只有 1 处（未重复）"
elif [ "${FRAME_LOG_IFS:-0}" -gt 1 ]; then
    fail "帧计数日志条件出现 $FRAME_LOG_IFS 次，应合并为 1 处"
else
    info "提示：未见 'frame_count - state.last_log_frame >= 50' 条件（若实现方式不同可忽略）"
fi

# ─────────────────────────────────────────────
# 6. 不该被改动的常量必须保持
# ─────────────────────────────────────────────
if has 'const MAX_SILENCE_FRAMES: u64 = 20' "$TARGET"; then
    pass "MAX_SILENCE_FRAMES 保持 20"
else
    fail "MAX_SILENCE_FRAMES 被改动（应保持 20）"
fi

if has 'const SILENCE_RMS_THRESHOLD: f64 = 2000.0' "$TARGET"; then
    pass "SILENCE_RMS_THRESHOLD 保持 2000.0"
else
    fail "SILENCE_RMS_THRESHOLD 被改动（应保持 2000.0）"
fi

# ─────────────────────────────────────────────
# 7. fmt / clippy / test
# ─────────────────────────────────────────────
echo
info "运行 cargo fmt --check"
if $DEV fmt >/tmp/acc2_fmt.log 2>&1; then
    pass "cargo fmt --check"
else
    fail "cargo fmt --check 失败"
    tail -15 /tmp/acc2_fmt.log | sed 's/^/     /'
fi

info "运行 cargo clippy -- -D warnings"
if $DEV clippy >/tmp/acc2_clippy.log 2>&1; then
    pass "cargo clippy -- -D warnings"
else
    fail "cargo clippy 失败"
    grep -E '^(error|warning: unused)' /tmp/acc2_clippy.log | head -15 | sed 's/^/     /'
fi

info "运行 cargo test"
if $DEV test >/tmp/acc2_test.log 2>&1; then
    pass "cargo test"
    grep -E 'test result:' /tmp/acc2_test.log | tail -3 | sed 's/^/     /'
else
    fail "cargo test 失败"
    grep -E '^(error|failures:)' -A5 /tmp/acc2_test.log | head -20 | sed 's/^/     /'
fi

# ─────────────────────────────────────────────
# 汇总
# ─────────────────────────────────────────────
echo
if [ "$FAILURES" -eq 0 ]; then
    echo "ALL CHECKS PASSED"
    exit 0
fi
echo "验收失败：$FAILURES 项"
exit 1
