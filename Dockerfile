# haimen 运行镜像（多阶段：先编译，再装最小运行环境）
#
# 构建：docker build -t haimen:local .

FROM rust:slim-bookworm AS builder

RUN HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY= no_proxy= \
    apt-get update && apt-get install -y --no-install-recommends \
        pkg-config cmake make clang libclang-dev \
        libssl-dev zlib1g-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY Cargo.toml Cargo.lock build.rs ./
COPY crates ./crates
COPY src ./src
COPY web-ui/dist ./web-ui/dist

RUN HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY= no_proxy= \
    cargo build --release --bin haimen

FROM debian:bookworm-slim

RUN HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY= no_proxy= \
    apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl tini \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/haimen /usr/local/bin/haimen

ENV HOME=/root
VOLUME ["/root/.haimen"]
EXPOSE 9527
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["haimen", "serve", "--host", "0.0.0.0", "--port", "9527", "--no-browser"]
