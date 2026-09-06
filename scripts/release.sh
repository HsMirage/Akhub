#!/usr/bin/env bash
# 发布工件构建脚本（§25、§27 阶段 6）。
#
# 用法：
#   ./scripts/release.sh           # 全量：测试 → Linux 双架构二进制 → 提示 Docker 命令
#   ./scripts/release.sh --skip-tests
#
# 交叉编译依赖 cargo-zigbuild（`cargo install cargo-zigbuild`）与 zig。
# Docker 多架构镜像用 buildx 在容器内原生编译（Dockerfile 不依赖宿主机工具链）。

set -euo pipefail
cd "$(dirname "$0")/.."

SKIP_TESTS=false
[[ "${1:-}" == "--skip-tests" ]] && SKIP_TESTS=true

echo "==> 1/4 构建管理后台"
(cd web && npm ci && npm run build)

echo "==> 2/4 校验（fmt / clippy / 测试）"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
if [[ "$SKIP_TESTS" != true ]]; then
    cargo test
fi

echo "==> 3/4 交叉编译 Linux 二进制"
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    cargo zigbuild --release --target "$target"
    mkdir -p dist
    artifact="dist/akhub-${target}"
    cp "target/$target/release/akhub" "$artifact"
    tar -czf "${artifact}.tar.gz" -C "target/$target/release" akhub
    echo "    ${artifact}.tar.gz"
done
# macOS / Windows 原生构建：
#   cargo build --release --target aarch64-apple-darwin
#   cargo zigbuild --release --target x86_64-pc-windows-gnu

echo "==> 4/4 Docker 多架构镜像（需要可用的 Docker daemon）"
cat <<'EOF'
    docker buildx create --use   # 首次需要
    docker buildx build --platform linux/amd64,linux/arm64 \
        -t <registry>/akhub:1.0.0 -t <registry>/akhub:latest --push .
EOF

echo "完成。二进制在 dist/，镜像构建命令如上。"
