#!/bin/sh
# Akhub 一键安装 / 升级脚本（Linux 与 macOS）。
#
#   curl -fsSL https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh | sh
#
# 或先看再跑（推荐——curl|sh 之前至少确认一遍脚本内容）：
#   curl -fsSLO https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh
#   less install.sh && sh install.sh
#
# 参数：
#   --version <tag>     安装指定版本（默认取最新 Release）
#   --dir <path>        安装目录（默认 /usr/local/bin，权限不足时自动 sudo）
#   --no-verify         跳过 sha256 校验（不推荐）
#   --libc <glibc|musl> 兼容旧参数；现在只发布 glibc 版，musl 已停发（会给出警告）
#   --service           额外安装 systemd 单元并启用（仅 Linux + root）
#   --dry-run           只打印将要做什么
#
# 环境变量等价物：AKHUB_VERSION / AKHUB_INSTALL_DIR / AKHUB_REPO
#
# 退出码：0 成功；1 参数错误；2 不支持的平台；3 下载失败；4 校验失败；5 安装失败。

set -eu

REPO="${AKHUB_REPO:-HsMirage/Akhub}"
# 镜像仓库路径必须全小写：Docker 拒绝含大写字母的 repository 名，
# 而 GitHub 的组织/仓库名经常是大小写混排的（HsMirage/Akhub）。
IMAGE="ghcr.io/$(printf '%s' "$REPO" | tr '[:upper:]' '[:lower:]')"
# 自建镜像 / 私有部署用 AKHUB_BASE_URL 指向另一个下载根；
# 留空时按 GitHub Releases 的地址拼。
BASE_URL="${AKHUB_BASE_URL:-}"
VERSION="${AKHUB_VERSION:-}"
INSTALL_DIR="${AKHUB_INSTALL_DIR:-/usr/local/bin}"
LIBC_PREF="glibc"
VERIFY=1
WITH_SERVICE=0
DRY_RUN=0

die() { printf 'install.sh: %s\n' "$1" >&2; exit "${2:-1}"; }
info() { printf '==> %s\n' "$1"; }
warn() { printf '警告：%s\n' "$1" >&2; }

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --version=*) VERSION="${1#*=}"; shift ;;
        --dir) INSTALL_DIR="$2"; shift 2 ;;
        --dir=*) INSTALL_DIR="${1#*=}"; shift ;;
        --libc) LIBC_PREF="$2"; shift 2 ;;
        --libc=*) LIBC_PREF="${1#*=}"; shift ;;
        --repo) REPO="$2"; shift 2 ;;
        --no-verify) VERIFY=0; shift ;;
        --service) WITH_SERVICE=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数：$1（用 --help 看用法）" 1 ;;
    esac
done

# ---------------------------------------------------------------- 平台探测
uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    Linux)  os_family="linux" ;;
    Darwin) os_family="macos" ;;
    *) die "不支持的平台：$uname_s。Windows 请用 install.ps1，其他平台请自行编译。" 2 ;;
esac

case "$uname_m" in
    x86_64|amd64)  arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) die "不支持的架构：$uname_m（当前只发布 x86_64 与 aarch64）" 2 ;;
esac

# 资产命名与 .github/workflows/release.yml 的 matrix.name 一一对应：
#   linux-x86_64 / linux-aarch64
#   macos-aarch64 / macos-x86_64
#
# Linux 现在只有 glibc 版（静态链接的 musl 版已停发）。--libc 保留是为了不让
# 旧脚本直接报参数错误，但选了 musl 也只能拿到 glibc 资产，所以明确警告一句。
if [ "$LIBC_PREF" = "musl" ]; then
    warn "--libc musl 已不再发布；本次安装 glibc 版（需要 glibc 2.34+）"
fi
if [ "$os_family" = "linux" ]; then
    platform="linux-${arch}"
else
    platform="macos-${arch}"
fi

# ---------------------------------------------------------------- 版本解析
if [ -z "$VERSION" ]; then
    info "查询最新版本"
    api="https://api.github.com/repos/${REPO}/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        body="$(curl -fsSL "$api" 2>/dev/null || true)"
    elif command -v wget >/dev/null 2>&1; then
        body="$(wget -qO- "$api" 2>/dev/null || true)"
    else
        die "找不到 curl 或 wget，无法下载" 3
    fi
    # 不依赖 jq：tag_name 在 JSON 顶层的第一个匹配就够了。
    VERSION="$(printf '%s' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
    [ -n "$VERSION" ] || die "无法确定最新版本，请用 --version vX.Y.Z 指定（GitHub API 可能限流）" 3
fi

# 允许 --version 1.0.0 与 --version v1.0.0 两种写法。
case "$VERSION" in
    v*) tag="$VERSION" ;;
    *)  tag="v${VERSION}" ;;
esac

asset="akhub-${tag}-${platform}.tar.gz"
# 下载根：GitHub Releases 下完整 URL 是 base + "/" + asset；
# 自建镜像（AKHUB_BASE_URL）下同样约定为 <根>/<tag>/<asset>，
# 也就是说镜像目录要保留 <tag> 这一层。
if [ -n "$BASE_URL" ]; then
    base="${BASE_URL%/}/${tag}"
else
    base="https://github.com/${REPO}/releases/download/${tag}"
fi

info "平台：${os_family}/${arch}（${platform}）"
info "版本：${tag}"
info "资产：${asset}"

if [ "$DRY_RUN" = "1" ]; then
    echo "将要下载：${base}/${asset}"
    echo "以及：    ${base}/checksums.txt"
    echo "安装到：  ${INSTALL_DIR}/akhub"
    [ "$WITH_SERVICE" = "1" ] && echo "并安装 systemd 单元 akhub.service"
    exit 0
fi

# ---------------------------------------------------------------- 下载
tmp="$(mktemp -d "${TMPDIR:-/tmp}/akhub-install.XXXXXX")"
# shellcheck disable=SC2064  # 这里就是要立刻展开 $tmp
trap "rm -rf '$tmp'" EXIT INT TERM

fetch() {
    # $1 = URL，$2 = 目标路径
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

info "下载 ${asset}"
fetch "${base}/${asset}" "${tmp}/${asset}" || die "下载失败：${base}/${asset}" 3

if [ "$VERIFY" = "1" ]; then
    info "校验 sha256"
    if fetch "${base}/checksums.txt" "${tmp}/checksums.txt"; then
        # coreutils 的 `sha256sum a b c` 输出 `hash  filename`，但 `sha256sum ./*`
        # 会输出 `hash  ./filename`——发布流程两种都可能用到（CI 用 ./ *，本地
        # 逐个列文件），所以匹配必须同时容忍带与不带 `./` 前缀的写法。
        expected="$(awk -v name="${asset}" '$2 == name || $2 == "./" name { print $1; exit }' \
            "${tmp}/checksums.txt")"
        if [ -z "$expected" ]; then
            die "checksums.txt 里没有 ${asset}，拒绝安装" 4
        fi
        if command -v sha256sum >/dev/null 2>&1; then
            actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
            actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
        else
            warn "找不到 sha256sum / shasum，跳过校验"
            actual="$expected"
        fi
        if [ "$actual" != "$expected" ]; then
            die "sha256 不匹配，下载可能被篡改或损坏
  期望：${expected}
  实际：${actual}" 4
        fi
        info "校验通过：${actual}"
    else
        warn "拿不到 checksums.txt，跳过校验"
    fi
else
    warn "已按 --no-verify 跳过 sha256 校验"
fi

# ---------------------------------------------------------------- 解包
info "解包"
tar -xzf "${tmp}/${asset}" -C "$tmp"
# 归档内层目录名就是资产名去掉扩展名。
inner="${tmp}/akhub-${tag}-${platform}"
[ -x "${inner}/akhub" ] || die "归档结构与预期不符，找不到 ${inner}/akhub" 4

# 先确认这个二进制真的能在本机跑起来，再动 /usr/local/bin。
"${inner}/akhub" --version >/dev/null 2>&1 \
    || warn "制品无法在当前系统执行（多半是 glibc 版本太旧；本机 glibc: $(ldd --version 2>/dev/null | head -n1 || echo 未知)）"

# ---------------------------------------------------------------- 安装
# 目录还不存在时先尝试直接创建：/tmp 下的路径、~/.local/bin 这类都在
# 用户自己的可写范围内，不该因为"目录不存在"就去要 sudo 密码
# （在非交互环境里那会直接失败，而用户看到的提示跟真正的原因毫无关系）。
if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || true
fi

target="${INSTALL_DIR}/akhub"
SUDO=""
if [ ! -d "$INSTALL_DIR" ] || [ ! -w "$INSTALL_DIR" ]; then
    if [ "$(id -u)" = "0" ]; then
        :
    elif command -v sudo >/dev/null 2>&1 && [ -t 0 ]; then
        SUDO="sudo"
        info "${INSTALL_DIR} 不可写，将使用 sudo"
    elif command -v sudo >/dev/null 2>&1; then
        # 非交互（curl | sh、CI）下 sudo 拿不到密码，直接说清楚该换哪个目录。
        die "${INSTALL_DIR} 不可写，且当前不是交互终端、sudo 无法输入密码。\n  改用：sh install.sh --dir \"$HOME/.local/bin\"  或先自行创建并授权该目录。" 5
    else
        die "${INSTALL_DIR} 不可写且没有 sudo；改用 --dir ~/.local/bin 安装" 5
    fi
fi

if [ -x "$target" ]; then
    old="$("$target" --version 2>/dev/null || echo '未知')"
    info "已安装版本：${old}"
    # 升级前留一份可回滚的副本，命名与 scripts/deploy.sh 保持一致。
    stamp="$(date +%Y%m%d-%H%M%S)"
    $SUDO cp -p "$target" "${target}.bak-${stamp}" 2>/dev/null \
        && info "旧版本已备份为 ${target}.bak-${stamp}"
fi

info "安装到 ${target}"
$SUDO install -m 755 "${inner}/akhub" "$target"

new_version="$("$target" --version 2>/dev/null || echo '（无法执行）')"
info "完成：${new_version}"

# ---------------------------------------------------------------- systemd
if [ "$WITH_SERVICE" = "1" ]; then
    if [ "$os_family" != "linux" ]; then
        warn "--service 只在 Linux 上有效，已跳过"
    elif ! command -v systemctl >/dev/null 2>&1; then
        warn "找不到 systemctl，已跳过服务安装"
    else
        unit="${inner}/deploy/akhub.service"
        if [ -f "$unit" ]; then
            info "安装 systemd 单元"
            $SUDO install -m 644 "$unit" /etc/systemd/system/akhub.service
            $SUDO systemctl daemon-reload
            # 单元里写死了 User=akhub 与 StateDirectory=akhub，先把用户建出来。
            if ! id akhub >/dev/null 2>&1; then
                info "创建系统用户 akhub"
                $SUDO useradd --system --home-dir /var/lib/akhub --create-home akhub
            fi
            $SUDO systemctl enable --now akhub
            info "服务已启动：systemctl status akhub"
        else
            warn "归档里没有 deploy/akhub.service，跳过"
        fi
    fi
fi

# ---------------------------------------------------------------- 后续
cat <<EOF

下一步：
  1. 前台试跑（首次会在数据目录生成主密钥）：
       AKHUB_DATA_DIR=./data ${target}
  2. 打开 http://127.0.0.1:8080/admin 设置管理员密码。
  3. 长期运行推荐 systemd（见 deploy/README.md）或 Docker：
       docker run -d --name akhub -p 127.0.0.1:8080:8080 -v akhub-data:/data ${IMAGE}:latest

备份提醒：数据目录里的 master.key 一旦丢失，数据库中加密保存的上游 API Key 无法恢复。
EOF
