#!/usr/bin/env bash
# Akhub 快速替换部署（生产用）。
#
# 与 `docker compose up -d` 的区别只有一点：**旧容器只等很短时间就杀**。
#
# 现场规律（同一台机、同一份 compose、同样的命令）：
#   v1.1.47  1 秒   —— 恰好没有在途长流
#   v1.1.48 59 秒   —— 有长流，compose 等它排空
#   v1.1.49 53 秒   —— 同上
#   v1.1.50 87 秒   —— 同上
# 差别全在"旧容器要等在途流结束"，不在命令。
#
# 所以这里把等待压到 `DRAIN_SECS`（默认 5 秒）：在途长流最多被截断这一次，
# 换来的是一次几秒的切换。要零截断就不传小值（或直接用 docker compose，
# 它会按 stop_grace_period 200s 排空）。
#
# 用法：
#   ./upgrade-fast.sh 1.1.51
#   DRAIN_SECS=30 ./upgrade-fast.sh 1.1.51     # 给长流多一点时间
set -euo pipefail

NEW_TAG="${1:?用法: $0 <新版本号，如 1.1.51>}"
DRAIN_SECS="${DRAIN_SECS:-5}"
COMPOSE_DIR="${COMPOSE_DIR:-/opt/akhub}"
HEALTH="${HEALTH:-http://127.0.0.1:8088/health/ready}"
IMAGE="ghcr.io/hsmirage/akhub:${NEW_TAG}"

cd "$COMPOSE_DIR"

# 只改 image: 那一行——注释里的 akhub:8080 与挂载路径 /var/lib/akhub 不能动。
SET_IMAGE="s|^\\( *image: ghcr.io/hsmirage/akhub:\\)[0-9][0-9.]*$|\\1${NEW_TAG}|"

echo "==> 1/4 拉取 $IMAGE"
docker pull "$IMAGE" >/dev/null
echo "    镜像就绪"

echo "==> 2/4 备份 compose"
STAMP="$(date +%Y%m%d-%H%M%S)"
cp -p docker-compose.yml "docker-compose.yml.bak-$STAMP"
mkdir -p binary-backups
CURRENT="$(sed -n 's|^ *image: ghcr.io/hsmirage/akhub:\([0-9][0-9.]*\)$|\1|p' docker-compose.yml)"
echo "$IMAGE" > "binary-backups/akhub-image-bak-$STAMP.txt"
echo "    当前版本：${CURRENT:-未知}"

echo "==> 3/4 停旧容器（最多等 ${DRAIN_SECS}s 排空）"
SWAP_START="$(date +%s)"
if docker ps -q -f name=^akhub$ | grep -q .; then
    docker update --restart=no akhub >/dev/null 2>&1 || true
    docker stop -t "$DRAIN_SECS" akhub >/dev/null 2>&1 || true
    docker rm -f akhub >/dev/null 2>&1 || true
fi

echo "==> 4/4 起新容器"
sed -i "$SET_IMAGE" docker-compose.yml
setsid nohup docker compose up -d akhub > "/tmp/akhub-deploy-${NEW_TAG}.log" 2>&1 < /dev/null &

for _ in $(seq 1 60); do
    if curl -fsS --max-time 2 "$HEALTH" >/dev/null 2>&1; then
        SWAP_END="$(date +%s)"
        echo "    READY，停机 $((SWAP_END - SWAP_START)) 秒"
        curl -s "${HEALTH%/ready}/version"; echo
        exit 0
    fi
    sleep 1
done

echo "!! 60 秒内未就绪，回滚到 ${CURRENT:-上一版}" >&2
cp -p "docker-compose.yml.bak-$STAMP" docker-compose.yml
setsid nohup docker compose up -d akhub > /tmp/akhub-rollback.log 2>&1 < /dev/null &
exit 1
