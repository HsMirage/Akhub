#!/bin/sh
# 容器入口（§25.1）。
#
# 两件事：
#   1. bind mount 的数据目录属主常常不是镜像里的 uid，进程又要以非 root 运行，
#      所以以 root 起步把属主纠正一次再降权；
#   2. 把镜像里的二进制**复制到数据目录**并运行那一份——容器内一键自更新的前提。
#
# 第 2 条是 2026-10-16 加的。原因：镜像里的 /usr/local/bin/akhub 属于 root 且位于
# 只读镜像层，以非 root 运行的服务进程**覆盖不了自己**（实测 touch 直接 Permission
# denied），于是容器部署永远升不了级。同机部署的 sub2api 能一键升级，正是因为它
# 的二进制 /app/sub2api 在容器可写层、属主就是运行用户：它下载新版覆盖自己、退出，
# 由 `restart: unless-stopped` 把同一个容器拉起来。
#
# 这里照搬那个思路，但不依赖镜像层可写：把二进制放到**数据卷**里跑。数据卷是持久
# 的、属主就是运行用户，所以进程能替换它；重启后入口脚本仍然运行那一份，升级得以
# 保留。
set -eu

DATA_DIR="${AKHUB_DATA_DIR:-/data}"
IMAGE_BIN="/usr/local/bin/akhub"
RUNTIME_DIR="$DATA_DIR/runtime"
RUNTIME_BIN="$RUNTIME_DIR/akhub"
# 记下"上次从镜像里复制出来的那份是哪个版本"，用来判断镜像有没有被换过。
STAMP="$RUNTIME_DIR/.image-version"

# 允许 `docker run <镜像> --version` 这种写法。
#
# 镜像的 CMD 是 /usr/local/bin/akhub，但用户在命令行追加参数时会**覆盖** CMD——
# 于是 `docker run <镜像> --version` 交给本脚本的是 "--version"。第一个参数以 -
# 开头时说明用户要跑的就是 akhub 自己。
#
# 记下"用户是不是显式要跑 akhub"：只有这种情况才允许下面把参数重定向到数据卷里
# 的那一份。像 `docker run <镜像> sh` 这种明确要跑别的命令的，不该被改写。
want_akhub=no
case "${1:-}" in
    -*) want_akhub=yes ;;
esac

if [ "$(id -u)" = "0" ]; then
    mkdir -p "$DATA_DIR"
    # 只改目录本身的属主，不动里面的文件：容器内的 akhub 会自己按 0600/0700
    # 收紧权限。递归 chown 在大目录上会拖慢每次启动。
    owner="$(stat -c '%u:%g' "$DATA_DIR" 2>/dev/null || echo '?')"
    if [ "$owner" != "10001:10001" ]; then
        echo "akhub-entrypoint: 修正 $DATA_DIR 属主 $owner -> 10001:10001"
        chown 10001:10001 "$DATA_DIR" || {
            echo "akhub-entrypoint: 无法修改 $DATA_DIR 属主，将以 10001 继续；若启动失败请检查宿主目录权限" >&2
        }
    fi
    # 以 root 起步时，下面这些准备工作在降权之前做完即可；复制本身用 gosu 跑，
    # 免得数据卷里出现 root 属主的文件。
    exec gosu akhub "$0" "$@"
fi

# ---- 以下都以运行用户身份执行 ----------------------------------------------

image_version="$("$IMAGE_BIN" --version 2>/dev/null | awk '{print $2}')" || image_version=""
[ -n "$image_version" ] || image_version="unknown"

# 数据卷可写时才用"可替换的那一份"。不可写（比如数据卷以只读挂进来）就退回镜像
# 里的二进制：服务照常能跑，只是面板会说一键升级不可用。
use_runtime=no
if mkdir -p "$RUNTIME_DIR" 2>/dev/null && [ -w "$RUNTIME_DIR" ]; then
    use_runtime=yes
fi

if [ "$use_runtime" = yes ]; then
    recorded="$(cat "$STAMP" 2>/dev/null || echo '')"
    if [ ! -x "$RUNTIME_BIN" ] || [ "$image_version" != "$recorded" ]; then
        # 两种情况要刷新：首次运行，或者镜像被换过（运维执行了 pull + up -d）。
        #
        # 判据是"镜像版本 ≠ 上次复制的镜像版本"，而不是"镜像比数据卷里新"：
        # 运维显式回滚到旧 tag 时也该以镜像为准。自更新过的二进制不会改动这个
        # 标记，所以重启后它仍然是"数据卷里那份更新"，不会被镜像顶回去。
        if [ -f "$RUNTIME_BIN" ] && [ "$image_version" != "$recorded" ]; then
            echo "akhub-entrypoint: 镜像版本 $recorded -> $image_version，改用镜像里的二进制"
        else
            echo "akhub-entrypoint: 准备可自更新的二进制副本（v$image_version）"
        fi
        tmp="$RUNTIME_DIR/.akhub-incoming-$$"
        if cp "$IMAGE_BIN" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$RUNTIME_BIN"; then
            printf '%s' "$image_version" > "$STAMP"
        else
            rm -f "$tmp"
            echo "akhub-entrypoint: 复制二进制到 $RUNTIME_BIN 失败，退回镜像里那一份（一键升级将不可用）" >&2
            use_runtime=no
        fi
    fi
else
    echo "akhub-entrypoint: $RUNTIME_DIR 不可写，运行镜像里的二进制（一键升级将不可用）" >&2
fi

if [ "$use_runtime" = yes ]; then
    # 能从数据卷里运行，说明"替换二进制 + 退出"这条链路是通的：容器带
    # `restart: unless-stopped`（本仓库的 compose 就是）时会被重新拉起。
    # 这个变量是给 akhub 的 can_restart() 看的，让它敢在升级后真的退出。
    AKHUB_ALLOW_RESTART=1
    export AKHUB_ALLOW_RESTART
    # 只替换"要跑 akhub"这一种情形的目标；用户显式指定别的命令时不要改写。
    if [ "$want_akhub" = yes ]; then
        set -- "$RUNTIME_BIN" "$@"
    elif [ "$#" -eq 0 ]; then
        # 没给参数（例如 entrypoint 被覆盖后 CMD 一起丢了）：默认跑服务本身。
        set -- "$RUNTIME_BIN"
    elif [ "$#" -eq 1 ] && [ "$1" = "$IMAGE_BIN" ]; then
        # CMD 的默认值：换成人可写的那一份。
        set -- "$RUNTIME_BIN"
    fi
fi

exec "$@"
