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
# 两道检查：
#   1. 字面量：构建配置里出现任何苹果目标名 / runner 名 / 打包动作就失败。
#   2. 结构：matrix 里声明的 os 取值不能是 macOS（防止用变量绕开第 1 道）。
#
# 只扫"会产出二进制"的构建配置。文档里写"macOS 已停止支持"是正常的，所以不扫 *.md。

set -euo pipefail
cd "$(dirname "$0")/.."

TARGETS=(
    .github/workflows/release.yml
    .github/workflows/ci.yml
    scripts/release.sh
    install.sh
    desktop/tauri.conf.json
    desktop/prepare-sidecar.sh
)

# 苹果目标三元组、macOS runner 标签（含 macos-13/14/15 这类带版本号的），
# 以及只有在打苹果包时才出现的工具。
PATTERN='-apple-darwin|macos-latest|macos-[0-9]|-macos-|hdiutil|codesign|notarytool|xcrun|xcodebuild'

failed=0
for f in "${TARGETS[@]}"; do
    [[ -f "$f" ]] || continue
    if grep -nE "$PATTERN" "$f"; then
        echo "::error file=$f::检测到 macOS 构建目标；本项目只支持 Linux 与 Windows（见 docs/BUILD.md）" >&2
        failed=1
    fi
done

# 结构检查：matrix 里声明的 os 取值不能是 macOS。
# 用 YAML 解析而不是 grep，这样间接写法也逃不掉。没有 pyyaml 时跳过而非失败。
if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' 2>/dev/null; then
    python3 - <<'PY' || failed=1
import glob, sys, yaml

bad = []
for path in sorted(glob.glob(".github/workflows/*.yml")):
    with open(path, encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    for job_name, job in (doc.get("jobs") or {}).items():
        matrix = (job.get("strategy") or {}).get("matrix") or {}
        if not isinstance(matrix, dict):
            continue
        entries = []
        if isinstance(matrix.get("include"), list):
            entries += matrix["include"]
        for key, values in matrix.items():
            if key == "include" or not isinstance(values, list):
                continue
            entries += [{key: v} for v in values]
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            for value in entry.values():
                if not isinstance(value, str):
                    continue
                low = value.lower()
                if "macos" in low or "darwin" in low:
                    bad.append(f"{path}:{job_name}: matrix 含 {value}")

for b in bad:
    print(f"::error::{b}", file=sys.stderr)
sys.exit(1 if bad else 0)
PY
fi

if [[ "$failed" -ne 0 ]]; then
    echo "平台门禁未通过：本项目只支持 Linux 与 Windows（见 docs/BUILD.md §1）" >&2
    exit 1
fi

echo "平台门禁通过：没有 macOS 构建目标"
