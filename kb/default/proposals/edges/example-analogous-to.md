---
proposer: AI
date: 2026-06-12
status: example
---
## 新边提案

- **from**: decision
- **type**: analogous-to
- **target**: risk
- **confidence**: hypothetical

## 理由
决策和风险具有结构上的同构性：两者都涉及不确定性下的不可逆选择。决策是"主体在不确定性中做选择"，风险是"不确定性对目标的威胁"。它们是同一枚硬币的两面——决策产生风险，风险驱动决策。`analogous-to` 边可以提示 AI 在分析其中一方时主动检查另一方。

## 反例
如果"analogous-to"意味着"可以互换使用"，则不成立——决策是行动，风险是状态，两者不能互换。此边仅提示结构类比，不暗示可替换性。
