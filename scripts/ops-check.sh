#!/usr/bin/env bash
# 运维自检：一次跑完「检查 Git / 查看远端状态 / 运行运维脚本」三项终端能力观察点。
#
# 背景：gpt-boom / gpt-5.6-sol 的反馈把「终端执行入口未挂载」记成了
# `functions__exec` / `exec_command`，而这两个名字既不是 Akhub 的工具，
# 也不是本机 DSH 部署的注册名（真正的入口是 bash/pwsh，PTC 预设下经 run_code
# 呈现）。这份脚本用可重复的方式证明终端入口当下是活的，并给出远端是否落后。
#
# 用法：
#   scripts/ops-check.sh            # 全量自检
#   scripts/ops-check.sh --quiet    # 只打印失败项与最终判定
#
# 退出码：0 = 全部通过；1 = 至少一项失败。

set -uo pipefail

cd "$(dirname "$0")/.."

QUIET=false
[[ "${1:-}" == "--quiet" ]] && QUIET=true

PASS=0
FAIL=0

say() { $QUIET || printf '%s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); say "  ✓ $*"; }
bad() { FAIL=$((FAIL + 1)); printf '  ✗ %s\n' "$*"; }

say "== 1/3 检查 Git 工作树 =="
if branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null); then
    ok "当前分支：$branch"
else
    bad "不在 Git 工作树里（git rev-parse 失败）"
fi

if dirty=$(git status --porcelain 2>/dev/null); then
    if [[ -z "$dirty" ]]; then
        ok "工作树干净"
    else
        say "  · 工作树有 $(printf '%s\n' "$dirty" | wc -l | tr -d ' ') 处未提交改动"
    fi
else
    bad "git status 失败"
fi

say
say "== 2/3 查看远端状态 =="
if remote=$(git remote get-url origin 2>/dev/null); then
    ok "origin：$remote"
else
    bad "没有 origin 远端"
    remote=""
fi

if upstream=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null); then
    ok "上游跟踪分支：$upstream"
    counts=$(git rev-list --left-right --count "$upstream"...HEAD 2>/dev/null)
    behind=$(printf '%s' "$counts" | awk '{print $1}')
    ahead=$(printf '%s' "$counts" | awk '{print $2}')
    if [[ "${behind:-0}" == "0" && "${ahead:-0}" == "0" ]]; then
        ok "与远端同步（0 落后 / 0 领先）"
    else
        say "  · 本地相对远端：落后 ${behind:-?} 个提交，领先 ${ahead:-?} 个提交"
    fi
else
    bad "当前分支没有上游跟踪分支"
fi

if [[ -n "$remote" ]] && git ls-remote --heads origin >/dev/null 2>&1; then
    ok "远端可连通（git ls-remote 成功）"
else
    bad "远端不可连通（git ls-remote 失败，检查网络或凭据）"
fi

say
say "== 3/3 运行运维脚本（语法检查 + 真实执行）=="
for s in scripts/*.sh; do
    if bash -n "$s" 2>/dev/null; then
        ok "语法检查通过：$s"
    else
        bad "语法检查失败：$s"
    fi
done

if probe=$(bash -c 'printf "ops-ok %s\n" "$(uname -srm)"' 2>/dev/null); then
    ok "shell 真实执行：$probe"
else
    bad "shell 无法执行命令"
fi

say
if [[ "$FAIL" -eq 0 ]]; then
    # 判定行在 --quiet 下也要打印：这是「只打印失败项与最终判定」的约定。
    printf '判定：终端执行入口可用（通过 %s 项，失败 0 项）\n' "$PASS"
    exit 0
fi
printf '判定：终端执行入口异常（通过 %s 项，失败 %s 项）\n' "$PASS" "$FAIL"
exit 1
