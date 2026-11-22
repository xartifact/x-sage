---
{
  "id": "typed-relation-reasoning",
  "atom": "为概念关联声明关系类型及其推导规则，区分作者断言与派生结论，限制哪些推理路径可以成立。",
  "layer": 2,
  "domain": [
    "engineering"
  ],
  "confidence": "hypothetical",
  "source": "synthesis",
  "triggers": [
    "类型化边",
    "关系代数",
    "传递闭包",
    "对称关系",
    "边语义"
  ],
  "edges": [
    {
      "type": "depends-on",
      "target": "constraint",
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

# 类型化关系推理

## 核心内容（molecule）

关联只有在语境中具有明确含义，才足以约束推理。类型化关系为每条边指定语义，并声明是否可传递、是否对称、是否禁止环以及允许的层向；这些规则限制从已知关系到新结论的合法步骤。普通链接只表明可导航，不具有同样的推理许可。

作者边和派生边必须可区分。对称关系允许反向查询，传递关系允许同类型路径闭包，但不能随意复合不同关系。置信度需要随证据传递，不能把弱假设经过多跳包装成确定结论。关系规则正确也不保证作者录入的事实正确。

x-sage 的 contradicts 表达辩证张力，不表示逻辑互斥；enables 表示理解导航，不是 depends-on 的逆。把互相促进改写成互相依赖会制造假环。查询入向关联与扩展出边是不同操作，不能用后者代替前者。

## 张力与开放问题

规则越强，表达越受限；规则越弱，错误路径越容易混入。新增关系应同时明确语义、派生规则和校验边界，不能只为界面增加一个标签。

## 激活建议

先加载定义前提：[constraint, context]。需要具体实现案例时再加载 x-sage-concept-kb；该实例不是定义前提。

## 来源与状态

2026-11-16 从 x-sage 项目架构、边语义文档及已确认项目总览提炼。候选拆分及边尚待审阅，hypothetical 不表示已验证的普遍规律。
