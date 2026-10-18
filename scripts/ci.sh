#!/usr/bin/env bash
#
# scripts/ci.sh —— 本地 CI
#
# 所有命令都在 Docker 开发容器里跑，本机不需要安装 Rust。
#   fmt    检查整个 workspace 的格式（--all）
#   clippy 检查整个 workspace 的 lib/bin（--workspace，不含 test target）
#   test   只跑根 crate 的测试（单线程，避免环境变量竞争；
#          不跑整个 workspace 是有意为之，勿加 --workspace）
#
# 用法：
#   ./scripts/ci.sh            # fmt + clippy + test
#   ./scripts/ci.sh fmt        # 只跑格式检查
#   ./scripts/ci.sh clippy     # 只跑 lint
#   ./scripts/ci.sh test       # 只跑测试

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEV_IMAGE="${HAIMEN_DEV_IMAGE:-haimen-dev:rust}"
CARGO_VOL="haimen-cargo-registry"
TARGET_VOL="haimen-target"

# 首次使用：自动构建开发镜像
if ! docker image inspect "$DEV_IMAGE" >/dev/null 2>&1; then
    echo "▶ 首次使用，构建开发镜像 $DEV_IMAGE ..."
    docker build -f Dockerfile.dev -t "$DEV_IMAGE" .
fi

in_dev() {
    # -i 保留 stdin：cargo 某些情况下会读
    docker run --rm -i \
        -v "$REPO_ROOT":/src \
        -v "$CARGO_VOL":/usr/local/cargo/registry \
        -v "$TARGET_VOL":/target \
        -e CARGO_TARGET_DIR=/target \
        -e CARGO_TERM_COLOR=always \
        -w /src \
        "$DEV_IMAGE" "$@"
}

step() {
    echo
    echo "══════════════════════════════════════════════════════"
    echo "▶ $*"
    echo "══════════════════════════════════════════════════════"
}

# 过滤 build.rs 的无害告警（容器里没有 Node，复用预构建的前端产物）
filter_noise() {
    grep -vE "前端构建失败|you may opt out|has been auto-installed|this might cause rustup" || true
}

run_fmt() {
    step "cargo fmt --check"
    if in_dev cargo fmt --all -- --check 2>&1 | filter_noise; then
        echo "✅ fmt 通过"
    else
        echo "❌ fmt 失败 —— 本地执行修复："
        echo "   docker run --rm -v \"\$PWD\":/src -w /src $DEV_IMAGE cargo fmt --all"
        return 1
    fi
}

run_clippy() {
    step "cargo clippy --workspace -- -D warnings"
    if in_dev cargo clippy --workspace -- -D warnings 2>&1 | filter_noise; then
        echo "✅ clippy 通过"
    else
        echo "❌ clippy 失败"
        return 1
    fi
}

run_test() {
    step "cargo test -- --test-threads=1"
    if in_dev cargo test -- --test-threads=1 2>&1 | filter_noise; then
        echo "✅ test 通过"
    else
        echo "❌ test 失败"
        return 1
    fi
}

case "${1:-all}" in
    fmt)    run_fmt ;;
    clippy) run_clippy ;;
    test)   run_test ;;
    all)
        run_fmt
        run_clippy
        run_test
        echo
        echo "══════════════════════════════════════════════════════"
        echo "✅ CI 全部通过"
        echo "══════════════════════════════════════════════════════"
        ;;
    *)
        echo "用法: $0 [all|fmt|clippy|test]" >&2
        exit 2
        ;;
esac
