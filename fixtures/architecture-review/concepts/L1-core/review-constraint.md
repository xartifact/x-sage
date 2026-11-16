---
id: review-constraint
atom: "约束是系统无法选择的外部条件，它限定可行设计空间而不评价方案好坏。"
layer: 1
domain: [architecture]
confidence: confirmed
source: synthesis
triggers: [约束, 限制, constraint, SLA, 预算]
edges:
  - type: depends-on
    target: review-subject
    confidence: confirmed
---

# 约束 Review Constraint

## 核心内容（molecule）

约束与前提的区别是来源：约束由环境施加，前提由设计者持有。违反约束的方案
不可行，违背前提的方案只是可能回答错了问题。

审查中出现的分歧常常是分类错误——一方把可协商的偏好当成约束，另一方把
真实的约束当成偏好。

## 张力与开放问题

约束本身可以冲突（成本与延迟），此时需要的是取舍记录，而不是一条更严格的约束。
