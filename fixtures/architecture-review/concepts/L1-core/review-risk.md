---
id: review-risk
atom: "风险是决策在前提失效时暴露的损失，大小取决于损失幅度与前提失效概率。"
layer: 1
domain: [architecture]
confidence: confirmed
source: synthesis
triggers: [风险, risk, 损失, 失效]
edges:
  - type: depends-on
    target: review-decision
    confidence: confirmed
  - type: depends-on
    target: review-constraint
    confidence: confirmed
---

# 风险 Review Risk

## 核心内容（molecule）

风险不是"可能出问题"的笼统担忧。可审查的风险要说出：哪一个前提失效、
失效后损失是什么、用什么信号能提前观察到。

没有观察信号的风险无法在事后区分"运气好"与"设计好"。
