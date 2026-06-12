---
id: goal
atom: "主体在特定语境下基于价值想要达到的状态，是行动的方向性锚点。"
layer: 1
domain: [product, philosophy]
confidence: confirmed
source: synthesis
triggers: [目标, 里程碑, 成功标准, 交付物, 方向, 追求, 目的, 靶点, goal, objective]
requires: [agent, value, context]
enables: [decision, risk, constraint]
edges:
  - type: depends-on
    target: agent
    confidence: confirmed
  - type: depends-on
    target: value
    confidence: confirmed
  - type: depends-on
    target: context
    confidence: confirmed
  - type: enables
    target: decision
    confidence: confirmed
  - type: enables
    target: risk
    confidence: confirmed
  - type: part-of
    target: tension
    confidence: confirmed
---

# 目标 Goal

## 核心内容（molecule）

目标是主体在特定语境下基于价值想要达到的状态。它将价值从抽象的"重要"转化为具体的"想要达到"——是价值的方向性表达。

### 从 L0 推导

目标不能脱离三个 L0 概念独立存在：
- **主体**：没有主体的目标是无意义的——目标必须归属于某个行动者。
- **价值**：目标是对价值的追求状态。"提升用户体验"之所以是目标，是因为用户体验是价值。
- **语境**：同一价值在不同语境下产生不同目标。"效率"在初创期意味着快速验证，在成熟期意味着稳定增长。

### 核心属性

1. **层级嵌套**：目标可以分解为子目标，形成目标树。顶层目标直接源自价值，子目标是实现路径上的中间状态。层级深度取决于问题复杂度，不是固定的。
2. **可验证性**：一个好的目标应该可以被判断为"已达成"或"未达成"。模糊的目标（"变得更好"）不是真正的目标，而是价值的同义反复——它没有完成状态。
3. **互斥性**：在特定语境下，两个目标可能互斥——这恰恰是张力在目标层的显现。目标互斥不是设计缺陷，而是价值冲突的自然结果。

### 跨域实例

| 域 | 目标实例 |
|----|---------|
| 产品 | 产品目标、用户目标、业务目标、增长指标 |
| 项目 | 里程碑、交付物、成功标准、时间节点 |
| 个人 | 人生目标、阶段性目标、技能习得目标 |

### 目标与需求的区别

目标是主体**想要达到**的状态，需求是**必须被满足**的状态。目标是积极的追求，需求是底线的守卫。两者都源自价值，但方向不同：目标向外扩展（追求更好），需求向内收敛（防止恶化）。一个系统可以没有目标但不能没有需求——没有需求意味着没有底线。

## 张力与开放问题

- **目标的隐含性**：很多行动背后有未被显式表述的目标。组织中的"潜规则"往往指向隐含目标，这些目标可能比显式目标更有影响力。
- **目标漂移**：在长期执行中，手段可能变成目标本身（手段-目的倒置）。系统如何检测目标漂移？
- **多主体目标冲突**：不同主体的目标在同一语境下冲突时，如何判定优先级？当前系统只记录冲突，不提供裁决机制。

## 激活建议

加载本概念时，建议联动加载：[value, need, tension]
