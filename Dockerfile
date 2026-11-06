# syntax=docker/dockerfile:1

# x-sage 是「文件即数据库」的应用：镜像内自带一个 KB（默认 kb/default），
# 运行时不需要数据库、缓存或任何外部服务。
FROM oven/bun:1 AS base
WORKDIR /app

# 版本信息在 base 固化：`runner` 的 ENV 决定容器内 /api/health 报告的
# version / commitHash / buildRef，部署流程拿它与本次推送的 commit 比对，
# 否则「旧容器还在跑」会被健康检查的 200 掩盖。
# builder 与 runner 都继承 base，因此三处名字必须一致：
# workflow build-args → 这里的 ARG → 下面的 ENV → process.env。
ARG APP_VERSION=dev
ARG GIT_COMMIT_HASH=unknown
ARG BUILD_REF=unknown
ENV APP_VERSION=${APP_VERSION}
ENV GIT_COMMIT_HASH=${GIT_COMMIT_HASH}
ENV BUILD_REF=${BUILD_REF}

# ---- 构建 ----
FROM base AS builder

# SPA 产物是纯 JS/CSS，跨架构完全一致，因此在原生 runner 上构建一次即可。
# 若上下文里已有 apps/viz/dist（CI 会先原生构建），直接复用；否则就地构建。
# 就地构建在 QEMU 模拟下会确定性 SIGSEGV —— 见下方说明。
ARG PREBUILT_VIZ=false

# 先只拷清单，让依赖层在源码变更时仍然命中缓存。
COPY package.json bun.lock ./
COPY packages/core/package.json ./packages/core/
COPY packages/service/package.json ./packages/service/
COPY apps/cli/package.json ./apps/cli/
COPY apps/server/package.json ./apps/server/
COPY apps/viz/package.json ./apps/viz/

RUN bun install --frozen-lockfile --ignore-scripts

COPY . .

# 已知问题：esbuild 在 QEMU 模拟下确定性 SIGSEGV（nil pointer dereference /
# "The service was stopped"）。实测 linux/amd64 构建于 arm64 主机必崩，反之亦然。
# 因此多架构发布必须预构建 SPA（PREBUILT_VIZ=true），不能靠 buildx 交叉构建。
# prebuilt 产物放在 viz-dist/（.dockerignore 排除 apps/viz/dist，因为工作区里的
# 那份可能过期；CI 把它作为 artifact 下载到 viz-dist/）。
RUN if [ "$PREBUILT_VIZ" = "true" ]; then \
      test -f viz-dist/index.html || { echo "PREBUILT_VIZ=true but viz-dist/index.html is missing"; exit 1; }; \
      rm -rf apps/viz/dist && mkdir -p apps/viz && cp -r viz-dist apps/viz/dist; \
      echo "using prebuilt SPA from viz-dist/"; \
    else \
      test -f apps/viz/dist/index.html && echo "WARNING: using working-tree apps/viz/dist"; \
      cd apps/viz && bun run build; \
    fi

# ---- 仅生产依赖 ----
# 构建阶段需要 vite / typescript / @types 等 devDependencies，运行阶段不需要。
# 单独再装一次，让 runner 的 node_modules 只剩运行时真正加载的包
# （typescript 23MB、esbuild 9MB、bun-types 8.6MB 都不进最终镜像）。
FROM base AS prod-deps

COPY package.json bun.lock ./
COPY packages/core/package.json ./packages/core/
COPY packages/service/package.json ./packages/service/
COPY apps/cli/package.json ./apps/cli/
COPY apps/server/package.json ./apps/server/
COPY apps/viz/package.json ./apps/viz/

RUN bun install --frozen-lockfile --ignore-scripts --production

# ---- 生产运行 ----
FROM base AS runner

# ca-certificates 不是可选项：缺了它容器内任何 HTTPS 请求都会失败。
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

ENV NODE_ENV=production

# Workspace 依赖布局不能裁剪：bun 把第三方包放在 /app/node_modules/.bun/，
# 再由每个包的 node_modules/<pkg> 以相对软链指过去。只拷根目录会让
# `apps/server/node_modules/hono` 成为悬空链接，容器启动即报 Cannot find package。
COPY --from=prod-deps /app/node_modules ./node_modules
COPY --from=builder /app/package.json ./package.json
COPY --from=prod-deps /app/apps/server/node_modules ./apps/server/node_modules
COPY --from=prod-deps /app/apps/cli/node_modules ./apps/cli/node_modules
COPY --from=prod-deps /app/packages/core/node_modules ./packages/core/node_modules
COPY --from=prod-deps /app/packages/service/node_modules ./packages/service/node_modules

# 运行期需要 core / service / server 的源码，外加 cli：它不是给最终用户装的，
# 而是让运维能在容器内对同一个 KB 执行 `bun apps/cli/src/index.ts --kb /app/kb/default ...`
# （写入提案、重建 graph.json）。最终用户装的是编译好的单一客户端，不含 KB。
COPY --from=builder /app/packages/core/package.json ./packages/core/
COPY --from=builder /app/packages/core/src ./packages/core/src
COPY --from=builder /app/packages/service/package.json ./packages/service/
COPY --from=builder /app/packages/service/src ./packages/service/src
COPY --from=builder /app/apps/server/package.json ./apps/server/
COPY --from=builder /app/apps/server/src ./apps/server/src
COPY --from=builder /app/apps/cli/package.json ./apps/cli/
COPY --from=builder /app/apps/cli/src ./apps/cli/src
COPY --from=builder /app/apps/viz/dist ./apps/viz/dist

# KB 数据打进镜像。若希望部署后仍能改概念文件，把它挂成 volume 覆盖此路径。
COPY --from=builder /app/kb ./kb

ENV PORT=3000
ENV HOST=0.0.0.0
ENV X_SAGE_KB=/app/kb/default
# 不设 X_SAGE_SERVER：容器内 CLI 默认直接读本地 KB，避免绕一圈连自己。
ENV PATH="/app/node_modules/.bin:${PATH}"

EXPOSE 3000

# 容器内自检，与 compose/部署脚本使用同一个端口与路径。
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
  CMD bun -e "fetch('http://localhost:3000/api/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"

CMD ["bun", "apps/server/src/index.ts"]
