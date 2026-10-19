#!/usr/bin/env bash
#
# 04 - notify 通路的机械验收
#
# 用法：
#   bash specs/notify-channel/04-acceptance.sh --task T1
#   bash specs/notify-channel/04-acceptance.sh --task T4    # 端到端（需要能访问部署中的 haimen）
#   bash specs/notify-channel/04-acceptance.sh              # 默认 T1
#
# 全部编译类检查在 Docker 开发容器里跑（本机不装 Rust）。
# 退出码 0 = 通过；非 0 = 失败。
#
# 注意：变量一律写成 ${VAR}。裸 $VAR 紧跟全角字符时，bash 会把多字节首字节
# 当成标识符的一部分而报 unbound variable —— 这个坑本项目踩过三次。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

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
    echo; echo "验收失败：${FAILURES} 项"; exit 1
fi
if [ ! -x "${DEV}" ]; then
    fail "缺少 ${DEV}"
    echo; echo "验收失败：${FAILURES} 项"; exit 1
fi
info "环境检查通过（Docker 可用）"

# ─────────────────────────────────────────────
# 0b. 改动白名单
# ─────────────────────────────────────────────
ALLOWED_CHANGED_RE='^(crates/haimen-xiaozhi/src/(notify|say_audio|lib|types)\.rs|src/gateway/mod\.rs|src/web/mod\.rs|specs/notify-channel/)'
CHANGED="$(git -c core.quotePath=false status --porcelain | awk '{print $2}' | sed 's/^"//; s/"$//')"
if [ -n "${CHANGED}" ]; then
    OUTSIDE="$(echo "${CHANGED}" | grep -Ev "${ALLOWED_CHANGED_RE}" || true)"
    if [ -n "${OUTSIDE}" ]; then
        fail "改动了白名单外的文件："
        echo "${OUTSIDE}" | sed 's/^/     /'
    else
        pass "改动都在白名单内"
    fi
fi

# ─────────────────────────────────────────────
# 1. 编译 + 测试（所有任务都要）
# ─────────────────────────────────────────────
info "cargo check + test（容器内）"
if docker run --rm -v "${PWD}":/src -w /src \
      -v "haimen-cargo-registry":/usr/local/cargo/registry \
      -v "haimen-target":/target -e CARGO_TARGET_DIR=/target \
      -e HTTP_PROXY= -e HTTPS_PROXY= \
      haimen-dev:rust sh -c 'cargo check --workspace && cargo test -p haimen-xiaozhi --lib' \
      >/tmp/notify-ci.log 2>&1; then
    pass "cargo check + 测试通过"
else
    fail "cargo check / 测试失败（见 /tmp/notify-ci.log）"
    tail -20 /tmp/notify-ci.log | sed 's/^/     /'
fi

# ─────────────────────────────────────────────
# 2. 按任务做静态检查
# ─────────────────────────────────────────────
case "${TASK}" in
T1)
    SAY="crates/haimen-xiaozhi/src/say_audio.rs"
    if [ -f "${SAY}" ]; then
        pass "存在 ${SAY}"
    else
        fail "缺少 ${SAY}"
    fi

    # 内存表 + 一次性语义
    has "SayAudioStore" "${SAY}"      && pass "有 SayAudioStore"      || fail "缺少 SayAudioStore"
    has "fn insert" "${SAY}"          && pass "有 insert"             || fail "缺少 insert"
    has "fn take" "${SAY}"            && pass "有 take（一次性取出）"  || fail "缺少 take"
    has "fn sweep" "${SAY}"           && pass "有 sweep（TTL 兜底）"   || fail "缺少 sweep"
    has "ttl" "${SAY}"                && pass "条目带 ttl"            || fail "条目缺 ttl"

    # 路由
    has "/xiaozhi/say" "${SAY}" crates/haimen-xiaozhi/src/lib.rs \
        && pass "路由 /xiaozhi/say 已注册" || fail "路由 /xiaozhi/say 未注册"

    # 音频类型必须是 Ogg（设备按 Ogg 解析）
    if grep -qE "audio/ogg|opus/ogg" "${SAY}" 2>/dev/null; then
        pass "Content-Type 是 audio/ogg"
    else
        fail "Content-Type 不是 audio/ogg"
    fi

    # 不得重定向（设备不跟随 3xx）
    if grep -qE "Redirect|MOVED_PERMANENTLY|temporary_redirect" "${SAY}" 2>/dev/null; then
        fail "出现了重定向响应（设备不跟随 3xx）"
    else
        pass "没有重定向响应"
    fi
    ;;
T2)
    NOTIFY="crates/haimen-xiaozhi/src/notify.rs"
    [ -f "${NOTIFY}" ] && pass "存在 ${NOTIFY}" || fail "缺少 ${NOTIFY}"

    # 接口契约
    has "/api/v1/xiaozhi/say" "${NOTIFY}" crates/haimen-xiaozhi/src/lib.rs \
        && pass "接口 /api/v1/xiaozhi/say 已注册" || fail "接口未注册"

    # 四个错误码
    for code in device_busy multiple_devices device_offline tts_failed; do
        has "${code}" "${NOTIFY}" && pass "错误码 ${code}" || fail "缺少错误码 ${code}"
    done

    # 409 要带实际状态
    has "device_busy" "${NOTIFY}" && grep -q "state" "${NOTIFY}" 2>/dev/null \
        && pass "device_busy 带 state 字段" || fail "device_busy 未带 state"

    # 鉴权可选
    has "say_token" crates/haimen-xiaozhi/src/types.rs \
        && pass "配置有 say_token" || fail "配置缺 say_token"

    # 下发的消息体
    has '"notify"' "${NOTIFY}" && pass "构造 type=notify" || fail "未构造 notify 消息"
    has "audio_url" "${NOTIFY}" && pass "带 audio_url"   || fail "缺 audio_url"

    # 音色策略：必须沿用配置，不得由调用方指定
    if grep -qE '\\bvoice\\b' "${NOTIFY}" 2>/dev/null; then
        if grep -qE 'voice_override|effective_voice|resolve_voice' "${NOTIFY}" 2>/dev/null; then
            pass "音色走与正常回复相同的解析路径"
        elif grep -qE 'cfg\\.voice|tts_config' "${NOTIFY}" 2>/dev/null; then
            fail "只读了配置、没考虑 voice_override，CLI 覆盖时会音色不一致"
        else
            fail "疑似接受调用方传入的音色（应与正常回复保持一致）"
        fi
    else
        info "未出现 voice 字样，人工确认音色是否来自配置"
    fi
    ;;
T3)
    DOC="specs/notify-channel/HA-配置样例.md"
    [ -f "${DOC}" ] && pass "存在 ${DOC}" || fail "缺少 ${DOC}"
    has "rest_command" "${DOC}" && pass "含 rest_command 样例" || fail "缺 rest_command 样例"
    has "xiaozhi_say" "${DOC}"  && pass "含服务名"             || fail "缺服务名"
    ;;
T4)
    info "端到端：需要一个部署中的 haimen（默认取脚本环境变量 HAIMEN_BASE）"
    BASE="${HAIMEN_BASE:-http://192.168.50.33:9527}"
    TOKEN="${HAIMEN_SAY_TOKEN:-}"

    AUTH=()
    if [ -n "${TOKEN}" ]; then
        AUTH=(-H "Authorization: Bearer ${TOKEN}")
    fi

    # 4a. 接口存在（不接受 404）
    CODE="$(curl -s -o /tmp/say-resp.json -w '%{http_code}' -m 20 \
        -X POST "${BASE}/api/v1/xiaozhi/say" \
        -H 'Content-Type: application/json' \
        "${AUTH[@]}" \
        -d '{"text":"验收测试，忽略我"}' 2>/dev/null || echo 000)"
    info "POST /api/v1/xiaozhi/say → HTTP ${CODE}"

    case "${CODE}" in
      200)
        pass "接口返回 200"
        # 4b. 响应结构
        python3 - <<'PY' || fail "响应结构不对"
import json, sys
d = json.load(open('/tmp/say-resp.json'))
assert d.get('ok') is True, d
url = d.get('audio_url', '')
assert url.startswith('http'), d
print(f"     audio_url = {url}")
PY
        # 4c. 音频可拉取，且是 Ogg Opus
        URL="$(python3 -c "import json;print(json.load(open('/tmp/say-resp.json')).get('audio_url',''))" 2>/dev/null)"
        if [ -n "${URL}" ]; then
            CT="$(curl -s -o /tmp/say-audio.bin -w '%{content_type}' -m 20 "${URL}" 2>/dev/null || true)"
            case "${CT}" in
              audio/ogg*|application/ogg*) pass "音频 Content-Type=${CT}" ;;
              *) fail "音频 Content-Type 异常: ${CT}" ;;
            esac
            if [ -s /tmp/say-audio.bin ]; then
                if head -c 4 /tmp/say-audio.bin | grep -q "OggS"; then
                    pass "音频是 Ogg 容器"
                else
                    fail "音频不是 Ogg（缺 OggS magic）"
                fi
            else
                fail "音频为空"
            fi

            # 4d. 一次性：再拉应该 404
            CODE2="$(curl -s -o /dev/null -w '%{http_code}' -m 10 "${URL}" 2>/dev/null || echo 000)"
            [ "${CODE2}" = "404" ] && pass "URL 一次性（第二次 404）" || fail "URL 未失效（第二次 ${CODE2}）"
        fi
        ;;
      409)
        # 设备忙也是合法响应，但要带 error 字段
        ERR="$(python3 -c "import json;print(json.load(open('/tmp/say-resp.json')).get('error',''))" 2>/dev/null)"
        [ -n "${ERR}" ] && pass "接口可用，设备忙（error=${ERR}）" || fail "409 但缺 error 字段"
        ;;
      401)
        pass "接口返回 401（说明 say_token 已配置并生效）"
        info "注意：默认 say_token 应为空、不校验；若这里是 401 请确认是有意开启"
        ;;
      404) fail "接口不存在（404）" ;;
      *)   fail "接口异常（HTTP ${CODE}）" ;;
    esac
    ;;
*)
    fail "未知任务 ${TASK}"
    ;;
esac

# ─────────────────────────────────────────────
echo
if [ "${FAILURES}" -eq 0 ]; then
    echo "验收通过（任务 ${TASK}）"
    exit 0
else
    echo "验收失败：${FAILURES} 项（任务 ${TASK}）"
    exit 1
fi
