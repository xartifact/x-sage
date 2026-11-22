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

# ---- sage-db（Rust）：P0 语义层门禁 ----
# 与 TS 部分互不依赖，因此不放进 `bun test` 重复跑；但它是**必过门**：
# 缺 Cargo.toml 或缺 toolchain 是本仓库的错误，不是「跳过」。toolchain 由
# sage-db/rust-toolchain.toml 固定。
echo "==> sage-db: generated protocol matches its authority"
bun tools/generate-sage-db-protocol.ts --check

echo "==> sage-db: workspace present"
if [ ! -f sage-db/Cargo.toml ]; then
  echo "✗ sage-db/Cargo.toml is missing; the Rust gate is mandatory" >&2
  exit 1
fi
if [ ! -f sage-db/rust-toolchain.toml ]; then
  echo "✗ sage-db/rust-toolchain.toml is missing; the toolchain pin is mandatory" >&2
  exit 1
fi

echo "==> sage-db: toolchain pin"
# rustup 会按 rust-toolchain.toml 选择工具链；这里断言它确实生效，
# 否则「我在本机能过」可能只是用了另一个 rustc。
PINNED="$(sed -n 's/^channel = "\(.*\)"$/\1/p' sage-db/rust-toolchain.toml)"
if [ -z "$PINNED" ]; then
  echo "✗ cannot read the pinned channel from sage-db/rust-toolchain.toml" >&2
  exit 1
fi
ACTUAL="$(cd sage-db && rustc --version | awk '{print $2}')"
if [ "$PINNED" != "$ACTUAL" ]; then
  echo "✗ rustc $ACTUAL does not match pinned $PINNED (is rustup managing this toolchain?)" >&2
  exit 1
fi

echo "==> sage-db: fmt"
( cd sage-db && cargo fmt --all -- --check )

echo "==> sage-db: clippy (-D warnings)"
( cd sage-db && cargo clippy --workspace --all-targets --locked -- -D warnings )

echo "==> sage-db: tests"
( cd sage-db && cargo test --workspace --locked )

echo "==> sage-db: dependency direction"
# 两个求值器必须独立：reference 只能依赖 types，infer 只能把 reference 作为
# dev-dependency。这是「两套算法」的结构保证，不是风格问题。
#
# 关键细节：`if pipeline` 里的失败不会被 `set -e` 传播，旧写法会在 cargo tree
# 自身失败时静默通过。这里先把输出落盘并显式检查 tree 的退出码，再检查内容。
check_no_edge() {
  local crate="$1" forbidden="$2" edges="$3"
  local out
  out="$(mktemp)"
  if ! ( cd sage-db && cargo tree -p "$crate" --edges "$edges" --prefix none ) >"$out" 2>&1; then
    echo "✗ cargo tree failed for $crate; cannot verify dependency direction:" >&2
    cat "$out" >&2
    rm -f "$out"
    exit 1
  fi
  if grep -qE "^${forbidden} v" "$out"; then
    echo "✗ $crate must not depend on $forbidden ($edges edges)" >&2
    cat "$out" >&2
    rm -f "$out"
    exit 1
  fi
  rm -f "$out"
}

# reference 不得依赖 infer（任何边）。
check_no_edge sage-db-reference sage-db-infer normal,build,dev
# infer 不得在生产/构建边依赖 reference（dev-only 是允许的）。
check_no_edge sage-db-infer sage-db-reference normal,build
# types 不得依赖任何 sage-db crate。
check_no_edge sage-db-types sage-db-infer normal,build,dev
check_no_edge sage-db-types sage-db-reference normal,build,dev

# 存储层方向（P1）：store 只在 {types, format} 之上再允许 cap-std（安全的 fd 相对
# 目录 API）；绝不允许 infer（存储不做推理），也绝不允许 store-tests（独立 oracle
# 不能被生产代码反向依赖，含 dev 边）。
check_no_edge sage-db-store sage-db-infer normal,build,dev
check_no_edge sage-db-store sage-db-store-tests normal,build,dev

# store 的直接依赖必须等于允许清单，而不是"恰好通过当前检查"。
# 用 --depth 1 --prefix none 只取直接依赖，再与清单逐项比较；任何新增依赖都会失败，
# 迫使它先经过显式批准而不是靠评审注意力发现。
check_direct_allowlist() {
  local crate="$1"; shift
  local out
  out="$(mktemp)"
  if ! ( cd sage-db && cargo tree -p "$crate" --edges normal --depth 1 --prefix none ) >"$out" 2>&1; then
    echo "✗ cargo tree failed for $crate; cannot verify direct dependencies:" >&2
    cat "$out" >&2
    rm -f "$out"
    exit 1
  fi
  # 第一行是 crate 自身，其余每行是一个直接依赖名。
  local actual
  actual="$(tail -n +2 "$out" | sed -E 's/ v.*//' | sort -u | tr '\n' ' ')"
  local expected
  expected="$(printf '%s\n' "$@" | sort -u | tr '\n' ' ')"
  if [ "$actual" != "$expected" ]; then
    echo "✗ $crate direct dependencies changed" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    rm -f "$out"
    exit 1
  fi
  rm -f "$out"
}

# `triomphe` is in the store's allowlist deliberately: it provides a safe, stable
# fallible `Arc::try_new` on the pinned toolchain (std's is `allocator_api`, unstable), and the
# published snapshot needs a construction that can refuse rather than abort. Exact pin,
# `default-features = false`, so it brings no serde or deref-trait transitives.
check_direct_allowlist sage-db-store sage-db-types sage-db-format cap-std triomphe
# The kernel reads complete after-images through Store, without depending on
# wire codecs, the harness, or a second ledger.
check_direct_allowlist sage-db-kernel sage-db-types sage-db-store sage-db-infer
# P4 credential slice uses mature hash/constant-time primitives and canonical Grants.
# Transport, token issuance and dependency additions require a separate bounded change.
check_direct_allowlist sage-db-server sage-db-types sage-db-kernel sage-db-store sage-db-protocol sha2 subtle httparse serde serde_json sage-db-query regex-syntax chrono
check_no_edge sage-db-server sage-db-store-tests normal,build,dev
# Protocol uses canonical identities/ledger and mature serialization generators only.
check_direct_allowlist sage-db-protocol sage-db-types prost pbjson pbjson-types serde serde_json
check_no_edge sage-db-protocol sage-db-store normal,build,dev
check_no_edge sage-db-protocol sage-db-kernel normal,build,dev
# Query consumes the read-only seam and pure optimized executor; Store/Kernel only in dev tests.
check_direct_allowlist sage-db-query sage-db-types sage-db-infer
check_no_edge sage-db-query sage-db-store normal,build
check_no_edge sage-db-query sage-db-kernel normal,build
check_no_edge sage-db-query sage-db-format normal,build
check_no_edge sage-db-query sage-db-reference normal,build
# Administrative import tooling consumes canonical production seams; none may depend back.
check_direct_allowlist sage-db-migrate sage-db-types sage-db-format sage-db-store sage-db-kernel serde serde_json
for canonical in sage-db-types sage-db-format sage-db-store sage-db-kernel sage-db-infer sage-db-reference sage-db-query sage-db-protocol sage-db-server; do
  check_no_edge "$canonical" sage-db-migrate normal,build,dev
done
check_no_edge sage-db-format sage-db-infer normal,build,dev
check_no_edge sage-db-format sage-db-store normal,build,dev

echo "==> all gates passed"
