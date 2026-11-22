# 迁移与验证计划

> 上游：[system.md](system.md)、[data-model.md](data-model.md)、[storage.md](storage.md)。
> 目标为将知识库的持久化权威切换至自研 sage-db。Bun 继续提供原域名和公开 REST API，
> 经私有数据库接口访问新权威。切换后 SQLite 和 Markdown 只保留为迁移或编辑格式，
> 在线读写均使用 sage-db。

## 1. 输入边界

支持两个入口：旧 Markdown KB 和旧服务导出的中立 archive。
若当前权威是结构化存储，必须由旧服务/一次性导出工具先产生 archive；不能只读
concepts/*.md 作为最新知识。旧格式 reader 只属于切换工具，不进入 sage-db 运行时。
当前服务还没有完整的全库导出端点，P5 必须实现并验证该工具，不能将文档投影
当作已经具备全量无损导出的证据。

archive 包含 format_version、source_kb_id、source_snapshot_id、source_digest、
记录 NDJSON、blob 清单与字节、原始文件校验和、声明身份及可用历史。
导出必须停写或使用一致性快照；资产与会话文件同时冻结，不能跨时刻拼接。

## 2. 完整映射

| 原对象 | sage-db 对象 | 验收 |
|---|---|---|
| concepts | Term + body blob | slug、atom、layer、domain、source、confidence、triggers、正文一致 |
| relations / Markdown edges | 独立 Assertion | 原始方向、注解、声明顺序与来源不丢；不自动归约 |
| claims | Claim | kind、文本、状态、原修订/时间可追溯 |
| evidence + evidence_targets | Evidence + EvidenceLink | 所有目标有映射；relation 目标映射到具体 Assertion |
| assets | Asset + blob | 类型、长度、SHA-256、原字节一致 |
| vectors | Vector + blob | model、dimension、revision、float32 字节一致；过期行保留但不参与检索 |
| proposals | Proposal + 历史审阅记录 | pending/approved/rejected/conflicted、payload、理由、作者/审阅者不丢 |
| frontier.md | FrontierEntry + 原文件 blob | 所有分节文字保留；不能仅保留第一张表 |
| checkpoints/*.json | SessionCheckpoint | concept/session、正文、修订和身份对应 |
| kb.json / registry / protocols / AI.md | KB 元数据与附件引用 | 属于 KB 的说明完整归档；部署 registry 另行切换 |
| MANIFEST.md / graph.json | 历史附件；新的派生查询 | 不能再成为写入真源 |

导入计划为每条源记录分配确定的 target ID，并持久保存映射和源校验和，保证重试
不重复生成对象。原标识以 legacy_id 保留，旧公共 slug 保持稳定。
不能恢复的执行者标 legacy_declared；从未归档的旧审计历史标 unavailable，不能编造。
已归档但字节缺失/哈希不符属于损坏，必须 fail-closed（CORRUPT），不能标成 unavailable 了事。
旧证据没有目标版本时，绑定导入时可确定的源版本并标 version_binding=import_baseline，
不声称该证据曾被审阅为支持全部历史修订。证据的声明采集者进入 `declared_captured_by`，
与导入执行者分开保存；不把声明值 relabel 成认证身份。

旧 pending 提案的基线若无法可靠映射成 sage-db 修订向量，保留原 payload 和基线，
标 conflicted 并说明 needs_rebase；不得自动批准，也不得强行绑定最新记录。
历史 approved/rejected 记录保留为审计，不再次应用。

## 3. 导入状态机

prepare → validate → import-to-new-root → verify → activate。
失败停在原阶段并输出逐对象报告；任何阶段都不改正在服务的源库。

1. 冻结输入，生成计划、ID 映射和 expected counts/hashes。
2. 逐条检查身份、字段、引用、blob、重复 ID；汇总全部错误，禁止遇一条就隐式成功退出。
3. 在未对外路由的新 sage-db 目录写入。大库按有界事务分批提交，记录 import_session_id
   和进度；批次可恢复，但 import_complete 之前新库不接受普通读写。
4. 重启新库、核对全量记录/哈希、查询与历史证据。完成标记在全部校验通过后提交。
5. 停止旧权威写入后最终核对 source_digest，确认 Bun 的必需公开读写路径已接入
   sage-db。切换 Bun 的存储连接并验证原客户端通过原入口读写。

断言或证据无效时默认阻止 activate。管理员若决定排除，必须提供明确的 exclusions
清单及原因，报告排除前后 counts/hash；不能把“invalid=1、stored=0”称为迁移完成。
导入重试读取已持久计划与 outcome；输入变化要求新计划，不盲目重导覆盖已审阅记录。

## 4. 规则与输出兼容

新建 KB 默认 formal-v1；迁移 KB 默认 legacy-v1，保留现有 layer 与合法层向。
升级 formal-v1 必须单独 dry-run 输出新增 depends-on、违规层向、多实例目标和混合环，
经显式管理操作后再提交规则版本；存储切换不自动改变规则配置。

两种配置都修复最宽路径算法。对称多来源也按 max 聚合，与旧代码取较弱值不同。
这些属于明确列出的语义修正，影子对比要报告每条变化及证据，不能把差异当噪声忽略。

API 兼容适配器保留既有资源与文本格式，新增基线向量/身份字段采用版本化 DTO。
文档输出排版可以规范化，但原始正文与旧文件作为附件保留；声明顺序需要兼容的
接口从导入顺序字段恢复，不能依赖新内部 ID 的排序。

验收不能只比较闭包条数：同时比较作者支撑、各事实注解、可复核证明、正文、
检索轴、证据链接、提案状态、资产字节和未来撤回后的行为。

## 5. 切换与恢复

切换期间禁止双写。旧数据快照只作为迁移输入/审计备份，不作为在线后备数据库。
启动配置显式指定 engine=sage-db 与数据目录；sage-db 缺失/损坏时失败，不回退旧服务。

在 sage-db 接受新写之前，可以取消切换并回到被冻结的源快照。
接受新写之后不能简单启动旧快照作为回滚，否则会丢失新事务；应修复 sage-db 或
从其已验证备份/日志恢复到新目录。逆向转换不属于 v1 承诺。

## 6. 验收矩阵

| 类别 | 必测反例与结果 |
|---|---|
| 基线 | 当前仓库 336 项测试是旧实现回归起点，不代表新引擎通过 |
| 导入 | 无变化重复导入不删已审阅关系；cross-KB/重复ID/坏blob不能生成成功切换 |
| 身份 | subject 与 payload.from 不同必须拒绝；unknown slug 不被 atom 误命中 |
| 审阅 | 两客户端基于同一对象修订，只有第一笔批准；无关对象不产生假冲突 |
| 生命周期 | 关系撤回后历史证据仍可解释；claim 修改后旧证据标 historicalSupport |
| 语义 | 三档最宽路径、仅 contested、多来源、同强度平局、R3、撤回最后支撑 |
| 分层 | 默认 KB、review-tension、五份 L2 草案、L3→L1 实例均保留语义 layer |
| 持久 | 各刷盘/rename 点故障、掉电模型、COMMIT_UNKNOWN、重启后同 request_id |
| 缓存 | 缺缓存可查询，错 digest 不可复用，取消任务不能发布半份缓存 |
| 资源 | 长链/密 DAG、巨型正文、旧快照 pin、向量扫描均在预算内结束 |
| 格式 | golden fixtures、未知 required tag/major 拒绝、旧 major 迁移无记录损失 |
| 备份 | 在线备份后独立开库并全量 verify；无 complete 标记不能恢复 |

## 7. 已知审查证据

2026-11-16 的代码审查在临时 KB 中复现了：重导入丢关系、跨库错误提前提交、
提案对象/基线错配、损坏库静默降级、证据悬空、未来 schema 不被拒绝、首达路径
置信度偏低。以上进入新引擎验收反例，不要求先继续扩展旧数据库才能开始自研。
设计优化阶段没有修改当前生产实现，也没有执行真实数据迁移。
