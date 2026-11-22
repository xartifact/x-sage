---
{
  "id": "budgeted-concept-activation",
  "atom": "主体按任务目标和上下文预算选择概念正文，同时保留必要定义前提，并显式声明未加载的知识边界。",
  "layer": 2,
  "domain": [
    "engineering"
  ],
  "confidence": "hypothetical",
  "source": "synthesis",
  "triggers": [
    "按需激活",
    "上下文预算",
    "触发词",
    "稀疏注意力",
    "molecule"
  ],
  "edges": [
    {
      "type": "depends-on",
      "target": "constraint",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "goal",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "context",
      "confidence": "hypothetical"
    }
  ]
}
---

# 预算约束激活

## 核心内容（molecule）

知识总量超过主体当前可处理的范围时，激活是对有限注意力的分配。主体先扫描摘要，再按目标选择正文；预算既限制加载规模，也要求对必要前提与扩展材料作不同处理。相关性只说明可能有用，不代表足以支撑结论。

执行过程是扫描索引、匹配触发词、声明概念集合、加载正文、检查缺失前提、再推理。预算不足时应缩小问题或分段推进，并记录缺口；不应悄悄省略关键前提后把结论当成充分验证。探索、验证、生成各自采用不同导航偏好。

x-sage 用 MANIFEST、atom 与 molecule 区分扫描和深入，REQUIRES 给出前提闭包，LOAD-NEXT 给出导航建议。五个正文是技能中的使用建议，不能当成所有模型和任务的通用最优值。预算约束激活也不意味着关键词匹配在所有检索问题上优于向量搜索。

## 张力与开放问题

覆盖面与注意力集中存在张力。触发词遗漏会使重要概念不可见，前提闭包也可能大于预算；应把这些缺口暴露出来，而不是用截断制造完整性假象。

## 激活建议

先加载定义前提：[constraint, goal, context]。需要具体实现案例时再加载 x-sage-concept-kb；该实例不是定义前提。

## 来源与状态

2026-11-16 从 x-sage 项目架构、边语义文档及已确认项目总览提炼。候选拆分及边尚待审阅，hypothetical 不表示已验证的普遍规律。
