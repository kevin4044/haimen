#!/usr/bin/env bash
#
# scripts/cd.sh —— 本地 CD：构建镜像 → 重建容器 → 验证真的换上了
#
# ⚠️ 核心教训：`docker restart` 不会换镜像。必须 `docker rm` + `docker run`。
#    这个脚本最后会【强制校验】容器实际使用的镜像 ID 等于本次构建出的镜像 ID，
#    不相等就报错退出。今天就是漏了这一步，白测一整轮。
#
# 用法：
#   ./scripts/cd.sh                 # 先跑 CI，再构建 + 部署
#   ./scripts/cd.sh --skip-ci       # 跳过 CI（急着验证时）
#   ./scripts/cd.sh --no-build      # 只重建容器，不重新构建镜像
#
# 环境变量：
#   HAIMEN_CONFIG_DIR   配置目录（默认 ~/works/haimen-data）
#   HAIMEN_PORT         宿主机端口（默认 9527）
#   HAIMEN_CONTAINER    容器名（默认 haimen-test）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CONFIG_DIR="${HAIMEN_CONFIG_DIR:-$HOME/works/haimen-data}"
HOST_PORT="${HAIMEN_PORT:-9527}"
CONTAINER="${HAIMEN_CONTAINER:-haimen-test}"
RUN_IMAGE="${HAIMEN_RUN_IMAGE:-haimen:local}"

SKIP_CI=0
NO_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --skip-ci)  SKIP_CI=1 ;;
        --no-build) NO_BUILD=1 ;;
        *) echo "未知参数: $arg" >&2; exit 2 ;;
    esac
done

step() {
    echo
    echo "══════════════════════════════════════════════════════"
    echo "▶ $*"
    echo "══════════════════════════════════════════════════════"
}

die() { echo "❌ $*" >&2; exit 1; }

# ─────────────────────────────────────────────
# 0. 环境检查
# ─────────────────────────────────────────────
step "环境检查"
docker info >/dev/null 2>&1 || die "Docker 不可用，请先启动 OrbStack"
command -v docker >/dev/null 2>&1 || die "找不到 docker"

GIT_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
GIT_DIRTY="$(git status --porcelain | wc -l | tr -d ' ')"
BRANCH="$(git branch --show-current 2>/dev/null || echo '-')"
echo "  分支:       $BRANCH"
echo "  提交:       $GIT_SHA"
echo "  未提交改动: $GIT_DIRTY 个 $([ "$GIT_DIRTY" != "0" ] && echo '⚠️  （镜像里会包含这些改动）')"
echo "  配置目录:   $CONFIG_DIR"
echo "  容器名:     $CONTAINER"
echo "  端口:       $HOST_PORT"

# ─────────────────────────────────────────────
# 1. CI
# ─────────────────────────────────────────────
if [ "$SKIP_CI" = "0" ]; then
    step "CI（fmt + clippy + test）"
    ./scripts/ci.sh || die "CI 未通过，已中止部署（旧容器保持运行）"
else
    step "CI"
    echo "  ⏭  已跳过（--skip-ci）"
fi

# ─────────────────────────────────────────────
# 2. 构建镜像
# ─────────────────────────────────────────────
if [ "$NO_BUILD" = "0" ]; then
    step "构建运行镜像 $RUN_IMAGE"
    docker build -t "$RUN_IMAGE" . || die "镜像构建失败，旧容器保持运行"
    # 额外打一个 commit 标签，方便回滚和对照
    docker tag "$RUN_IMAGE" "${RUN_IMAGE%%:*}:$GIT_SHA" 2>/dev/null || true
    echo "  ✅ 已构建并标记为 ${RUN_IMAGE%%:*}:$GIT_SHA"
else
    step "构建镜像"
    echo "  ⏭  已跳过（--no-build）"
fi

# ─────────────────────────────────────────────
# 3. 准备配置目录
# ─────────────────────────────────────────────
step "准备配置目录"
mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/settings.toml" ]; then
    echo "  ⚠️  $CONFIG_DIR/settings.toml 不存在，容器首次启动会因缺凭证退出"
    echo "      请在 Web 界面 http://localhost:$HOST_PORT 配置 ASR / TTS / Agent"
fi
ls -la "$CONFIG_DIR" | sed 's/^/  /'

# ─────────────────────────────────────────────
# 4. 重建容器（关键：不是 restart）
# ─────────────────────────────────────────────
step "重建容器（rm + run，不是 restart）"
if docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "  移除旧容器 $CONTAINER"
    docker rm -f "$CONTAINER" >/dev/null
fi

echo "  启动新容器"

# 小智传输协议。mqtt 模式下必须额外暴露 broker 端口与 UDP 音频端口，
# 否则设备连不上（OTA 下发的是宿主机局域网地址）。
XZ_PROTOCOL="websocket"
if [ -f "$CONFIG_DIR/settings.toml" ]; then
    PARSED=$(grep -E '^\s*protocol\s*=\s*"mqtt"' "$CONFIG_DIR/settings.toml" 2>/dev/null | head -1)
    [ -n "$PARSED" ] && XZ_PROTOCOL="mqtt"
fi

# MQTT broker 端口：取 settings 里 endpoint 的端口，缺省 1883
MQTT_PORT="1883"
if [ -f "$CONFIG_DIR/settings.toml" ]; then
    EP=$(grep -E '^\s*endpoint\s*=' "$CONFIG_DIR/settings.toml" 2>/dev/null | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -n "$EP" ] && echo "$EP" | grep -q ':'; then
        MQTT_PORT="${EP##*:}"
    fi
fi

# UDP 音频端口：取 settings 里 bind_addr 的端口，缺省 8888
UDP_PORT="8888"
if [ -f "$CONFIG_DIR/settings.toml" ]; then
    UB=$(grep -E '^\s*bind_addr\s*=' "$CONFIG_DIR/settings.toml" 2>/dev/null | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -n "$UB" ] && echo "$UB" | grep -q ':'; then
        UDP_PORT="${UB##*:}"
    fi
fi

if [ "$XZ_PROTOCOL" = "mqtt" ]; then
    echo "  小智协议: mqtt  → 额外暴露 TCP ${MQTT_PORT}（broker）与 UDP ${UDP_PORT}（音频）"
    docker run -d --name "$CONTAINER" \
        -p "${HOST_PORT}:9527" \
        -p "${MQTT_PORT}:${MQTT_PORT}" \
        -p "${UDP_PORT}:${UDP_PORT}/udp" \
        -v "$CONFIG_DIR":/root/.haimen \
        --restart unless-stopped \
        "$RUN_IMAGE" >/dev/null
else
    echo "  小智协议: websocket（只暴露 HTTP ${HOST_PORT}）"
    docker run -d --name "$CONTAINER" \
        -p "${HOST_PORT}:9527" \
        -v "$CONFIG_DIR":/root/.haimen \
        --restart unless-stopped \
        "$RUN_IMAGE" >/dev/null
fi

# ─────────────────────────────────────────────
# 5. 【强制校验】容器用的确实是刚构建的镜像
# ─────────────────────────────────────────────
step "校验容器镜像（防止跑到旧镜像上）"
BUILT_ID="$(docker inspect "$RUN_IMAGE" --format '{{.Id}}')"
RUNNING_ID="$(docker inspect "$CONTAINER" --format '{{.Image}}')"

echo "  刚构建的镜像: $BUILT_ID"
echo "  容器实际用的: $RUNNING_ID"

if [ "$BUILT_ID" != "$RUNNING_ID" ]; then
    echo
    echo "❌ 致命：容器没有用上新镜像！"
    echo "   这正是 docker restart 的坑。请检查上面的 docker run 是否成功。"
    exit 1
fi
echo "  ✅ 一致"

# ─────────────────────────────────────────────
# 6. 健康检查
# ─────────────────────────────────────────────
step "健康检查"
HEALTH_OK=0
for i in $(seq 1 20); do
    sleep 2
    if docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
        CODE="$(curl -s -o /dev/null -w '%{http_code}' -m 3 \
            "http://127.0.0.1:${HOST_PORT}/api/v1/system/info" 2>/dev/null || echo 000)"
        if [ "$CODE" = "200" ]; then
            HEALTH_OK=1
            break
        fi
    else
        echo "  ⚠️  容器退出了，看日志："
        docker logs "$CONTAINER" 2>&1 | tail -20 | sed 's/^/     /'
        exit 1
    fi
done

if [ "$HEALTH_OK" != "1" ]; then
    echo "  ❌ 健康检查失败（20 次重试后仍不可用）"
    echo "  日志："
    docker logs "$CONTAINER" 2>&1 | tail -20 | sed 's/^/     /'
    exit 1
fi

echo "  ✅ HTTP 200"
curl -s -m 3 "http://127.0.0.1:${HOST_PORT}/api/v1/system/info" | sed 's/^/  /'

# ─────────────────────────────────────────────
# 7. OTA 端点自检（设备接入用）
# ─────────────────────────────────────────────
step "OTA 端点自检"
OTA_BODY='{"version":2,"mac_address":"DE:AD:BE:EF:00:01","uuid":"selftest","chip_model_name":"esp32s3","application":{"name":"xiaozhi","version":"1.0.0"},"board":{"type":"bread-compact-wifi-lcd","name":"selftest"},"flash_size":16777216,"minimum_free_heap_size":"200000"}'
OTA_CODE="$(curl -s -o /tmp/cd_ota.json -w '%{http_code}' -m 5 -X POST \
    "http://127.0.0.1:${HOST_PORT}/xiaozhi/ota/" \
    -H 'Content-Type: application/json' \
    -H 'Device-Id: DE:AD:BE:EF:00:01' \
    -H 'Client-Id: selftest' \
    -H 'Activation-Version: 1' \
    -d "$OTA_BODY" 2>/dev/null || echo 000)"
echo "  POST /xiaozhi/ota/ → HTTP $OTA_CODE"
if [ "$OTA_CODE" = "200" ]; then
    python3 -c "
import json
d = json.load(open('/tmp/cd_ota.json'))
ws = (d.get('websocket') or {}).get('url')
print(f'  websocket.url = {ws}')
" 2>/dev/null || cat /tmp/cd_ota.json 2>/dev/null | head -c 300
    echo "  ✅ OTA 正常（注意：地址必须带结尾斜杠 /xiaozhi/ota/）"
else
    echo "  ⚠️  OTA 端点异常，设备会连不上"
fi

# ─────────────────────────────────────────────
# 8. 汇总
# ─────────────────────────────────────────────
step "部署完成"
echo "  分支:       $BRANCH @ $GIT_SHA"
echo "  镜像:       $RUN_IMAGE (= ${RUN_IMAGE%%:*}:$GIT_SHA)"
echo "  容器:       $CONTAINER ($(docker ps --filter "name=$CONTAINER" --format '{{.Status}}'))"
echo "  配置:       $CONFIG_DIR/settings.toml"
echo "  Web UI:     http://localhost:$HOST_PORT"
echo "  OTA 地址:   http://<本机局域网IP>:$HOST_PORT/xiaozhi/ota/   ⚠️ 结尾斜杠不能少"
echo
echo "  看日志:     docker logs -f $CONTAINER"
echo "  看状态:     ./scripts/status.sh"
echo
echo "  ⚠️  设备如果卡在旧会话，需要【断电 10 秒】重启才能重连"
