---
{
  "id": "definition-dependency-layering",
  "atom": "按理解一个概念所必需的定义前提组织抽象层次，使理解顺序可解释，避免把重要性或实现顺序误当层级。",
  "layer": 2,
  "domain": [
    "engineering"
  ],
  "confidence": "hypothetical",
  "source": "synthesis",
  "triggers": [
    "定义依赖",
    "概念分层",
    "L0",
    "L1",
    "L2",
    "L3"
  ],
  "edges": [
    {
      "type": "depends-on",
      "target": "context",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "constraint",
      "confidence": "hypothetical"
    }
  ]
}
---

# 定义依赖分层

## 核心内容（molecule）

主体在特定语境中理解概念时，已有知识限定了哪些定义可以直接使用。定义依赖分层将这种理解约束显式化：若解释 A 必须使用 B，则 B 是 A 的前提。层级表达定义的抽象程度，依赖边表达具体前提，两者不能互相替代。

先区分不可在体系内继续分解的基础词汇、由其定义的主干概念、领域细化和具体实例，再检查每个定义实际引用什么。移除某个前提仍能理解定义，说明它可能只是相关知识，不应强行设为 depends-on。共同出现、代码调用、部署顺序都不足以证明定义依赖。

x-sage 采用 L0—L3，并计算 depends-on 闭包。L3 产品实例可以替换，抽象原则不能因为当前只有一个实现就依赖该实例。本文自身是知识组织领域的 L2 方法，既不是新的 L0 公理，也不要求所有领域都使用四层。

## 张力与开放问题

分层越细，维护成本越高；层向合法不保证定义合理。循环定义需要重新划界或承认共同基础，不能靠换层号掩盖。

## 激活建议

先加载定义前提：[context, constraint]。需要具体实现案例时再加载 x-sage-concept-kb；该实例不是定义前提。

## 来源与状态

2026-11-16 从 x-sage 项目架构、边语义文档及已确认项目总览提炼。候选拆分及边尚待审阅，hypothetical 不表示已验证的普遍规律。
