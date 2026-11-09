#!/usr/bin/env bash
# x99 上的 x-sage 部署目录初始化。需要在服务器上以 root 执行一次。
#
#   scp scripts/bootstrap-x99.sh tailscale.x99-arch-server.local:/tmp/
#   ssh tailscale.x99-arch-server.local 'sudo bash /tmp/bootstrap-x99.sh'
#
# 做四件事：
#   1. 把 /opt/xartifact/x-sage 交给 binzhan 属主、github-actions 属组，
#      并设 setgid，使双方都能读写（binzhan 运维，runner 部署）
#   2. 安装 docker-compose.yml（内嵌，服务器上没有仓库副本）
#   3. 若 .env 不存在，生成一个含随机 write token 的
#   4. 打印后续步骤
set -euo pipefail

DEPLOY_DIR=/opt/xartifact/x-sage
OWNER=binzhan
GROUP=github-actions

if [ "$(id -u)" -ne 0 ]; then
  echo "must run as root: sudo bash $0" >&2
  exit 1
fi

id "$OWNER" >/dev/null 2>&1 || { echo "user $OWNER not found" >&2; exit 1; }
getent group "$GROUP" >/dev/null 2>&1 || { echo "group $GROUP not found" >&2; exit 1; }

echo "==> preparing $DEPLOY_DIR"
mkdir -p "$DEPLOY_DIR"

# setgid (2xxx) 让在目录内新建的文件自动继承属组，否则 binzhan 写的文件
# runner 读不到，反之亦然。这是双方协作的关键位。
chown "$OWNER:$GROUP" "$DEPLOY_DIR"
chmod 2775 "$DEPLOY_DIR"

echo "==> installing docker-compose.yml"
cat > "$DEPLOY_DIR/docker-compose.yml" <<'COMPOSE'
# x-sage 生产部署（单进程：API + 静态前端，同端口）
#
# 由 CI 更新：deploy job 只做 docker pull + docker compose up -d，
# 不会重新生成本文件。要改端口/挂载，改这里后重跑一次部署。
services:
  x-sage:
    image: ghcr.io/xartifact/x-sage:${IMAGE_TAG:-latest}
    container_name: x-sage
    restart: always
    env_file: .env
    environment:
      NODE_ENV: production
      PORT: 3000
      HOST: 0.0.0.0
      X_SAGE_KB: /app/kb/default
    ports:
      - '${HOST_PORT:-3001}:3000'
    healthcheck:
      # 容器内探测：始终是 3000，不受宿主映射影响
      test:
        [
          'CMD-SHELL',
          'bun -e "fetch(\"http://localhost:3000/api/health\").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"',
        ]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 20s
COMPOSE

echo "==> installing .env"
if [ -f "$DEPLOY_DIR/.env" ]; then
  echo "    .env exists, leaving it alone"
else
  # HOST=0.0.0.0 时 server 会拒绝在无 token 情况下启动（bind 非回环必须鉴权）。
  # 这条不是可选配置，缺了容器直接重启循环。
  TOKEN=$(openssl rand -hex 24)
  cat > "$DEPLOY_DIR/.env" <<ENVFILE
IMAGE_TAG=latest
HOST_PORT=3001
X_SAGE_WRITE_TOKEN=$TOKEN
ENVFILE
  chmod 640 "$DEPLOY_DIR/.env"
  echo "    generated .env with a random write token"
fi

chown "$OWNER:$GROUP" "$DEPLOY_DIR/docker-compose.yml" "$DEPLOY_DIR/.env"
chmod 664 "$DEPLOY_DIR/docker-compose.yml"

echo
echo "==> done"
ls -la "$DEPLOY_DIR"
echo
echo "next:"
echo "  1. verify image visibility:  docker pull ghcr.io/xartifact/x-sage:latest"
echo "  2. first deploy:             cd $DEPLOY_DIR && docker compose up -d"
echo "  3. re-run CI:                gh workflow run docker-build.yml --ref main"
echo
echo "write token (for clients that mutate the KB):"
grep '^X_SAGE_WRITE_TOKEN=' "$DEPLOY_DIR/.env" | cut -d= -f2 | sed 's/^/  /'
