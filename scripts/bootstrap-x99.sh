#!/usr/bin/env bash
# x99 上的 x-sage 部署目录初始化。需要在服务器上以 root 执行一次（可重复执行）。
#
#   curl -fsSL https://raw.githubusercontent.com/xartifact/x-sage/main/scripts/bootstrap-x99.sh -o /tmp/b.sh
#   sudo bash /tmp/b.sh
#
# 做四件事：
#   1. /opt/xartifact/x-sage 归 binzhan 属主、github-actions 属组，setgid 2775，
#      使双方都能读写（binzhan 运维，runner 部署）
#   2. 从仓库下载 docker-compose.yml 与 data/ 骨架
#   3. 若 .env 不存在，生成一个含随机 write token 的
#   4. 打印后续步骤
#
# 关键：compose **不内嵌**在此脚本里。内嵌会与仓库版本各自演化，而两者的
# 漂移是静默的——x99 曾因此漏掉 volumes，导致容器内写入落在可写层，重建即丢失。
set -euo pipefail

DEPLOY_DIR=/opt/xartifact/x-sage
OWNER=binzhan
GROUP=github-actions
RAW_BASE=https://raw.githubusercontent.com/xartifact/x-sage/main

if [ "$(id -u)" -ne 0 ]; then
  echo "must run as root: sudo bash $0" >&2
  exit 1
fi

id "$OWNER" >/dev/null 2>&1 || { echo "user $OWNER not found" >&2; exit 1; }
getent group "$GROUP" >/dev/null 2>&1 || { echo "group $GROUP not found" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "docker not found" >&2; exit 1; }

echo "==> preparing $DEPLOY_DIR"
mkdir -p "$DEPLOY_DIR"

# setgid (2xxx): files created inside inherit the group, so binzhan-written
# files stay readable by the runner and vice versa.
chown "$OWNER:$GROUP" "$DEPLOY_DIR"
chmod 2775 "$DEPLOY_DIR"

echo "==> fetching docker-compose.yml"
# Always overwrite: this file is derived from the repo, never hand-edited here.
# Anything site-specific belongs in .env.
curl -fsSL "$RAW_BASE/docker-compose.yml" -o "$DEPLOY_DIR/docker-compose.yml.tmp"
mv "$DEPLOY_DIR/docker-compose.yml.tmp" "$DEPLOY_DIR/docker-compose.yml"

echo "==> preparing the KB volume"
# compose mounts ./data/kb over /app/kb, so writes (proposals, checkpoints,
# regenerated graph.json) survive container recreation.
#
# The mount hides the image's copy, so an empty host directory would leave the
# server with no KB at all. Seed it from the image on first run; afterwards the
# host copy is the authority and is never overwritten.
KB_DIR="$DEPLOY_DIR/data/kb"
mkdir -p "$KB_DIR"

if [ -z "$(ls -A "$KB_DIR" 2>/dev/null)" ]; then
  IMAGE=ghcr.io/xartifact/x-sage:latest
  echo "    seeding from $IMAGE"
  if docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull "$IMAGE" >/dev/null 2>&1; then
    # A throwaway container is the only way to read the baked-in layer without
    # racing a running deployment.
    docker create --name x-sage-seed "$IMAGE" >/dev/null
    docker cp x-sage-seed:/app/kb/. "$KB_DIR"/ 2>/dev/null || true
    docker rm -f x-sage-seed >/dev/null 2>&1 || true
    echo "    seeded $(find "$KB_DIR" -type f | wc -l) files"
  else
    echo "    WARNING: could not pull $IMAGE; the volume will be empty and the" >&2
    echo "    server will fail to start until it is seeded." >&2
  fi
fi

echo "==> installing .env"
if [ -f "$DEPLOY_DIR/.env" ]; then
  echo "    .env exists, leaving it alone"
else
  # HOST=0.0.0.0 makes the server refuse to start without a token; this is not
  # optional, and its absence shows up as a restart loop rather than an error.
  TOKEN=$(openssl rand -hex 24)
  cat > "$DEPLOY_DIR/.env" <<ENVFILE
IMAGE_TAG=latest
HOST_PORT=3001
X_SAGE_WRITE_TOKEN=$TOKEN
ENVFILE
  chmod 640 "$DEPLOY_DIR/.env"
  echo "    generated .env with a random write token"
fi

chown -R "$OWNER:$GROUP" "$DEPLOY_DIR"
chmod 664 "$DEPLOY_DIR/docker-compose.yml"
chmod 600 "$DEPLOY_DIR/.env"

echo
echo "==> done"
ls -la "$DEPLOY_DIR"
echo
echo "next:"
echo "  1. verify image visibility:  docker pull ghcr.io/xartifact/x-sage:latest"
echo "  2. first deploy:             cd $DEPLOY_DIR && docker compose up -d"
echo "  3. verify the volume:        docker inspect x-sage --format '{{range .Mounts}}{{.Source}} -> {{.Destination}}{{println}}{{end}}'"
echo
echo "write token (for clients that mutate the KB):"
grep '^X_SAGE_WRITE_TOKEN=' "$DEPLOY_DIR/.env" | cut -d= -f2 | sed 's/^/  /'
