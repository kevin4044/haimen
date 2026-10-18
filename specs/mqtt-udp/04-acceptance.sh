#!/usr/bin/env bash
#
# 04 - MQTT+UDP 传输层机械验收
#
# 用法：
#   bash specs/mqtt-udp/04-acceptance.sh --task T1
#   bash specs/mqtt-udp/04-acceptance.sh            # 默认 T1
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
    fail "缺少可执行脚本 $DEV"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi

# ─────────────────────────────────────────────
# 1. 改动白名单（按任务区分）
# ─────────────────────────────────────────────
declare -a ALLOWED_FILES=()
declare -a ALLOWED_PREFIXES=()

# 白名单采用【全部任务文件的并集 + specs/ + scripts/】。
# 原因：多个任务在同一工作区增量推进，当验收 T1 时 T2 的产物可能已经存在，
# 不应因此判 T1 失败。白名单的真正目的是拦住 Cursor 乱改无关源码
# （ws.rs / protocol.rs / strategy.rs / types.rs / ota.rs 有单独的硬性检查）。
ALLOWED_FILES=(
    "crates/haimen-xiaozhi/src/protocol_udp.rs"
    "crates/haimen-xiaozhi/src/mqtt_broker.rs"
    "crates/haimen-xiaozhi/src/mqtt.rs"
    "crates/haimen-xiaozhi/src/udp_audio.rs"
    "crates/haimen-xiaozhi/src/mqtt_udp.rs"
    "crates/haimen-xiaozhi/src/lib.rs"
)
ALLOWED_PREFIXES=("specs/mqtt-udp/" "scripts/")

case "$TASK" in
  T1)
    TARGET="crates/haimen-xiaozhi/src/protocol_udp.rs"
    MODULE="protocol_udp"
    MIN_TESTS=10
    ;;
  T2)
    TARGET="crates/haimen-xiaozhi/src/mqtt_broker.rs"
    MODULE="mqtt_broker"
    MIN_TESTS=8
    ;;
  *)
    fail "未知任务 $TASK（已实现 T1 / T2）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
    ;;
esac

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
done < <(git -c core.quotePath=false status --porcelain)

[ "$WHITELIST_OK" = "1" ] && pass "改动都在白名单内"

# ─────────────────────────────────────────────
# 2. 文件存在
# ─────────────────────────────────────────────
if [ ! -f "$TARGET" ]; then
    fail "缺少目标文件 $TARGET"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
pass "目标文件存在"

# ─────────────────────────────────────────────
# 3. 公开 API 形状（T1 专用）
# ─────────────────────────────────────────────
if [ "$TASK" = "T1" ]; then
    for sym in \
        "struct UdpAudioHeader" \
        "struct UdpCrypto" \
        "enum UdpProtoError" \
        "fn encode" \
        "fn decode" \
        "fn from_hex" \
        "fn build_header" \
        "fn seal" \
        "fn open" \
        "const SIZE"
    do
        if has "$sym" "$TARGET"; then
            pass "存在 $sym"
        else
            fail "缺少 $sym"
        fi
    done

    # 大端：必须出现 to_be_bytes / from_be_bytes
    if has "to_be_bytes" "$TARGET" && has "from_be_bytes" "$TARGET"; then
        pass "使用大端字节序（to_be_bytes / from_be_bytes）"
    else
        fail "缺少大端字节序处理（to_be_bytes / from_be_bytes）"
    fi

    # AES-CTR 计数器块必须来自包头，不能是固定 nonce 直接加密
    if has "copy_from_slice" "$TARGET"; then
        pass "存在计数器块拷贝逻辑（copy_from_slice）"
    else
        fail "缺少计数器块构造（应从包头 16 字节 copy_from_slice）"
    fi
fi

if [ "$TASK" = "T2" ]; then
    for sym in \
        "struct MqttBrokerConfig" \
        "struct BrokerIncoming" \
        "struct BrokerOutgoing" \
        "struct MqttBrokerHandle" \
        "enum MqttBrokerError" \
        "fn start" \
        "fn publish_to" \
        "fn connected_clients" \
        "fn incoming" \
        "CONNECT" \
        "CONNACK" \
        "PUBLISH" \
        "PUBACK" \
        "SUBSCRIBE" \
        "SUBACK" \
        "PINGREQ" \
        "PINGRESP"
    do
        if has "$sym" "$TARGET"; then
            pass "存在 $sym"
        else
            fail "缺少 $sym"
        fi
    done

    # 变长整数必须有续位处理（0x80 掩码）
    if has "0x80" "$TARGET" || has "0b1000_0000" "$TARGET"; then
        pass "存在变长整数续位处理"
    else
        fail "缺少 MQTT remaining_length 变长整数编解码（续位 0x80）"
    fi

    # 必须有独立的 TCP 监听
    if has "TcpListener" "$TARGET"; then
        pass "使用 TcpListener 监听"
    else
        fail "缺少 TcpListener（broker 需要 TCP 监听）"
    fi
fi

# ─────────────────────────────────────────────
# 4. 禁止事项
# ─────────────────────────────────────────────
# 库代码禁止 unwrap / expect / panic（排除 #[cfg(test)] 之后的部分较难，故只统计总数并与基线比较）
UNWRAP_COUNT=$(grep -c -E "\.unwrap\(\)|\.expect\(|panic!\(" "$TARGET" 2>/dev/null)
UNWRAP_COUNT=${UNWRAP_COUNT:-0}
TEST_MARKER=$(grep -n "mod tests" "$TARGET" 2>/dev/null | head -1 | cut -d: -f1)
if [ -n "$TEST_MARKER" ]; then
    BEFORE_TEST=$(head -n $((TEST_MARKER - 1)) "$TARGET" 2>/dev/null | grep -c -E "\.unwrap\(\)|\.expect\(|panic!\(")
    BEFORE_TEST=${BEFORE_TEST:-0}
    if [ "${BEFORE_TEST:-0}" -eq 0 ]; then
        pass "库代码（非测试区）无 unwrap/expect/panic"
    else
        fail "库代码（非测试区）出现 ${BEFORE_TEST} 处 unwrap/expect/panic"
    fi
else
    info "未找到 mod tests，跳过 unwrap 分区检查"
fi

# 不能新增依赖
if grep -q "thiserror" crates/haimen-xiaozhi/Cargo.toml 2>/dev/null; then
    info "Cargo.toml 已含 thiserror（允许）"
fi
CARGO_DIFF=$(git diff --stat -- crates/haimen-xiaozhi/Cargo.toml Cargo.toml Cargo.lock 2>/dev/null)
if [ -z "$CARGO_DIFF" ]; then
    pass "未改动依赖清单"
else
    fail "不应改动依赖清单（只允许已有的 aes / ctr / thiserror）"
    echo "$CARGO_DIFF" | sed 's/^/     /'
fi

# 不能碰 ws.rs / protocol.rs / strategy.rs / types.rs
for f in crates/haimen-xiaozhi/src/ws.rs crates/haimen-xiaozhi/src/protocol.rs \
         crates/haimen-xiaozhi/src/strategy.rs crates/haimen-xiaozhi/src/types.rs; do
    if git status --porcelain | grep -q " $f$"; then
        fail "不应改动 $f"
    fi
done
pass "未触碰 ws.rs / protocol.rs / strategy.rs / types.rs"

# 模块已导出
LIB="crates/haimen-xiaozhi/src/lib.rs"
if has "protocol_udp" "$LIB"; then
    pass "lib.rs 已导出 protocol_udp 模块"
else
    fail "lib.rs 未声明 pub mod protocol_udp;"
fi

# ─────────────────────────────────────────────
# 5. 测试数量
# ─────────────────────────────────────────────
TEST_COUNT=$(grep -c "#\[test\]" "$TARGET" 2>/dev/null)
TEST_COUNT=${TEST_COUNT:-0}
if [ "${TEST_COUNT:-0}" -ge "$MIN_TESTS" ]; then
    pass "测试数量 ${TEST_COUNT} >= ${MIN_TESTS}"
else
    fail "测试数量 ${TEST_COUNT} < ${MIN_TESTS}"
fi

# ─────────────────────────────────────────────
# 6. fmt / clippy / test
# ─────────────────────────────────────────────
echo
info "运行 cargo fmt --check"
if $DEV fmt >/tmp/acc_mqtt_fmt.log 2>&1; then
    pass "cargo fmt --check"
else
    fail "cargo fmt --check 失败"
    tail -15 /tmp/acc_mqtt_fmt.log | sed 's/^/     /'
fi

info "运行 cargo clippy -- -D warnings"
if $DEV clippy >/tmp/acc_mqtt_clippy.log 2>&1; then
    pass "cargo clippy -- -D warnings"
else
    fail "cargo clippy 失败"
    grep -E '^(error|warning)' /tmp/acc_mqtt_clippy.log | head -15 | sed 's/^/     /'
fi

info "运行 cargo test"
if $DEV test >/tmp/acc_mqtt_test.log 2>&1; then
    pass "cargo test"
    grep -E 'test result:' /tmp/acc_mqtt_test.log | tail -4 | sed 's/^/     /'
    # 确认 protocol_udp 的测试真的跑了
    MODULE_TESTED=$(grep -cE "${MODULE}::tests::" /tmp/acc_mqtt_test.log)
    MODULE_TESTED=${MODULE_TESTED:-0}
    if [ "$MODULE_TESTED" -ge "$TEST_COUNT" ]; then
        pass "${MODULE} 的测试确实执行了（${MODULE_TESTED} 个 >= 文件内 ${TEST_COUNT} 个）"
    else
        fail "${MODULE} 的测试未执行或不足（执行 ${MODULE_TESTED} 个 < 文件内 ${TEST_COUNT} 个）"
    fi
else
    fail "cargo test 失败"
    grep -E '^(error|failures:)' -A5 /tmp/acc_mqtt_test.log | head -25 | sed 's/^/     /'
fi

# ─────────────────────────────────────────────
# 汇总
# ─────────────────────────────────────────────
echo
if [ "$FAILURES" -eq 0 ]; then
    echo "ALL CHECKS PASSED ($TASK)"
    exit 0
fi
echo "验收失败：$FAILURES 项"
exit 1
