#!/usr/bin/env bash
#
# MQTT + UDP 传输层端到端测试
#
# 做的事：
#   1. 编译 example（mqtt_udp_demo）
#   2. 在容器里启动演示服务（内置 broker + UDP + EchoStrategy）
#   3. 用 Python 设备模拟器走完整固件流程：
#      MQTT 连接 → hello 握手 → UDP 建链 → 上传音频 → 收下行音频
#   4. 断言：tts start/stop、下行音频、序列号连续、timestamp 非 0 且按 60 递增
#
# 不需要真机、不需要外部 broker、不影响线上实例（用独立容器 + 独立端口）。
#
# 用法：
#   ./scripts/test-mqtt-udp.sh              # 默认端口
#   ./scripts/test-mqtt-udp.sh --keep       # 保留容器便于排查
#   ./scripts/test-mqtt-udp.sh --verbose    # 打印模拟器详细日志
#
# 退出码：0 = 通过；非 0 = 失败

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

KEEP=0
VERBOSE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        --verbose|-v) VERBOSE="--verbose"; shift ;;
        *) shift ;;
    esac
done

CONTAINER="xiaozhi-mqtt-udp-e2e"
DEV_IMAGE="haimen-dev:rust"
CARGO_VOL="haimen-cargo-registry"
TARGET_VOL="haimen-target"
BROKER_PORT=11883   # 容器内端口，避开 1883 冲突
UDP_PORT=18888
PY="${PY:-/Users/kevin/.dsh/dsh-runtimes/dsh-primary-runtime/dependencies/python/bin/python3}"

FAILURES=0
fail() { echo "FAIL $*"; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS $*"; }
step() { echo; echo "── $* ──────────────────────────────"; }

cleanup() {
    if [ "$KEEP" -eq 1 ]; then
        echo
        echo "（--keep：保留容器 $CONTAINER，排查完记得 docker rm -f $CONTAINER）"
        echo "   看日志: docker logs $CONTAINER"
        return
    fi
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ─────────────────────────────────────────────
step "0. 环境检查"
# ─────────────────────────────────────────────
if ! docker info >/dev/null 2>&1; then
    fail "Docker 不可用（请先启动 OrbStack）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
pass "Docker 可用"

if [ ! -x "$PY" ]; then
    fail "找不到 Python: $PY（可用 PY=... 覆盖）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
if ! "$PY" -c 'import paho.mqtt' 2>/dev/null; then
    fail "Python 缺少 paho-mqtt（$PY -m pip install paho-mqtt）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
pass "Python + paho-mqtt 就绪"

if ! docker image inspect "$DEV_IMAGE" >/dev/null 2>&1; then
    fail "缺少开发镜像 $DEV_IMAGE（先跑 ./scripts/dev.sh image 或 ./scripts/ci.sh）"
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
pass "开发镜像就绪"

# ─────────────────────────────────────────────
step "1. 编译 example"
# ─────────────────────────────────────────────
if docker run --rm -i \
    -v "$REPO_ROOT":/src -w /src \
    -v "$CARGO_VOL":/usr/local/cargo/registry \
    -v "$TARGET_VOL":/target \
    -e CARGO_TARGET_DIR=/target \
    -e HTTP_PROXY= -e HTTPS_PROXY= -e http_proxy= -e https_proxy= \
    "$DEV_IMAGE" cargo build -p haimen-xiaozhi --example mqtt_udp_demo \
    >/tmp/e2e_build.log 2>&1
then
    pass "example 编译通过"
else
    fail "example 编译失败"
    tail -20 /tmp/e2e_build.log | sed 's/^/     /'
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi

# ─────────────────────────────────────────────
step "2. 启动演示服务"
# ─────────────────────────────────────────────
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" \
    -v "$REPO_ROOT":/src -w /src \
    -v "$TARGET_VOL":/target \
    -e CARGO_TARGET_DIR=/target \
    -e BROKER_BIND="0.0.0.0:${BROKER_PORT}" \
    -e UDP_BIND="0.0.0.0:${UDP_PORT}" \
    "$DEV_IMAGE" /target/debug/examples/mqtt_udp_demo >/dev/null 2>&1

# 等 broker 起来
BROKER_IP=""
for _ in $(seq 1 20); do
    sleep 1
    BROKER_IP=$(docker inspect "$CONTAINER" --format \
        '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' 2>/dev/null)
    [ -z "$BROKER_IP" ] && continue
    if nc -z -w 1 "$BROKER_IP" "$BROKER_PORT" 2>/dev/null; then
        break
    fi
    BROKER_IP=""
done

if [ -z "$BROKER_IP" ]; then
    fail "演示服务的 broker 未就绪"
    echo "     容器日志："
    docker logs "$CONTAINER" 2>&1 | tail -15 | sed 's/^/     /'
    echo; echo "验收失败：$FAILURES 项"; exit 1
fi
pass "broker 就绪: ${BROKER_IP}:${BROKER_PORT}"
echo "     演示服务日志："
docker logs "$CONTAINER" 2>&1 | head -12 | sed 's/^/     /'

# ─────────────────────────────────────────────
step "3. 运行设备模拟器"
# ─────────────────────────────────────────────
"$PY" scripts/xiaozhi_sim/device_sim.py \
    --broker "${BROKER_IP}:${BROKER_PORT}" \
    --wait 6 $VERBOSE > /tmp/e2e_sim.log 2>&1
SIM_EXIT=$?

echo "     模拟器输出（尾部）："
tail -30 /tmp/e2e_sim.log | sed 's/^/     /'

# ─────────────────────────────────────────────
step "4. 断言"
# ─────────────────────────────────────────────
if [ "$SIM_EXIT" -eq 0 ]; then
    pass "模拟器整体判定 PASS"
else
    fail "模拟器整体判定 FAIL（exit=$SIM_EXIT）"
fi

# 逐项断言，便于定位
check_sim() {
    local name="$1" pattern="$2"
    if grep -q -- "$pattern" /tmp/e2e_sim.log; then
        pass "$name"
    else
        fail "$name"
    fi
}
check_sim "收到 tts start"          "✅ 收到 tts start"
check_sim "收到 tts stop"           "✅ 收到 tts stop"
check_sim "收到下行音频"            "✅ 收到下行音频"
check_sim "序列号从 1 连续"          "✅ 序列号从 1 连续"
check_sim "下行 timestamp 非 0"     "✅ 下行 timestamp 非 0"
check_sim "下行 timestamp 按 60 递增" "✅ 下行 timestamp 按 60 递增"

# hello 响应合规性（模拟器内部会严格校验，这里额外确认它没报错）
if grep -q "hello 响应合规" /tmp/e2e_sim.log; then
    pass "hello 响应合规（transport=udp / key+nonce 16B / server 非 0.0.0.0）"
else
    fail "hello 响应不合规或未收到"
fi

# 演示服务侧不应有 panic
if docker logs "$CONTAINER" 2>&1 | grep -qiE "panicked|RUST_BACKTRACE"; then
    fail "演示服务出现 panic"
    docker logs "$CONTAINER" 2>&1 | grep -iE "panicked" -A5 | head -10 | sed 's/^/     /'
else
    pass "演示服务无 panic"
fi

# ─────────────────────────────────────────────
step "汇总"
# ─────────────────────────────────────────────
if [ "$FAILURES" -eq 0 ]; then
    echo
    echo "ALL CHECKS PASSED (MQTT+UDP 端到端)"
    exit 0
fi
echo
echo "验收失败：$FAILURES 项"
echo "完整日志：/tmp/e2e_sim.log  /tmp/e2e_build.log"
exit 1
