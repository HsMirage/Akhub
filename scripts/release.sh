#!/usr/bin/env bash
# 本地发版脚本（§25、§27 阶段 6）。
#
# 做四件事：构建管理后台 → 校验（fmt/clippy/测试）→ 交叉编译各平台二进制 →
# 打包并生成 checksums.txt。CI 的 tag 发版走 .github/workflows/release.yml，
# 两条路径共用 scripts/package.sh，所以资产命名与目录布局不会分叉。
#
# 用法：
#   ./scripts/release.sh                     # 全量：校验 + 全部平台 + 校验和
#   ./scripts/release.sh --skip-tests        # 跳过 cargo test（仍然跑 fmt/clippy）
#   ./scripts/release.sh --only linux-aarch64,macos-aarch64
#   ./scripts/release.sh --no-checksums
#
# 交叉编译依赖：
#   cargo install cargo-zigbuild   &&   安装 zig（https://ziglang.org/download/）
#   rustup target add <triple> ...
# 只装了一部分工具链时用 --only 挑平台，缺工具的目标会被跳过并给出提示，
# 而不是把整个发版卡死。
#
# Docker 多架构镜像不在本地构建（QEMU 下编译 aws-lc-sys 会慢到不可用），
# 由 CI 在原生 arm64 runner 上完成；本地命令见结尾提示。

set -euo pipefail
cd "$(dirname "$0")/.."

SKIP_TESTS=false
CHECKSUMS=true
ONLY=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-tests) SKIP_TESTS=true; shift ;;
        --no-checksums) CHECKSUMS=false; shift ;;
        --only) ONLY="$2"; shift 2 ;;
        --only=*) ONLY="${1#*=}"; shift ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "未知参数：$1" >&2; exit 2 ;;
    esac
done

# 平台名 → 目标三元组。顺序就是报告顺序，也是 --only 的取值来源。
# 平台名必须与 .github/workflows/release.yml 的 matrix.name 完全一致：
# 安装脚本按这个名字拼资产文件名。
PLATFORMS=(
    "linux-x86_64:x86_64-unknown-linux-gnu"
    "linux-aarch64:aarch64-unknown-linux-gnu"
        "macos-aarch64:aarch64-apple-darwin"
    "macos-x86_64:x86_64-apple-darwin"
    "windows-x86_64:x86_64-pc-windows-gnu"
)

selected() {
    [[ -z "$ONLY" ]] && return 0
    [[ ",$ONLY," == *",$1,"* ]]
}

have() { command -v "$1" >/dev/null 2>&1; }

echo "==> 1/4 构建管理后台"
(cd web && npm ci --no-audit --no-fund && npm run build)

echo
echo "==> 2/4 校验（fmt / clippy / 测试）"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
if [[ "$SKIP_TESTS" == true ]]; then
    echo "    （--skip-tests：跳过 cargo test）"
else
    cargo test
fi

echo
echo "==> 3/4 交叉编译各平台二进制"
if ! have cargo-zigbuild; then
    echo "找不到 cargo-zigbuild：cargo install cargo-zigbuild" >&2
    echo "（CI 上用各平台原生 runner，不需要它）" >&2
    exit 1
fi

mkdir -p dist
# 每轮发版前清掉旧的暂存目录，避免上一次的文件被夹进这一次的归档。
rm -rf dist/akhub-v*-*/ 2>/dev/null || true

BUILT=()
SKIPPED=()
for entry in "${PLATFORMS[@]}"; do
    platform="${entry%%:*}"
    target="${entry#*:}"
    selected "$platform" || continue

    echo "  - $platform ($target)"
    # 逐个目标检查工具链：缺哪个就跳过哪个，而不是炸掉整轮发版。
    # rustup 的 target 是必需的——即使是 macOS 上从 arm64 交叉编译 x86_64，
    # 也需要 rustup target add x86_64-apple-darwin；Apple SDK 多架构只解决
    # 链接器与系统库那一半，标准库那一半仍然来自 rustup。
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
        echo "    缺少目标 $target（rustup target add $target），跳过"
        SKIPPED+=("$platform")
        continue
    fi

    if ! cargo zigbuild --release --locked --target "$target"; then
        echo "    编译失败，跳过 $platform" >&2
        SKIPPED+=("$platform")
        continue
    fi

    binary="target/$target/release/akhub"
    [[ "$target" == *windows* ]] && binary="$binary.exe"

    if [[ ! -f "$binary" ]]; then
        echo "    编译产物不存在：$binary" >&2
        SKIPPED+=("$platform")
        continue
    fi

    scripts/package.sh "$target" "$platform" "$binary"
    BUILT+=("$platform")
done

if [[ ${#BUILT[@]} -eq 0 ]]; then
    echo "没有构建任何平台，检查 --only 的取值" >&2
    exit 1
fi

echo
echo "==> 4/4 校验和"
if [[ "$CHECKSUMS" == true ]]; then
    (
        cd dist
        rm -f checksums.txt
        # 用 nullglob 收集产物：`--only linux-x86_64` 这类只构建部分平台的
        # 调用里，`./*.exe` 会一个都匹配不到；未开启 nullglob 时它保持字面量，
        # sha256sum 会报 "No such file" 并让整个发版脚本失败。
        shopt -s nullglob
        targets=(./*.tar.gz ./*.exe)
        shopt -u nullglob
        if [ ${#targets[@]} -eq 0 ]; then
            echo "    dist/ 里没有可校验的产物，跳过" >&2
            exit 0
        fi
        # macOS 自带 shasum，Linux 用 sha256sum。两者输出格式一致。
        if have sha256sum; then
            sha256sum "${targets[@]}" > checksums.txt
        elif have shasum; then
            shasum -a 256 "${targets[@]}" > checksums.txt
        else
            echo "    找不到 sha256sum / shasum，跳过" >&2
            exit 0
        fi
        cat checksums.txt
    )
fi

echo
echo "==> 完成"
echo "    产出目录：dist/"
printf '    已构建：%s\n' "${BUILT[*]}"
[[ ${#SKIPPED[@]} -gt 0 ]] && printf '    已跳过：%s（缺少工具链或编译失败）\n' "${SKIPPED[*]}"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
cat <<EOF

    发布步骤：
      1. 确认 Cargo.toml 的版本是 $VERSION，tag 必须打 v$VERSION
         （release.yml 里有一道校验，对不上直接失败）
      2. git tag v$VERSION && git push origin v$VERSION
         CI 会自动构建 5 个平台并推送多架构镜像到 ghcr.io/hsmirage/akhub（镜像路径全小写）
      3. 本地产物要手工上传时：
         gh release create v$VERSION dist/*.tar.gz dist/*.zip dist/checksums.txt

    本地构建多架构镜像（需要 Docker Desktop / colima 已启动）：
      ./scripts/docker-build.sh              # 构建 amd64+arm64 到本地
      ./scripts/docker-build.sh --push       # 构建并推送

EOF
