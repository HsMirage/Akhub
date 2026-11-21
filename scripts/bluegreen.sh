#!/usr/bin/env bash
# Akhub 蓝绿升级：在途长流一条不断，切换秒级。
#
# 问题：`docker compose up -d` 先停旧容器，而旧容器要等在途流结束
#（stop_grace_period 200s）。实测 v1.1.48/49/50 分别停了 59/53/87 秒。
#
# 做法：宿主 8088 交给 socat 常驻（它只是个 TCP 转发，不解析 HTTP，SSE 原样透传），
# 容器改成监听内部端口 18088/18089（blue/green 两个槽位）。升级时：
#   1. 在空闲槽位起新版本（只读挂载，不碰数据）；
#   2. 健康后把 socat 指向新槽位——毫秒级，在途 TCP 连接不受影响；
#   3. 旧槽位留着跑完在途流，跑完自动退场。
#
# 两条入口都覆盖：
#   - sub2api → http://89.58.17.138:8088（走 socat）
#   - new-api → 改成 http://89.58.17.138:8088（同样走 socat；已验证容器可达宿主 IP）
# 这样不再依赖容器名 "akhub"，网络别名切换那套复杂度就不需要了。
#
# 数据安全：同一份 SQLite 不能两个实例同时写（会互相覆盖内存快照）。
# 所以新槽位在切换前一律只读挂载；切换后旧槽位摘掉网络、只读不写，写入权唯一。
#
# 用法：
#   /opt/akhub/bluegreen.sh 1.1.51      # 升级
#   /opt/akhub/bluegreen.sh --rollback  # 切回上一槽位
#   /opt/akhub/bluegreen.sh --status    # 看当前槽位
set -euo pipefail

BLUE=18088
GREEN=18089
HOST_PORT=8088
STATE=/opt/akhub/.bg-active
IMAGE_REPO=ghcr.io/hsmirage/akhub

log() { printf '\n==> %s\n' "$*"; }
active() { cat "$STATE" 2>/dev/null || echo blue; }
other() { [ "$(active)" = blue ] && echo green || echo blue; }
port() { [ "$1" = blue ] && echo $BLUE || echo $GREEN; }

healthy() { curl -fsS --max-time 2 "http://127.0.0.1:$(port "$1")/health/ready" >/dev/null 2>&1; }

socat_point_to() {
    local p
    p="$(port "$1")"
    pkill -f "socat TCP-LISTEN:${HOST_PORT}," 2>/dev/null || true
    sleep 0.3
    setsid nohup socat TCP-LISTEN:"${HOST_PORT}",fork,reuseaddr,backlog=1024 \
        TCP:127.0.0.1:"$p" >> /var/log/akhub-socat.log 2>&1 < /dev/null &
    sleep 0.4
}

run_slot() {
    local color="$1" image="$2" mode="$3"   # mode: ro | rw
    local p; p="$(port "$color")"
    docker rm -f "akhub-$color" >/dev/null 2>&1 || true
    local vol="/var/lib/akhub:/data"
    [ "$mode" = ro ] && vol="/var/lib/akhub:/data:ro"
    local extra=()
    [ "$mode" = rw ] && extra=(--restart unless-stopped --memory 4g --memory-swap 4g --cpus 2.0)
    docker run -d --name "akhub-$color" "${extra[@]}" \
        --user 999:990 \
        -p "127.0.0.1:${p}:8080" \
        -e AKHUB_LISTEN=0.0.0.0:8080 -e AKHUB_DATA_DIR=/data \
        -e MALLOC_ARENA_MAX=2 -e RUST_LOG=akhub=info,warn \
        -v "$vol" \
        --health-cmd '["CMD","/usr/local/bin/akhub","--healthcheck"]' \
        --health-interval 10s --health-retries 3 --health-start-period 5s \
        --label "com.akhub.color=$color" \
        "$image" >/dev/null
}

# ---- 一次性改造：把 8088 从 compose 交给 socat ----
if [ ! -f "$STATE" ]; then
    log "一次性改造：8088 → socat，容器改监听 $BLUE/$GREEN"
    STAMP="$(date +%Y%m%d-%H%M%S)"
    cp -p /opt/akhub/docker-compose.yml "/opt/akhub/docker-compose.yml.bak-bg-$STAMP"
    CUR_IMG="$(docker inspect akhub --format '{{.Config.Image}}' 2>/dev/null || echo "$IMAGE_REPO:latest")"
    echo "$CUR_IMG" > /opt/akhub/.bg-current-image

    # 停旧、按新拓扑起在 blue 槽（可写，它是唯一写者）
    docker update --restart=no akhub >/dev/null 2>&1 || true
    docker stop -t 60 akhub >/dev/null 2>&1 || true
    docker rm -f akhub >/dev/null 2>&1 || true
    sed -i 's|^ *- "8088:8080"$|      - "127.0.0.1:18088:8080"   # bluegreen: 8088 由 socat 接管|' /opt/akhub/docker-compose.yml
    grep -q 'bluegreen: 8088 由 socat 接管' /opt/akhub/docker-compose.yml || {
        echo "!! 改造失败：compose 里没找到 8088:8080，请手工确认" >&2; exit 1; }

    run_slot blue "$CUR_IMG" rw
    for i in $(seq 1 60); do healthy blue && break; [ $i -eq 60 ] && { echo "!! blue 未就绪" >&2; exit 1; }; sleep 1; done
    socat_point_to blue
    echo blue > "$STATE"
    log "改造完成"
    curl -s "http://127.0.0.1:${HOST_PORT}/health/version"; echo
    echo "提示：请把 new-api 里 base_url=http://akhub:8080 的渠道改成 http://89.58.17.138:8088"
    exit 0
fi

case "${1:-}" in
--status)
    echo "active: $(active)  ($(port "$(active)"))"
    docker ps -a --filter label=com.akhub.color --format '{{.Names}} {{.Image}} {{.Status}}'
    curl -s "http://127.0.0.1:${HOST_PORT}/health/version"; echo
    exit 0 ;;
--rollback)
    T="$(other)"
    log "回滚到 $T"
    healthy "$T" || { echo "!! $T 未就绪，无法回滚" >&2; exit 1; }
    socat_point_to "$T"
    echo "$T" > "$STATE"
    curl -s "http://127.0.0.1:${HOST_PORT}/health/version"; echo
    exit 0 ;;
esac

NEW_TAG="${1:?用法: $0 <版本号> | --rollback | --status}"
IMAGE="${IMAGE_REPO}:${NEW_TAG}"
T="$(other)"; TP="$(port "$T")"

log "1/5 拉取 $IMAGE"
docker pull "$IMAGE" >/dev/null

log "2/5 在 $T（:$TP）只读预热"
run_slot "$T" "$IMAGE" ro
for i in $(seq 1 45); do
    healthy "$T" && { echo "    预热完成（${i}s）"; break; }
    [ $i -eq 45 ] && { echo "!! 预热超时，生产未受影响" >&2; docker rm -f "akhub-$T" >/dev/null 2>&1; exit 1; }
    sleep 1
done
curl -s --max-time 3 "http://127.0.0.1:${TP}/health/version"; echo

log "3/5 切换 socat → $T"
T0="$(date +%s)"
socat_point_to "$T"
echo "    切流 $(($(date +%s) - T0)) 秒"

log "4/5 让 $T 转正（可写），旧槽排在后面退场"
run_slot "$T" "$IMAGE" rw
echo "$T" > "$STATE"
for i in $(seq 1 60); do curl -fsS --max-time 2 "http://127.0.0.1:${HOST_PORT}/health/ready" >/dev/null 2>&1 && break; sleep 1; done

OLD="$(other)"
if docker ps -q -f "name=^akhub-${OLD}$" | grep -q .; then
    docker update --restart=no "akhub-$OLD" >/dev/null 2>&1 || true
    echo "    旧槽 akhub-$OLD 继续跑在途流，最多等 200 秒"
    for i in $(seq 1 200); do
        docker ps -q -f "name=^akhub-${OLD}$" | grep -q . || { echo "    ${i}s 后排空完毕"; break; }
        sleep 1
    done
    docker rm -f "akhub-$OLD" >/dev/null 2>&1 || true
fi

log "5/5 完成"
curl -s "http://127.0.0.1:${HOST_PORT}/health/version"; echo
docker ps --filter label=com.akhub.color --format '{{.Names}} {{.Image}} {{.Status}}'
