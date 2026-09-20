#!/usr/bin/env bash
# Akhub 单机部署/升级脚本。
#
# 构建当前前端 + Linux x86_64 二进制，上传到服务器，原子替换并验证健康检查；
# 失败时自动回滚到上一份二进制。
#
# 用法：
#   AKHUB_DEPLOY_HOST=1.2.3.4 ./scripts/deploy.sh
#   ./scripts/deploy.sh --host 1.2.3.4 --port 22
#   ./scripts/deploy.sh --skip-build          # 复用已构建产物
#
# 目标主机必须显式给出：脚本不内置任何默认服务器，避免把个人基础设施信息
# 写进公开仓库。推荐放进本地环境变量或 .env（.env 已在 .gitignore 里）。
#
# 可用环境变量：
#   AKHUB_DEPLOY_HOST  必填，目标主机
#   AKHUB_DEPLOY_PORT  默认 22
#   AKHUB_DEPLOY_KEY   默认 ~/.ssh/id_ed25519
#   AKHUB_DEPLOY_USER  默认 root
#   AKHUB_DEPLOY_SERVICE（默认 akhub）/ AKHUB_DEPLOY_HEALTH（默认 http://127.0.0.1:8080/health/ready）
set -euo pipefail

cd "$(dirname "$0")/.."

HOST="${AKHUB_DEPLOY_HOST:-}"
PORT="${AKHUB_DEPLOY_PORT:-22}"
KEY="${AKHUB_DEPLOY_KEY:-$HOME/.ssh/id_ed25519}"
USER="${AKHUB_DEPLOY_USER:-root}"
SERVICE="${AKHUB_DEPLOY_SERVICE:-akhub}"
HEALTH="${AKHUB_DEPLOY_HEALTH:-http://127.0.0.1:8080/health/ready}"
TARGET="x86_64-unknown-linux-gnu"
REMOTE_BIN="/usr/local/bin/akhub"
SKIP_BUILD=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --key) KEY="$2"; shift 2 ;;
        --service) SERVICE="$2"; shift 2 ;;
        --health) HEALTH="$2"; shift 2 ;;
        --user) USER="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
        *) echo "未知参数：$1" >&2; exit 2 ;;
    esac
done

# 目标主机没有默认值：缺了就明确报错，而不是往某个内置地址上部署。
if [[ -z "$HOST" ]]; then
    cat >&2 <<'EOF'
未指定目标主机。用法：
  AKHUB_DEPLOY_HOST=1.2.3.4 ./scripts/deploy.sh
  ./scripts/deploy.sh --host 1.2.3.4 --port 22 --key ~/.ssh/deploy_key
EOF
    exit 2
fi

SSH=(ssh -i "$KEY" -p "$PORT" -o BatchMode=yes -o ConnectTimeout=10 "$USER@$HOST")
log() { printf '\n==> %s\n' "$*"; }

if [[ "$SKIP_BUILD" != true ]]; then
    log "1/5 构建管理后台"
    (cd web && npm run build)

    log "2/5 交叉编译 Linux $TARGET"
    cargo zigbuild --release --target "$TARGET"
fi

BIN="target/$TARGET/release/akhub"
[[ -f "$BIN" ]] || { echo "找不到构建产物：$BIN" >&2; exit 1; }

log "3/5 打包并上传"
mkdir -p dist
STAMP="$(date +%Y%m%d-%H%M%S)"
ARTIFACT="dist/akhub-$TARGET-$STAMP"
cp "$BIN" "$ARTIFACT"
LOCAL_SHA="$(shasum -a 256 "$BIN" | awk '{print $1}')"
echo "    本地二进制：${ARTIFACT}（sha256 ${LOCAL_SHA:0:12}…）"
scp -i "$KEY" -P "$PORT" -o BatchMode=yes "$BIN" "$USER@$HOST:/tmp/akhub.new"

log "4/5 远程替换并重启 $SERVICE"
"${SSH[@]}" bash -s -- "$LOCAL_SHA" "$REMOTE_BIN" "$SERVICE" "$HEALTH" <<'REMOTE'
set -euo pipefail
SHA="$1"; BIN="$2"; SERVICE="$3"; HEALTH="$4"

echo "    校验上传文件 sha256"
REMOTE_SHA="$(sha256sum /tmp/akhub.new | awk '{print $1}')"
[[ "$REMOTE_SHA" == "$SHA" ]] || { echo "    sha256 不一致，中止"; exit 1; }
chmod +x /tmp/akhub.new

echo "    停止服务并备份现有二进制"
systemctl stop "$SERVICE"
STAMP="$(date +%Y%m%d-%H%M%S)"
if [[ -f "$BIN" ]]; then cp -p "$BIN" "$BIN.bak-$STAMP"; fi

echo "    安装新二进制"
install -m755 /tmp/akhub.new "$BIN"
rm -f /tmp/akhub.new
systemctl start "$SERVICE"

echo "    等待健康检查 $HEALTH"
ok=false
for _ in $(seq 1 40); do
    if curl -fsS --max-time 2 "$HEALTH" >/dev/null 2>&1; then ok=true; break; fi
    sleep 1
done

if [[ "$ok" != true ]]; then
    echo "    健康检查失败，回滚到 $BIN.bak-$STAMP"
    systemctl stop "$SERVICE"
    if [[ -f "$BIN.bak-$STAMP" ]]; then cp -p "$BIN.bak-$STAMP" "$BIN"; fi
    systemctl start "$SERVICE"
    sleep 2
    systemctl --no-pager --lines=20 status "$SERVICE" || true
    exit 1
fi

echo "    健康检查通过"
systemctl --no-pager --lines=5 status "$SERVICE" | head -8
REMOTE

log "5/5 完成"
echo "    服务：$SERVICE @ $HOST:$PORT"
echo "    二进制备份：$REMOTE_BIN.bak-*（保留在服务器上，便于手动回滚）"
