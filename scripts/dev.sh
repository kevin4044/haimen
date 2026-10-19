#!/usr/bin/env bash
#
# haimen 二次开发工具
#
# 设计原则：本机不装 Rust，所有 cargo 操作都在容器里跑。
# 用命名卷缓存 cargo registry 和 target，保证增量编译够快。
#
# 用法:
#   ./scripts/dev.sh init     首次准备：构建开发镜像
#   ./scripts/dev.sh check    cargo check（最快反馈）
#   ./scripts/dev.sh fmt      格式检查
#   ./scripts/dev.sh clippy   Lint 检查（-D warnings）
#   ./scripts/dev.sh test     跑测试
#   ./scripts/dev.sh verify   fmt + clippy + test（提交前完整检查）
#   ./scripts/dev.sh build    release 构建
#   ./scripts/dev.sh image    构建运行镜像 haimen:local
#   ./scripts/dev.sh run      启动容器（http://localhost:9527）
#   ./scripts/dev.sh stop     停止容器
#   ./scripts/dev.sh logs     看日志
#   ./scripts/dev.sh shell    进容器交互
#   ./scripts/dev.sh clean    删除缓存卷（下次编译会变慢）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEV_IMAGE="${HAIMEN_DEV_IMAGE:-haimen-dev:rust}"
RUN_IMAGE="${HAIMEN_RUN_IMAGE:-haimen:local}"
CONTAINER_NAME="${HAIMEN_CONTAINER:-haimen-dev-run}"
HOST_PORT="${HAIMEN_PORT:-9527}"
CONFIG_DIR="${HAIMEN_CONFIG_DIR:-$HOME/.haimen-dev}"

CARGO_VOL="haimen-cargo-registry"
TARGET_VOL="haimen-target"

die() { echo "✗ $*" >&2; exit 1; }

require_docker() {
    command -v docker >/dev/null 2>&1 || die "找不到 docker，请先启动 OrbStack"
    docker info >/dev/null 2>&1 || die "docker 连不上，请先启动 OrbStack"
}

# 在开发容器里执行命令
# - 源码目录挂进来，改动立即可见
# - registry / target 用命名卷，避免每次都重新下载和全量编译
in_dev() {
    docker run --rm -i \
        -v "$REPO_ROOT":/src \
        -v "$CARGO_VOL":/usr/local/cargo/registry \
        -v "$TARGET_VOL":/target \
        -e CARGO_TARGET_DIR=/target \
        -e CARGO_TERM_COLOR=always \
        -w /src \
        "$DEV_IMAGE" "$@"
}

# 容器以 root 跑（命名卷是 root 属主），结束后把可能被改写的文件还给当前用户
fix_owner() {
    if [ -f Cargo.lock ] && [ "$(stat -f '%Su' Cargo.lock 2>/dev/null)" = "root" ]; then
        docker run --rm -v "$REPO_ROOT":/src alpine chown -R "$(id -u):$(id -g)" /src/Cargo.lock >/dev/null 2>&1 || true
    fi
}

cmd="${1:-help}"
shift || true

case "$cmd" in
    init)
        require_docker
        echo "▶ 构建开发镜像 $DEV_IMAGE"
        docker build -f Dockerfile.dev -t "$DEV_IMAGE" .
        echo "✅ 完成。接下来可以 ./scripts/dev.sh check"
        ;;

    check)
        require_docker
        in_dev cargo check "$@"
        fix_owner
        ;;

    fmt)
        require_docker
        in_dev cargo fmt --all -- --check "$@"
        ;;

    clippy)
        require_docker
        # workspace 级：覆盖 crates/ 下全部成员 crate 的 lib/bin
        # （不加 --all-targets：test target 下存在既有的 cfg(test) 代码告警）
        in_dev cargo clippy --workspace -- -D warnings "$@"
        fix_owner
        ;;

    test)
        require_docker
        # 单线程，避免环境变量竞争。
        # 必须加 --workspace：之前只跑根 crate，导致 crates/ 下的测试
        # （如 haimen-xiaozhi 的 protocol_udp / mqtt_broker）在 CI 里从未执行。
        # 已确认全 workspace 测试可通过（585 passed / 0 failed）。
        in_dev cargo test --workspace -- --test-threads=1 "$@"
        fix_owner
        ;;

    verify)
        require_docker
        echo "▶ fmt"
        in_dev cargo fmt --all -- --check
        echo "▶ clippy"
        in_dev cargo clippy --workspace -- -D warnings
        echo "▶ test"
        # 必须和 test 目标一致加 --workspace：否则 crates/ 下的测试
        # （haimen-xiaozhi / haimen-openai 等）根本不跑，verify 却报"全部通过"。
        in_dev cargo test --workspace -- --test-threads=1
        fix_owner
        echo "✅ 全部通过"
        ;;

    build)
        require_docker
        in_dev cargo build --release --bin haimen "$@"
        fix_owner
        ;;

    image)
        require_docker
        echo "▶ 构建运行镜像 $RUN_IMAGE"
        docker build -t "$RUN_IMAGE" .
        docker images "$RUN_IMAGE" --format '{{.Repository}}:{{.Tag}}  {{.Size}}'
        ;;

    run)
        require_docker
        mkdir -p "$CONFIG_DIR"
        docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
        docker run -d --name "$CONTAINER_NAME" \
            -p "$HOST_PORT:9527" \
            -v "$CONFIG_DIR":/root/.haimen \
            "$RUN_IMAGE"
        echo "✅ 已启动"
        echo "   Web UI: http://localhost:$HOST_PORT"
        echo "   配置:   $CONFIG_DIR/settings.toml"
        echo "   日志:   ./scripts/dev.sh logs"
        ;;

    stop)
        docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
        echo "✅ 已停止"
        ;;

    logs)
        docker logs -f "$CONTAINER_NAME"
        ;;

    shell)
        require_docker
        in_dev bash
        ;;

    clean)
        docker volume rm -f "$CARGO_VOL" "$TARGET_VOL" >/dev/null 2>&1 || true
        echo "✅ 已清空缓存卷（下次编译会重新下载依赖）"
        ;;

    help|*)
        sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
        ;;
esac
