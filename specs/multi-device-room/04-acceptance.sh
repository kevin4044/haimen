#!/usr/bin/env bash
#
# 04 - 机械验收：多设备与房间（设备目录 / 房间上下文 / 定向通知）
#
# 用法：
#   bash specs/multi-device-room/04-acceptance.sh              # 只读检查
#   TARGET_DEVICE="客厅小智" bash specs/multi-device-room/04-acceptance.sh
#   TARGET_ROOM="客厅"       bash specs/multi-device-room/04-acceptance.sh
#
# 环境变量：
#   HAIMEN_URL     haimen HTTP 地址，默认 http://127.0.0.1:9527
#   SAY_TOKEN      [xiaozhi] say_token；非空时带 Authorization: Bearer
#   TARGET_DEVICE  非空时额外实测「按设备/名字定向 say」
#   TARGET_ROOM    非空时额外实测「按房间定向 say」
#
# 退出码 0 = 全部 PASS；非 0 = 有 FAIL。
# 不依赖 jq：JSON 一律用 python3 解析。

set -euo pipefail

HAIMEN_URL="${HAIMEN_URL:-http://127.0.0.1:9527}"
SAY_TOKEN="${SAY_TOKEN:-}"
TARGET_DEVICE="${TARGET_DEVICE:-}"
TARGET_ROOM="${TARGET_ROOM:-}"

BASE_URL="${HAIMEN_URL%/}"
FAILURES=0

pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; FAILURES=$((FAILURES + 1)); }
info() { echo "INFO $*"; }

# ─────────────────────────────────────────────
# 0. 环境检查
# ─────────────────────────────────────────────
if ! command -v curl >/dev/null 2>&1; then
    echo "FAIL 缺少 curl"
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL 缺少 python3（本脚本用 python3 解析 JSON）"
    exit 1
fi

info "目标 haimen: ${BASE_URL}"
if [ -n "${SAY_TOKEN}" ]; then
    info "say_token: 已设置（Authorization: Bearer ***）"
else
    info "say_token: 未设置（接口不校验鉴权，内网自用默认）"
fi

# ─────────────────────────────────────────────
# python3 断言脚本（stdin 收 JSON；只用双引号，避免与 shell 引号冲突）
# ─────────────────────────────────────────────
PY_DEVICES="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)

if not isinstance(data, dict):
    print("响应不是 JSON 对象")
    raise SystemExit(1)
if data.get("ok") is not True:
    print("ok 不是 true: %r" % (data.get("ok"),))
    raise SystemExit(1)

devices = data.get("devices")
if not isinstance(devices, list):
    print("devices 不是数组: %s" % type(devices).__name__)
    raise SystemExit(1)

for idx, item in enumerate(devices):
    if not isinstance(item, dict):
        print("devices[%d] 不是对象" % idx)
        raise SystemExit(1)
    for key in ("device_id", "client_id", "name", "room", "online", "state"):
        if key not in item:
            print("devices[%d] 缺少字段 %s" % (idx, key))
            raise SystemExit(1)
    if not isinstance(item.get("online"), bool):
        print("devices[%d].online 不是布尔: %r" % (idx, item.get("online")))
        raise SystemExit(1)

print("共 %d 台设备" % len(devices))
for item in devices:
    state = item.get("state") if item.get("state") is not None else "-"
    online = "online" if item.get("online") else "offline"
    room = item.get("room") or "<未分配>"
    name = item.get("name") or item.get("device_id")
    client_id = item.get("client_id") or "-"
    print(
        "    %s | device=%s | room=%s | %s | state=%s | client_id=%s"
        % (name, item.get("device_id"), room, online, state, client_id)
    )
PY
)"

PY_HEALTH="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)

if not isinstance(data, dict) or data.get("status") != "ok":
    print("status 不是 ok: %r" % (data,))
    raise SystemExit(1)
print("status=ok version=%s service=%s" % (data.get("version", "?"), data.get("service", "?")))
PY
)"

PY_SAY_OK="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)
if data.get("ok") is not True:
    print("ok 不是 true: %r" % (data,))
    raise SystemExit(1)
print(
    "device=%s client_id=%s name=%s room=%s duration_ms=%s"
    % (
        data.get("device", "?"),
        data.get("client_id", "?"),
        data.get("name", "?"),
        data.get("room", "?"),
        data.get("duration_ms", "?"),
    )
)
PY
)"

PY_SAY_409="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)
error = data.get("error")
if error not in ("device_busy", "multiple_devices"):
    print("非预期的 409 error: %r" % (error,))
    raise SystemExit(1)
if error == "multiple_devices":
    devices = data.get("devices")
    if not isinstance(devices, list) or not devices:
        print("multiple_devices 缺少候选列表 devices")
        raise SystemExit(1)
    names = ", ".join(str(d.get("name") or d.get("device_id")) for d in devices if isinstance(d, dict))
    print("error=multiple_devices candidates=[%s]" % names)
else:
    print("error=device_busy state=%s" % data.get("state", "-"))
PY
)"

PY_ROOM_404="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)
if data.get("error") != "device_offline":
    print("error 不是 device_offline: %r" % (data.get("error"),))
    raise SystemExit(1)
print("error=device_offline room=%s" % data.get("room", "<缺 room 回显>"))
PY
)"

PY_ROOM_409="$(cat <<'PY'
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception as exc:
    print("非法 JSON: %s" % exc)
    raise SystemExit(1)
error = data.get("error")
if error not in ("multiple_devices", "device_busy"):
    print("非预期的 409 error: %r" % (error,))
    raise SystemExit(1)
if error == "multiple_devices":
    devices = data.get("devices")
    if not isinstance(devices, list) or not devices:
        print("multiple_devices 缺少候选列表 devices")
        raise SystemExit(1)
    names = ", ".join(str(d.get("name") or d.get("device_id")) for d in devices if isinstance(d, dict))
    print("error=multiple_devices candidates=[%s]" % names)
else:
    print("error=device_busy state=%s" % data.get("state", "-"))
PY
)"

# ─────────────────────────────────────────────
# HTTP 辅助：输出 body + 换行 + 状态码，再由 split_response 拆开
# ─────────────────────────────────────────────
http_get() {
    local path="$1"
    if [ -n "${SAY_TOKEN}" ]; then
        curl -sS --max-time 15 \
            -H "Authorization: Bearer ${SAY_TOKEN}" \
            -w $'\n%{http_code}' \
            "${BASE_URL}${path}"
    else
        curl -sS --max-time 15 \
            -w $'\n%{http_code}' \
            "${BASE_URL}${path}"
    fi
}

post_json() {
    local path="$1" payload="$2"
    if [ -n "${SAY_TOKEN}" ]; then
        curl -sS --max-time 30 -X POST \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer ${SAY_TOKEN}" \
            -d "${payload}" \
            -w $'\n%{http_code}' \
            "${BASE_URL}${path}"
    else
        curl -sS --max-time 30 -X POST \
            -H "Content-Type: application/json" \
            -d "${payload}" \
            -w $'\n%{http_code}' \
            "${BASE_URL}${path}"
    fi
}

RESP_CODE=""
RESP_BODY=""

# 把 curl 的 "body\n<code>" 拆成 RESP_CODE / RESP_BODY。
split_response() {
    local raw="$1"
    RESP_CODE="$(printf '%s' "${raw}" | tail -n 1)"
    RESP_BODY="$(printf '%s' "${raw}" | sed '$d')"
}

print_body() {
    if [ -z "${RESP_BODY}" ]; then
        echo "     body: <空>"
    else
        echo "     body: ${RESP_BODY}"
    fi
}

# 用 stdin 收 JSON、-c 收断言脚本（避免 heredoc 与管道同时占用 stdin）。
run_py() {
    local script="$1" body="$2"
    printf '%s' "${body}" | python3 -c "${script}" 2>&1
}

# ─────────────────────────────────────────────
# 1. GET /api/v1/xiaozhi/devices —— 设备清单
# ─────────────────────────────────────────────
echo
echo "── 1. GET /api/v1/xiaozhi/devices"
if ! RAW="$(http_get "/api/v1/xiaozhi/devices")"; then
    fail "请求 /api/v1/xiaozhi/devices 失败（haimen 没起？${BASE_URL}）"
else
    split_response "${RAW}"
    if [ "${RESP_CODE}" != "200" ]; then
        fail "设备清单应返回 200，实际 ${RESP_CODE}"
        print_body
    else
        if OUT="$(run_py "${PY_DEVICES}" "${RESP_BODY}")"; then
            pass "设备清单可解析：$(printf '%s' "${OUT}" | head -n 1)"
            printf '%s\n' "${OUT}" | tail -n +2
        else
            fail "设备清单校验失败：${OUT}"
            print_body
        fi
    fi
fi

# ─────────────────────────────────────────────
# 2. GET /health
# ─────────────────────────────────────────────
echo
echo "── 2. GET /health"
if ! RAW="$(http_get "/health")"; then
    fail "请求 /health 失败（haimen 没起？${BASE_URL}）"
else
    split_response "${RAW}"
    if [ "${RESP_CODE}" != "200" ]; then
        fail "/health 应返回 200，实际 ${RESP_CODE}"
        print_body
    else
        if OUT="$(run_py "${PY_HEALTH}" "${RESP_BODY}")"; then
            pass "/health 正常：${OUT}"
        else
            fail "/health 响应不符合预期：${OUT}"
            print_body
        fi
    fi
fi

# ─────────────────────────────────────────────
# 3. 可选：按 device（MAC / client_id / 配置的名字）定向 say
# ─────────────────────────────────────────────
echo
if [ -z "${TARGET_DEVICE}" ]; then
    info "未设置 TARGET_DEVICE，跳过按设备定向 say"
else
    echo "── 3. POST /api/v1/xiaozhi/say（device=${TARGET_DEVICE}）"
    PAYLOAD="$(python3 - "${TARGET_DEVICE}" <<'PY'
import json
import sys

print(json.dumps({"text": "这是一条多设备验收通知，听到说明链路通了", "device": sys.argv[1]}, ensure_ascii=False))
PY
    )"
    if ! RAW="$(post_json "/api/v1/xiaozhi/say" "${PAYLOAD}")"; then
        fail "按设备 say 请求失败（网络错误/超时）"
    else
        split_response "${RAW}"
        print_body
        case "${RESP_CODE}" in
        200)
            # 200 = 已下发；notify 无 ACK，不保证真的播放。
            if OUT="$(run_py "${PY_SAY_OK}" "${RESP_BODY}")"; then
                pass "按设备 say 已下发（200）：${OUT}"
            else
                fail "按设备 say 的 200 响应不符合预期：${OUT}"
            fi
            ;;
        409)
            # 设备忙也算链路通：能定位到设备，只是当前不在 idle。
            if OUT="$(run_py "${PY_SAY_409}" "${RESP_BODY}")"; then
                pass "按设备 say 返回 409（链路通，目标不可用）：${OUT}"
            else
                fail "按设备 say 的 409 响应不符合预期：${OUT}"
            fi
            ;;
        *)
            fail "按设备 say 应返回 200 或 409，实际 ${RESP_CODE}"
            ;;
        esac
    fi
fi

# ─────────────────────────────────────────────
# 4. 可选：按 room 定向 say
# ─────────────────────────────────────────────
echo
if [ -z "${TARGET_ROOM}" ]; then
    info "未设置 TARGET_ROOM，跳过按房间定向 say"
else
    echo "── 4. POST /api/v1/xiaozhi/say（room=${TARGET_ROOM}）"
    PAYLOAD="$(python3 - "${TARGET_ROOM}" <<'PY'
import json
import sys

print(json.dumps({"text": "这是一条按房间的验收通知，听到说明房间定向通了", "room": sys.argv[1]}, ensure_ascii=False))
PY
    )"
    if ! RAW="$(post_json "/api/v1/xiaozhi/say" "${PAYLOAD}")"; then
        fail "按房间 say 请求失败（网络错误/超时）"
    else
        split_response "${RAW}"
        print_body
        case "${RESP_CODE}" in
        200)
            if OUT="$(run_py "${PY_SAY_OK}" "${RESP_BODY}")"; then
                pass "按房间 say 已下发（200）：${OUT}"
            else
                fail "按房间 say 的 200 响应不符合预期：${OUT}"
            fi
            ;;
        404)
            # 房间无在线设备（或房间名写错）：响应体应回显 room，便于排查。
            if OUT="$(run_py "${PY_ROOM_404}" "${RESP_BODY}")"; then
                pass "按房间 say 返回 404（该房间无在线设备）：${OUT}"
            else
                fail "按房间 say 的 404 响应不符合预期：${OUT}"
            fi
            ;;
        409)
            # 房间内多台 → multiple_devices；房间内唯一一台正忙 → device_busy。
            if OUT="$(run_py "${PY_ROOM_409}" "${RESP_BODY}")"; then
                pass "按房间 say 返回 409（不广播，需指定设备）：${OUT}"
            else
                fail "按房间 say 的 409 响应不符合预期：${OUT}"
            fi
            ;;
        *)
            fail "按房间 say 应返回 200 / 404 / 409，实际 ${RESP_CODE}"
            ;;
        esac
    fi
fi

# ─────────────────────────────────────────────
# 5. 汇总
# ─────────────────────────────────────────────
echo
if [ "${FAILURES}" -eq 0 ]; then
    echo "验收通过：全部 PASS"
    if [ -z "${TARGET_DEVICE}" ] && [ -z "${TARGET_ROOM}" ]; then
        info "只读模式：设置 TARGET_DEVICE / TARGET_ROOM 可实测定向 say"
    fi
    exit 0
fi

echo "验收失败：${FAILURES} 项 FAIL"
exit 1
