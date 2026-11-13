# AGENTS.md — x-sage 项目 AI 工作手册

## 项目定位

x-sage 是一个 **AI-Native 概念知识库系统**。
不同于传统数据库（面向机器检索）和传统文档（面向人类阅读），
本系统为大模型的推理模式从头设计：结构化元数据驱动按需激活，而非语义相似度。

## 当前阶段

**实现阶段** — 已有可运行的核心、service 层、CLI、HTTP 服务与可视化。
KB 内容（`kb/default/`）仍在扩充：L0/L1 稳定，L2/L3 持续新增。

---

## 仓库结构

```
x-sage/
├── packages/
│   ├── core/      # 纯函数：解析、图算法、schema（无 I/O 副作用入口）
│   └── service/   # 用例 + 路由 + 渲染（handleRequest / render.ts；不依赖 hono）
├── apps/
│   ├── cli/       # REST 客户端：只发 HTTP 请求，不读 KB
│   ├── server/    # HTTP 适配器：鉴权/压缩/上限 + 静态托管（生产单端口）
│   └── viz/       # SPA：只从 /api/viz 取数
├── kb/<kb-id>/    # 知识库数据（概念 markdown 是唯一真源）
└── docs/          # 设计文档
```

**分层约束**：`service` 不依赖 `hono`；路由与文本渲染都只在 `service`
（`api.ts` 的 `handleRequest`、`render.ts`）。`apps/server` 是唯一对外入口，
`apps/cli` 与 `apps/viz` 都只是它的 REST 客户端 —— **没有本地模式**，
客户机不携带任何 KB 数据。

## 验证命令

```bash
bun run ci          # typecheck + 契约测试 + 全部测试 + KB 校验
bun run typecheck   # 5 个包
bun test            # 114 个测试，约 0.6s

# 交互式使用需要先起服务（CLI 是纯客户端，没有本地模式）
bun run server &
x-sage --server http://127.0.0.1:3001 validate --graph
```

测试覆盖四层契约：`packages/core`（关系代数、闭包、环、层向）、
`packages/service`（真实 KB 的集成行为）、`apps/server`（HTTP 状态码与载荷、
写保护）、`apps/cli`（退出码、stdout，以及远程/本地逐字节一致）、
`scripts/`（CI 与镜像契约）。**改动语义层前先跑 `bun test`** —— 它保护的是
`contradicts` 对称性、`depends-on` 传递闭包、入向查询这几条容易被无意破坏的规则。

## 知识库结构

每个知识库位于 `kb/<kb-id>/`，包含：

```
kb/<kb-id>/
├── kb.json          # 该 KB 的元数据（id、name、description、domains）
├── AI.md            # 该 KB 对 AI 的自说明文件
├── MANIFEST.md      # 全局原子索引，每次会话必须加载
├── graph.json       # 机器可读的图拓扑
├── frontier.md      # 激活边界：已识别未深入的节点和未解张力
├── concepts/
│   ├── L0-axioms/   # 公理：无法被其他概念定义，5-15 个
│   ├── L1-core/     # 核心：用 L0 定义的主干词汇，20-80 个
│   ├── L2-support/  # 支撑：用 L0+L1 定义的细化概念
│   └── L3-instances/# 实例：某个抽象概念在具体场景下的落地
├── checkpoints/     # 跨会话状态快照
├── protocols/       # 三种对话协议（exploration / validation / generation）
└── proposals/       # AI 写回暂存区（待人类 review）
    ├── edges/
    └── atoms/
```

全局文件：
- `registry.json` — 所有 KB 的注册表
- `docs/` — 系统设计文档（见下方链接）

---

## 概念文件格式

```yaml
---
id: concept-id
atom: "一句精华描述（最多 200 字符）"
layer: 0 | 1 | 2 | 3
domain: [product, personal-thought]
confidence: confirmed | hypothetical | contested
source: personal | domain | synthesis
triggers: [关键词A, 关键词B]
edges:
  - type: is-a | part-of | depends-on | enables | contradicts | instantiates | analogous-to
    target: concept-id           # 本 KB 内引用
    # target: kb-id::concept-id # 跨 KB 引用
    confidence: confirmed
---

# 概念名称

## 核心内容（molecule）
完整概念描述，500-1500 tokens。

## 张力与开放问题
与其他概念的张力点，未解决的矛盾。

## 激活建议
加载本概念时，建议联动加载：[concept-id-x]
```

---

## 查询返回格式

查询结果根据消费者类型选择格式：

### `--format ai`（默认，面向推理）

```
[CONCEPT: concept-id]
ATOM: 一句精华
LAYER: 0 | SOURCE: personal | CONFIDENCE: confirmed
EDGES: is-a→parent-id, contradicts→other-id, enables→downstream-id
REQUIRES: [prereq-id]
TRIGGERS: [关键词A, 关键词B]
LOAD-NEXT: [recommended-id-1, recommended-id-2]
---
{molecule 内容}
```

`REQUIRES` 是 `depends-on` 的传递闭包，`LOAD-NEXT` 是直接 `enables`；两者都在服务端算好后随响应下发。

### `--format human`（面向阅读）

返回概念文件的原始 Markdown，直接渲染。

### `--format manifest`（面向扫描）

```
| id | atom | layer | source | confidence | triggers |
```

用于会话开始时加载全局清单，或快速浏览所有概念。

**查询类型 × 默认格式：**

| 查询 | 人类默认 | AI 默认 |
|------|---------|---------|
| `activate <id>` | `--format human` → markdown | 默认 text（AI 格式）|
| `search <keyword>` | text | text（`--format manifest` → 表格）|
| `edges <id>` | text | text（`--json` → JSON）|
| `validate` | text | text（`--json` → JSON）|

---

## AI-Native 设计原则

本系统满足五个 AI-Native 属性：

| AI 认知特征 | 系统响应 |
|------------|---------|
| 上下文窗口有硬性上限 | 三层激活：MANIFEST 永驻，概念文件按需加载 |
| 注意力是稀疏的 | triggers + 类型化边驱动激活路径 |
| 推理是路径依赖的 | `depends-on` 的传递闭包保证前置概念先行加载 |
| 无持久记忆 | Checkpoint + frontier 跨会话保留进度 |
| 可以写，不只读 | AI 有标准写回路径（proposals/ 目录）|

AI 可执行的写回操作：

| 操作 | 触发条件 | 写入位置 |
|------|---------|---------|
| `propose-edge` | 发现未记录的概念关联 | `proposals/edges/` |
| `refine-atom` | atom 描述不足以区分相似概念 | `proposals/atoms/` |
| `refine-body` | 正文结论已被纠正，需整段改写 | `proposals/bodies/` |
| `flag-tension` | 发现未记录的矛盾 | `frontier.md` |
| `create-checkpoint` | 会话结束时固化推理进度 | `checkpoints/` |

---

## 对话激活协议

**会话开始时必须加载：**
1. `kb/<id>/MANIFEST.md`（全局原子索引）
2. `kb/<id>/frontier.md`（当前未解张力）

**激活流程：**
1. 识别问题中的触发关键词，对照 MANIFEST 的 triggers 列
2. 声明需要激活的概念 ID 列表
3. 加载对应概念文件，在激活子图内推理
4. 如发现边指向未加载概念，声明需要扩展激活

**三种模式的激活策略：**
- 探索：沿 `enables` 边广度优先展开
- 验证：优先加载 `contradicts` 边的节点
- 生成：从 L0 出发，沿 `instantiates` 链向下推演

---

## 设计文档索引

| 文档 | 内容 |
|------|------|
| [architecture.md](docs/architecture.md) | 系统整体架构与 AI-Native 设计原则 |
| [concept-layers.md](docs/concept-layers.md) | L0-L3 分层体系定义与判断标准 |
| [l0-candidates.md](docs/l0-candidates.md) | L0 公理候选（主体、价值、语境、张力）|
| [l1-candidates.md](docs/l1-candidates.md) | L1 核心概念候选（目标、约束、决策、需求、风险）|
| [concept-reshaping-cognition.md](docs/concept-reshaping-cognition.md) | 概念如何重塑 AI 认知行为和目标 |
| [concept-checkpoint-sessions.md](docs/concept-checkpoint-sessions.md) | 复杂概念的跨会话管理机制 |
| [edge-semantics.md](docs/edge-semantics.md) | 边类型的关系代数、读取时推理与双真源收敛记录 |
| [deployment.md](docs/deployment.md) | Docker/CI 部署、x99 上线、版本断言与故障排查 |

---

## 多知识库

所有 KB 在 `registry.json` 中注册。跨 KB 引用格式：`kb-id::concept-id`。
新建 KB：在 `kb/` 下创建目录 + `kb.json`，然后在 `registry.json` 注册。
