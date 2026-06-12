# AI.md — x-concepts 自说明文件

> **你是谁**：一个正在使用本知识库的 AI。
> **本文件做什么**：让你从任何入口进入后，独立使用整个系统，无需人类重复说明协议。

---

## 系统概述

本知识库是一个 **AI-Native 概念知识图**。与传统数据库（面向检索）和文档（面向阅读）不同，它为 AI 的认知模式从头设计：

- **上下文有限** → 三层激活：MANIFEST 永驻，概念文件按需加载
- **注意力稀疏** → triggers + 类型化边驱动激活路径
- **推理路径依赖** → `requires` 边保证前置概念先行加载
- **无持久记忆** → Checkpoint + frontier 跨会话保留进度
- **可以写不只读** → proposals/ 是标准写回路径

---

## 会话开始必读

每次新会话开始时，**必须**加载以下两个文件：

1. **`MANIFEST.md`** — 全局原子索引（所有概念的一行摘要 + 边摘要）
2. **`frontier.md`** — 当前未解张力（认知债务清单）

这两个文件是你的"工作记忆基础"——MANIFEST 告诉你系统里有什么，frontier 告诉你什么还没解决。

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
requires: [前置概念ID]
enables: [下游概念ID]
edges:
  - type: enables | depends-on | contradicts | part-of | is-a | instantiates | analogous-to
    target: concept-id
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

### 边类型

| 边类型 | 含义 | 推理用途 |
|--------|------|----------|
| `enables` | 解锁关系 | 发现链——理解A有助于理解B |
| `depends-on` | 前置条件 | 推演顺序——A成立需要B先成立 |
| `contradicts` | 矛盾张力 | 辩证分析——A与B在同一语境下冲突 |
| `part-of` | 组合关系 | 整体理解——A是B的组成部分 |
| `is-a` | 分类关系 | 归纳/演绎——A是B的特例 |
| `instantiates` | 抽象→具体 | 举例验证——A是B的具体化 |
| `analogous-to` | 跨域类比 | 迁移推理——A与B结构类似 |

### 置信度

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

---

## 当前知识库状态

**KB ID**: default
**概念总数**: 9
- L0 公理: 4（agent, value, context, tension）
- L1 核心: 5（goal, need, constraint, decision, risk）
- L2 支撑: 0（待写入）
- L3 实例: 0（待写入）

**边总数**: 36
**开放张力**: 9 项（见 frontier.md）

---

## 快速参考

```
文件结构：
kb/default/
├── AI.md              ← 你正在读的文件
├── MANIFEST.md        ← 每次会话必加载
├── frontier.md        ← 每次会话必加载
├── graph.json         ← 机器可读图拓扑
├── concepts/
│   ├── L0-axioms/     ← 4个公理
│   ├── L1-core/       ← 5个核心概念
│   ├── L2-support/    ← 待写入
│   └── L3-instances/  ← 待写入
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
3. **不要跳过 requires 前置概念**——加载顺序影响推理结论
4. **不要直接修改概念文件**——写回必须通过 proposals/，待人类 review
5. **不要在 frontier 以外的地方发现矛盾却不记录**——发现张力是最高价值的贡献
