# MANIFEST — 全局原子索引

> 每次 AI 会话开始时必须加载本文件。
> 格式：每个概念一行紧凑描述 + 触发关键词。

| id | atom | layer | source | confidence | triggers |
|----|------|-------|--------|------------|----------|
| review-subject | 被审查的系统是审查的边界：它声明什么在范围内、什么不属于本次判断。 | 0 | synthesis | confirmed | 审查对象, 系统边界, scope, system under review |
| review-premise | 前提是设计成立所依赖的未验证假设，审查的任务之一就是找出并检验它。 | 0 | synthesis | confirmed | 前提, 假设, premise, assumption |
| review-decision | 架构决策是在约束与前提之下做出的可追溯选择，包含被否决的替代方案。 | 1 | synthesis | confirmed | 决策, 架构决策, ADR, decision |
| review-constraint | 约束是系统无法选择的外部条件，它限定可行设计空间而不评价方案好坏。 | 1 | synthesis | confirmed | 约束, 限制, constraint, SLA, 预算 |
| review-risk | 风险是决策在前提失效时暴露的损失，大小取决于损失幅度与前提失效概率。 | 1 | synthesis | confirmed | 风险, risk, 损失, 失效 |
| review-tension | 审查张力是两条都被证据支持的结论之间的冲突，不能靠加权平均消解。 | 2 | synthesis | contested | 张力, 冲突, tension, conflict, 矛盾 |
