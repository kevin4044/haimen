#!/usr/bin/env bash
#
# scripts/status.sh —— 一键体检：代码 / 镜像 / 容器 / 配置 / 设备 是否一致
#
# 排查时先跑这个，能一眼看出：
#   · 容器是不是在跑旧镜像（今天的坑）
#   · 配置有没有生效
#   · 设备连没连上
#   · 有没有 ASR 错误
#
# 用法：./scripts/status.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CONFIG_DIR="${HAIMEN_CONFIG_DIR:-$HOME/works/haimen-data}"
HOST_PORT="${HAIMEN_PORT:-9527}"
CONTAINER="${HAIMEN_CONTAINER:-haimen-test}"
RUN_IMAGE="${HAIMEN_RUN_IMAGE:-haimen:local}"

ok()   { printf "  ✅ %s\n" "$*"; }
bad()  { printf "  ❌ %s\n" "$*"; }
warn() { printf "  ⚠️  %s\n" "$*"; }
section() { echo; echo "── $* ──"; }

echo "════════════════════════════════════════"
echo "  haimen 状态体检"
echo "════════════════════════════════════════"

# ── 1. 代码 ──
section "代码"
echo "  分支:       $(git branch --show-current 2>/dev/null || echo '-')"
echo "  提交:       $(git rev-parse --short HEAD 2>/dev/null || echo '-') $(git log -1 --format='%s' 2>/dev/null | cut -c1-60)"
DIRTY="$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
if [ "$DIRTY" = "0" ]; then ok "工作区干净"; else warn "有 $DIRTY 个未提交改动"; fi

# ── 2. 镜像 ──
section "镜像"
if docker image inspect "$RUN_IMAGE" >/dev/null 2>&1; then
    BUILT_ID="$(docker inspect "$RUN_IMAGE" --format '{{.Id}}')"
    BUILT_TIME="$(docker inspect "$RUN_IMAGE" --format '{{.Created}}')"
    echo "  $RUN_IMAGE"
    echo "    ID:   $(echo "$BUILT_ID" | cut -c1-19)"
    echo "    构建: $BUILT_TIME"
    echo "    大小: $(docker image inspect "$RUN_IMAGE" --format '{{.Size}}' | awk '{printf "%.0f MB", $1/1024/1024}')"
else
    bad "镜像 $RUN_IMAGE 不存在"
    BUILT_ID=""
fi

# ── 3. 容器 ──
section "容器"
if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
    ok "运行中 — $(docker ps --filter "name=$CONTAINER" --format '{{.Status}}')"
    RUNNING_ID="$(docker inspect "$CONTAINER" --format '{{.Image}}')"
    echo "    镜像: $(echo "$RUNNING_ID" | cut -c1-19)"
    if [ -n "$BUILT_ID" ] && [ "$BUILT_ID" = "$RUNNING_ID" ]; then
        ok "容器镜像 == 当前镜像（代码是最新的）"
    else
        bad "容器镜像 ≠ 当前镜像 —— 跑的是旧代码！"
        echo "       修复: ./scripts/cd.sh --skip-ci    （注意 docker restart 没用）"
    fi
    echo "    重启策略: $(docker inspect "$CONTAINER" --format '{{.HostConfig.RestartPolicy.Name}}')"
    echo "    挂载:"
    docker inspect "$CONTAINER" --format '{{range .Mounts}}      {{.Source}} → {{.Destination}}{{println}}{{end}}' | sed '/^$/d'
elif docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
    bad "存在但未运行"
    docker logs "$CONTAINER" 2>&1 | tail -10 | sed 's/^/      /'
else
    bad "容器 $CONTAINER 不存在"
    RUNNING_ID=""
fi

# ── 4. HTTP 服务 ──
section "HTTP 服务"
CODE="$(curl -s -o /tmp/st_sys.json -w '%{http_code}' -m 3 "http://127.0.0.1:${HOST_PORT}/api/v1/system/info" 2>/dev/null || echo 000)"
if [ "$CODE" = "200" ]; then
    ok "http://127.0.0.1:${HOST_PORT} 响应正常"
    cat /tmp/st_sys.json 2>/dev/null | sed 's/^/      /'
    echo
else
    bad "HTTP ${CODE}（服务未响应）"
fi

# ── 5. 语音配置 ──
section "语音配置（脱敏）"
if [ "$CODE" = "200" ]; then
    for p in asr tts agent; do
        echo "  [$p]"
        curl -s -m 5 "http://127.0.0.1:${HOST_PORT}/api/v1/settings/$p" 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin).get('data') or {}
    print(f\"    active_provider: {d.get('active_provider')}\")
    for name, cfg in (d.get('providers') or {}).items():
        if not isinstance(cfg, dict):
            continue
        shown = {}
        for k, v in cfg.items():
            if any(t in k.lower() for t in ('key', 'token', 'secret')):
                shown[k] = f'<{len(str(v))} 字符>' if v else '<空>'
            else:
                shown[k] = v
        print(f'    {name}: {shown}')
except Exception as e:
    print(f'    读取失败: {e}')
" 2>/dev/null
    done
else
    warn "服务不可用，跳过"
fi

# ── 6. 配置文件 ──
section "配置文件"
if [ -f "$CONFIG_DIR/settings.toml" ]; then
    ok "$CONFIG_DIR/settings.toml（$(wc -c < "$CONFIG_DIR/settings.toml") 字节）"
else
    bad "$CONFIG_DIR/settings.toml 不存在"
fi

# ── 7. 设备连接 ──
section "设备连接"
DEVS="$(lsof -nP -iTCP:"$HOST_PORT" -sTCP:ESTABLISHED 2>/dev/null | grep -vE "127\.0\.0\.1|\[::1\]" | awk '{print $9}' | sort -u)"
if [ -n "$DEVS" ]; then
    ok "已连接："
    echo "$DEVS" | sed 's/^/      /'
else
    warn "没有设备连接（设备可能卡在旧会话，需要断电 10 秒重启）"
fi

# ── 8. 最近错误 ──
section "最近日志（错误/警告，去重）"
if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
    docker logs "$CONTAINER" --since 10m 2>&1 \
        | grep -E "ERROR|WARN" \
        | sed -E 's/^[[:space:]]*//' \
        | sed -E 's/device_id=[0-9a-f:]+//' \
        | sort | uniq -c | sort -rn | head -8 \
        | sed 's/^/  /'
    COUNT="$(docker logs "$CONTAINER" --since 10m 2>&1 | grep -cE "ERROR" || true)"
    echo
    if [ "${COUNT:-0}" -gt 0 ]; then
        bad "近 10 分钟有 $COUNT 条 ERROR"
        echo "      完整错误："
        docker logs "$CONTAINER" --since 10m 2>&1 | grep -E "ERROR" | tail -5 | sed 's/^/      /'
    else
        ok "近 10 分钟无 ERROR"
    fi
else
    warn "容器未运行，跳过"
fi

echo
echo "════════════════════════════════════════"
