# 部署

生产由 Bun 容器提供 REST 和静态前端，独立的 Rust sage-db 服务负责知识库存储。
本地开发镜像包含 Markdown 示例库（`/app/kb`）。

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
生产的 Bun 网关提供 `/api/*` 与静态前端，通过私有回环地址访问 Rust 数据库。

## 生产（x99）

生产由 x99 上的 `x-sage-bun-release.timer` 跟踪 GitHub `main`。旧版 GitHub
Actions 部署工作流保持停用。它使用的旧 Compose 路径已经退出生产。

发布脚本以生产运维用户的身份运行。从 GitHub 获取 `main` 的精确提交，运行仓库的
`scripts/ci.sh`，以短提交号构建 Linux 镜像，再从 `127.0.0.1:3301` 验证候选网关。
候选验证使用独立的投影目录。发布前后核对当前 Rust 数据库的读写就绪状态及其源摘要。
随后在现有生产 Compose 项目替换 Bun 服务，并核对公网首页、图谱与健康接口。
发布验证失败时恢复上一版 Bun 镜像与配置。Rust 进程和数据库目录继续由原有运维流程管理。

如果新提交的 `sage-db/` Git tree 改变，网关自动发布会停止在构建之前。
按[原生运行环境操作说明](../deploy/native-candidate/OPERATIONS.md)验收新的 Rust 发行物，
完成数据库版本验证后，再更新运维配置中的 `SAGE_RUST_TREE`。

在 x99 上把 [发布脚本](../deploy/native-candidate/release-bun.sh) 安装到
`~/.local/lib/x-sage-release/release-bun.sh`，并把配套的
[`.service`](../deploy/native-candidate/x-sage-bun-release.service) 和
[`.timer`](../deploy/native-candidate/x-sage-bun-release.timer) 放到 `~/.config/systemd/user/`。
在权限为 `0600` 的 `~/.config/x-sage/release.env` 中设置实际生产 Compose 目录
`SAGE_PROD_SLOT`、已验收的 `SAGE_RUST_TREE`（用对应提交的
`git rev-parse <commit>:sage-db` 获取）和[固定的协议生成工具](design/protocol-codegen.md)
`SAGE_DB_PROTOC`、`SAGE_DB_TS_PROTO_PLUGIN`。安装后执行：

```bash
systemctl --user daemon-reload
systemctl --user enable --now x-sage-bun-release.timer
systemctl --user start x-sage-bun-release.service
journalctl --user -u x-sage-bun-release.service
```

### 端口

生产网关监听 `127.0.0.1:3001`，候选网关使用 `127.0.0.1:3301`，
原生数据库服务仅监听 `127.0.0.1:3901`。

### 版本断言

`/api/health` 返回 `version` / `commitHash` / `buildRef`。网页顶栏显示运行版本与七位提交号。

```json
{"ok":true,"kb":"/app/kb/default","version":"0.1.0","commitHash":"abc1234","buildRef":"main"}
```

生产部署将其与当次构建的包版本、`main` 提交前七位和分支名比对。

这三个值跨四跳传递，改任何一处的名字都会静默退化为 `dev`/`unknown`：

```
build-args → Dockerfile ARG (base 阶段) → Dockerfile ENV → process.env
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
journalctl --user -u x-sage-bun-release.service -n 50
docker logs --tail=50 x-sage-native-production-bun-1
docker inspect --format '{{.State.Health.Status}}' x-sage-native-production-bun-1
curl -fsS http://127.0.0.1:3001/api/health
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
