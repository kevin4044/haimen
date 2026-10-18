#!/usr/bin/env bash
#
# 04 - 机械验收：openai provider
#
# 全部检查都在 Docker 开发容器里跑（本机不装 Rust）。
# 退出码 0 = 通过；非 0 = 失败。
# 失败时按行输出以 "FAIL" 开头的行（handoff.mjs 会收集它们）。
#
# 用法：在仓库根执行 bash specs/openai-provider/04-acceptance.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

FAILURES=0
fail() { echo "FAIL $*"; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS $*"; }
info() { echo "INFO $*"; }

DEV="./scripts/dev.sh"

# 静默 grep（文件不存在时也不打印错误）
has() { grep -q -- "$1" "$2" 2>/dev/null; }
count_matches() { grep -c -- "$1" "$2" 2>/dev/null || echo 0; }

# ─────────────────────────────────────────────
# 0. 环境
# ─────────────────────────────────────────────
if ! docker info >/dev/null 2>&1; then
    fail "Docker 不可用（请先启动 OrbStack）"
    echo
    echo "验收失败：$FAILURES 项"
    exit 1
fi

if [ ! -x "$DEV" ]; then
    fail "缺少可执行脚本 $DEV"
    echo
    echo "验收失败：$FAILURES 项"
    exit 1
fi

# ─────────────────────────────────────────────
# 1. 改动白名单
# ─────────────────────────────────────────────
ALLOWED_FILES=(
    "Cargo.toml"
    "Cargo.lock"
)
ALLOWED_PREFIXES=(
    "crates/haimen-openai/"
    "specs/openai-provider/"
)

is_allowed() {
    local path="$1"
    local f
    for f in "${ALLOWED_FILES[@]}"; do
        [ "$path" = "$f" ] && return 0
    done
    local p
    for p in "${ALLOWED_PREFIXES[@]}"; do
        case "$path" in
            "$p"*) return 0 ;;
        esac
    done
    return 1
}

WHITELIST_OK=1
while IFS= read -r line; do
    [ -z "$line" ] && continue
    # porcelain 格式：XY <path>，重命名会有 "old -> new"
    path="${line:3}"
    path="${path##* -> }"
    if ! is_allowed "$path"; then
        fail "改动超出白名单: $path"
        WHITELIST_OK=0
    fi
done < <(git status --porcelain)

if [ "$WHITELIST_OK" = "1" ]; then
    pass "改动都在白名单内"
fi

# ─────────────────────────────────────────────
# 2. crate 结构
# ─────────────────────────────────────────────
for f in "crates/haimen-openai/Cargo.toml" \
         "crates/haimen-openai/src/lib.rs" \
         "crates/haimen-openai/src/agent.rs"; do
    if [ -f "$f" ]; then
        pass "存在 $f"
    else
        fail "缺少 $f"
    fi
done

# ─────────────────────────────────────────────
# 3. workspace 注册
# ─────────────────────────────────────────────
if has 'crates/haimen-openai' Cargo.toml; then
    pass "Cargo.toml 已包含 haimen-openai（workspace 或依赖）"
else
    fail "Cargo.toml 未包含 crates/haimen-openai"
fi

if has 'haimen-openai = { path = "crates/haimen-openai"' Cargo.toml; then
    pass "Cargo.toml 已把 haimen-openai 声明为依赖"
else
    fail "Cargo.toml 缺少 haimen-openai 依赖声明"
fi

# ─────────────────────────────────────────────
# 4. registry 接入
# ─────────────────────────────────────────────
if has 'use haimen_openai' src/agents/registry.rs; then
    pass "registry.rs 已 use haimen_openai"
else
    fail "registry.rs 缺少 use haimen_openai"
fi

if has '"openai"' src/agents/registry.rs; then
    pass "registry.rs 已注册 openai provider"
else
    fail "registry.rs 未注册 openai provider"
fi

# ─────────────────────────────────────────────
# 5. trait 实现
# ─────────────────────────────────────────────
if has 'impl AgentProvider for OpenAIAgent' crates/haimen-openai/src/agent.rs; then
    pass "已实现 impl AgentProvider for OpenAIAgent"
else
    fail "agent.rs 缺少 impl AgentProvider for OpenAIAgent"
fi

for fn in "fn name" "async fn process" "async fn process_stream" "async fn check_available"; do
    if has "$fn" crates/haimen-openai/src/agent.rs; then
        pass "已实现 $fn"
    else
        fail "agent.rs 缺少 $fn"
    fi
done

# ─────────────────────────────────────────────
# 6. 流式关键点：必须有 SSE 处理，且不能先收完再吐
# ─────────────────────────────────────────────
if has 'stream' crates/haimen-openai/src/agent.rs && has 'true' crates/haimen-openai/src/agent.rs; then
    pass "请求体里设置了 stream"
else
    fail "agent.rs 未见 stream:true（流式是硬性要求）"
fi

if has '[DONE]' crates/haimen-openai/src/agent.rs; then
    pass "已处理 SSE 结束标记 [DONE]"
else
    fail "agent.rs 未见 [DONE] 处理"
fi

if has 'bytes_stream' crates/haimen-openai/src/agent.rs || has 'unfold' crates/haimen-openai/src/agent.rs || has 'mpsc' crates/haimen-openai/src/agent.rs; then
    pass "已使用流式读取/转发机制"
else
    fail "agent.rs 未见流式读取机制（bytes_stream / unfold / mpsc）"
fi

# ─────────────────────────────────────────────
# 7. 单元测试数量
# ─────────────────────────────────────────────
TEST_COUNT=$(count_matches '#\[test\]' crates/haimen-openai/src/agent.rs)
if [ "$TEST_COUNT" -ge 6 ]; then
    pass "单元测试数量 $TEST_COUNT >= 6"
else
    fail "单元测试不足：只有 $TEST_COUNT 个（要求 >= 6）"
fi

# ─────────────────────────────────────────────
# 8. 禁用项（软检查，仅提示）
# ─────────────────────────────────────────────
if grep -nE '\bunwrap\(\)|\bexpect\(' crates/haimen-openai/src/agent.rs 2>/dev/null \
        | grep -v '#\[cfg(test)\]' >/dev/null 2>&1; then
    info "提示：agent.rs 里有 unwrap/expect，请确认只在测试代码中"
fi

if grep -rqE '\.unwrap\(\)' crates/haimen-openai/src/lib.rs 2>/dev/null; then
    fail "lib.rs 出现 unwrap()"
fi

# ─────────────────────────────────────────────
# 9. fmt / clippy / test（容器内执行）
# ─────────────────────────────────────────────
echo
info "运行 cargo fmt --check"
if $DEV fmt >/tmp/acc_fmt.log 2>&1; then
    pass "cargo fmt --check"
else
    fail "cargo fmt --check 失败"
    tail -15 /tmp/acc_fmt.log | sed 's/^/     /'
fi

info "运行 cargo clippy -- -D warnings"
if $DEV clippy >/tmp/acc_clippy.log 2>&1; then
    pass "cargo clippy -- -D warnings"
else
    fail "cargo clippy 失败"
    grep -E '^(error|warning: unused)' /tmp/acc_clippy.log | head -15 | sed 's/^/     /'
fi

info "运行 cargo test"
if $DEV test >/tmp/acc_test.log 2>&1; then
    pass "cargo test"
    grep -E 'test result:' /tmp/acc_test.log | tail -3 | sed 's/^/     /'
else
    fail "cargo test 失败"
    grep -E '^(error|failures:)' -A5 /tmp/acc_test.log | head -20 | sed 's/^/     /'
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
