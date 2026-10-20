#!/usr/bin/env bash
# 平台上限门禁：本项目只构建 Linux 与 Windows。
#
# macOS 于 2026-10-20 停掉（见 docs/BUILD.md §1）。删掉 matrix 条目是不够的：
# 一条 `-apple-darwin` 目标、一个 `macos-latest` runner 就能让发版重新产出苹果
# 产物，而那种改动在 review 里很容易被当成"顺手补个平台"放过去。所以做成机械检查。
#
# ci.yml 与 release.yml 都调用这里：tag 发版不经过 ci.yml，两边都得卡一次。
# 具体实现只在这一个文件里（与 scripts/package.sh 同样的理由）。
#
# 只查"会产出苹果二进制"的三处。文档里写"macOS 已停止支持"是正常的，所以不扫 *.md。

set -euo pipefail
cd "$(dirname "$0")/.."

if grep -rn -- '-apple-darwin\|macos-latest\|macos-' \
    .github/workflows/release.yml scripts/release.sh install.sh; then
    echo "::error::检测到 macOS 构建目标；本项目只支持 Linux 与 Windows（见 docs/BUILD.md）" >&2
    exit 1
fi

echo "平台门禁通过：没有 macOS 构建目标"
