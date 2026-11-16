---
id: review-premise
atom: "前提是设计成立所依赖的未验证假设，审查的任务之一就是找出并检验它。"
layer: 0
domain: [architecture]
confidence: confirmed
source: synthesis
triggers: [前提, 假设, premise, assumption]
edges:
  - type: depends-on
    target: review-subject
    confidence: confirmed
---

# 前提 Review Premise

## 核心内容（molecule）

前提不是约束。约束来自外部世界（预算、法规、上游 SLA），前提来自设计者
对世界的判断（"流量不会翻倍"、"团队会维护这个服务"）。前提出错时，设计
并不违反约束，但它所回答的问题已经变了。

审查的价值往往不在于找 bug，而在于把没人写下来的前提摆到桌面上。
