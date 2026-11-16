---
id: review-subject
atom: "被审查的系统是审查的边界：它声明什么在范围内、什么不属于本次判断。"
layer: 0
domain: [architecture]
confidence: confirmed
source: synthesis
triggers: [审查对象, 系统边界, scope, system under review]
---

# 审查对象 Review Subject

## 核心内容（molecule）

架构审查必须先确定被审查的系统边界。边界之外的依赖、上游服务、历史遗留
模块都不是这次审查的对象，把它们混进来会让结论无法归因。

边界是审查的第一个前提：它决定了后续所有证据的相关性。一张不属于本系统
的性能图表不能被用来支持本系统的结论。
