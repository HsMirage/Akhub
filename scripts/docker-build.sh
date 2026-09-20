#!/usr/bin/env bash
# 本地构建 Akhub 容器镜像（§25.1）。
#
# 用法：
#   ./scripts/docker-build.sh                 # 构建 amd64+arm64 到本地 buildx 缓存
#   ./scripts/docker-build.sh --push          # 构建并推送
#   ./scripts/docker-build.sh --load          # 只构建本机架构并载入 docker images
#   ./scripts/docker-build.sh --platforms linux/amd64
#   ./scripts/docker-build.sh --tag myreg/akhub:dev
#
# 为什么默认不 --load：多架构镜像没法 load 进本地 docker（docker 只存单架构
# 镜像）。要本地跑就用 --load，它只构建当前架构。
#
# 需要 Docker daemon 在跑：
#   Docker Desktop：直接启动即可
#   colima：colima start

set -euo pipefail
cd "$(dirname "$0")/.."

PLATFORMS="linux/amd64,linux/arm64"
TAG="akhub:dev"
ACTION="build"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --push) ACTION="push"; shift ;;
        --load) ACTION="load"; shift ;;
        --platforms) PLATFORMS="$2"; shift 2 ;;
        --platforms=*) PLATFORMS="${1#*=}"; shift ;;
        --tag) TAG="$2"; shift 2 ;;
        --tag=*) TAG="${1#*=}"; shift ;;
        -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "未知参数：$1" >&2; exit 2 ;;
    esac
done

if ! docker info >/dev/null 2>&1; then
    cat >&2 <<'EOF'
连不上 Docker daemon。先启动一个：
  Docker Desktop：启动应用
  colima：colima start
EOF
    exit 1
fi

# buildx 不是每个 docker 安装都自带；没有就明确告诉怎么装。
if ! docker buildx version >/dev/null 2>&1; then
    echo "缺少 docker buildx 插件（Docker Desktop 自带；Linux 上装 docker-buildx-plugin）" >&2
    exit 1
fi

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

# --load 只能载入单架构；给了多平台就退化成当前平台。
if [[ "$ACTION" == "load" && "$PLATFORMS" == *,* ]]; then
    ARCH="$(uname -m)"
    case "$ARCH" in
        arm64|aarch64) PLATFORMS="linux/arm64" ;;
        *)             PLATFORMS="linux/amd64" ;;
    esac
    echo "==> --load 只能载入单架构，收敛为 $PLATFORMS"
fi

OUT=""
case "$ACTION" in
    push) OUT="--push" ;;
    load) OUT="--load" ;;
    *)    OUT="" ;;   # 只构建，留在 buildx 缓存里
esac

echo "==> 构建 $TAG（$PLATFORMS，版本 $VERSION）"
# shellcheck disable=SC2086  # $OUT 是有意不加引号的空值/单参数
docker buildx build \
    --file Dockerfile \
    --target runtime \
    --platform "$PLATFORMS" \
    --build-arg "AKHUB_VERSION=$VERSION" \
    --tag "$TAG" \
    $OUT \
    .

echo
case "$ACTION" in
    build)
        echo "构建完成（只在 buildx 缓存里，没有 --load / --push）。"
        echo "想本地跑：  ./scripts/docker-build.sh --load && docker run --rm -p 127.0.0.1:8080:8080 $TAG"
        ;;
    load)
        echo "已载入本地镜像：$TAG"
        echo "运行：  docker run --rm -p 127.0.0.1:8080:8080 -v akhub-data:/data $TAG"
        ;;
    push)
        echo "已推送到 $TAG"
        ;;
esac
