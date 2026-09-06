#!/usr/bin/env bash
# 在临时源码副本中运行未修复问题的验收用例，不修改项目常规测试目录。
set -euo pipefail

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/akhub-review.XXXXXX")
rsync -a \
    --exclude /target/ --exclude /data/ --exclude /.git/ \
    --exclude /web/node_modules/ --exclude /review/ \
    "$project_dir/" "$probe_dir/"
cp "$project_dir/review/2026-09-06/gateway_probes.rs" "$probe_dir/tests/review_probes.rs"
printf '复现源码副本：%s\n' "$probe_dir"
CARGO_TARGET_DIR="$project_dir/target" cargo test --locked \
    --manifest-path "$probe_dir/Cargo.toml" --test review_probes -- --nocapture
