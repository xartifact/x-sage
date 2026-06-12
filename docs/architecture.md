# x-concepts：面向大模型的概念知识库架构

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
| 推理是路径依赖的，加载顺序影响结论 | `requires` 边保证前置概念先于后置概念加载 |
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
边不是无差别的"相关性"，而是语义类型：

| 边类型 | 含义 | 推理用途 |
|--------|------|----------|
| `is-a` | 分类关系 | 归纳/演绎 |
| `part-of` | 组合关系 | 整体理解 |
| `depends-on` | 前置条件 | 推演顺序 |
| `enables` | 解锁关系 | 发现链 |
| `contradicts` | 矛盾张力 | 辩证分析 |
| `instantiates` | 抽象→具体 | 举例验证 |
| `analogous-to` | 跨域类比 | 迁移推理 |

---

## 文件结构

```
x-concepts/
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
    └── atoms/               # refine-atom 输出
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
requires: [prerequisite-concept-id]
enables: [downstream-concept-id-1, downstream-concept-id-2]
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
| 前提依赖 | 无 | `requires` 边保证顺序 |
| 维护成本 | 需要重新嵌入 | 直接编辑 markdown |
| AI 可操作性 | 需要专用工具 | 直接 read/grep 即可 |

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
