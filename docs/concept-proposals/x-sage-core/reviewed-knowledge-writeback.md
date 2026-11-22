---
{
  "id": "reviewed-knowledge-writeback",
  "atom": "将知识修订先保存为带理由和基线的候选，经审阅与并发检查后成为正式知识，使写入能力与可信发布分离。",
  "layer": 2,
  "domain": [
    "engineering"
  ],
  "confidence": "hypothetical",
  "source": "synthesis",
  "triggers": [
    "知识写回",
    "提案",
    "review",
    "revision",
    "并发冲突",
    "refine-body"
  ],
  "edges": [
    {
      "type": "depends-on",
      "target": "agent",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "constraint",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "risk",
      "confidence": "hypothetical"
    }
  ]
}
---

# 审阅式知识写回

## 核心内容（molecule）

主体能够提出知识修订，不等于修订已经得到验证。审阅式写回把候选生成、证据审查和正式应用分开，约束未经核实的内容直接成为后续推理基础。修订理由说明为何改变，版本基线说明针对哪份知识改变，两者回答不同问题。

读取现有版本，形成明确范围的候选，比较差异和证据，再在基线未发生变化时应用。若基线变化，应重新理解冲突并重提，不应静默覆盖。审阅许可与版本一致性相互独立：版本未变不能证明内容正确，内容正确也不能证明可以覆盖他人修改。

x-sage 用边、atom 和正文提案承载不同粒度的修改；正文提案通过 expectedRevision 防止并发覆盖，合并时保留 frontmatter。该保护应按实际接口能力描述，不能假定每种提案都有相同版本机制。正式概念、候选和 checkpoint 是不同状态，保存候选并不意味着进入可激活概念图。

## 张力与开放问题

审阅提高可信度，但也增加积压与延迟。需要保留待处理状态和失败原因；不能为了减少积压而省略审查，也不能把机械校验通过等同于语义验证。

## 激活建议

先加载定义前提：[agent, constraint, risk]。需要具体实现案例时再加载 x-sage-concept-kb；该实例不是定义前提。

## 来源与状态

2026-11-16 从 x-sage 项目架构、边语义文档及已确认项目总览提炼。候选拆分及边尚待审阅，hypothetical 不表示已验证的普遍规律。
