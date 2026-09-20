#!/bin/sh
# 容器入口（§25.1）。
#
# 存在的原因只有一个：bind mount 的数据目录属主常常不是 10001，
# 而进程必须以非 root 运行。这里以 root 起步把属主纠正一次，再降权执行。
# 用命名卷时这一步是空操作，不会有额外副作用。
set -eu

DATA_DIR="${AKHUB_DATA_DIR:-/data}"

# 允许 `docker run <镜像> --version` 这种写法。
#
# 镜像的 CMD 是 `/usr/local/bin/akhub`，但用户在命令行追加参数时会**覆盖**
# CMD——于是 `docker run <镜像> --version` 交给本脚本的是 "--version"，
# 后面 gosu 会把它当成可执行文件名去找，报
# `exec: "--version": executable file not found in $PATH`。
# 第一个参数以 - 开头时说明用户要跑的就是 akhub 自己，补上路径即可。
case "${1:-}" in
    -*) set -- /usr/local/bin/akhub "$@" ;;
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
    exec gosu akhub "$@"
fi

# 已经在非 root 下（compose 里写了 user:）就直接执行。
exec "$@"
