# MANIFEST — 全局原子索引

> 每次 AI 会话开始时必须加载本文件。
> 格式：每个概念一行紧凑描述 + 边摘要 + 触发关键词。

| id | atom | layer | source | confidence | triggers |
|----|------|-------|--------|------------|----------|
| agent | 能行动、有意图的实体，是所有行为、目标、价值判断的不可还原的载体。 | 0 | synthesis | confirmed | 主体, 用户, 自我, 他人, 行动者, 组织, 团队, 利益相关者, agent, actor |
| value | 对主体重要的事物，是所有目标选择和价值判断的终极依据，无法再分解为更基础的概念。 | 0 | synthesis | confirmed | 价值, 价值观, 重要, 优先, 好坏, 判断标准, 追求, 信仰, 取舍依据 |
| context | 决定概念如何具体生效的环境条件，使抽象概念获得具体意义的前提本身。 | 0 | synthesis | confirmed | 语境, 场景, 环境, 条件, 背景, 情境, 阶段, 前提, 上下文, context |
| tension | 两个价值或目标在同一语境下无法同时被满足的状态，是推理和决策存在的根本原因。 | 0 | synthesis | confirmed | 张力, 冲突, 矛盾, 两难, 权衡, 取舍, 博弈, 对立, 悖论, tradeoff |
| goal | 主体在特定语境下基于价值想要达到的状态，是行动的方向性锚点。 | 1 | synthesis | confirmed | 目标, 里程碑, 成功标准, 交付物, 方向, 追求, 目的, 靶点, goal, objective |
| need | 主体在特定语境下，因价值缺口导致的必须被满足的状态，是行动的底线驱动。 | 1 | synthesis | confirmed | 需求, 必须, 缺口, 痛点, 底线, 缺乏, 诉求, 需要, necessity, requirement |
| constraint | 在特定语境下限制主体可能行动空间的条件，是可能性的边界而非障碍。 | 1 | synthesis | confirmed | 约束, 限制, 边界, 条件, 制约, 预算, 截止, 依赖, limitation, boundary |
| decision | 主体在张力中，于约束条件下做出的不可逆选择，是行动的起点而非推理的终点。 | 1 | synthesis | confirmed | 决策, 选择, 取舍, 判断, 定夺, 方向, 决定, 抉择, decision, choice |
| risk | 在特定语境下，目标实现可能被破坏的不确定张力，是未来的潜在损失。 | 1 | synthesis | confirmed | 风险, 不确定性, 威胁, 失败, 损失, 隐患, 脆弱, 危机, risk, uncertainty |
| latent-goal | 未被显式表述但实际驱动主体行为的目标，比声明的目标更能预测行动。 | 2 | synthesis | confirmed | 隐含目标, 潜在目标, 真实目的, 真实意图, 私下目标, 隐性目的, latent goal, hidden agenda |
| goal-drift | 手段逐渐变成目的本身的过程，原始目标被遗忘或替代而主体未察觉。 | 2 | synthesis | confirmed | 目标漂移, 手段目的倒置, 目标替代, 目标遗忘, 过程异化, goal drift, goal displacement |
| pseudo-need | 用户表达的解决方案而非真实需求，将'如何做'误认为'要什么'的常见谬误。 | 2 | synthesis | confirmed | 伪需求, 表面需求, 虚假需求, 解决方案伪装, 伪诉求, pseudo need, false requirement |
| latent-need | 主体未意识到但行为中反复显现的需求，在需求缺口未满足时以症状形式表达。 | 2 | synthesis | confirmed | 隐性需求, 潜在需求, 未意识需求, 隐藏需求, 深层需求, latent need, unspoken need |
| hard-soft-constraint | 硬约束不可通过行动改变，软约束可在资源投入后调整——区分二者是决策的前提。 | 2 | synthesis | confirmed | 硬约束, 软约束, 不可变约束, 可变约束, 物理约束, 协商约束, hard constraint, soft constraint |
| constraint-cascade | 一个层面的约束沿依赖链传导到下游，使低层约束比表面看到的更严苛。 | 2 | synthesis | confirmed | 约束传导, 约束链, 约束传播, 级联约束, 依赖链约束, constraint cascade, constraint propagation |
| decision-traceability | 保留决策的推理结构以供回溯学习，使决策可解释而非只看结果好坏。 | 2 | synthesis | confirmed | 决策追溯, 决策记录, ADR, 决策可解释, 决策日志, 回溯, decision traceability, ADR |
| risk-asymmetry | 下行损失远大于上行收益的风险结构，使'避免灾难'比'追求最优'更理性。 | 2 | synthesis | confirmed | 风险不对称, 不对称风险, 下行风险, 毁灭性风险, 尾部风险, 凸性风险, risk asymmetry, tail risk |

## 边摘要

```
=== L0 公理 ===
agent --enables--> value
agent --enables--> tension
value --enables--> tension
value --enables--> goal
value --enables--> need
context --enables--> tension
context --enables--> constraint
tension --enables--> decision
tension --enables--> risk

=== L1 核心 ===
goal --depends-on--> agent
goal --depends-on--> value
goal --depends-on--> context
goal --enables--> decision
goal --enables--> risk
goal --part-of--> tension
need --depends-on--> agent
need --depends-on--> value
need --depends-on--> context
need --enables--> decision
need --contradicts--> goal (hypothetical)
constraint --depends-on--> agent
constraint --depends-on--> context
constraint --enables--> decision
constraint --enables--> risk
constraint --contradicts--> goal
decision --depends-on--> agent
decision --depends-on--> tension
decision --depends-on--> constraint
decision --depends-on--> goal
decision --depends-on--> need
decision --enables--> risk
risk --depends-on--> tension
risk --depends-on--> context
risk --depends-on--> goal
risk --depends-on--> constraint
risk --enables--> decision

=== L2 支撑 ===
latent-goal --part-of--> goal
latent-goal --depends-on--> agent
latent-goal --depends-on--> context
latent-goal --contradicts--> goal
goal-drift --part-of--> goal
goal-drift --depends-on--> goal
goal-drift --depends-on--> context
goal-drift --depends-on--> tension
goal-drift --contradicts--> goal
pseudo-need --part-of--> need
pseudo-need --depends-on--> need
pseudo-need --depends-on--> agent
pseudo-need --depends-on--> context
pseudo-need --contradicts--> need
latent-need --part-of--> need
latent-need --depends-on--> need
latent-need --depends-on--> agent
latent-need --depends-on--> value
latent-need --contradicts--> pseudo-need (hypothetical)
hard-soft-constraint --part-of--> constraint
hard-soft-constraint --depends-on--> constraint
hard-soft-constraint --depends-on--> agent
hard-soft-constraint --depends-on--> context
hard-soft-constraint --enables--> decision
constraint-cascade --part-of--> constraint
constraint-cascade --depends-on--> constraint
constraint-cascade --depends-on--> context
constraint-cascade --depends-on--> tension
constraint-cascade --enables--> risk
decision-traceability --part-of--> decision
decision-traceability --depends-on--> decision
decision-traceability --depends-on--> agent
decision-traceability --depends-on--> context
risk-asymmetry --part-of--> risk
risk-asymmetry --depends-on--> risk
risk-asymmetry --depends-on--> value
risk-asymmetry --depends-on--> tension
risk-asymmetry --contradicts--> goal (hypothetical)
```
