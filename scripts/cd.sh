#!/usr/bin/env bash
#
# scripts/cd.sh —— CD：交叉编译 → 部署到 Mac mini（原生二进制）→ 验证
#
# 历史：这原来是 Docker 版（构建镜像 → 重建容器 → 校验镜像 ID）。
# 2026-10-19 迁到 Mac mini 后改成原生部署，原因见 specs/运维-迁移到Mac-mini.md:
#   Colima 的 Docker 不转发 UDP 端口（实测宿主机 lsof 无监听、对端 Connection
#   refused），而音频通道就是 UDP，所以容器方案在 mini 上不可用。
#
# 用法：
#   ./scripts/cd.sh                 # CI → 构建 → 部署 → 验证
#   ./scripts/cd.sh --skip-ci       # 跳过 CI（急着验证时）
#   ./scripts/cd.sh --no-deploy     # 只构建并校验产物，不部署
#   ./scripts/cd.sh --rollback      # 回滚到上一版（bin/haimen.prev）
#
# 环境变量：
#   HAIMEN_DEPLOY_HOST   部署目标（默认 wangjunlong@192.168.50.33）
#   HAIMEN_REMOTE_DIR    远端目录，相对远端 ${HOME}（默认 haimen）
#   HAIMEN_HTTP_PORT     健康检查端口（默认 9527）
#   HAIMEN_LABEL         launchd 标签（默认 ai.haimen.serve）
#
# ⚠️ 这个脚本存在的意义是下面这条安全约束：
#   新二进制必须【在目标机上真跑通 --version】才允许替换旧的。
#   交叉编译 + 部署目标设错，可以产出"本机能构建、目标机加载不了"的产物
#   （编译机 macOS 27，目标机 15.7；不设 MACOSX_DEPLOYMENT_TARGET 就会踩）。
#   所以顺序是：构建 → 校验架构与 minos → 传输 → 目标机试跑 → 备份 → 替换
#   → 重启 → 校验线上 sha 与本次构建一致。任何一步失败都保留旧二进制。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

DEPLOY_HOST="${HAIMEN_DEPLOY_HOST:-wangjunlong@192.168.50.33}"
REMOTE_DIR="${HAIMEN_REMOTE_DIR:-haimen}"
HTTP_PORT="${HAIMEN_HTTP_PORT:-9527}"
LABEL="${HAIMEN_LABEL:-ai.haimen.serve}"

# 目标机是 Intel Mac mini，跑 macOS 15.7.5
TARGET="x86_64-apple-darwin"
TOOLCHAIN="+stable"          # 见下方说明：不能用项目固定的 1.96 工具链
DEPLOY_TARGET_MACOS="15.0"

SKIP_CI=0
NO_DEPLOY=0
ROLLBACK=0
for arg in "$@"; do
  case "${arg}" in
    --skip-ci)   SKIP_CI=1 ;;
    --no-deploy) NO_DEPLOY=1 ;;
    --rollback)  ROLLBACK=1 ;;
    -h|--help)   sed -n '2,26p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "未知参数: ${arg}（用 --help 看用法）" >&2; exit 2 ;;
  esac
done

STEP=0
step() { STEP=$((STEP + 1)); echo; echo "▶ [${STEP}] $*"; }
info() { echo "   $*"; }
ok()   { echo "   ✅ $*"; }
warn() { echo "   ⚠️  $*"; }
die()  { echo "   ❌ $*" >&2; exit 1; }

SSH_OPTS=(-o ConnectTimeout=10 -o BatchMode=yes)
SSH_DEST_HOST="${DEPLOY_HOST#*@}"

remote() { ssh "${SSH_OPTS[@]}" "${DEPLOY_HOST}" "$@"; }

# ─────────────────────────────────────────────
# 0. 环境检查
# ─────────────────────────────────────────────
step "环境检查"

GIT_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
GIT_DIRTY="$(git status --porcelain | wc -l | tr -d ' ')"
BRANCH="$(git branch --show-current 2>/dev/null || echo '-')"
info "分支 ${BRANCH} @ ${GIT_SHA}，未提交改动 ${GIT_DIRTY} 个"

if ! remote true 2>/dev/null; then
  die "SSH 连不上 ${DEPLOY_HOST}（需要免密登录）"
fi
ok "SSH 可达 ${DEPLOY_HOST}"

REMOTE_ARCH="$(remote 'uname -m' 2>/dev/null)"
[ "${REMOTE_ARCH}" = "x86_64" ] || die "目标机架构是 ${REMOTE_ARCH}，期望 x86_64"
ok "目标机架构 x86_64"

# ─────────────────────────────────────────────
# 回滚分支
# ─────────────────────────────────────────────
if [ "${ROLLBACK}" = "1" ]; then
  step "回滚到上一版（bin/haimen.prev）"
  remote "bash -s" <<REMOTE_EOF
set -euo pipefail
BIN="\${HOME}/${REMOTE_DIR}/bin/haimen"
[ -f "\${BIN}.prev" ] || { echo "   ❌ 没有 \${BIN}.prev，无法回滚"; exit 1; }
cp "\${BIN}.prev" "\${BIN}"
chmod +x "\${BIN}"
launchctl kickstart -k "gui/\$(id -u)/${LABEL}"
echo "   已回滚并重启"
REMOTE_EOF
  sleep 5
  remote "lsof -nP -iTCP:${HTTP_PORT} -sTCP:LISTEN >/dev/null 2>&1" \
    && ok "回滚后端口 ${HTTP_PORT} 已监听" || die "回滚后端口未监听"
  echo
  echo "✅ 已回滚到 ${DEPLOY_HOST}:${REMOTE_DIR}/bin/haimen.prev"
  exit 0
fi

# ─────────────────────────────────────────────
# 1. CI
# ─────────────────────────────────────────────
if [ "${SKIP_CI}" = "0" ]; then
  step "CI（fmt + clippy + test）"
  ./scripts/ci.sh || die "CI 未通过，已中止部署（线上服务保持运行）"
  ok "CI 通过"
else
  step "CI 已跳过（--skip-ci）"
fi

# ─────────────────────────────────────────────
# 2. 交叉编译
# ─────────────────────────────────────────────
step "交叉编译 ${TARGET}（部署目标 macOS ${DEPLOY_TARGET_MACOS}）"

command -v cargo >/dev/null 2>&1 || die "找不到 cargo"

# 为什么必须 +stable：项目 rust-toolchain.toml 固定 1.96，而那条工具链
# 交叉编译时 proc-macro 全部 `can't find crate`
# （tokio_macros / thiserror_impl / zerofrom_derive）。标准 stable 正常。
if ! rustup target list --installed --toolchain stable 2>/dev/null | grep -qx "${TARGET}"; then
  info "安装交叉目标 ${TARGET} …"
  rustup target add "${TARGET}" --toolchain stable >/dev/null 2>&1 \
    || die "安装 ${TARGET} 失败"
fi
ok "目标 ${TARGET} 就绪（工具链 ${TOOLCHAIN}）"

# ort 在构建期要从 cdn.pyke.io 下载 ONNX Runtime 二进制（每平台约 19MB）。
# 该 CDN 直连只有 30~40 KB/s —— 19MB 要 8 分钟，而 ort 的 build script 有全局
# 超时，会直接失败。这里自动从 macOS 系统代理推导，可用 HAIMEN_BUILD_PROXY 覆盖。
if [ -z "${HTTPS_PROXY:-}" ]; then
  PROXY_HOST="$(scutil --proxy 2>/dev/null | awk '/HTTPSProxy/{print $3}')"
  PROXY_PORT="$(scutil --proxy 2>/dev/null | awk '/HTTPSPort/{print $3}')"
  if [ -n "${PROXY_HOST}" ] && [ -n "${PROXY_PORT}" ]; then
    export HTTPS_PROXY="http://${PROXY_HOST}:${PROXY_PORT}"
    export HTTP_PROXY="${HTTPS_PROXY}"
    info "构建代理: ${HTTPS_PROXY}（ort 下载 ONNX Runtime 用）"
  else
    warn "没检测到系统代理；如果 ort 下载超时，用 HAIMEN_BUILD_PROXY=http://host:port 指定"
  fi
fi

export MACOSX_DEPLOYMENT_TARGET="${DEPLOY_TARGET_MACOS}"
BIN="target/${TARGET}/release/haimen"

BUILD_START=$(date +%s)
if ! cargo "${TOOLCHAIN}" build --release --target "${TARGET}"; then
  die "构建失败（线上服务未受影响）"
fi
BUILD_SECS=$(( $(date +%s) - BUILD_START ))
[ -f "${BIN}" ] || die "产物不存在: ${BIN}"
ok "构建完成（${BUILD_SECS}s，$(ls -lh "${BIN}" | awk '{print $5}')）"

# ─────────────────────────────────────────────
# 3. 产物校验（防止把跑不起来的二进制推上去）
# ─────────────────────────────────────────────
step "产物校验"

FILE_INFO="$(file "${BIN}" 2>/dev/null || true)"
echo "${FILE_INFO}" | grep -q "x86_64" || die "产物架构不是 x86_64: ${FILE_INFO}"
ok "架构 x86_64"

MINOS="$(otool -l "${BIN}" 2>/dev/null | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')"
[ -n "${MINOS}" ] || die "读不到产物 minos"
if awk -v a="${MINOS}" -v b="${DEPLOY_TARGET_MACOS}" 'BEGIN{exit !(a+0 <= b+0)}'; then
  ok "minos ${MINOS} ≤ ${DEPLOY_TARGET_MACOS}（目标机 macOS 15.7.5 可加载）"
else
  die "minos ${MINOS} 高于 ${DEPLOY_TARGET_MACOS}，目标机会拒绝加载"
fi

NON_SYSTEM_LIBS="$(otool -L "${BIN}" 2>/dev/null | tail -n +2 \
  | grep -vE '/usr/lib|/System/' | awk '{print $1}' || true)"
if [ -n "${NON_SYSTEM_LIBS}" ]; then
  warn "依赖了非系统动态库（目标机可能没有）："
  echo "${NON_SYSTEM_LIBS}" | sed 's/^/         /'
  die "请改静态链接，或把库一并部署"
fi
ok "只依赖系统库，自包含"

LOCAL_SHA="$(shasum -a 256 "${BIN}" | awk '{print $1}')"
info "sha256 ${LOCAL_SHA:0:16}…"

if [ "${NO_DEPLOY}" = "1" ]; then
  step "只构建（--no-deploy）"
  ok "产物: ${BIN}"
  exit 0
fi

# ─────────────────────────────────────────────
# 4. 传输 + 目标机试跑（此时还没动线上）
# ─────────────────────────────────────────────
step "传输并试跑（不动线上）"

REMOTE_TMP="/tmp/haimen-deploy-$$"
trap 'remote "rm -f ${REMOTE_TMP}" >/dev/null 2>&1 || true' EXIT

scp -q "${SSH_OPTS[@]}" "${BIN}" "${DEPLOY_HOST}:${REMOTE_TMP}" || die "scp 失败"
ok "已传输至 ${DEPLOY_HOST}:${REMOTE_TMP}"

REMOTE_SHA="$(remote "shasum -a 256 ${REMOTE_TMP} | awk '{print \$1}'" 2>/dev/null)"
[ "${REMOTE_SHA}" = "${LOCAL_SHA}" ] || die "传输校验失败（本地 ${LOCAL_SHA} / 远端 ${REMOTE_SHA}）"
ok "sha256 一致"

# ★ 关键：在目标机上真跑一次。跑不起来就到此为止，线上仍是旧版。
REMOTE_VER="$(remote "chmod +x ${REMOTE_TMP} && ${REMOTE_TMP} --version" 2>&1 | head -1)"
echo "${REMOTE_VER}" | grep -q "^haimen " || die "目标机试跑失败: ${REMOTE_VER}"
ok "目标机试跑通过: ${REMOTE_VER}"

# ─────────────────────────────────────────────
# 5. 备份 → 原子替换 → 重启
# ─────────────────────────────────────────────
step "替换并重启服务"

remote "REMOTE_TMP='${REMOTE_TMP}' REMOTE_DIR='${REMOTE_DIR}' LABEL='${LABEL}' bash -s" <<'REMOTE_EOF' || die "远端部署失败"
set -euo pipefail
DIR="${HOME}/${REMOTE_DIR}"
BIN="${DIR}/bin/haimen"
mkdir -p "${DIR}/bin"

if [ -f "${BIN}" ]; then
    cp "${BIN}" "${BIN}.prev"
    echo "   已备份旧版本 → bin/haimen.prev"
fi

# mv 在同一文件系统内是原子的，不会出现"半截二进制"
mv "${REMOTE_TMP}" "${BIN}"
chmod +x "${BIN}"
echo "   已替换 ${BIN}"

# kickstart -k：杀掉当前进程后按 plist 拉起（比 bootout+bootstrap 干净）
if ! launchctl kickstart -k "gui/$(id -u)/${LABEL}" 2>/dev/null; then
    echo "   kickstart 失败，回退 bootout + bootstrap"
    launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "${HOME}/Library/LaunchAgents/${LABEL}.plist"
fi
echo "   已重启 launchd 服务"
REMOTE_EOF
trap - EXIT
ok "部署完成"

# ─────────────────────────────────────────────
# 6. 验证真的换上了
# ─────────────────────────────────────────────
step "部署后验证"

# 端口就绪（最多 20 秒）
READY=0
for _ in $(seq 1 20); do
  if remote "lsof -nP -iTCP:${HTTP_PORT} -sTCP:LISTEN >/dev/null 2>&1" 2>/dev/null; then
    READY=1; break
  fi
  sleep 1
done
[ "${READY}" = "1" ] || die "服务没起来：HTTP 端口 ${HTTP_PORT} 无监听"
ok "HTTP 端口 ${HTTP_PORT} 已监听"

# 线上二进制必须就是本次构建的那个
RUNNING_SHA="$(remote "shasum -a 256 \${HOME}/${REMOTE_DIR}/bin/haimen | awk '{print \$1}'" 2>/dev/null)"
[ "${RUNNING_SHA}" = "${LOCAL_SHA}" ] || die "线上二进制不是本次构建！本地 ${LOCAL_SHA} / 线上 ${RUNNING_SHA}"
ok "线上二进制 == 本次构建（sha256 一致）"

# launchd 状态
LAUNCHD_LINE="$(remote "launchctl list 2>/dev/null | grep '${LABEL}'" 2>/dev/null || true)"
echo "${LAUNCHD_LINE}" | grep -q "${LABEL}" || die "launchd 里找不到 ${LABEL}"
LAUNCHD_EXIT="$(echo "${LAUNCHD_LINE}" | awk '{print $2}')"
[ "${LAUNCHD_EXIT}" = "0" ] || die "launchd 上次退出码 ${LAUNCHD_EXIT}（非 0）"
ok "launchd 正常（退出码 0）"

# 三个端口
MISSING=""
remote "lsof -nP -iTCP:${HTTP_PORT} -sTCP:LISTEN >/dev/null 2>&1" || MISSING="${MISSING} ${HTTP_PORT}/tcp"
remote "lsof -nP -iTCP:1883 -sTCP:LISTEN >/dev/null 2>&1"       || MISSING="${MISSING} 1883/tcp"
remote "lsof -nP -iUDP:8888 >/dev/null 2>&1"                    || MISSING="${MISSING} 8888/udp"
[ -z "${MISSING}" ] || die "端口未监听:${MISSING}"
ok "端口正常（${HTTP_PORT}/tcp, 1883/tcp, 8888/udp）"

# HTTP 健康
CODE="$(curl -s -o /dev/null -w '%{http_code}' -m 5 "http://${SSH_DEST_HOST}:${HTTP_PORT}/api/v1/system/info" 2>/dev/null || echo 000)"
[ "${CODE}" = "200" ] || die "HTTP 健康检查失败（${CODE}）"
ok "HTTP /api/v1/system/info → 200"

# OTA 冒烟：endpoint 必须指向目标机
OTA_JSON="$(curl -s -m 8 -X POST "http://${SSH_DEST_HOST}:${HTTP_PORT}/xiaozhi/ota/" \
  -H 'Content-Type: application/json' -H 'Device-Id: DE:AD:BE:EF:00:01' \
  -d '{"version":2,"mac_address":"DE:AD:BE:EF:00:01","uuid":"cd-selftest","application":{"name":"xiaozhi","version":"2.5.2"},"board":{"type":"bread-compact-wifi-lcd","name":"cd","mac":"DE:AD:BE:EF:00:01"}}' 2>/dev/null || true)"
OTA_ENDPOINT="$(echo "${OTA_JSON}" | python3 -c 'import json,sys; print((json.load(sys.stdin).get("mqtt") or {}).get("endpoint",""))' 2>/dev/null || true)"
[ -n "${OTA_ENDPOINT}" ] || die "OTA 冒烟失败（拿不到 mqtt.endpoint）"
ok "OTA mqtt.endpoint = ${OTA_ENDPOINT}"
case "${OTA_ENDPOINT}" in
  "${SSH_DEST_HOST}":*) ok "endpoint 指向本机，正确" ;;
  *) warn "endpoint 指向 ${OTA_ENDPOINT}，不是本机 ${SSH_DEST_HOST} —— 设备会连错地方！" ;;
esac

# 清理 OTA 自检产生的假设备记录，避免固件面板出现 DE:AD:BE:EF:00:01。
# 旧版本没有该端点时只 warn，不阻断部署。
FORGET_CODE="$(curl -s -o /dev/null -w '%{http_code}' -m 8 -X POST \
  "http://${SSH_DEST_HOST}:${HTTP_PORT}/api/v1/xiaozhi/firmware/report/forget" \
  -H 'Content-Type: application/json' -d '{"device":"DE:AD:BE:EF:00:01"}' 2>/dev/null || echo 000)"
if [ "${FORGET_CODE}" = "200" ]; then
  ok "已清理 OTA 自检设备记录"
else
  warn "OTA 自检设备记录清理失败（HTTP ${FORGET_CODE}），可稍后手动删除"
fi

# 进程
PID="$(remote "pgrep -f '${REMOTE_DIR}/bin/haimen serve' | head -1" 2>/dev/null || true)"
[ -n "${PID}" ] || die "找不到运行中的 haimen 进程"
ok "进程运行中（PID ${PID}）"

# ─────────────────────────────────────────────
# 7. 汇总
# ─────────────────────────────────────────────
echo
echo "════════════════════════════════════════════"
echo "✅ CD 完成"
echo "   分支:    ${BRANCH} @ ${GIT_SHA}"
echo "   目标:    ${DEPLOY_HOST}（原生二进制，launchd: ${LABEL}）"
echo "   版本:    ${REMOTE_VER}"
echo "   sha256:  ${LOCAL_SHA:0:16}…"
echo "   OTA:     http://${SSH_DEST_HOST}:${HTTP_PORT}/xiaozhi/ota/   ⚠️ 结尾斜杠不能少"
echo "   Web UI:  http://${SSH_DEST_HOST}:${HTTP_PORT}"
echo
echo "   看日志:  ssh ${DEPLOY_HOST} 'tail -f ~/${REMOTE_DIR}/logs/haimen.log'"
echo "   回滚:    ./scripts/cd.sh --rollback"
echo
echo "   ⚠️  设备不重启就不会重新拉 OTA；改了 endpoint 需断电重启设备"
echo "════════════════════════════════════════════"
