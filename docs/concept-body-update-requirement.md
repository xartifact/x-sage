# 新需求：通过 CLI 维护概念完整正文

> 状态：**已实现**（2026-11-13）。日期：2026-11-13。
> 核查：2026-11-13，对照代码与线上实例逐条验证，结论见文末《核查附录》。
> 实现说明见文末《实现记录》；与草案的差异在那里逐条列出。

> 来源：用户要求“将这个作为一个新的需求，写入 x-sage docs”（`human_asserted`）。下述命令与接口为待评审草案，不是现有能力承诺。

## 1. 背景与实际案例

当前 CLI 能提交、合并 atom 与边提案，也能保存 checkpoint，但不能修改概念 Markdown 正文（molecule、张力与开放问题、激活建议等部分）。这会造成摘要与正文结论不一致，后续激活完整概念时重新引入已纠正的知识。checkpoint 只能补充上下文，不能代替正文纠错。

本次实际案例：用户澄清 **x-core 是纯技术层面的底层基础库**。三个概念的 atom 已通过 CLI 定向纠偏，checkpoint 已记录来源，但以下正文仍需修正：

| 概念 | 待纠正内容 |
|---|---|
| `xartifact-product-matrix` | 把 x-core 标为“Agent 核心”，并将 Agent 循环复用直接归给它 |
| `xartifact-ai-native-toolchain` | 将 x-core 称为“共享内核” |
| `x-artificer-agent-core` | 根据 x-core 的旧定位推导两者职责重叠 |

## 2. 用户故事与目标

作为知识库维护者或外部 Agent，我希望通过 CLI 读取、提议修改并更新概念完整正文，以便在不登录服务端、不直接操作 KB 文件的情况下，让完整概念与已确认知识保持一致。

成功条件：在仅能访问 HTTP 服务的客户机上完成正文纠偏；修改可审阅、有理由、不会覆盖并发修改，并可通过完整激活与校验确认生效。

## 3. 首期范围

- 读取已有概念的完整正文及版本标识。
- 从本地 UTF-8 Markdown 文件或 stdin 提交正文提案。
- 审阅修改前后内容与差异，复用指定提案合并流程。
- 合并时校验版本、概念格式与身份，原子替换服务端概念正文。
- 明确返回应用结果及派生图、manifest 的重建状态。

首期不做概念创建、重命名、删除、分层迁移、任意路径写入、批量正则替换、自动修改关联概念或绕过提案的强制覆盖。正文接口不承担元数据编辑。

## 4. 命令与 REST 草案

命令名称可在实施设计时调整，所需工作流如下：

```bash
# 新能力：JSON 返回正文、完整概念的 revision 及当前 atom 等必要上下文
x-sage concept body xartifact-product-matrix --format json

# 新能力：只提交正文提案；也支持 --file - 从 stdin 读取
x-sage refine-body xartifact-product-matrix \
  --file molecule.md \
  --expected-revision '<读取时获得的 revision>' \
  --reason '按用户澄清将 x-core 更正为纯技术基础库'

# 新能力：审阅指定提案的差异
x-sage proposals show '<提案文件名>' --format diff

# 复用既有指定提案合并入口，不隐式 merge all
x-sage proposals merge '<提案文件名>'

# 沿用既有派生物维护与校验
x-sage graph generate --manifest
x-sage validate --graph --json
```

REST 草案：扩展已有 concept 读取契约，提供结构化 `body` 和 `revision`，保持原格式兼容；扩展 `POST /api/proposals` 支持 `kind: "body"`，字段为 `conceptId`、`proposedBody`、`expectedRevision`、`reason`；提供指定提案的详情与 diff；扩展现有 `POST /api/proposals/merge` 处理正文提案。

正文提案可存于 `proposals/bodies/`，与现有 `edges/`、`atoms/` 对齐。具体路径和输出结构在接口设计时定稿，不新增第二套知识真源。

## 5. 必须守住的行为

### 正文与元数据分离

正文指 YAML frontmatter 之后的完整 Markdown，包含标题、molecule、张力及激活建议等段落。合并只替换正文，保留所有既有元数据字段值，包括 `id`、`atom`、`layer`、`domain`、`confidence`、`source`、`triggers`、`edges`。

输入只接受正文，拒绝以 frontmatter 夹带元数据修改。以现有概念格式校验为基线，拒绝空白正文、损坏格式及超限输入，返回具体错误；合法代码块、列表、中文和未修改段落应完整保留。

### 版本与并发保护

- `revision` 基于完整概念内容生成稳定标识，不能仅用文件修改时间。
- 提交提案和合并时均检查基线；合并必须在现有 KB 写锁内重读和校验。
- 期间发生 atom、边或正文更新都视为基线变化，返回 `409` 和当前版本，不静默覆盖。
- 冲突后提案保持可审阅；重新读取、重建差异后再提交。

### 写入与结果反馈

- 复用现有鉴权、请求体上限、KB 写锁和原子文件替换；concept id 由服务端定位，禁止路径穿越。
- 合并前完成输入与概念格式校验。校验、鉴权、冲突或写入失败，不留下半个概念文件，不提前把提案标为已应用。
- 提案保留基线、候选正文及理由，合并后留存可追溯记录；重试沿用既有幂等语义。
- 回执区分“正文已应用”与“graph/manifest 待重建”。首期可沿用显式重建命令，不要求多个派生文件跨文件事务化；重建失败必须明确报告，不能称整体更新完成。
- 读回正文必须等于实际应用的候选正文，不能请求成功却静默丢字段。CLI 失败非零退出，JSON 输出可解析。

## 6. 架构约束与落点

遵循 `AGENTS.md` 的 C/S 分层：

| 层 | 责任 |
|---|---|
| `packages/core` | 正文提案格式、正文/元数据分离、版本与差异计算、合并基础能力 |
| `packages/service` | 读取与提案用例、契约、锁内版本复核、校验和合并编排；不依赖 Hono |
| `apps/server` | 复用 HTTP 适配、鉴权与请求上限 |
| `apps/cli` | 纯 REST 客户端；读取明确指定的输入文件或 stdin，不扫描本地 KB |
| 文档与 Skill | 命令、冲突处理、正文纠偏及重建流程；实现前不宣传为已有能力 |

概念 Markdown 仍是唯一真源。不能只把新正文写进 checkpoint 或旁路存储，却不更新概念文件。本地输入文件是用户提交材料，不是 CLI 的本地 KB 模式。

## 7. 验收标准

1. 客户机没有 KB 目录，仅凭服务地址与合法认证，能完成读取 → 提案 → diff → 指定合并 → 正文读回。
2. 使用可辨识中文、多行文本、标题、列表和代码块逐项写入读回，无静默丢失，未修改段落保留。
3. 合并前后所有 frontmatter 字段值一致；atom 和边不被旧快照覆盖。
4. 两客户端基于同一 revision 修改，首次合并成功，后者冲突；无丢失更新，失败提案不误标成功。
5. 提交正文提案后、合并前发生 atom 或边更新，同样拒绝过期合并。
6. 非法 id、未知概念、缺少理由、空正文、frontmatter 注入、超限输入、无权限及写入故障均有行为测试；原文件和提案状态保持契约规定的结果。
7. 重复合并同一已应用提案符合既有幂等语义，不误合并其他提案，不破坏 atom/边提案行为。
8. `activate` 能完整读回新正文；重建图与 manifest 后，搜索、激活及 `validate --graph` 一致且无陈旧派生物。
9. 服务端测试验证实际落盘；CLI 黑盒验证文件/stdin 内容、revision/reason 真实进入请求，以及退出码和 JSON 输出。
10. 用 x-core 三个节点演练：更正 Agent 核心定位，移除据此推导的重叠判断，保留其他事实和开放问题；完整激活不再重现已撤回结论。

## 8. 推进顺序与退出条件

先定稿正文、revision、提案和失败语义，再实现 core → service/server → CLI，最后补回归、文档与 Skill。实施前按项目管理约定登记故事任务；本文不是已创建任务或已认领的证明。
退出条件：上述验收通过、现有测试与 CI 不回归，x-core 正文纠偏可通过新 CLI 在隔离 KB 中完整复现。本次仅记录需求，不实现、不部署，也不借此直接改写远端概念正文。

---

## 核查附录（2026-11-13）

本节记录对上述需求的核查结果：哪些断言与当前系统一致、哪些前提不成立、哪些约束必须补进设计。正文需求未改动。

### A. 与现状一致的断言

- 正文不可经 CLI 修改：`POST /api/proposals` 仅接受 `kind: "edge" | "atom"`；atom 合并只替换 frontmatter 的 `atom`（`packages/core/src/proposals/merge.ts`）。
- 本次案例真实：三个 atom 已于 2026-11-13 12:12 合并，正文旧结论仍在（`xartifact-product-matrix.md`、`xartifact-ai-native-toolchain.md`、`x-artificer-agent-core.md` 各自的"Agent 核心 / 共享内核 / 与 x-core 重叠"段落）。checkpoint `xartifact-product-matrix-s1.json` 已把这三处列为 frontier。

### B. 必须修正的前提

1. **正文写入不得复用 `formatConceptFile`。** 它是整档重序列化：实测 `risk-asymmetry.md` 往返 1732→1780 字节（`id` 加引号、`domain`/`triggers` 改块序列、edge 改 JSON 内联、末尾换行丢失）。线上三个已合并文件结尾分别无换行，即已发生正文顺带改写。§5「未修改段落完整保留」要求**字节拼接**：原 frontmatter 块原样保留，只替换其后部分。
2. **`formatConceptFile` 不转义 YAML。** 实测 atom 含 `"` 时合并后文件不可解析：`validate` 422，`/api/viz` 400（UI 全白）。元数据不动必须靠拼接保证，不能靠解析后再序列化。
3. **`findConceptFile` 可路径穿越并写穿 KB。** 实测 `concept: ../../../outside-concept` 的提案合并成功，改写了 KB 目录外的文件。§5「禁止路径穿越」需明确：新实现不复用该函数，校验 `^[a-z0-9-]+$` 且 `filename === frontmatter.id`。
4. **提案文件名不唯一。** atom 提案用 `${conceptId}-${date}.md`，同日两次提交互相覆盖（两次均返回 201，只剩后者）。正文提案需带时间戳与随机后缀。
5. **`merge` 现有语义与 §5/§7 冲突。** 实测：失败时 HTTP 仍 200；CLI 打印 `✗ Failed` 但 exit 0；指定不存在的提案得到 `No pending proposals to merge.` 且 exit 0；`status: rejected` 的提案被指定合并后照常写入概念。正文合并不能继承这套语义，需 409 + 非零退出，并在设计里说明是否连带修正 edge/atom。
6. **幂等重试顺序未定义。** 崩溃于"正文已写、提案未标 approved"之间时，重试会因 revision 已变而 409。必须：当前正文 == 候选正文即视为已应用，先置 `approved`，再走 revision 校验。
7. **revision 基准需重定。** 若按文件字节，`merge all` 中 atom 先重写 frontmatter 会让同批正文提案全部 409。建议 `revision = sha256(canonicalFrontmatterJSON + "\n" + body)`：满足"atom/边变更即失效"，且不受格式化抖动影响。
8. **§7.8 的 manifest 验收无法由 `validate --graph` 承担。** 实测把 `MANIFEST.md` 改成垃圾后该命令仍 200：它只比对 `graph.json` 的节点/边漂移；`search` 与 `GET /api/manifest` 都内存重算、不读文件。"无陈旧派生物"需单独断言：落盘内容 == 重渲染结果。
9. **正文合并不产生派生文件变更。** 实测仅改 molecule、保留 frontmatter，重建后 `graph.json` 与 `MANIFEST.md` 字节不变——两者只从 frontmatter 派生。§4 流程里的 `graph generate --manifest` 对正文合并是多余动作，回执应只报"正文已应用"，否则会出现无意义的"重建失败"。
10. **读接口形态应下沉。** 在 `GET /api/concepts/:id` 上加 `body`/`revision` 会连带执行 `activate`（`buildGraph` + 语义闭包 + 邻居），且其 `markdown` 是含 frontmatter 的整文件、还会额外补一个换行（线上 3526 字节 vs 文件 3525）。建议子资源 `GET /api/concepts/:id/body`，只读文件并解析 frontmatter。

### C. 需补进设计的约束

- 提案信封不要用「frontmatter + 候选正文」：实测正文若以 `---` 开头，`gray-matter` 往返会把它解析成元数据。改用 JSON 信封或显式分隔哨兵。
- 正文上限需写明：服务端全局 `bodyLimit` 为 256 KiB（含 JSON 转义），超限须返回 413 与具体字段。
- `reason` 必填与现有 atom 提案（可选）不一致，需二选一并对齐。
- 空白正文当前合法：只留 frontmatter 的文件 `validate` 200、`activate` 返回 `molecule: ""`。拒绝空白正文须在提交与合并两处显式校验。
- diff 的产出格式（unified、上下文行数）与实现位置（core 计算、service 渲染）需定稿；`packages/core` 现只有 `gray-matter` 与 `zod`，引入 diff 依赖或自写 LCS 是显式决策。
- §7.6 的"非法 id / 未知概念"需绑定具体状态码：现有 dispatcher 依赖错误信息含 "not found" 才映射 404，`ApiError(409)` 需显式构造。

---

## 实现记录（2026-11-13）

按《核查附录》列出的缺陷，先修既有写回路径，再在修好的基础上实现正文写回。

### 命令

```bash
x-sage concept body <id> [--format json|markdown]      # 正文 + revision
x-sage refine-body <id> --file <path|-> \
  --expected-revision <rev> --reason "why"             # 提交正文提案
x-sage proposals show <file> [--json]                  # unified diff
x-sage proposals merge <file>                          # 应用
x-sage proposals list                                  # 三类提案一并列出
```

### REST

| 方法 | 路径 | 语义 |
|---|---|---|
| GET | `/api/concepts/:id/body` | `{conceptId, body, revision, frontmatter, atom}`；`format=markdown` 只返回正文 |
| POST | `/api/proposals` | `kind: "body"`，字段 `conceptId`、`proposedBody`、`expectedRevision`、`reason` |
| GET | `/api/proposals/:file` | 单个提案；正文提案带 `diff`、`hunks`、`applies`、`currentRevision` |
| POST | `/api/proposals/merge` | 返回 `merged` / `conflicts` / `failed` / `derivedStale` |

### 与草案的差异

1. **读接口下沉为子资源** `GET /concepts/:id/body`，不扩 `GET /concepts/:id`：后者会构建语义闭包与邻域，只为一个文件的文本。
2. **`revision` 基准**为 `sha256(canonical frontmatter + "\n" + body)`，不是文件字节：atom/边合并会重写 frontmatter 的排版，按字节会让同批正文提案全部误报冲突。
3. **提交时也校验 revision**（草案只写合并时）。基线与 live 不符直接 409，避免作者提交一个注定被拒、且需先审阅的提案。
4. **提案信封为 JSON**（`proposals/bodies/*.json`），不是「frontmatter + 正文」：正文以 `---` 开头时后者会被 gray-matter 解析成元数据。
5. **去掉 `graph generate --manifest` 步骤**：正文不参与 `graph.json` / `MANIFEST.md` 生成，实测重建后二者字节不变。回执按 `derivedStale` 区分。
6. **合并状态码**：冲突 409，提案本身不合法 422，不再一律 200；CLI 由此得到非零退出码。
7. **正文为空的判定**改为重新解析候选文档，不额外要求 H1——以概念格式校验为唯一基线。

### 同时修复的既有缺陷

| 缺陷 | 修复 |
|---|---|
| `formatConceptFile` 不转义 YAML，atom 含引号会毁文件 | 改用 gray-matter 序列化；全 KB 22 个概念语义往返一致 |
| `findConceptFile` 可路径穿越，合并写穿 KB | `resolveConceptPath` 校验 slug 且要求 `filename === frontmatter.id` |
| 提案文件名同日互撞，后者覆盖前者 | 文件名加时间戳与随机后缀 |
| 提案文件名本身可穿越 | `assertProposalFilename` 拒绝 `..` 与目录分隔 |
| 显式合并无视 `status` | 非 `pending` 拒绝应用（`approved` 视为已合并） |
| atom 提案无基线，覆盖并发修改 | 以提案引用的「当前 atom」为基线，不符即 409 |
| 合并失败仍返回 200 / CLI 退出 0 | 失败 422、冲突 409；指定不存在的提案为 404 |
| 超长 atom 在合并时才失败并写坏文件 | 提交时即校验 200 字符上限 |

### 测试

`bun test` 193 项通过，新增覆盖：正文读写往返、frontmatter 字节不变、并发冲突、崩溃后重试、
提案命名、路径穿越、YAML 转义、diff 只含变更区、HTTP 状态码、CLI 退出码与 stdin 输入。
