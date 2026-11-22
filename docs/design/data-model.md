# 数据模型

> 上游：[system.md](system.md)；语义：[axiomatics.md](axiomatics.md)；迁移：[migration.md](migration.md)。

## 1. 权威记录与派生视图

所有记录变更编码为事件，提交后不可改写历史。当前记录可以有新修订；
这与“日志只追加”不矛盾。Fact、CSR、BM25 和闭包缓存都是可重建视图。

| 记录 | 必需内容 | 生命周期与约束 |
|---|---|---|
| Term | id、slug、atom、layer、domain、confidence、source、triggers、body_ref | active / retired；正文不可遗漏 |
| RelationSpec | 固定关系 ID、transitive、symmetric、acyclic、layer_rule、implies_dependency | 随版本化规则配置变更；不是客户端自由脚本 |
| Assertion | assertion_id、from、relation、target、confidence、revision、actor、reason、declaration_order | active / retracted；独立来源不合并 |
| Claim | claim_id、concept_id、kind、text_ref、confidence、status | accepted / superseded / rejected |
| Evidence | evidence_id、kind、title、locator、excerpt_ref、captured_at/by | 更正产生新修订，保留旧记录 |
| EvidenceLink | evidence_id/revision、target_kind/id/revision | 目标为 Term / Claim / Assertion 的特定修订 |
| Asset | sha256、media_type、byte_size | 引用不可变 blob；内容与声明校验一致 |
| Vector | subject_kind/id/revision、model_id、dimensions、embedding_revision、values_ref | 同 subject/model 当前一份；历史留存；过期不参加打分 |
| Proposal | id、kind、subject、base_revisions、payload_ref、reason、author、status | pending / approved / rejected / conflicted，均持久化 |
| FrontierEntry | id、concept_refs、text、priority、status | open / resolved；保留原始说明 |
| SessionCheckpoint | id、session 定位、content_ref、revision | 原生定位使用 Term 键和 u32 编号。公共定位使用 slug 和正的 JS 安全整数 |
| RequestOutcome | principal、request_id、payload_digest、result、commit_version | 幂等重试依据，与事务一起持久化 |

created_at、updated_at、revision 是公共元数据；事件还记录**执行者**（已验证 principal）
与原因。文档里声明的 author/capturedBy 是**来源属性**，与执行者并存但不互换：

| 字段 | 含义 | 来源 |
|---|---|---|
| `recorded_by` | 真正执行这次写入的已验证身份 | 服务层认证结果 |
| `declared_author` / `declared_captured_by` | 文档/证据声明的作者与采集者 | 原文，原样保留 |

旧系统只有声明值，因此导入时 `recorded_by` 标 `legacy_declared`（缺失，不编造），
声明值原样进入 `declared_*`。**不做 relabel**：把 `capturedBy` 提升为执行者会永久
丢掉“谁实际提交”的审计信息，也无法在日后与真实认证身份区分。

部署级凭证配置不是知识记录，备份契约单独列出它的责任人。

## 2. 身份、版本与引用

- 每 KB 有不可变 kb_uuid。TermId 为库内 u32，其他逻辑记录使用 128-bit ID。
  对外引用携带 KB 身份；所有 ID 永不复用，溢出时拒绝写入。
- slug 是概念对外稳定名称，v1 不支持修改或复用；atom 是可编辑摘要，不是字典键。
- Revision 是记录内单调 u64。任何状态或内容变化都递增，避免内容改回原样时的 ABA。
- Version 是 KB 内提交序号；提案提交也递增 Version，但不改变活动事实。
- content_digest 表达字节内容；revision 表达编辑历史；两者不能互换。
- 文档导出同时给出 document_hash 和 base_revisions（含关系集合修订）；
  编辑提交必须带基线向量，不能仅凭正文哈希判断审阅期间是否发生变化。

同一 (from, relation, target) 可以由多条 Assertion 支撑。幂等按请求身份判定，
不按三元组判定。修改置信度产生该 Assertion 的新修订；撤回点名 assertion_id。
Fact 按三元组聚合活动支撑，注解取 max；作者、证据、时间仍保存在支撑记录中。
对称边可以规范化计算键，但原始 Assertion 的方向和来源不能被重写。

引用在整个候选事务上校验，因此同事务可以先建多个节点再连边。
EvidenceLink 绑定被审阅的具体修订：claim 改写后，旧证据不自动成为新内容的支持。
默认响应区分 currentSupport 与 historicalSupport。

删除语义统一为退役/撤回。Term 退役时，事务必须同时撤回所有活动入/出断言，
并处置所属活动 claims；否则 REFERENCE_IN_USE。证据仍可查看历史目标。
只指向历史修订的向量不得参加当前检索。

### 2.1 证据、资产与生命周期

Evidence 是权威记录，不是附件路径的别名：

| 元素 | 规则 |
|---|---|
| `evidence_id` | 128-bit，独立分配；证据更正产生新 revision，旧记录保留 |
| `kind` | `text / image`，与现有实现一致；新增 kind 需要单独批准的产品需求 |
| `title` | 必填，`u32` 长度前缀 UTF-8，≤ 4 KiB |
| `locator` | 可选自由定位串（URL、仓库路径、页码）；**不是**库内对象引用，不参与引用校验 |
| `excerpt_ref` | 可选 blob 哈希；摘录正文按 blob 访问，不放进事件 |
| `asset_sha256` | 可选，指向 Asset；`kind=image` 时必填 |
| `captured_at` | 提交者写入的采集时间（文档声明的采集时刻，非提交时刻） |
| `declared_captured_by` | 文档声明的采集者，是**来源属性**，不是认证身份 |
| `recorded_by` | 已验证 principal（事件 actor），由服务层写入 |

`declared_captured_by` 与 `recorded_by` 必须分成两个字段：现有 bug 是把文档里的
`capturedBy` 当作执行者，导入后无法再区分“谁真的做了这次提交”和“作者声称的证据
由谁采集”。迁移时两者都原样保留，**不做任何 relabel**（migration §2 的
`legacy_declared` 只用于缺失的执行者，不用于把声明值升级成认证值）。

EvidenceLink 是独立记录：`(evidence_id, evidence_revision, target_kind, target_id,
target_revision)`。目标修订必须**已存在**且属于同一 KB，否则 REJECTED
（不属于“稍后补”的悬空引用）。

current / historical 是**读取时派生**的分类，不是持久化状态：

    current     ⇔ target_revision == current_revision(target)
    historical  ⇔ 其它（目标已有更新修订、已退役，或目标记录仅存历史）

因此目标被编辑或退役时，链接记录本身**不变**，只改变派生分类；不存在“每次目标
编辑都改写所有入向链接”的权威突变。查询默认只返回 current，显式请求历史返回全部。

Asset 是不可变内容记录：`sha256`、`media_type`、`byte_size`、`declared_by`、
`declared_at`。生命周期与 Evidence 解耦：

- 声明后内容不得改动；字节与声明不符即 REFUSED，不写入。
- v1 不因没有活动 EvidenceLink 就删除 blob；孤儿资产只报告，不自动回收（见 storage §7）。
- 媒体类型以字节签名判定，与声明不符即拒绝；SVG 不在允许列表。
- 资产字节从不进入事件流，只以摘要引用；摘要是未加域前缀的 SHA-256（storage §4.3）。

### 2.2 KB 元数据、模型目录与导入记账

这三类记录同样是权威事件，不能只存在于配置文件或部署脚本里：

| 记录 | 必需内容 | 说明 |
|---|---|---|
| KB 元数据 | `kb_uuid`、slug、name、description、domains、created_at、revision | genesis 事务的一部分；slug 与 Term slug 分属不同命名空间 |
| 模型目录 | `model_id`、dimensions、标识算法、provider、revision、status | 向量必须引用已登记模型；退役模型的历史向量保留但不再参与检索 |
| 导入记账 | `import_session_id`、stage、source_digest、plan_ref、expected_counts、import_complete | 让导入可断点续跑、可对账；`import_complete` 之前新库不接受普通读写 |

`kb.json`、模型配置与 `MANIFEST.md` 是**本库投影**，可从该 KB 的日志重建；把它们当作
唯一真源会让重启后的元数据与知识记录不一致。

`registry.json` 是**部署级**记录（哪些 KB 被挂载、别名与路由），不属于任何单个 KB 的
日志，也**不能**从某个 KB 的日志重建。它与其凭据、连接串一起由部署配置单独管理并
单独备份（server §5）：备份一个 KB 目录不会备份“它在哪台机器上以什么名字被服务”。

导入记账同理属于该 KB：`import_session_id` 绑定一个源快照，不表达部署拓扑。

### 2.3 历史修订的定位与读取

证据、提案基线与审计都引用具体修订，而在线可直接查询的只有 8 个版本
（server §4）。历史修订因此需要独立于整库快照的定位方式：

    RevisionIndex:
      (record_kind, record_id, revision) -> (version, tx_id, event_seq, value_ref)
      (record_kind, record_id)            -> 修订升序列表

规则：

1. 索引是**可重建投影**，从已提交 tx 段重放生成；它不是权威，损坏即重建。
2. 它按修订记录，不按版本保存整库快照：读取历史修订只需该 record 的那条事件与其
   引用的 blob，不需要把任何整个版本载入内存。
3. 事件按 `(version, tx_id, event_seq)` 单调追加，因此索引条目一旦写入不会改变；
   重放可以从最后一个已确认条目续建，不必从头扫描。
4. 在线查询窗口只限制“按版本整库读取”，不限制按修订读取单条记录。
   响应必须标注 `source_version`，让调用者知道它是从历史事件重建而非来自当前快照。
5. `RevisionIndex` 条目缺失但 tx 段存在时按需重建该记录；重建需要扫描日志前缀，
   计入查询预算，超限返回 BUDGET_EXCEEDED 而不是“记录不存在”。
6. 记录从未存在返回 ABSENT。**权威字节缺失或哈希不符返回 CORRUPT**（system I3 /
   storage §6），不是普通的 UNAVAILABLE：那是“这个库已经损坏”，调用方不能把它当
   作一次可重试的暂时失败继续服务。UNAVAILABLE 仅用于迁移时明确标注为
   `unavailable` 的无历史记录（migration §2 允许标注为缺失的旧审计信息）。

单条修订的**可重建性**必须由事件本身保证，不能依赖“重放整库”：每个记录变更事件
必须携带足以重建该修订的完整 after-image（或相对基线的 delta，且基线修订在同一
日志前缀内可取）。v1 选择 after-image：单条记录的状态字节远小于整库，代价是日志
变大，换来“按修订读取不需要重放无关记录”。delta 方案只有在能证明任意修订都能在
有界前缀内重建时才可采用。

checkpoint 与重放义务：checkpoint 只加速“当前状态”的加载，**不替代**历史修订
的可追溯性。任何丢弃 tx 段或 blob 的操作都必须同时使对应 `RevisionIndex` 条目
不可声称可用；v1 不做截断，因此历史修订在保留窗口外仍可按修订读取（代价是有界
日志扫描）。历史读取的资源边界：单次历史读取只允许访问该记录的事件与其 blob，
不得因解析一条历史证据而加载无关记录或整个版本。

| 情形 | 结果 |
|---|---|
| 修订在索引中且事件与 blob 校验通过 | 返回记录 + `source_version` |
| 索引缺失、日志前缀完整 | 有界重建后返回；计入预算 |
| 日志前缀存在但所需 blob 缺失/哈希不符 | **CORRUPT**：停止该库正常服务，显式恢复备份 |
| 迁移标注为 unavailable 的历史审计 | UNAVAILABLE（记录存在，内容从未归档） |
| 记录或修订从未存在 | ABSENT |
| 重建超预算 | BUDGET_EXCEEDED；不得回答 ABSENT |

## 3. layer 与定义深度

layer 仍是 0..3 的语义分类，作为 Term 的单一权威字段保存。
导出目录名由它生成；导入时目录与字段不符必须报告，不能出现第二权威。
axiom 是 layer == 0 的派生判定，不再另存可独立修改的 bool。

另算 definition_depth：在定义关系 DAG 上，无父节点为 0，其他节点为
1 + max(parent.depth)。它是拓扑指标，不限制为 0..3，也不替代 layer。

因此 L3 实例可以指向 L1；L2 没有定义性边也不会自动变 L1。
默认库 22 个概念碰巧能按旧公式复原，不能推广成所有 KB 的约束。
审查夹具 review-tension 与五份 L2 概念草案必须保持原 layer。

## 4. 关系模式

| 关系 | transitive | symmetric | acyclic | layer_rule | 定义关系 |
|---|---|---|---|---|---|
| is-a | 是 | 否 | 是 | target <= source | 是 |
| part-of | 是 | 否 | 是 | target <= source | 是 |
| instantiates | 否 | 否 | 是 | target <= source | 是 |
| depends-on | 是 | 否 | 是 | 不限 | 否 |
| enables | 否 | 否 | 否 | 不限 | 否 |
| contradicts | 否 | 是 | 否 | 不限 | 否 |
| analogous-to | 否 | 是 | 否 | 不限 | 否 |

固定关系 ID 与规则模板受引擎支持集限制，新增关系仍需契约/客户端兼容测试，
不能承诺“改一行数据就支持任意语义”。UI 从服务端获取模式，消除散落的对称判断。

两种明确版本化的配置：

- legacy-v1：保持当前边的层向、同关系传递和对称补全，不新增定义蕴含。
- formal-v1：定义边蕴含 depends-on；定义关系目标 layer 严格更小；
  instantiates 每主体至多一个不同目标，且活动 L3 至少有一个实例化目标。

formal-v1 的定义关系与 depends-on 并集必须无环。规则配置与模式全文摘要进入
rules_digest，**规则 profile 本身也必须随快照传递**：digest 是不透明的比较值，只凭它
无法判断是 legacy-v1 还是 formal-v1，因此快照与查询都携带 (profile, rules_digest)，
并在求值前检查两者一致，不一致直接拒绝而不是挑一个。`engine_version` 是第三个独立
字段（实现版本），不参与规则选择也不进入 graph_digest（storage §4.3）。
更改配置是一笔显式迁移事务，先 dry-run 输出违规与事实变化，
不在导入时自动升级。最宽路径注解算法适用于两种配置，其与旧实现的差异须单列。
新库默认 formal-v1，旧库迁移默认 legacy-v1。profile 只选择已实现的固定规则模板，
不引入可执行脚本或任意用户规则。

## 5. 写入与审阅

知识修改走 Proposal → Review；管理导入是唯一明确授权的例外，并记录导入来源。
向量/资产上传、会话状态保存可走有类型的独立事务，不伪装成已审阅知识断言。

批准事务执行：读取最新状态 → 检查 reviewer 权限 → 检查 proposal 为 pending →
检查所有 base_revisions → 构造候选记录集 → 校验引用与图约束 → 应用内容与
proposal=approved → 持久提交。失败不能提前标 approved。

TermCreate 提案提交时在同一事务中预留新的 Term ID 与 Proposal ID。subject 是要创建的
Term 键，此时不存在对应记录，也没有现有修订基线。审批检查该键与 slug 仍未被占用，
并用当前已提交的规则校验加入新 Term 后的候选图。审批成功原子写入 Term 初始修订、
提案状态和回执。拒绝或冲突不会创建 Term，预留 ID 永不复用。

关系提案的 subject 必须等于 payload.from；EvidenceLink 的全部目标加入基线集合。
关系集合修订在增删改任一支撑时递增。冲突结果将提案标 conflicted，作为独立的
持久事务结果返回；不得在事务外补写状态。重试用同 request_id 返回该原结果。

### 5.1 基线何时失效

基线只约束**提案自己声明要读写的对象集合**，不是整个 KB：

| 集合 | 来源 | 失效条件 |
|---|---|---|
| 写入集合 | payload 直接修改的记录（Term 正文/字段、Assertion、Claim、Evidence、关系集合） | 任一成员当前修订 ≠ 基线修订 |
| 读取集合 | 提案语义依赖但未修改的对象（父概念、被引用的证据目标、relate 的两端） | 任一成员当前修订 ≠ 基线修订 |
| 显式 `expect_version` | 调用方要求整库未变（仅管理操作） | 当前 Version ≠ 期望 Version |

由此得到两条必须同时成立的规则：

1. **批准时**检查等式：`current_revision(member) == base_revision(member)` 对读取与
   写入集合的每个成员都成立，否则提案转 conflicted（持久结果，可重新提交）。
2. **普通提交时**不检查任何提案的基线。一个无人处理的 pending 提案随时可能变陈旧；
   它的陈旧**不得**让无关的知识修改失败。只有被显式批准的那个提案会因其自身基线
   不符而冲突。

被批准提案的应用与提案状态在同一事务内提交。因此“提案已 approved 但内容未写入”
不是合法中间状态；反之，写入成功但状态未更新也不能发生。

历史与完整性约束不受基线放宽影响：

- 目标修订必须存在（历史修订也算存在，见 §2.3），引用不得悬空。
- EvidenceLink 的目标修订必须在批准时仍存在；它指向历史修订是合法的，
  指向不存在的修订是 rejected。
- 已 approved 的提案永不被再次应用；重试返回原 outcome。
- 管理操作可以用 `expect_version` 要求整库未变，但不得用它替代对象级基线检查。

因其他提案而被阻塞的只有一种情形：同一对象上存在另一个已被批准并提交的修改，
使本提案的写入集合成员修订变化。那时冲突是真实的，而不是“有别的 pending 提案”。

## 6. 文档与检索投影

Markdown 是完整记录的编辑投影，包含稳定 ID 与修订基线；正文、claims 可编辑，
证据/资产是只读投影，修改只读块应报错，不能静默忽略。删除 claims 行表示
superseded，删除关系表示撤回对应 Assertion，均保留历史和证据链接。

向量和断言永久区分。模型目录绑定 model_id、dimensions、标识算法；所有输入
转 float32 前后都检查有限性与非零范数。subject_revision 绑定生成向量时的对象修订，
embedding_revision 标识该向量的生成版本；检索前检查前者是否仍为当前对象修订。
失效向量列入统计，不当作当前语义证据。向量相似度不生成 Assertion。

## 7. 跨 KB 边界

v1 禁止跨 KB Assertion、EvidenceLink、事务和自动推理。外部地址可以作为
Evidence.locator，不冒充一个可解析的库内 TermId。foreign_ref 旁表推迟到有
明确命名空间、授权与失效契约后再设计；本版不承诺隐藏的跨库图能力。
