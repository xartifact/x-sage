# AI.md — x-sage 自说明文件

> **你是谁**：一个正在使用本知识库的 AI。
> **本文件做什么**：让你从任何入口进入后，独立使用整个系统，无需人类重复说明协议。

---

## 系统概述

本知识库是一个 **AI-Native 概念知识图**。与传统数据库（面向检索）和文档（面向阅读）不同，它为 AI 的认知模式从头设计：

- **上下文有限** → 三层激活：MANIFEST 永驻，概念文件按需加载
- **注意力稀疏** → triggers + 类型化边驱动激活路径
- **推理路径依赖** → `depends-on` 边及其传递闭包保证前置概念先行加载
- **无持久记忆** → Checkpoint + frontier 跨会话保留进度
- **可以写不只读** → proposals/ 是标准写回路径

---

## 会话开始必读

每次新会话开始时，**必须**加载以下两个文件：

1. **`MANIFEST.md`** — 全局原子索引（所有概念的一行摘要 + 边摘要）
2. **`frontier.md`** — 当前未解张力（认知债务清单）

这两个文件是你的"工作记忆基础"——MANIFEST 告诉你系统里有什么，frontier 告诉你什么还没解决。

---

## 工具：优先用 CLI，不要手工读全库

概念多了以后，手工 `read` 每个文件会烧掉上下文。后端已经把激活、派生和校验做好了，
输出就是你可以直接消费的格式。

**所有客户端只走后端 API**（标准 C/S）：CLI 与浏览器是同一个 REST 接口的两个客户端，
没有本地模式，客户机不需要 KB 文件。

```bash
# 客户端指向一个部署
x-sage --server http://127.0.0.1:3001 search 决策

# 激活一个概念：完整 molecule + 邻居 atom
#   REQUIRES 是 depends-on 的传递闭包（全部前置，不是直接前置）
#   LOAD-NEXT 是 enables 的直接目标
x-sage --server http://127.0.0.1:3001 activate decision
x-sage --server http://127.0.0.1:3001 activate decision --strategy validation --depth 1
#   --strategy exploration | validation | generation
#   --depth N（默认 1；validation 策略默认 2）
#   --format human（直接给用户看的原文）

# 两个概念怎么连上的
x-sage --server http://127.0.0.1:3001 path risk value

# 查一个概念的关系（含指向它的边）
x-sage --server http://127.0.0.1:3001 edges goal --type contradicts --dir in
x-sage --server http://127.0.0.1:3001 edges decision --type depends-on --derived --json
#   --type T      只看一种边
#   --dir in|out|both   in = 谁指向它（activate 看不到这些）
#   --derived     含传递闭包推出的边
#   --json        结构化输出，可直接 jq
```

`X_SAGE_SERVER` 可替代 `--server`；写操作需要 `--token` 或 `X_SAGE_TOKEN`（若服务端启用）。

**读 `activate` 输出的三条规矩**：

1. `EDGES` 行只列**该节点自己的出边**。指向它的边不在其中——`goal` 没有任何
   `contradicts` 出边，但有 5 条入向 `contradicts`。这类问题用 `edges <id> --dir in`，
   不要用 `--strategy validation` 把整张子图拉进来（实测 10382 字符 vs 688 字符）。
   对称关系（`contradicts`、`analogous-to`）标 `↔`，不区分方向。
2. `REQUIRES` 含派生的间接前置。想只要手写的直接依赖，看 `EDGES` 里的 `depends-on`。
3. 邻居只有 atom 没有 molecule。要深入哪个，就再 `activate` 它一次。

**同一套数据也直接走 REST**（CLI 只是它的一个客户端）：

```bash
curl 'localhost:3001/api/manifest?format=text'
curl 'localhost:3001/api/concepts/decision?format=text'
curl 'localhost:3001/api/concepts/goal/edges?type=contradicts&dir=in'
curl 'localhost:3001/api/viz'                    # 可视化完整载荷
```

响应格式用 `format` 协商：`json`（默认）、`text`（AI 文本）、`markdown`（原文）、`manifest`（表格）。

---

## 工具：改完必须重建与校验

概念文件是唯一真源，`MANIFEST.md` 和 `graph.json` 都是生成物：

```bash
x-sage --server <url> graph generate     # 概念文件 → graph.json
x-sage --server <url> manifest --write   # 概念文件 → MANIFEST.md
x-sage --server <url> validate --graph   # 文件 schema + 图完整性 + graph.json 是否过期
# 等价于（幂等写入用 PUT）：
#   PUT /api/graph      PUT /api/manifest      GET /api/validate?graph=true
```

`validate --graph` 会在 `graph.json` 与概念文件不一致时报 `stale-graph`。
提交前跑它，不要靠肉眼确认。

> 注意：`proposals merge` **不会**自动重建 `graph.json`，合并后必须手动 `graph generate`。

> 服务端位置：仓库里没有常驻服务。要在本地跑一遍校验，用 `bun run ci`（它直接在进程内
> 调用同一个 dispatcher）；要交互式使用，先 `bun run server` 再让客户端连上。

---

## 概念文件结构

每个概念位于 `concepts/{layer}/{id}.md`，包含：

```yaml
---
id: concept-id
atom: "一句精华描述（≤200字符）"
layer: 0 | 1 | 2 | 3
domain: [product, philosophy]
confidence: confirmed | hypothetical | contested
source: personal | domain | synthesis
triggers: [关键词A, 关键词B]
edges:
  - type: is-a
    target: 更抽象的概念ID
    confidence: confirmed
  - type: depends-on
    target: 前置概念ID
    confidence: confirmed
  - type: enables
    target: 下游概念ID
    confidence: confirmed
---
# 概念名称
## 核心内容（molecule）
完整描述...
## 张力与开放问题
...
## 激活建议
建议联动加载：[concept-id-x]
```

### 四层体系

| 层 | 名称 | 数量 | 特征 |
|----|------|------|------|
| L0 | 公理 Axioms | 5-15 | 不可用其他概念定义，系统地基 |
| L1 | 核心 Core | 20-80 | 用 L0 定义的主干词汇 |
| L2 | 支撑 Support | 数百 | 用 L0+L1 定义的细化概念 |
| L3 | 实例 Instances | 按需 | 抽象概念在具体场景下的落地 |

### 边类型（形式语义）

每条边不是无差别的"相关"，而是有明确代数性质的关系。下表由
`packages/core/src/graph/semantics.ts` 的 `RELATION_SPECS` 定义，校验器按此表执行。

| 边类型 | 含义 | 传递 | 对称 | 无环 | 层向 |
|--------|------|------|------|------|------|
| `is-a` | 分类关系（A 是 B 的特例） | 是 | 否 | 是 | 指向同层或更抽象 |
| `part-of` | 组成关系（A 是 B 的一部分） | 是 | 否 | 是 | 指向同层或更抽象 |
| `depends-on` | 前置条件（A 需 B 先成立） | 是 | 否 | 是 | 不限 |
| `enables` | 导航（理解 A 有助于理解 B） | 否 | 否 | 否 | 不限 |
| `instantiates` | 抽象→具体（A 是 B 的实例） | 否 | 否 | 是 | 指向同层或更抽象 |
| `contradicts` | 辩证张力（A 与 B 冲突） | 否 | 是 | 否 | 不限 |
| `analogous-to` | 跨域类比（A 与 B 结构类似） | 否 | 是 | 否 | 不限 |

**两条容易误读的规则**：

- `enables` 不是 `depends-on` 的逆。`risk` 与 `decision` 互相 enable，但没有先后——若把它当逆属性派生，会造出假环。
- `contradicts` 不是逻辑互斥。一个概念可以是 X 的特例同时又与 X 张力对立（`goal-drift`、`latent-goal`、`pseudo-need` 都是），因此不做互斥校验。

`depends-on`、`is-a`、`part-of` 的传递闭包由工具在读取时计算，概念文件里只写**直接**关系，不要手写可推导出的边。对称关系（`contradicts`、`analogous-to`）也只写一个方向。

`requires` / `enables` 两个 frontmatter 数组已删除——它们是 `edges` 的手写副本，实测 18 条 `enables` 中有 8 条与边不一致。读 `depends-on` / `enables` 边即可。

### 置信度

置信度回答"这个断言能不能作为推理基础"：

| 值 | 含义 | 可否作为推理基础 |
|----|------|-----------------|
| `confirmed` | 经过验证 | ✅ 可以 |
| `hypothetical` | 待验证 | ⚠️ 可用于探索，不可作为公理 |
| `contested` | 存在对立观点 | ❌ 需要辩证处理 |

---

## 三种工作模式

根据用户意图选择模式：

| 用户意图 | 模式 | 策略 | 协议文件 |
|----------|------|------|---------|
| "告诉我关于X" / "X和Y有什么关系" | 探索 | 沿 `enables` 边广度优先展开 | `protocols/exploration.md` |
| "X真的对吗" / "X有什么问题" | 验证 | 优先加载 `contradicts` 边节点 | `protocols/validation.md` |
| "如何设计X" / "在X条件下怎么办" | 生成 | 从 L0 出发沿 `instantiates` 链推演 | `protocols/generation.md` |

**选择方式**：分析用户输入的触发关键词。如果不确定，默认使用探索模式。

加载对应协议文件获取完整执行流程。

---

## 激活协议

1. 从用户输入中提取关键词
2. 对照 MANIFEST.md 的 triggers 列匹配概念
3. 声明需要激活的概念 ID 列表
4. 加载对应概念文件（完整 molecule）
5. 在激活子图内推理
6. 如发现边指向未加载概念，声明需要扩展激活

**上下文预算**：建议同时持有 ≤ 5 个概念的完整 molecule。超过时，优先保留 P0-P2 优先级概念（见探索协议的优先级表）。

---

## 写回操作

你不是只读消费者——你是知识库的贡献者。每次推理中发现的新东西，按标准格式写回：

写回有两条路径，**优先用 CLI**：`x-sage propose-edge` 会按正确格式落盘；
下面的手工格式留给 CLI 不覆盖的情况。`proposals merge` 现在会先按 schema 校验提案，
非法边类型会被拒绝且不改动概念文件——但**它不检查图约束**（环、层向），
所以合并后仍要跑 `validate --graph`。

### `propose-edge`：发现未记录的概念关联

**写入位置**：`proposals/edges/{timestamp}-{from}-{type}-{to}.md`

```markdown
---
proposer: AI
date: 2026-06-12
status: pending
---
## 新边提案

- **from**: agent
- **type**: contradicts
- **target**: context
- **confidence**: hypothetical

## 理由
{为什么你认为这条边应该存在}
## 反例（如有）
{什么情况下这条边不成立}
```

### `refine-atom`：atom 描述需要改进

**写入位置**：`proposals/atoms/{concept-id}-{timestamp}.md`

```markdown
---
proposer: AI
date: 2026-06-12
concept: agent
status: pending
---
## 当前 atom
{现有的 atom 文本}

## 建议新 atom
{改进后的 atom 文本}

## 改进理由
{为什么新描述更好——区分了什么、消除了什么歧义}
```

### `flag-tension`：发现未记录的矛盾

**写入位置**：追加到 `frontier.md` 的"概念内张力"表格

```
| {张力描述} | {涉及概念} | 开放 | {说明} |
```

> **不要用 `x-sage frontier add`。** 它解析表格后只回写 4 列，
> 会抹掉 `frontier.md` 里"概念内张力"和"L2→L3 待展开方向"两整节（实测 29 行表格剩 6 行）。
> 用 `edit` 在对应表格末尾追加一行。

### `create-checkpoint`：会话结束固化进度

**写入位置**：`checkpoints/{concept-id}-s{N}.json`

```json
{
  "concept": "目标概念ID",
  "session": 1,
  "date": "2026-06-12",
  "established": ["本次确认的结论列表"],
  "exploredEdges": ["本次探索的边列表"],
  "frontier": [
    { "id": "未深入概念", "reason": "为什么值得探索", "priority": "high" }
  ],
  "nextEntryPoint": "下次会话建议从哪里开始"
}
```

**CLI 只有 `create` 和 `read`，没有 `update`**：`checkpoint create` 写出的是全空骨架
（`established: []`、`nextEntryPoint: ""`），内容需要你自己用 `write` 填进去或者覆盖该文件。
路径：`kb/default/checkpoints/{concept}-s{N}.json`。

---

## 当前知识库状态

> 这段数字会随概念增删而失效。**信任 `MANIFEST.md`、`graph.json` 和 `x-sage validate --graph`
> 的输出，不要引用本节的计数。** 保留本节只为说明此刻的规模量级。

**KB ID**: default
**概念总数**: 22
- L0 公理: 4（agent, value, context, tension）
- L1 核心: 5（goal, need, constraint, decision, risk）
- L2 支撑: 8（各 L1 的细化形式与结构）
- L3 实例: 5（抽象概念的具体落地）

**边总数**: 94 条手写边（`depends-on` 55、`enables` 18、`contradicts` 7、`part-of` 5、`instantiates` 5、`is-a` 4）。
`is-a`/`part-of`/`depends-on` 的传递闭包与 `contradicts`/`analogous-to` 的对称反向由工具在读取时计算，
所以**实际可用的边远多于 94 条**——不要以手写条数判断连通性。

---

## 快速参考

```
文件结构：
kb/default/
├── AI.md              ← 你正在读的文件
├── MANIFEST.md        ← 每次会话必加载
├── frontier.md        ← 每次会话必加载
├── graph.json         ← 生成的图拓扑（勿手改；graph generate 重建）
├── concepts/
│   ├── L0-axioms/     ← 4个公理
│   ├── L1-core/       ← 5个核心概念
│   ├── L2-support/    ← 8个支撑概念
│   └── L3-instances/  ← 5个实例
├── protocols/
│   ├── exploration.md ← 探索模式协议
│   ├── validation.md ← 验证模式协议
│   └── generation.md  ← 生成模式协议
├── proposals/
│   ├── edges/         ← propose-edge 输出
│   └── atoms/         ← refine-atom 输出
└── checkpoints/       ← 跨会话快照
```

---

## 禁止事项

1. **不要把 hypothetical 当 confirmed 用**——未验证的概念不可作为推理基础
2. **不要消除张力的两端**——张力是真实的，只能管理不能消除
3. **不要跳过 depends-on 前置概念**——加载顺序影响推理结论
4. **不要直接修改概念文件**——写回必须通过 proposals/，待人类 review
5. **不要在 frontier 以外的地方发现矛盾却不记录**——发现张力是最高价值的贡献
