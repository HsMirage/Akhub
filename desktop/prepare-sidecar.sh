#!/usr/bin/env bash
# 把编译好的 akhub 二进制放到 Tauri externalBin 期望的位置。
#
# Tauri 要求 externalBin 的文件名带上目标三元组：
#   binaries/akhub-<target-triple>        （Windows 还要加 .exe）
# 打包时 Tauri 会把它复制进安装包目录，并去掉三元组，
# 所以运行时 sidecar 名恒为 akhub（见 desktop/src/main.rs）。
#
# 用法：
#   desktop/prepare-sidecar.sh <target-triple> <akhub 二进制路径>
#
# 例：
#   desktop/prepare-sidecar.sh x86_64-pc-windows-msvc target/x86_64-pc-windows-msvc/release/akhub.exe
#
# 只打 Windows：macOS 于 2026-10-20 停掉（见 docs/BUILD.md §1）。

set -euo pipefail

TARGET="${1:?缺少目标三元组，例如 x86_64-pc-windows-msvc}"
BINARY="${2:?缺少 akhub 二进制路径}"

[[ -f "$BINARY" ]] || { echo "找不到二进制：$BINARY" >&2; exit 1; }

DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$DIR/binaries"

EXT=""
case "$TARGET" in
    *windows*) EXT=".exe" ;;
esac

DEST="$DIR/binaries/akhub-$TARGET$EXT"
cp "$BINARY" "$DEST"
chmod +x "$DEST" 2>/dev/null || true

echo "$DEST"
