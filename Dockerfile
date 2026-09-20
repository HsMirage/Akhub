# 三阶段构建（§25.1）：
#   web       —— 构建 React 管理后台，产物由 rust-embed 在编译期嵌进二进制
#   builder   —— 编译 Rust 二进制
#   runtime   —— 只带二进制、CA 证书与 tini 的最小运行镜像
#
# 刻意只用经典 Dockerfile 语法，**不用 `RUN --mount=type=cache`**：
# 那要求 BuildKit，而 Linux 上 `apt install docker.io` 装出来的 docker 常常
# 没有 buildx 组件，用户一句 `docker build .` 就会撞上 "the --mount option
# requires BuildKit" 而完全构建不出来。依赖编译的加速由下面的分层缓存
# （先 COPY 清单编依赖、再 COPY 源码编本体）承担，cache mount 的边际收益
# 不值得换掉这份兼容性。
#
# 本地命令：
#   docker build -t akhub:latest .
#   docker buildx build --platform linux/amd64,linux/arm64 -t akhub:latest --push .

# ---------------------------------------------------------------- 前端
FROM node:24-slim AS web

WORKDIR /web
# 只复制清单，让依赖层在源码变动时仍然命中缓存。
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ---------------------------------------------------------------- 后端
FROM rust:1.96-slim-bookworm AS builder

WORKDIR /build

# aws-lc-sys（rustls 的默认加密后端）会编译 C 代码，需要 cmake；
# 镜像里默认没有，缺了它会以「Could not find cmake」失败。
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# 先只复制清单，把依赖层单独缓存下来：源码变动不会导致重新编译全部依赖。
COPY Cargo.toml Cargo.lock build.rs ./
# web/dist 必须存在——rust-embed 的 #[folder] 在编译期解析路径，缺目录直接编译失败。
RUN mkdir -p src web/dist \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo '<!doctype html><title>placeholder</title>' > web/dist/index.html \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY assets ./assets
COPY tests ./tests
COPY --from=web /web/dist ./web/dist
# 触碰源文件，确保上一步的空壳产物不会被误当成最新构建结果。
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---------------------------------------------------------------- 运行
FROM debian:bookworm-slim AS runtime

# tini 让 PID 1 正确转发 SIGTERM/SIGINT 并回收僵尸进程；没有它，
# 容器停止时信号到不了 akhub，优雅关闭的 180 秒宽限期形同虚设（§25.3）。
# gosu 用于把入口脚本从 root 降权到 akhub。
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini gosu \
    && rm -rf /var/lib/apt/lists/*

# 服务进程以非 root 运行（§25.1）。uid/gid 固定，便于宿主侧 bind mount 时对齐权限。
RUN groupadd --system --gid 10001 akhub \
    && useradd --system --uid 10001 --gid 10001 --no-create-home --home-dir /data akhub \
    && mkdir -p /data \
    && chown akhub:akhub /data

COPY --from=builder /build/target/release/akhub /usr/local/bin/akhub
COPY deploy/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# /data 是唯一持久卷：SQLite、主密钥与临时目录都在这里。
VOLUME ["/data"]
ENV AKHUB_DATA_DIR=/data \
    AKHUB_LISTEN=0.0.0.0:8080 \
    RUST_LOG=akhub=info,warn
EXPOSE 8080

# 显式声明停止信号；容器侧必须配 stop_grace_period，否则 Docker 默认 10 秒
# 就会 SIGKILL 掉在途的长流式请求（§25.3）。
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/akhub", "--healthcheck"]

# 刻意不写 USER：entrypoint 需要以 root 起步才能纠正 bind mount 进来的数据
# 目录属主（命名卷场景下这是空操作），随后立刻 gosu 到 akhub。
# 因此「镜像里跑的服务进程是 10001」这条要用进程 uid 验证，
# 而不是 `docker exec id -u`（那沿用的是镜像 USER，即 root）。
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["/usr/local/bin/akhub"]
