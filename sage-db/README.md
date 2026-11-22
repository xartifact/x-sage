# sage-db

x-sage 的自研知识图数据库（Rust）。设计契约见
[`system.md`](../docs/design/system.md)，当前切换条件见
[`sage-db-readiness.md`](../docs/design/sage-db-readiness.md)。

## 当前状态

| 范围 | 状态 |
|---|---|
| P0 共享类型、优化求值器、独立参考求值器 | 已实现并验收 |
| P1 格式、提交、完整回放、历史读取、故障/进程恢复 harness | 约定软件范围已验收；格式仍为 draft major 0 |
| P2 业务内核 | 迭代中：Claim/现有 Term 文本及 RelationAssert 提案、规范图校验与原子审阅、SessionCheckpoint/Frontier CAS 生命周期、当前正文读取及全类型物理扫描 |
| P2 完整写入生命周期与原子审阅 | 未完成 |
| P3 查询 | SnapshotView、完整事实查询、Requires 前置顺序、作者断言最短导航与授权 catalog 已实现；传输、搜索和缓存未完成 |
| P4 服务 | 凭证验证、类型协议、八条命令适配和响应组装已实现；Rust HTTP/RPC 服务未完成 |
| P5 迁移切换 | 归档、原生准备与暂存执行器已实现；私有模型/Btrfs 导入与重启验证通过；正式停写、恢复工具和生产切换未完成 |

**尚不能接管真实知识库。** 当前应用仍使用 Markdown / SQLite 服务路径，
尚无可用的 sage-db 服务或完整运维 CLI。已具备针对 x99 指定内核/Btrfs 的
checked `for_root` 平台策略和真实 Store/进程测试；它不构成通用 Linux 或设备掉电保证。
模型故障恢复与真实进程 SIGKILL 测试不构成设备掉电持久性证明。
不得通过覆盖 capability 绕过该限制。

## Workspace

- `types`：共享契约，纯类型；无第三方依赖、无 I/O。
- `infer`：优化图校验、推理与有界证明；生产仅依赖 types。
- `reference`：独立参考求值器，仅依赖 types。
- `format`：显式字节编解码、记录、摘要与格式上限。
- `store`：文件 seam、共享资源账本、原子发布、回放、历史读取及单一带标记视图。
- `kernel`：业务准入、文本与 RelationAssert 提案写入，依赖 types / store / infer。
  `submit_claim_edit` 持久保存 pending 提案；`review_claim_edit` 从实际 payload
  推导依赖，把内容与提案最终状态、幂等 outcome 放进同一事务。
  RelationAssert 审阅使用已提交规则校验完整候选图，并原子写入断言、提案和 outcome。
  Term 创建/退役及其他记录生命周期尚未完成；规范 V7 生命周期约束仍由 kernel 负责，
  infer 的 V1–V6 图校验不替代 V7。
- `query`：完整事实求值、有证据的点查询、Requires 顺序与作者断言导航，生产仅依赖 types / infer。
- `protocol`：单一 proto 权威生成 Rust/TS DTO，使用 canonical SharedLedger 做语法准入。
- `server`：凭证、八条业务命令适配与规范响应组装；尚无运行时监听器。
- `migrate`：全量原生准备、自包含源包与受验证令牌保护的暂存执行器；生产切换驱动仍未完成。
- `store-tests`：独立模型、故障 oracle 与真实公开 API 的集成测试，永不被生产依赖。

只读 `assess_approval` 仍仅是基线评估，不能替代完整写入。当前 Claim 文本写入
已执行幂等优先、实际 payload 依赖推导和原子决策；它不修改图结构，也不代表
完整 P2 已验收。操作字节与范围见
[`kernel-operations.md`](../docs/design/kernel-operations.md)。
各阶段的身份与证据边界见
[`sage-db-baseline.md`](../docs/design/sage-db-baseline.md)。

## 验证

工具链固定为 `rust-toolchain.toml` 的 Rust 1.98.1，不修改全局 rustup。

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

根目录 `scripts/ci.sh` 包含这些质量门、工具链校验与 crate 依赖方向检查，
以及当前 TypeScript 应用和 CI 契约的回归。`cargo tree` 失败会使依赖门失败。


The current P2 auxiliary slice also provides SessionCheckpoint create/revise/read:
WriteAux and Read are separate capabilities, updates use exact object revisions,
content is hash-verified behind a retained owner, and retries retain original outcomes.
A budgeted identified-record visitor supports later graph/query assembly. These APIs
are not a complete query/server/migration implementation or permission to activate an
incomplete import; see the real41-source warning in the readiness document.

Prior bounded adoption:581 source files, capture `final-bounded-text-frontier-20261119T191342Z`
(MANIFEST b6fe81bd; evidence ec203ed9/141). Local exact CI Rust1394/0 across95 summaries,
Bun473/0; contract52/0 overlaps Bun; docs/toolsTS0. Same581 on x99: Rust1394/0,
platform6/0 and real-file5/0, before/after hashes581/581 and lint/docs0. The15 adopted
paths do not activate a KB. Existing-Term atom/body command policy is200 UTF-16 units;
format800 bytes is unchanged. Complete graph lifecycles, query/service and native
import remain open. These navigation bytes were edited after the immutable capture.

Prior source adoption binds602 payloads at
`final-bounded-protocol-query-command-20261119T202537Z` (manifest181ae950; evidence16c23fb9/143).
Local exact CI: Rust1486/0 over103 summaries, Bun475/0, contract54/0 overlaps Bun,
strict docs/toolsTS0. Same602 on x99 passed Rust1486/0, policy6/0 and real-file5/0,
lint/fmt/docs0, source checks before/after. Protocol code generation requires the pinned
compiler/plugin setup in [`protocol-codegen.md`](../docs/design/protocol-codegen.md).
There is still no HTTP/RPC listener or complete native import executor; source adoption is not
phase acceptance or activation. Only these three navigation documents changed after capture.

## Prior bounded continuation: native preparation and response assembly (2026-11-20)

Root adopted16 paths (7 existing +9 new;595 outside existing inputs unchanged)
from `final-bounded-native-prepare-response-20261119T213824Z`: MANIFEST7916a989/611,
EVIDENCEa9f51307/177, CAPTURE8d4678b1. Local exact CI passed Rust1554/0 over108
summaries (12 explicitly ignored), Bun476/0 and contract55/0 (overlaps Bun), strict
docs/toolsTS0. The same611 source verified before/after on x99 and passed Rust1554/0,
policy6/0, real-file/process5/0 and clippy/fmt/strictdocs0.

Adopted: additive profile-aware Genesis/bootstrap (the old entry remains FormalV1);
canonical response assembly preserving recorded versions, original decision status,
Unknown request identity and real failure classes; self-contained native import
preparation with all73 source files in its required-reference package. Preparation
constructs262 family rows /264 whole-state rows and13 historical proposal restatements.
The real73-file dry-prepare probe passed separately from37 self-contained fixture tests
plus1 explicitly ignored operator probe. This is preparation, not committed import.

Native staged execution remains held for verification and resource findings. No
listener, REST compatibility, full query service, phase acceptance or cutover follows.
Real x99 Markdown41 is still authoritative. No production KB/service/config write,
stage/commit/push/deploy occurred; Git index and36 local KB files remain unchanged.
This navigation edit changes only three documentation payloads after capture.

## Prior bounded continuation: native execution, catalog and Requires (2026-11-20)

Root adopted22 paths (11 existing +11 new;600 outside existing payloads unchanged)
from `final-bounded-native-executor-catalog-requires-20261119T230820Z`:
MANIFEST1007aaea/622, EVIDENCEa2ca0d5b/321, CAPTURE91933ee4. Exact local CI passed
Rust1623/0 over112 summaries (12 explicitly ignored), Bun476/0 and contract55/0
(overlaps Bun), strict docs/toolsTS0. The same622 source verified before/after on x99
and passed Rust1623/0, policy6/0, actual-file/process5/0, clippy/fmt/strictdocs0.
All73 files in the five OS test artifact directories matched remote and local hashes.

Adopted: guarded staged native execution with complete current/history value checks,
blob and watermark verification; Read-authorized borrowed catalog with import isolation
and exact slug resolution; one-meter query continuation and complete unfiltered Requires
with dependency ordering, full answer/proofs and total work statistics. These components
have independent scoped source reviews; no phase is declared complete.

A separate591-source private rehearsal used byte-identical RootR7 executor production:
real73-file input,264 whole rows,90 blobs,5 watermarks, two reopens, verification then
completion. Model and actual x99 Btrfs invocations each passed one operator test; the
completed Snapshot has41 terms/173 assertions under LegacyV1. The final receipts bind
actual bytes; earlier construction receipts are explicitly superseded outside frozen
reports. This is not a final production source write-freeze or activation.

Navigation and fallible JSON ingress remain separate held candidates. No Rust listener,
complete REST compatibility, backup/restore protocol, production KB/service/config write,
stage/commit/push/deploy or cutover occurred. Git index and36 local KB files were unchanged.
This navigation edit changes only three documentation payloads after the immutable capture.


## Current bounded continuation: relation proposals (2026-11-20)

Capture `final-bounded-relation-proposals-20261120T014228Z` binds MANIFEST99ec71a9/628,
EVIDENCEf3e63cac/802 and CAPTURE89b314aa. Root adopted9 paths (6 existing +3 new);
619 outside existing payloads stayed unchanged. Exact local and x99 Rust runs each
passed1675/0 over114 summaries,12 ignored; local Bun476/0 and contract55/0 overlap.
Strict docs/toolsTS0; x99 policy6/0, actual-file/process5/0, lint/fmt/docs0 and73
artifact hash comparisons passed. Initial log pull255 was recovered by a separate,
hash-verified transfer of12 unchanged logs; tests were not rerun for that transfer.

RelationAssert op10/11 provide persisted authored-edge proposals and atomic approval
under the committed profile. Empty declared attribution and the canonical800-byte
ceiling are preserved separately from reviewer identity. The dependency contract now
includes the permitted kernel->infer edge. This is bounded P2 progress. JSON ingress,
backup/restore and the Rust runtime remain isolated candidates. The read-only x99
recheck at2026-11-20T01:37:46Z found73 files/335915 bytes with zero drift and no
knowledge.db; production Markdown41 remains authoritative. No phase acceptance or
cutover occurred. Only three navigation documents change after this capture.

## Prior bounded continuation: authored navigation (2026-11-20)

Root adopted4 paths (1 existing +3 new;621 outside existing payloads unchanged)
from `final-bounded-authored-navigation-20261119T234756Z`: MANIFESTf1200d5e/625,
EVIDENCEbbf25419/959, CAPTURE1bee02ed. Exact local CI passed Rust1640/0 over113
summaries (12 explicitly ignored), Bun476/0, contract55/0 (overlaps Bun), strict
docs/toolsTS0. The same625 source verified before/after on x99 and passed Rust1640/0,
policy6/0, actual-file/process5/0 and clippy/fmt/strictdocs0. All73 OS artifact files
matched remote and local hashes.

Adopted navigation returns the shortest undirected path over active authored assertions,
with original assertion identity/revision/direction and deterministic canonical ties. It is
not a semantic Fact or an inferred relationship. One canonical meter accounts actual
work rows, slot bytes, comparisons and attended reconstruction; budget/stop/allocation/
extent/invariant errors remain distinct. Requires exports and its scoped behavior remain
unchanged through the merge. No JSON ingress or relation-assert candidate is adopted.

A separate622-source private x99 Btrfs operator rehearsal imported the real73-file capture,
reopened, verified all264 whole rows/90 blobs/5 marks, completed, then reopened again.
All287 term/relation queries agreed with the independent reference executor;356 fact rows
had replayable proofs. All41 Requires queries matched reference rows and dependency order
(259 ordered terms in aggregate). This is an operator probe, not old Bun byte parity,
RSS/allocator evidence, full private-overlay CI, power-loss or service availability.

A read-only source recheck at2026-11-19T23:45:52Z still found73 files/335915 bytes and zero
changes versus the preparation capture; the old service remains Markdown41. This is not
the final write-stop freeze. No production KB/service/config write, stage/commit/push/deploy,
phase acceptance or cutover occurred. Index and36 local KB files were unchanged. This
navigation edit changes only three documentation payloads after the immutable capture.
