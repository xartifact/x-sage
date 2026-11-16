---
id: review-tension
atom: "审查张力是两条都被证据支持的结论之间的冲突，不能靠加权平均消解。"
layer: 2
domain: [architecture]
confidence: contested
source: synthesis
triggers: [张力, 冲突, tension, conflict, 矛盾]
edges:
  - type: depends-on
    target: review-premise
    confidence: confirmed
  - type: depends-on
    target: review-risk
    confidence: confirmed
  - type: contradicts
    target: review-decision
    confidence: contested
---

# 审查张力 Review Tension

## 核心内容（molecule）

当两份证据各自成立却指向相反结论时，审查者面对的是张力而不是错误。此时
正确的动作是把两份证据都列出来，指出它们各自依赖的前提，并说明在哪个
前提下哪一份成立。

把张力平均成一个"综合评分"会丢掉最重要的信息：分歧本身。

## 张力与开放问题

张力可以长期存在。它被解决的方式通常是某个前提被证伪，而不是某一方被说服。
