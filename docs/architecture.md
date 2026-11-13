# x-sage：面向大模型的概念知识库架构

> 后续需求：[通过 CLI 维护概念完整正文](concept-body-update-requirement.md)（2026-11-13，需求提案，未实现）。补齐目前 atom/边写回与完整 molecule 纠偏之间的缺口。

## 设计背景

传统数据库面向机器检索（SQL/向量），传统文档面向人类阅读。
本系统是一个 **AI-Native** 的知识组织系统：为 AI 的认知模式从头设计，而非为人类设计后让 AI 适配。

- 概念数量持续增长，总量超出上下文限制
- 概念之间有复杂的拓扑关联
- 支持三种交互模式：探索、澄清验证、生成推演
- 不依赖向量数据库

**核心洞察**：大模型的内部机制是稀疏激活——只有相关特征被激活。
本系统在外部模仿这个机制：用结构化元数据驱动按需激活，而非语义相似度。

---

## AI-Native 设计原则

"AI-Native"不是"对 AI 友好"，而是在每一层设计决策上都对齐 AI 的认知模式。

AI 的认知模式有五个关键特征，系统必须逐一响应：

| AI 认知特征 | 系统响应 |
|------------|---------|
| 上下文窗口是工作记忆，有硬性上限 | 三层激活模型：MANIFEST 永驻，概念文件按需加载 |
| 注意力是稀疏的，相关信息需要显式激活 | triggers 字段 + 类型化边驱动激活路径 |
| 推理是路径依赖的，加载顺序影响结论 | `depends-on` 边的传递闭包保证前置概念先于后置概念加载 |
| 无持久记忆，每次会话需重建上下文 | Checkpoint + frontier 机制跨会话保留进度 |
| 可以写，不只是读 | AI 有标准写回路径，贡献新边、更新 atom、标记张力 |

### 自说明性（Self-describing）

知识库自己知道怎么被 AI 用。根目录的 `AI.md` 是整个系统对 AI 的说明书——AI 从任何入口进入这个项目，读 `AI.md` 即可独立工作，无需人类每次重新说明协议。

### 双向性（Bidirectional）

AI 不只是知识库的消费者，也是贡献者。每次推理中发现的新连接、对 atom 的改进、识别出的张力，都有标准格式写回。人类 review 后合并，知识库随对话积累而演化。

AI 可执行的标准写回操作：

| 操作 | 触发条件 | 输出格式 |
|------|---------|---------|
| `propose-edge` | 发现两个概念间存在未记录的关联 | 新边定义 + 理由 |
| `refine-atom` | 当前 atom 描述不足以区分相似概念 | 新 atom 候选 + 对比说明 |
| `refine-body` | 正文结论已被推翻，需整段改写 | 候选正文 + revision 基线 + 理由 |
| `flag-tension` | 发现两个概念存在未记录的矛盾 | 写入 frontier.md |
| `create-checkpoint` | 会话结束时固化本次推理进度 | checkpoint 文件 |

### 置信度分层（Confidence Tiering）

概念文件区分"确定的知识"和"假设性连接"。AI 在推理时不会把猜测当地基。

```
confidence: confirmed    # 经过验证，可作为推理基础
confidence: hypothetical # 待验证，可用于探索但不可作为公理
confidence: contested    # 存在已知对立观点，需要辩证处理
```

---

## 核心架构：三层激活模型

### 第一层：全局清单（永远在上下文中）

`MANIFEST.md` —— 每个对话开始时必须加载，是系统的"工作记忆"。
格式：每个概念一行的紧凑原子描述 + 边列表摘要。
**预算目标**：500 个概念控制在 4000 tokens 以内。

```
| id | atom（1句精华） | layer | domain | triggers |
|----|----------------|-------|--------|----------|
| concept-a | ... | 1 | philosophy | [关键词] |
```

### 第二层：概念节点文件（按需激活）

每个概念一个 `.md` 文件，包含三种粒度的内容：

- **atom**：1-3句，写在文件头 frontmatter，同时出现在 MANIFEST
- **molecule**：完整概念内容（500-1500 tokens）
- **cluster 引用**：指向应该联动加载的相关概念 ID

### 第三层：图拓扑文件（机器可读）

`graph.json` —— 编码所有节点和**类型化的边**。
边不是无差别的"相关性"，而是有代数性质的语义关系。这张表的实现是
`packages/core/src/graph/semantics.ts` 的 `RELATION_SPECS`，校验器与遍历器都按它执行。

| 边类型 | 含义 | 推理用途 | 传递 | 对称 | 无环 | 层向 |
|--------|------|----------|------|------|------|------|
| `is-a` | 分类关系 | 归纳/演绎 | 是 | 否 | 是 | 指向同层或更抽象 |
| `part-of` | 组合关系 | 整体理解 | 是 | 否 | 是 | 指向同层或更抽象 |
| `depends-on` | 前置条件 | 推演顺序 | 是 | 否 | 是 | 不限 |
| `enables` | 解锁关系 | 发现链 | 否 | 否 | 否 | 不限 |
| `instantiates` | 抽象→具体 | 举例验证 | 否 | 否 | 是 | 指向同层或更抽象 |
| `contradicts` | 辩证张力 | 辩证分析 | 否 | 是 | 否 | 不限 |
| `analogous-to` | 跨域类比 | 迁移推理 | 否 | 是 | 否 | 不限 |

三个刻意的非本体决定：

- **`enables` 不是 `depends-on` 的逆。** `risk` 与 `decision` 互为因果、互相 enable，
  但没有加载先后。若把它当逆属性派生，会造出 `decision → risk → decision` 的假环。
- **`contradicts` 不是 `owl:disjointWith`。** 一个概念可以是 X 的特例同时又与 X 张力对立
  （`goal-drift`、`latent-goal`、`pseudo-need`），因此不做互斥校验——那是这个 KB 最有价值的结构。
- **`confidence` 是本体没有的维度。** 传递闭包上取路径最弱置信度：
  结论的可信度不会高于链上最弱的一环。

---

## 文件结构

```
x-sage/
├── AI.md                    # 自说明入口：AI 读此文件即可独立使用整个系统
├── MANIFEST.md              # 全局原子索引（永远加载）
├── graph.json               # 机器可读的图拓扑
├── frontier.md              # 激活边界：已识别但未深入的节点和未解张力
│
├── concepts/                # 按 layer 分组
│   ├── L0-axioms/           # 核心公理，最稳定，confidence: confirmed
│   ├── L1-core/             # 主干概念
│   ├── L2-support/          # 支撑细节
│   └── L3-instances/        # 具体实例/案例
│
├── checkpoints/             # 跨会话状态快照
│
├── domains/                 # 按领域的视角索引（可选）
│   ├── philosophy/
│   ├── product/
│   └── engineering/
│
├── protocols/               # AI 对话协议
│   ├── exploration.md       # 探索模式 prompt
│   ├── validation.md        # 澄清/验证模式 prompt
│   └── generation.md        # 生成/推演模式 prompt
│
└── proposals/               # AI 写回暂存区（待人类 review）
    ├── edges/               # propose-edge 输出
    ├── atoms/               # refine-atom 输出
    └── bodies/              # refine-body 输出（JSON，带 expectedRevision）
```

---

## 概念文件格式

```markdown
---
id: concept-id
atom: "这个概念的一句精华描述"
layer: 1
domain: [philosophy, product]
confidence: confirmed
triggers: [关键词A, 关键词B]
edges:
  - {type: is-a, target: parent-concept, confidence: confirmed}
  - {type: contradicts, target: opposing-concept, confidence: confirmed}
  - {type: analogous-to, target: cross-domain-concept, confidence: hypothetical}
---

# 概念名称

## 核心内容（molecule）
...完整的概念描述，500-1500 tokens...

## 张力与开放问题
...与其他概念的张力点，未解决的矛盾...

## 激活建议
加载本概念时，建议联动加载：[concept-id-x, concept-id-y]
```

---

## 对话激活协议

每次对话开始时的标准 prompt 头：

```
[系统] 当前加载：MANIFEST.md（全局原子索引）
请按以下协议工作：
1. 识别问题中涉及的概念（对照 MANIFEST 的 triggers）
2. 声明需要激活的概念 ID 列表
3. 等待概念文件加载后，在激活的子图内推理
4. 如发现边指向未加载概念，声明需要扩展激活
```

三种模式对应不同激活策略：

- **探索模式**：从 atom 出发，顺着 `enables` 边广度优先展开
- **验证模式**：优先加载 `contradicts` 和 `tensions-with` 边的节点
- **生成模式**：从 L0 公理出发，沿 `instantiates` 链向下推演

---

## 对比向量数据库

| | 向量数据库 | 本系统 |
|--|-----------|--------|
| 相似性来源 | 嵌入距离（隐式，模型学到的） | 显式边类型（作者意图） |
| 矛盾表达 | 相似度高≈相关，无法区分对立 | `contradicts` 边明确编码 |
| 推理路径 | 黑箱检索 | 可解释的图遍历 |
| 前提依赖 | 无 | `depends-on` 传递闭包保证顺序 |
| 维护成本 | 需要重新嵌入 | 直接编辑 markdown |
| AI 可操作性 | 需要专用工具 | 直接 read/grep 即可 |

---

## 读取时推理

概念文件只写**直接**关系，可推导的部分在读取时计算，避免手写副本漂移：

| 计算 | 函数 | 效果 |
|------|------|------|
| 对称补全 | `completeSymmetricEdges` | `contradicts` / `analogous-to` 只写一个方向，反向自动补 |
| 传递闭包 | `transitiveClosure` | `is-a` / `part-of` / `depends-on` 推出全部可达边，`derived` 字段记见证路径 |
| 环检测 | `findEdgeCycles` | 对声明无环的关系报错，单环只报一次 |
| 层向校验 | `layerViolation` | `instantiates` / `is-a` / `part-of` 不得指向更具体的层 |

早期版本的 frontmatter 同时有 `requires` / `enables` 数组和 `edges` 列表，
实测 18 条 `enables` 中 8 条与边不一致，两个真源已删除，边是唯一来源。

---

## 进程架构：标准 C/S + B/S

**服务端是唯一读得到知识库的组件。** 所有客户端——CLI、浏览器、未来的任意宿主——
都通过同一套 REST API 访问；服务端把结果渲染成 JSON 或文本。因此不存在「客户端 A
和客户端 B 行为不一致」这类问题：只有一个实现点。

```
                      ┌──────────────────────────┐
                      │  packages/core           │  纯函数：解析、图算法、schema
                      └────────────▲─────────────┘
                                   │
                      ┌────────────┴─────────────┐
                      │  packages/service        │  用例 + 路由 + 渲染
                      │  api.ts  handleRequest   │  (ctx, request) → {status, body}
                      │  render.ts               │
                      └────────────▲─────────────┘
                                   │
                      ┌────────────┴─────────────┐
                      │  apps/server (Hono)      │  唯一入口：鉴权/gzip/体积/静态
                      └────────────▲─────────────┘
                                   │ HTTP /api/*
                 ┌─────────────────┼─────────────────┐
        ┌────────┴───────┐ ┌───────┴────────┐ ┌──────┴────────┐
        │ apps/cli       │ │ apps/viz (SPA) │ │ 任意第三方     │
        │ REST 客户端    │ │ REST 客户端    │ │ REST 客户端    │
        └────────────────┘ └────────────────┘ └───────────────┘
```

### 关键约束

| 约束 | 理由 |
|---|---|
| **没有本地模式** | CLI 只有 `--server`；它不读 KB、不带图算法，客户机可以是空的 |
| 路由只在 `service/src/api.ts` | 一个 `handleRequest`，HTTP 是它唯一的对外形态 |
| 渲染只在服务端（`render.ts`） | 文本格式只有一个实现点；换客户端不改行为 |
| `service` 不依赖 Hono | Hono 只做传输层（鉴权/gzip/静态），业务与路由可独立测试 |
| API 是 RESTful 的 | 写操作按 `PUT`（幂等重建）/`POST`（创建）区分 |
| `apps/viz` 只认 `/api/viz` | 没有落盘数据可以过期 |

CLI 是薄传输层：解析 argv → 拼一个 HTTP 请求 → 打印响应体 → 透传退出码。
它对本仓库唯一的知识是「命令到路径」的映射。

### 启动

```bash
# 服务端（开发）
bun run server     # → http://127.0.0.1:3001

# 前端开发（Vite 把 /api 代理到 3001）
bun run viz        # → http://localhost:5173

# 生产：单进程同时托管 API 与构建产物
bun run build      # 构建 apps/viz/dist
bun run server     # → http://127.0.0.1:3001 同时提供 / 与 /api

# 客户端：任何机器，只要网络可达
x-sage --server https://x-sage.example.com activate decision
```

### 环境变量

**服务端**

| 变量 | 默认 | 作用 |
|---|---|---|
| `X_SAGE_KB` | `<repo>/kb/default` | 服务哪个知识库 |
| `PORT` / `HOST` | `3001` / `127.0.0.1` | 监听地址（默认不对外暴露） |
| `CORS_ORIGIN` | `*` | 允许的来源 |
| `X_SAGE_WEB_DIST` | `apps/viz/dist` | 静态资源目录，不存在则只提供 API |
| `X_SAGE_WRITE_TOKEN` | 未设置 | 设置后所有写请求需要 Bearer token |
| `APP_VERSION` / `GIT_COMMIT_HASH` / `BUILD_REF` | `dev`/`unknown` | 由镜像固化，见 [deployment.md](deployment.md) |

**客户端**

| 变量 | 作用 |
|---|---|
| `X_SAGE_SERVER` | 等价于 `--server` |
| `X_SAGE_TOKEN` | 等价于 `--token` |

### REST API

资源路径，动作用 HTTP 方法表达。读端点默认返回 JSON，可用 `format` 协商：
`text`（AI 文本）、`markdown`（概念原文）、`manifest`（表格）。

| 方法 | 路径 | 语义 |
|---|---|---|
| GET | `/api/health` | 存活、当前 KB、构建身份 |
| GET | `/api/meta` | 节点/边计数 |
| GET | `/api/manifest` | MANIFEST 全文（AI 开卷第一步） |
| PUT | `/api/manifest` | 重建并落盘 MANIFEST.md（幂等） |
| GET | `/api/graph?semantic=true` | 图；`semantic` 含派生边 |
| PUT | `/api/graph` | 由概念文件重建 graph.json（幂等；`{"manifest":true}` 一并重建 MANIFEST） |
| GET | `/api/viz` | 可视化完整载荷（含 molecule 与 meta） |
| GET | `/api/search?q=` | 关键词检索 |
| GET | `/api/concepts/:id?strategy=&depth=` | 激活：概念 + 邻域 |
| GET | `/api/concepts/:id/edges?type=&dir=` | 边查询；`dir=in` 即「谁指向它」 |
| GET | `/api/concepts/:id/body` | 正文（frontmatter 之后全部）+ `revision`；`format=markdown` 只返回正文 |
| GET | `/api/path?from=&to=` | 最短路径 |
| GET | `/api/validate?graph=true` | 校验；不通过返回 422 |
| GET | `/api/frontier` | 未解张力 |
| GET | `/api/proposals` | 待审提案（边 + atom + 正文） |
| GET | `/api/proposals/:file` | 单个提案；正文提案附带与当前正文的 unified diff |
| POST | `/api/proposals` | 新建提案；body 的 `kind` 为 `edge`、`atom` 或 `body` |
| POST | `/api/proposals/merge` | 合并提案回概念文件（`/merge` 是业界惯例，保留） |
| GET | `/api/checkpoints/:concept?session=` | 读快照（缺省取最新） |
| POST | `/api/checkpoints/:concept` | 建空骨架（201） |
| PUT | `/api/checkpoints/:concept` | 覆盖保存完整快照 |

**为什么这样分**：重建 graph/MANIFEST 是幂等的（同样输入必得同样输出），所以用 `PUT`；
创建提案每次产生新文件，所以用 `POST`；三种提案共用集合路径，类型放在 body 的 `kind`，
避免用子路径编码动作。

**正文与元数据分开的两个端点**：`/concepts/:id` 是激活（整图 + 语义闭包 + 邻域），
`/concepts/:id/body` 只读一个文件。正文写回也不复用 atom 的整档重序列化——
它按字节替换 frontmatter 之后的部分，元数据原样保留。

**冲突用 409**：正文提案带 `expectedRevision`（提交与合并各校验一次），
基线变化返回 409 且提案保持可审阅；合并失败（提案本身不合法）返回 422。

### 正文写回流程

正文是 frontmatter 之后的全部 Markdown（molecule、张力、激活建议）。它的写回走提案，
但和 atom 提案有三点不同，都是为了让纠错可审阅、可重试、不误伤元数据：

```bash
x-sage concept body <id> --json                       # 取 body + revision
x-sage refine-body <id> --file candidate.md \
  --expected-revision <revision> --reason "为什么改"   # 提交提案
x-sage proposals show <file>                          # unified diff
x-sage proposals merge <file>                         # 应用
x-sage concept body <id> --format markdown            # 读回
```

| 约束 | 实现 |
|---|---|
| 元数据不动 | 合并按字节替换 frontmatter 之后的部分，不解析后重序列化 |
| 并发保护 | `revision = sha256(canonical frontmatter + body)`；提交与合并各校验一次 |
| 冲突可恢复 | 409，提案保持 `pending`，重新读取后重提 |
| 重试幂等 | 正文已写但状态未落盘时，磁盘正文等于候选正文即视为已应用 |
| 派生文件 | 正文不参与 `graph.json` / `MANIFEST.md` 的生成，回执明确说明无需重建 |

### 写保护

`X_SAGE_WRITE_TOKEN` 设置后，所有 `POST`/`PUT`/`PATCH`/`DELETE` 必须有
`Authorization: Bearer <token>`（常量时间比较，先比长度避免抛错）。
未设置且 `HOST` 非回环时，server 拒绝启动而不是静默开放写入口。
请求体上限 256 KB，响应按 `Accept-Encoding` gzip。

### 打包客户端

```bash
bun build apps/cli/src/index.ts --compile --outfile x-sage   # 约 60 MB，目标机无需 bun
bun build apps/cli/src/index.ts --target=bun --outfile x-sage.js  # 0.3 MB，需 bun
```

---

## 实现路线

### 阶段一：核心骨架
1. 固化 frontmatter schema（含 `confidence` 字段）
2. 编写 `graph.json` 的 JSON Schema
3. 确定 MANIFEST.md 的聚合规则
4. 写入第一批 L0 公理级概念（5-10个）

### 阶段二：AI-Native 入口
5. 编写 `AI.md`（自说明文件，AI 的操作手册）
6. 编写三个模式的对话协议（`protocols/` 目录）
7. 定义 `proposals/` 写回格式（AI 贡献的标准结构）

### 阶段三：会话连续性
8. 定义 checkpoint 文件格式
9. 定义 `frontier.md` 的更新协议

### 阶段四：工具化（可选）
10. 脚本自动从概念文件聚合 MANIFEST.md 和 graph.json
11. Claude Code 命令 `/activate <concept-id>` 自动加载相关文件
12. 脚本将 `proposals/` 内容 merge 回概念文件

---

## 验证方式

1. 选取 3-5 个有复杂关联的概念，手写成文件
2. 把 MANIFEST.md 贴入 Claude 对话
3. 提出一个跨越多个概念的问题
4. 观察 Claude 是否能：
   - 正确识别需要激活的概念
   - 沿着边合理扩展
   - 利用 `contradicts` 边发现张力
5. 迭代调整 atom 描述和边类型直到效果满意
