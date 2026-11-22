# 源迁移计划 v1（import plan v1）

> 状态：P5 **控制工件**生成器。不是导入器，不是持久化格式，不是切换授权。
> 实现：`tools/sage-db-import-plan.ts`；测试：`tools/sage-db-import-plan.test.ts`。
> 上游契约：[migration.md](migration.md) §1–§3、[data-model.md](data-model.md) §1–§2。

## 1. 它在链路中的位置

```
线上 KB ──(source-archive v1 冻结工具)──▶ 归档目录 ──(本工具)──▶ migration plan JSON ──▶ 后续导入器
                                          已验证、已限界        显式映射 + 未解决清单
```

输入**只能是**已验证的源归档，不能是线上服务，也不能直接读 Markdown 目录。归档由
`tools/sage-db-source-archive.ts` 产生并 `verifySourceArchive` 复核；本工具在读取任何
blob 之前先跑该校验，因此损坏的捕获会被拒绝，而不是被“部分规划”。

计划文件是**迁移控制工件**：

- 它陈述“如果导入，会写成什么”，不是记录本身；
- 它不分配最终 ID、不写目标库、不声称任何未解析字段的无损性；
- 它**不是**第二套持久化 schema。下游不得把计划当作记录内容的权威。

## 2. 三态状态机

状态分两个**独立轴**，v2 起明确区分：

| 轴 | 值 | 含义 |
|---|---|---|
| `source.extraction` | `complete` / `incomplete` | 源侧抽取是否全部成功（无 errors） |
| `status` | `complete` / `incomplete` | 抽取成功**且**没有 `unresolved` 目标要求 |
| `target_mapping` | 恒为 `not_ready` | 映射就绪另需人工审阅，本工具从不声明 |

v1 只有单一 `status`，且没有目标要求清单，于是“源里都有、目标字段缺失”的计划会显示为
`complete` 且零 unresolved——那是错误陈述。v2 把 `created_at`、`recorded_by` 等目标必需字段
一律列入 `unresolved` 并给出建议策略，因此真实语料的 `status` 是 `incomplete`，
即使 `source.extraction=complete`。

两个状态字段的判定：

没有 `ready`、`activate`、`success` 字段。就绪与切换是后续独立的人为决定，不是本工具的输出。

### 2.1 审计身份（v2 修正）

目标类型是 `CommonFields { recorded_by: Option<String>, actor_origin: ActorOrigin, reason, created_at }`。
v1 写 `actor: "unavailable"`，**类型与语义都错**：`recorded_by` 是“已验证 principal”，不是来源枚举。

v2 输出：

```json
"audit": { "recorded_by": null, "actor_origin": "LegacyDeclared", "reason": null, "created_at": "unavailable" }
```

源里声明的作者（`proposer`）放在独立的 `declared_author`，**绝不**提升为 `recorded_by`。

### 2.2 运行上下文（v2 修正）

v1 默认 `created_at = new Date()`，同一归档两次运行会得到不同计划，却仍声称确定性。
v2 要求显式 `--plan-context`（`label` / `created_at` / `purpose`），记录 `run.context_digest`；
缺上下文即 `E_USAGE`，绝不读时钟。

## 3. 确定性

两次对同一归档的运行产生**逐字节相同**的计划（测试断言 `canonicalJson` 相等）。

| 对象 | 顺序 |
|---|---|
| Term | 层目录顺序 `L0-axioms → L1-core → L2-support → L3-instances`，层内按路径字节序 |
| Term `target_id` | 上述顺序的 1-based 序号 |
| Assertion | 按 `source_id`（`slug:declaration_order`）字节序；`declaration_order` 保留原 `edges` 列表下标 |
| Assertion `target_id` | 按已排序顺序 1-based 编号 |
| Proposal / Checkpoint | 源路径字节序 |
| Frontier | 文档出现顺序：分节顺序，再节内行位置 |
| Attachment | `files.ndjson` 的归档顺序 |

计划文件的序列化使用 canonical JSON（键排序、无空白），所以 diff 有意义、哈希可复核。

## 4. 映射内容

### 4.1 Term

`slug / atom / layer / domain / confidence / source / triggers` 全部来自 frontmatter 的解析结果。

**正文是两个不同的东西，不能混用：**

| 字段 | 含义 |
|---|---|
| `body {offset,length,sha256}` | 源文件中 frontmatter 结束行之后的**精确字节**：前导空行、尾随换行、CRLF、Unicode 全部保留 |
| `file {bytes,sha256,blob}` | 整份源文件（含 frontmatter），记录级溯源 |
| `projection {body_sha256,trimmed:true}` | 服务层归一化投影（trim 后），**只作参考**，永不作为字节权威 |

v1 把 `hash(trim 后的 molecule)` 与整份文件 blob 并列，两者互相矛盾——摘要不描述该 blob 的任何
字节区间。v2 改为绑定 `offset + length + body_sha256` 到原文件 blob，可由源文件逐字节复现。

`missing_target_fields` 列出源无法提供、但目标必需的字段。

### 4.2 Assertion

每条 authored edge 一条映射：原始方向、关系、置信度、声明顺序、来源文件都在。
**不自动归约**：不合并对称边、不推导 `depends-on` 的逆向边、不补全缺失的反向断言。

`actor` / `reason` 恒为 `"unavailable"`：旧语料没有任何作者、理由或时间戳，
编造它们会污染审计轨迹。导入时 `recorded_by` 必须标 `legacy_declared`。

### 4.3 Proposal

每个 `proposals/` 文件都保留原始字节。状态取自文件自身：

- `approved` → 作为历史审阅记录；`reviewed_at` / `reviewer` 声明为 unavailable（文件里没有）。
- `example` → **模板，不是记录**。文件保留为附件、列在 `unavailable` 里，**不会**产生 Proposal 记录，
  也不占用记录 ID。因此计划里**没有** `template` 字段：一个恒为 `false` 的字段只会让读者去找
  一个不可能出现的 `template: true`。
  把示例当记录会凭空制造一次从未发生的审阅。
- 其他状态原样透传，不解释、不升级。

### 4.4 Checkpoint

`concept` / `session` 来自 JSON 字段，内容按字节 + 哈希绑定。概念不在本库时记 `UNKNOWN_CHECKPOINT_CONCEPT`
并进入 `unresolved`——不猜它属于哪个概念。

### 4.5 Frontier

**所有分节、所有表格**都读，不只是第一张表（`migration.md` 明确禁止“只保留第一张表”）。

既有的 `parseFrontierMarkdown` 只认第一张表且只取 4 列，因此本工具自带分节/分表解析，
逐行保留 `cells` 与原始 `raw` 行文本。缺少已知分节（未决概念 / 概念内张力 / L2→L3 待展开方向）
是一条错误，不是静默省略。

**身份必须唯一**：`source_id = <section>/<table>/<row>`，其中 table 在同一分节内递增。v1 用
`section#position`，position 在每张新表处重置，导致同分节两张表的行 id 相同。同名分节重复出现时，
第二次的 scope 记为 `<title>#2`。另有一个全局 `declaration_order`，按文档出现顺序。

### 4.6 Attachment

源树中**每一个**文件都在 `attachments` 里，包括 `AI.md`、`protocols/`、`kb.json`、`MANIFEST.md`、
`graph.json` 与 `.backup-*` 目录内容。解析投影与原始字节是两件事：前者可能被用到，后者永远保留。

## 5. 拒绝与未解析

| 代码 | 条件 |
|---|---|
| `DUPLICATE_PATH` | 归档里同一路径出现两次 |
| `DUPLICATE_ID` | 两个文件声明同一 concept slug |
| `SLUG_PATH_MISMATCH` | frontmatter `id` 与文件名不一致 |
| `UNKNOWN_CONCEPT_LAYER` | 概念文件不在四个已注册层目录内 |
| `INVALID_CONCEPT` | frontmatter 无法通过既有 schema |
| `UNKNOWN_EDGE_TARGET` | 边的 target 不在本源内 |
| `CROSS_KB_EDGE` | 边的 target 形如 `kb::concept`；结构化存储无法绑定跨库目标 |
| `UNKNOWN_CHECKPOINT_CONCEPT` | checkpoint 指向不存在的概念 |
| `MISSING_FRONTIER` / `MISSING_FRONTIER_SECTION` | 缺少 frontier 文件或已知分节 |
| `INVALID_PROPOSAL` / `INVALID_CHECKPOINT` | 头部或 JSON 结构不可读 |

`errors` 累积**全部**问题（不遇一条就退出），每条带 `source` 与 `index`。
`unresolved` 单独列出“目标字段已知需要、但源里没有”的项。

## 6. 边界

六个上限全部由调用方给出，CLI 无任何默认值：

| 选项 | 约束 |
|---|---|
| `--max-entries` | 归档文件数（也是校验器的 blob 目录项上限） |
| `--max-bytes` | 本工具从归档读取的源字节总量 |
| `--max-records` | 计划记录总数（见下方“一个开关，两种计数”） |
| `--max-archive-bytes` | 交给 `verifySourceArchive` 的归档字节上限，同时界定 manifest 和 plan-context JSON 文档 |
| `--max-line-bytes` | 交给校验器的单行上限（**不**用于 `archive.json`，它和 plan-context JSON 文档由 `--max-archive-bytes` 约束） |
| `--max-diagnostics` | `errors` + `unresolved` + `unavailable` 的累计条数上限 |

`maxRecords` 是**全局**映射记录上限：附件、Term、Assertion、Proposal、Checkpoint、Frontier 行
都算映射记录。附件、Term、Assertion、Proposal、Checkpoint 都是**在 push 之前**逐条扣减。

**一个开关，两种计数**：`--max-records` 同时喂给两个计数器，它们的单位不同，数字不可比：

| 计数器 | 数的是什么 | 拒绝时说明 |
|---|---|---|
| 校验器 `maxRecords` | **流记录**（`files.ndjson`/`rows.ndjson` 的行数） | 归档本身的性质 |
| 计划 `recordBudget` | **映射记录**（附件+Term+Assertion+Proposal+Checkpoint+Frontier 行） | 计划本身的性质 |

Frontier 是唯一的例外，且**明确不是**逐条扣减：它的行数先被 `remaining = maxRecords - 已用`
预界（解析器在超限前就拒绝，不会无界增长），数组建成后**一次性聚合扣减**
`recordBudget.spend(frontier.length)`。

关键点**不是**“frontier 是最后映射的族”——那个说法不成立。关键在于聚合扣减发生**在任何后续族的
第一次扣减之前**，于是后续族看到的是已经减少的余额，它自己的逐条 `spend` 会在同一个计数器上拒绝。
因此“解析器有界 + 聚合扣减紧随其后”才是实际保证；把 frontier 挪到中间也不会静默超限。

R2 只检查 `attachments.length` 本身，于是 73 个附件
加 262 条其他映射可以在上限 300 下产出 335 条——这个缺口在 R3 关闭。

辅助诊断（errors/unresolved/unavailable）不是映射记录，单独用 `--max-diagnostics` 限制，
避免“没有产出记录却在无限累积抱怨”。

**边界形状**：`files.ndjson` 在 `maxArchiveBytes` 下整块读入后扫描（有界，但不是流式行读取器）；
行上限在扫描时生效。`compareUtf8` 每次比较分配两个临时 buffer。这些都按输入规模有界，
**不是** allocator/RSS 证明。

上限在增长**之前**检查（`Budget.spend` 先判后加），不是事后计数。读取只来自已校验归档的
blob，并在读取时重新计算哈希：摘要不符即 `E_ARCHIVE`，不做“大致相信”。

## 7. CLI

```bash
bun tools/sage-db-import-plan.ts \
  --archive <verified-archive-dir> --out <new-plan.json> \
  --max-entries 500 --max-bytes 1000000 --max-records 5000 \
  --max-archive-bytes 2000000 --max-line-bytes 4000000 \
  [--created-at ISO]
```

退出码：`0` 计划完整；`64` 用法错误；`65` 拒绝（归档/边界/输出已存在）；`66` 计划生成但 `incomplete`。

`--out` 已存在时拒绝且不触碰原文件（`wx` 语义）。

`--out` **落在 archive 目录内（或就是 archive 目录本身）**时同样拒绝：往被描述的归档里写文件会
让它多出一个未被引用的文件、不再自校验，计划自己声明的绑定也被自己的输出路径作废。该检查发生在
**任何写入之前**，测试断言归档字节与文件集合完全不变。

## 8. 非主张

- 不声称映射无损：未解析字段在 `unresolved` 里，缺失事实在 `unavailable` 里。
- 不声称目标 ID 是最终 ID：它们是本计划的提议值，导入器可以重新分配但必须记录映射。
- 不声称计划可以跨源复用：`source.archive_content_digest` 与 `source.source_digest` 绑定到唯一一次捕获。
- 不声称就绪或可切换。

生成结束时还核对实际发出的六类映射记录总数与共享预算计数一致；不一致属于工具缺陷，拒绝生成计划。
