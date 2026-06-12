---
id: decision
atom: "主体在张力中，于约束条件下做出的不可逆选择，是行动的起点而非推理的终点。"
layer: 1
domain: [product, philosophy]
confidence: confirmed
source: synthesis
triggers: [决策, 选择, 取舍, 判断, 定夺, 方向, 决定, 抉择, decision, choice]
requires: [agent, tension, constraint]
enables: [risk]
edges:
  - type: depends-on
    target: agent
    confidence: confirmed
  - type: depends-on
    target: tension
    confidence: confirmed
  - type: depends-on
    target: constraint
    confidence: confirmed
  - type: depends-on
    target: goal
    confidence: confirmed
  - type: depends-on
    target: need
    confidence: confirmed
  - type: enables
    target: risk
    confidence: confirmed
---

# 决策 Decision

## 核心内容（molecule）

决策是主体在张力中，于约束条件下做出的选择。它不是推理的终点（"最优解已找到"），而是行动的起点（"在不确定中选择了一个方向"）。

### 从 L0/L1 推导

- **主体**：决策必须归属于某个主体——没有主体的决策是算法输出，不是决策。决策的核心是"谁为这个选择负责"。
- **张力**：没有张力就没有真正的决策——如果选项之间没有冲突，选择是计算而非决策。张力的存在是决策的前提。
- **约束**：约束限定了可选空间。决策是在受限空间内的选择，不是在无限可能中的自由挑选。
- **目标/需求**：目标和需求为决策提供评价标准——选择哪个选项，取决于它多大程度满足需求并推进目标。

### 核心属性

1. **不可逆性**：真正的决策有不可逆成分。完全可逆的选择是探索而非决策——你可以无限试错。决策的不可逆性正是它的重量来源。
2. **信息不完备性**：决策总是在信息不完备时做出的。如果你有所有信息，那不是决策，是计算。信息不完备 + 不可逆 = 决策的本质。
3. **可追溯性**：好的决策可以被追溯——不是为了追责，而是为了学习。知道"当时为什么这么选"比"选对了没有"更有价值，因为语境会变，但推理结构可以复用。

### 跨域实例

| 域 | 决策实例 |
|----|---------|
| 产品 | 产品方向、功能取舍、技术选型、优先级排序 |
| 项目 | 资源分配、里程碑调整、风险应对策略 |
| 个人 | 人生选择、职业方向、行动取舍、承诺 |

### 决策的三种模式

根据 AGENTS.md 中的对话协议，决策对应三种激活策略：
- **探索模式**：沿 `enables` 边展开决策的可能后果
- **验证模式**：沿 `contradicts` 边检查决策的盲点
- **生成模式**：从公理出发推演决策的深层合理性

## 张力与开放问题

- **决策瘫痪**：当张力过大或信息过于不完备时，主体可能无法做出决策。系统如何帮助识别"应该延迟决策"的情况？
- **决策的隐性成本**：每个决策都有未被计入的成本——机会成本、注意力消耗、团队士气影响。如何让隐性成本显性化？
- **决策与后悔**：好的决策不保证好的结果——语境的不可预测性使得结果可以偏离决策质量。如何区分"决策质量"和"结果质量"？

## 激活建议

加载本概念时，建议联动加载：[tension, constraint, goal]
