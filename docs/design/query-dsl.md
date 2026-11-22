# 查询与命令契约

> 模型：[data-model.md](data-model.md)；预算：[inference.md](inference.md)；传输：[server.md](server.md)。

## 1. 三层接口

CLI 文本命令 → 类型化 Query/Command → 固定执行计划。
文本语法不是通用图语言；客户端不能提交规则或任意递归表达式。
CLI、REST 兼容适配器和 protobuf 使用同一动词注册表，不能维护互相漂移的能力清单。

v1 不先实现通用 Rel IR。固定计划足以支持既定负载；内部算子只有
Lookup、Scan、Expand、Reach、Filter、Rank、Project、Explain、Diff。
未来组合需求出现后再提炼 IR，不为序列化一个尚不存在的优化器增加维护面。

## 2. 动词与语义

| 动词 | 参数摘要 | 语义 |
|---|---|---|
| manifest | filter、limit | 概念摘要的确定性扫描 |
| document | term | 正文 + 可编辑投影 + 基线向量 |
| claims / evidence | typed target | 当前与历史支持分开返回 |
| edges | term、relation?、direction、include_derived | 默认作者支撑；可显式请求语义边 |
| requires | term、min_conf?、proof? | depends-on 最宽路径，前提完整性显式报告 |
| load-next | term | enables 一跳，不推导闭包 |
| argues | term | contradicts 双向聚合，保留全部支撑 |
| activate | term、strategy、depth、context_budget | 有预算的概念导航，不把导航路径当作蕴含 |
| explain | from、relation、to | 一条确定的最强证明，或指定版本内不可推导 |
| search | text、vector?、model?、scope? | BM25 / 余弦 / RRF；关系与相似度分开 |
| validate | scope | V1–V7 与资源诊断 |
| diff | from_version、to_version、scope | 权威记录的修订差异；派生差异显式 opt-in 且有界 |
| tier | term | layer 与 definition_depth 分别返回 |
| stats | — | 基础记录、缓存、资源与提交统计 |
| proposal submit/get/list/review | typed payload、baselines | 持久候选和审阅，不直接写事实 |
| asset upload / vector upsert | typed content | 独立有界事务 |
| frontier / checkpoint | typed operation | 会话状态读取与 CAS 写入 |
| admin import/export/backup/restore/verify | typed args | 管理操作，不能由普通查询触发 |

assert/retract 是 Proposal 中的变更操作，不是绕过审阅的公开写动词。
受信任导入通过单独的 admin 路径。高级管理员也要留下操作原因与审计记录。

## 3. 路径与身份

+ 仅用于声明 transitive 的关系，至少一跳；* 允许查询零跳 Identity。
非传递关系的 +/* 返回 RELATION_NOT_TRANSITIVE，不能返回空集。
有界 navigation 使用单独操作，结果标为路径而非 Fact。

未知概念是 UNKNOWN_TERM；已退役对象可在历史查询中读取。
公开概念名使用稳定 slug；内部数值 ID、Version、Revision 在 JSON 中编码为十进制
字符串，避免 JavaScript 的整数精度损失。响应携带 kb_uuid，不用 atom 做身份匹配。

## 4. 结果信封

    Result {
      kb_uuid, version, rules_digest, engine_version,
      rows, proof?,                  # proof roots are per row
      output: Full | Truncated { by, returned },   # display prefix only
      cache: Hit | Miss,
      stats: { scanned_nodes, scanned_edges, steps, work_bytes, elapsed_us }
    }

    # 计算失败不是 Result 的变体：预算/证明/取消/deadline 耗尽返回类型化错误，
    # 不返回 rows。因此“没有这一行”只可能来自一次完整求值。

Fact 行包含端点、关系、effectiveConfidence、asserted、derived、争议标记，并携带该行在
proof arena 中的根（`proof?`）。`with_proof=true` 时**每一行**都必须有可解析的根；空结果
不得携带任何根。根属于行，不属于“整个结果的最后一个节点”。

**P0 行不是“全部支撑”视图。** 内部 Fact 行只包含：

| 字段 | P0 内容 |
|---|---|
| 一个直接作者支撑 witness（若本方向存在） | 最强那条的 assertion id + revision + confidence，或 None |
| `disputed` | 任意有效支撑/路径存在 contested 的布尔标记 |
| `proof` | 一条选定的最强证明根 |

它**不含**全部支撑列表，也不含全部证明路径。最终查询层的“全部支撑引用”需求仍然成立，
但它由后续 query 层从**同一 snapshot** 的权威 Assertion 索引按有界预算读取明细实现，
不是 P0 内部行数组的职责。P0 不新增无界 support 数组。

因此禁用一种“实现”：为了让支撑视图变小而丢弃争议支撑。`disputed` 与 confidence 必须由
**全部**活动支撑聚合得出；明细可以按需分页读取，聚合不可以。
Claim/Evidence/文档不是 Fact，使用各自 typed row，不能强行套成 (TermId, Annotation)。
查询执行缓存状态与知识权威状态分离；不能用 Rebuilt 暗示数据比 Hit 更新。

## 5. 预算、完整性与排序

每请求都有 max_steps、max_work_rows、max_work_bytes、max_rows、max_output_bytes、
max_proof_nodes、deadline_ms。服务端作预估以拒绝明显超限请求，运行时仍逐步计费。
估计不是停止检查的理由。

- requires/explain/事实点查/search/语义 diff 是精确计算。工作预算耗尽返回
  BUDGET_EXCEEDED（或 deadline/proof 对应错误），不把中间候选当最终答案。
- 精确计算在工作预算内完成后，按确定全序排序；显示上限（max_rows /
  max_output_bytes）可以只输出前缀。截断是**输出**属性，在执行**之后**应用，
  不表示计算被中断，也不改变“哪些事实成立”的结论。
- manifest/导航允许显式 depth/行数/上下文字节限制，报告未展开边界。
  activate 还返回 prerequisites_complete；不足时不能宣称推理上下文完整。
- 未完成查询不能回答 absent：中断根本没有结果。完整求值下的 absent 也仅指当前规则内
  不可推导。
- 同一事实的多条支撑按注解取 max，但争议标记独立保留：有 confirmed 与 contested
  支撑时 effectiveConfidence 为 confirmed，同时 disputed=true 并返回全部支撑引用。
  多个支撑“谁算数”由规则决定，不由返回顺序决定。

图事实排序：(confidence desc, layer desc, endpoint_id asc, relation_id asc)。
记录列表按 stable ID；搜索按融合得分 desc、kind、ID；证明按 infer 的平局规则。
这些排序不是依赖拓扑序。requires 另提供 prerequisite_order（按当前规则补齐单跳种子后依赖 DAG 的
前置优先拓扑序），供需要先加载前提的消费者使用，不能用 layer 排序代替它。

排序分两层，不能混用：

- **规范顺序**（digest、差分比较、缓存序列化用）：`(to, relation_id, from)`。
  它与展示无关，只要求同一组事实在任意输入顺序、任意缓存状态下给出相同序列。
- **展示排序**（上面的 confidence desc / layer desc 等）：由查询层在规范结果之上
  应用，属于呈现策略，不进入 result_digest。

`result_digest` 只覆盖规范事实行与证明语义，不覆盖 elapsed/cache 等执行统计。

max_rows 只限制显示不限制计算；max_output_bytes 同时限制正文/证明字节，
不能仅靠 5,000 行阻止单条巨大文档耗尽客户端上下文。

P0 的 `SourceExecutor` 是内部完整事实求值层：只受 steps/work_rows/work_bytes/
proof_nodes 与 control 约束，返回完整事实集合，**不**应用 max_rows/max_output_bytes。
输出限制属于后续 query/展示层，它在精确计算与规范排序之后才可截断；因此展示上限
不可能制造 absent。文档写回使用完整导出，
超限报错，不能导出被截断却看似可安全写回的 Markdown。

## 6. 流式与版本

v1 没有分页 cursor。精确查询先在有界工作区完成计算和排序，再流式传输：

    Header { kb_uuid, version, rules_digest, engine_version, query_id }
    Row { ..fact, proof_root? } / ProofChunk ...
    Footer { output: Full | Truncated { by, returned }, stats, result_digest }

Header 不承诺尚未知道的截断情况：截断是输出属性，只有 Footer 才权威。客户端必须收到
成功 Footer 才接受结果；连接中断、取消、错误终帧都表示此次结果未完成，而“未完成”不是
“空结果”。Unary JSON 把相同字段放在一个信封里。
流式降低传输峰值，不宣称消除了求值工作区或排序内存。

流式不改变计算契约：Frame 序列在 Header 之后、Footer 之前只承载已经完整计算出的
事实，不承载“暂时置信度”。计算阶段失败不会开始发送 Row。

每请求开始取得一个 Snapshot 引用，全文与证据也从它引用的 blob 读取。
at_version 钉版不存在时返回 VERSION_TOO_OLD，不自动改查 latest。
同一版本的语义行、排序、证明和 result_digest 必须可复现；elapsed/cache 等执行统计
允许不同，不属于确定性摘要。

## 7. 混合检索

BM25 使用可重建倒排索引，分词版本进入 index_key；向量使用外部上传值，
按 model_id/维度筛选后做有预算的精确余弦扫描。v1 不引入 ANN 索引。
RRF 按轴排名融合，不能直接相加 BM25 与余弦原始分数。

索引绑定 (search_digest, tokenizer_ver, index_ver)，与图缓存分别失效。
search_digest 对可检索记录的类型、ID、修订、活动状态、scope 字段与文本 blob 哈希
规范排序后求摘要；不能只散列正文而遗漏可见范围或记录退役。不同于单个 blob 的
content_digest，它标识当前快照的完整检索投影。
没有匹配索引时可在预算内扫描重建；否则返回 INDEX_NOT_READY，不能用旧索引
或空结果冒充当前结果。构建失败不阻止其他基础图读写。

输出保留 signals、assertedRelations、vectorAssociations、claims、evidence；
相似度只描述本次查询、同一模型的测量。过期、缺失、非法向量分别统计。
scope 过滤在读取目标索引时执行，不先加载全库正文再筛选。
