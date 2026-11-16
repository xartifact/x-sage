---
id: review-decision
atom: "架构决策是在约束与前提之下做出的可追溯选择，包含被否决的替代方案。"
layer: 1
domain: [architecture]
confidence: confirmed
source: synthesis
triggers: [决策, 架构决策, ADR, decision]
edges:
  - type: depends-on
    target: review-subject
    confidence: confirmed
  - type: depends-on
    target: review-premise
    confidence: confirmed
  - type: depends-on
    target: review-constraint
    confidence: confirmed
  - type: enables
    target: review-risk
    confidence: confirmed
---

# 决策 Review Decision

## 核心内容（molecule）

一个可审查的决策记录包含：被选方案、被否决的替代方案、当时的前提与约束、
以及什么情况下应该重新考虑。缺少后两项时，决策无法在环境变化后被检验。

决策不是推理的终点。它是在信息不完整时对方向的承诺，因此必然带风险。
