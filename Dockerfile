# 前端阶段：构建 React 管理后台，产物在下一阶段被嵌入二进制。
FROM node:24-slim AS web

WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# 构建阶段：Rust 二进制。
FROM rust:1.96-slim-bookworm AS builder

WORKDIR /build

# 先只复制清单，让依赖层在源码变动时仍能命中缓存。
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src web/dist \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY assets ./assets
COPY tests ./tests
COPY --from=web /web/dist ./web/dist
# 触碰源文件，确保上一步的空壳产物不会被误当成最新构建结果。
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# 运行阶段：只带二进制与 CA 证书。
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 以非 root 用户运行（§25.1）。
RUN useradd --system --uid 10001 --create-home --home-dir /home/akhub akhub \
    && mkdir -p /data && chown akhub:akhub /data

COPY --from=builder /build/target/release/akhub /usr/local/bin/akhub

USER akhub
WORKDIR /data

# /data 是唯一持久卷：SQLite、主密钥与临时目录都在这里。
VOLUME ["/data"]
ENV AKHUB_DATA_DIR=/data \
    AKHUB_LISTEN=0.0.0.0:8080 \
    RUST_LOG=akhub=info,warn
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/akhub", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/akhub"]
