# 架构审查固定案例（Architecture Review Case）

> 用途：验证结构化知识存储是否真的支撑一次技术架构审查——前提、冲突证据、
> 混合检索各自可追溯。它是**验收夹具**，不是产品内置的业务逻辑：服务端不认识
> 「架构审查」这个词，只提供概念、断言、证据、关系与检索。

夹具位置：`fixtures/architecture-review/`。

## 为什么需要固定案例

只看「能不能存下一条断言」无法判断系统是否可用。一次真实审查要回答的问题是：

1. 这个决策依赖哪些**前提**？前提里哪些是**未验证**的？
2. 哪些**证据**支持某个结论？证据原文在哪里？
3. 两条结论是否**冲突**？冲突各自依赖什么前提？
4. 换个措辞提问，还能找回来吗（同一件事的不同说法）？

前三个问题要求结构化记录；第四个要求混合检索。固定案例把答案写死，任何回归
都能被指出是「记录丢了」还是「检索退化了」。

## 知识库内容

| id | 层 | 内容 |
|---|---|---|
| `review-subject` | L0 | 审查边界：什么在范围内 |
| `review-premise` | L0 | 前提：设计依赖的未验证假设 |
| `review-constraint` | L1 | 约束：环境施加的不可选条件 |
| `review-decision` | L1 | 决策：含被否决替代方案的可追溯选择 |
| `review-risk` | L1 | 风险：前提失效时的损失与观察信号 |
| `review-tension` | L2 | 张力：两条都被证据支持的结论相冲突（`contested`） |

关系：`review-decision` depends-on `review-premise` / `review-constraint`；
`review-risk` depends-on `review-decision`；`review-tension` **contradicts**
`review-decision`（`contested`）。这一条 `contradicts` 是案例的关键：张力无法
靠检索消解，只能被并列展示。

证据文档（不导入数据库，作为证据的 `locator` + `excerpt` 来源）：

- `evidence/review-2026-11-04-capacity.md` — 容量评审：3 倍流量假设未压测。
- `evidence/review-2026-11-06-latency.md` — 延迟评审：满足当前约束，但线性外推
  到 3 倍流量即超限。这段记录本身包含张力。
- `assets/latency-chart.png` — 16×16 PNG，用于验证图片证据的内容寻址与取回。

## 固定断言（审查结论）

由测试以提案 + 审阅的方式写入，而不是烘焙进夹具文件——它们必须经过审阅门：

| 概念 | 断言 | kind | confidence |
|---|---|---|---|
| `review-premise` | 峰值 QPS 在 2027 年前不超过 3 倍当前值 | premise | hypothetical |
| `review-constraint` | p99 延迟必须低于 500ms | constraint | confirmed |
| `review-tension` | 满足当前约束与前提失效后的风险不同向 | risk | contested |

每条都挂证据：容量假设 → 容量评审记录；延迟结论 → 延迟评审记录 + 延迟曲线图。

## 期望的检索行为（确定性夹具向量）

测试用**夹具嵌入**，不是真实模型：把文本映射到 5 个轴的 0/1 向量
（`容量`、`延迟`、`约束`、`风险`、`决策`）。这样余弦值是确定的，断言可以写死，
也不会把随机向量伪装成语义搜索。

| 查询 | 期望 |
|---|---|
| 词法 `约束` | 首位是 `review-constraint`；`signals[0].axis === "lexical"` 且带命中词 |
| 词法 `延迟` | 命中 `review-tension` / `review-constraint` 等含该词的记录 |
| `限制`（只在 triggers 里） | `signals` 反映实际命中；不应伪造向量轴命中 |
| 向量查询（`约束` 轴=1）+ 无关关键词 | `review-constraint` 仍被找回，`signals` 含 `vector` 轴 |
| 任意混合结果 | `assertedRelations` 只含断言的关系；`vectorAssociations` 单独一栏，`cosine` 为 null 或实测值 |
| 改写某概念正文后 | 该概念的旧向量 `excludedStale` +1，且不出现在结果里 |

最后一条是过期向量的核心验收：**改过的文本不能继续用旧嵌入参与排序**。

## 夹具验证

旧服务的混合检索案例是历史记录。现有夹具仍供 Rust 推理与离线迁移测试使用。

```bash
(cd sage-db && cargo test -p sage-db-infer --test real_kb)
bun test tools/sage-db-source-archive.test.ts
```

## 案例证明了什么、没证明什么

证明：

- 前提可以被记录为独立断言，并标注 `hypothetical`——推理时不能当地基。
- 一条断言可以同时挂文本与图片证据，并反查回概念。
- `contradicts` 加 `contested` 让张力作为结构存在，而不是被平均掉。
- 混合检索返回匹配依据；改写正文后旧向量被排除。
- 未经审阅的提交不进入权威：提交后、审阅前检索不到。

没证明：

- 真实嵌入模型的质量——夹具向量只证明机制，不证明语义。
- 大规模性能（夹具仅 6 个概念；图是每次请求从行重建的）。
- 跨知识库审查：结构化存储一库一文件，跨库引用被显式拒绝。
