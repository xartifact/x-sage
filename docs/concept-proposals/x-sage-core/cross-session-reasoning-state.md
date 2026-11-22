---
{
  "id": "cross-session-reasoning-state",
  "atom": "把可恢复的结论、证据范围、未决问题和下一入口持久化，使后续会话能续接推理而不把历史假设当成事实。",
  "layer": 2,
  "domain": [
    "engineering"
  ],
  "confidence": "hypothetical",
  "source": "synthesis",
  "triggers": [
    "checkpoint",
    "frontier",
    "跨会话",
    "推理恢复",
    "会话快照"
  ],
  "edges": [
    {
      "type": "depends-on",
      "target": "agent",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "context",
      "confidence": "hypothetical"
    },
    {
      "type": "depends-on",
      "target": "goal",
      "confidence": "hypothetical"
    }
  ]
}
---

# 跨会话推理状态

## 核心内容（molecule）

会话结束后，主体的工作上下文不一定保留。跨会话推理状态把继续达成目标所需的信息外置，保留已确认结论、探索范围、未解决问题与恢复入口。其对象是可续接的推理任务，不是对所有对话逐字归档。

快照回答已经得到了什么、凭什么、在什么语境中成立；边界记录回答尚不知道什么、为何值得继续。恢复时应先检查时效和前提变化，再决定哪些结论可以沿用。历史记录只能提供待核实的起点，不能自动取得比当前证据更高的权威。

x-sage 通过 checkpoint 的 established、exploredEdges、frontier、nextEntryPoint 保存结构化状态；知识库 frontier 是跨会话未决边界。与 decision-traceability 的区别是：后者解释某个决策为何作出，前者恢复包括未决探索在内的工作状态，并不要求已作出决策。

## 张力与开放问题

压缩会丢失细节，全量保留又会重新占满上下文。保存时应标注证据和日期，并保持假设与已确认结论分离，避免多次转述逐渐放大置信度。

## 激活建议

先加载定义前提：[agent, context, goal]。需要具体实现案例时再加载 x-sage-concept-kb；该实例不是定义前提。

## 来源与状态

2026-11-16 从 x-sage 项目架构、边语义文档及已确认项目总览提炼。候选拆分及边尚待审阅，hypothetical 不表示已验证的普遍规律。
