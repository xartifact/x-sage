#!/usr/bin/env bash
# 每个门都必须会失败，失败必须向上传播。
#
# 本地与 CI 复用同一脚本（`bun run ci`），避免「本地过、CI 挂」。
# x-sage 的测试不依赖数据库或网络，也不使用 mock.module，因此无需 --isolate。
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

echo "==> typecheck (5 packages)"
bun run typecheck

echo "==> CI contract tests"
bun test scripts/ci-contract.test.ts

echo "==> tests"
bun test

echo "==> KB and graph validation"
# 同时检查 schema、图完整性与 graph.json 是否落后于概念文件。
# CLI 是纯 REST 客户端，仓库里没有常驻服务。这里直接用 server 的 app 在进程内
# 处理一次请求：与 HTTP 走同一条 dispatcher，覆盖同一份校验逻辑。
bun -e '
  const { createApp } = await import("./apps/server/src/app.ts");
  const app = createApp({ kbRoot: "./kb/default" });
  const res = await app.request("/api/validate?graph=true&format=text");
  process.stdout.write(await res.text());
  if (res.status >= 400) process.exit(1);
'

echo "==> all gates passed"
