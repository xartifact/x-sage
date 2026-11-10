# 部署

x-sage 是「文件即数据库」的单进程应用：容器内同时提供 API 与静态前端，不依赖数据库、
缓存或任何外部服务。KB 数据烘焙在镜像里（`/app/kb`）。

## 本地

```bash
cp .env.example .env      # 按需改 HOST_PORT / IMAGE_TAG
docker compose up -d --build
curl localhost:3001/api/health
```

打开 <http://localhost:3001>。

客户端（CLI）不含任何 KB 数据，指向容器即可：

```bash
x-sage --server http://localhost:3001 activate decision
```

## 架构形态

标准 C/S + B/S：服务端是唯一读得到知识库的组件，CLI 与浏览器都是它的 REST 客户端。
生产环境一个进程同时提供 `/api/*` 与静态前端。

## 生产（x99）

CI 在 push 到 `main` / `master` 时构建镜像、推送到 ghcr，然后在 self-hosted runner 上部署。

服务器上需要一次性准备部署目录（属主是 runner 用户 `github-actions`）：

```bash
# 在 x99 上
sudo mkdir -p /opt/xartifact/x-sage && sudo chown github-actions:github-actions /opt/xartifact/x-sage
# 放置只含 compose 的项目文件（CI 只做 pull + up，不构建）
```

`/opt/xartifact/x-sage/docker-compose.yml` 必须与仓库版本一致，关键是 image 可插值：

```yaml
services:
  x-sage:
    image: ghcr.io/xartifact/x-sage:${IMAGE_TAG:-latest}
    container_name: x-sage
    restart: unless-stopped
    environment:
      NODE_ENV: production
      PORT: 3000
      HOST: 0.0.0.0
      X_SAGE_KB: /app/kb/default
    ports:
      - '3001:3000'
    healthcheck:
      test: ['CMD-SHELL', 'bun -e "fetch(\"http://localhost:3000/api/health\").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"']
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 20s
```

硬编码标签会让 `docker pull alpha` 成功却仍启动旧镜像，且不报错。

### 端口

x99 上 3000 / 3200 / 5005 / 8787 已被其他产品占用，x-sage 使用 **3001**。

### 版本断言

`/api/health` 返回 `version` / `commitHash` / `buildRef`：

```json
{"ok":true,"kb":"/app/kb/default","version":"0.1.0","commitHash":"abc1234","buildRef":"main"}
```

部署流程会把它与本次推送的 `GITHUB_SHA::7` / `GITHUB_REF_NAME` 比对。200 只能证明容器活着，
无法区分「新容器」与「旧容器还在跑」——这正是断言存在的原因。

这三个值跨四跳传递，改任何一处的名字都会静默退化为 `dev`/`unknown`：

```
workflow build-args → Dockerfile ARG (base 阶段) → Dockerfile ENV → process.env
```

`scripts/ci-contract.test.ts` 断言了这条链路和 compose 不得覆盖它。

## 镜像构建的两个硬约束

**1. esbuild 在 QEMU 下确定性崩溃。** 实测在 arm64 主机上交叉构建 `linux/amd64`，
`vite build` 必崩（`panic: nil pointer dereference` / `The service was stopped`），反向亦然。
因此 CI 先在原生 runner 上构建 SPA 并作为 artifact 传递，镜像构建时用
`PREBUILT_VIZ=true` 复用，不走 buildx 内的 `vite build`。SPA 产物是纯 JS/CSS，跨架构一致。

**2. 不能裁剪 workspace 的 node_modules 布局。** bun 把第三方包放在
`/app/node_modules/.bun/`，各包的 `node_modules/<pkg>` 以相对软链指过去。只拷根目录会让
`apps/server/node_modules/hono` 变成悬空链接，容器启动即报 `Cannot find package`。

## 故障排查

```bash
docker compose logs --tail=50 x-sage
docker inspect --format '{{.State.Health.Status}}' x-sage
docker exec x-sage bun -e "fetch('http://localhost:3000/api/health').then(r=>r.json()).then(console.log)"
```

| 现象 | 原因 |
|---|---|
| `Cannot find package 'hono'` | 镜像裁剪了 `apps/*/node_modules` |
| health 一直 `starting` | 端口不是容器内的 3000，或 `X_SAGE_KB` 指向不存在的路径 |
| 版本显示 `dev`/`unknown` | build-args 与 Dockerfile ARG/ENV 名字不一致，或 compose 覆盖了它们 |
| 部署后仍是旧版本 | compose 的 image 标签硬编码，`docker pull` 拉了但没被使用 |
| 部署日志出现 `open /opt/xartifact/x-sage/.env: permission denied` | `.env` 是 `600 binzhan`，runner 用户 `github-actions` 读不到。必须是 `640` 且属组 `github-actions` |
| 页面空白且 `/api/viz` 报错 | KB 未打进镜像：`COPY --from=builder /app/kb ./kb` 缺失 |

### 概念数量对不上时

先看服务端，再看前端。`/api/viz` 的 `meta` 给出真值：

```bash
curl -s https://x-sage.ts.xartifact.dev/api/viz | jq '.meta'
# {"conceptCount":41,...,"layerCounts":{"L0":4,"L1":5,"L2":11,"L3":21}}
```

若 `conceptCount` 正确而页面只显示少量节点，问题在筛选：可视化默认按 `LAYERS`
渲染全部层级，筛选面板每层都显示对应数量。历史上默认只勾选 L0/L1，导致新概念
（几乎都落在 L2/L3）看起来从未上线。
