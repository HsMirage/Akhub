#!/usr/bin/env bash
# 把一个已编译好的二进制打包成发行工件（§25、§27 阶段 6）。
#
# 本地发版（scripts/release.sh）与 CI（.github/workflows/release.yml）都调用
# 这里，**布局和命名只在这一个文件里定义**。两边各写一份 tar 命令的结果就是
# 安装脚本在某一边找不到文件，而那种错误只有用户才会遇到。
#
# 用法：
#   scripts/package.sh <target-triple> <platform-name> <二进制路径>
#
# 例：
#   scripts/package.sh x86_64-unknown-linux-musl linux-x86_64-musl \
#       target/x86_64-unknown-linux-musl/release/akhub
#
# 产出：
#   dist/akhub-<版本>-<platform-name>.tar.gz   （Windows 目标产出 .zip）
#
# 归档内层目录名 == 资产名去掉扩展名，安装脚本依赖这一点定位可执行文件。

set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:?缺少目标三元组，例如 x86_64-unknown-linux-musl}"
PLATFORM="${2:?缺少平台名，例如 linux-x86_64-musl}"
BINARY="${3:?缺少二进制路径}"

[[ -f "$BINARY" ]] || { echo "找不到二进制：$BINARY" >&2; exit 1; }

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
[[ -n "$VERSION" ]] || { echo "无法从 Cargo.toml 读出版本号" >&2; exit 1; }

# 允许用 AKHUB_TAG 覆盖：本地预演 vX.Y.Z 的资产名时不必真的打 tag。
TAG="${AKHUB_TAG:-v$VERSION}"
ASSET="akhub-$TAG-$PLATFORM"
STAGE="dist/$ASSET"

rm -rf "$STAGE"
mkdir -p "$STAGE"

install -m755 "$BINARY" "$STAGE/akhub"

# 发行包自带文档与部署示例：离线机器上 tar -xzf 之后就能看到怎么装，
# 不用回头找仓库。缺失的条目只警告不失败，避免文档改名把发版卡住。
for entry in README.md NOTICES.md LICENSE; do
    if [[ -f "$entry" ]]; then
        cp "$entry" "$STAGE/"
    else
        echo "警告：缺少 $entry，发行包将不包含它" >&2
    fi
done
for entry in deploy install.sh install.ps1; do
    if [[ -e "$entry" ]]; then
        cp -R "$entry" "$STAGE/"
    else
        echo "警告：缺少 $entry，发行包将不包含它" >&2
    fi
done

# 生成 zip 归档。
#
# 不能直接假定有 `zip`：GitHub 的 Windows runner 上 Git Bash 只带 tar/unzip，
# 没有 zip 命令，会在发版当天以 "zip: command not found" 失败。这里按可用性
# 依次回退到 python（三个平台都预装），保证同一份打包逻辑到处能跑。
make_zip() {
    local asset="$1"
    if command -v zip >/dev/null 2>&1; then
        (cd dist && zip -qr "$asset.zip" "$asset")
        return
    fi

    local python=""
    if command -v python3 >/dev/null 2>&1; then
        python="python3"
    elif command -v python >/dev/null 2>&1; then
        python="python"
    fi
    if [[ -n "$python" ]]; then
        # 用环境变量传参，避免把资产名拼进 python 源码里做引号体操。
        ASSET="$asset" "$python" -c '
import os, shutil
asset = os.environ["ASSET"]
os.chdir("dist")
shutil.make_archive(asset, "zip", ".", asset)
'
        return
    fi

    echo "找不到 zip 也找不到 python，无法生成 $asset.zip" >&2
    echo "装一个 zip（Debian: apt install zip / macOS 自带）后重试" >&2
    return 1
}

case "$TARGET" in
    *windows*)
        # 二进制重新命名成 .exe，安装脚本按这个名字去找。
        mv "$STAGE/akhub" "$STAGE/akhub.exe"
        # zip 是增量写入的：同名旧归档还在的话，上一次的内容会被原样保留，
        # 于是发出来的包里混着两个版本的二进制。必须先删干净。
        rm -f "dist/$ASSET.zip"
        make_zip "$ASSET"

        # 同时单独放一个裸 exe。
        #
        # Unix 那边必须打包：可执行权限靠文件模式的 +x 位，浏览器下载会丢掉它，
        # 裸传 ELF 用户拿到的是「权限不足」。Windows 没有这个问题——能不能跑
        # 只看扩展名——所以「必须打包」的理由在这里不成立，剩下的只是压缩
        # （17 MB -> 6 MB）和顺带捎上文档。让用户为这两点被迫多走
        # 「解压 → 进一层目录 → 运行」三步并不划算，两个都给最省事。
        cp "$STAGE/akhub.exe" "dist/$ASSET.exe"
        ARTIFACT="$ASSET.zip $ASSET.exe"
        ;;
    *)
        tar -czf "dist/$ASSET.tar.gz" -C dist "$ASSET"
        ARTIFACT="$ASSET.tar.gz"
        ;;
esac

# 可能产出多个文件（Windows 同时给 zip 与裸 exe），逐个打印便于调用方解析。
for artifact in $ARTIFACT; do
    echo "dist/$artifact"
done
