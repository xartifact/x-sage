# 存储与恢复协议

> 上游：[data-model.md](data-model.md)；服务行为：[server.md](server.md)。
> v1 使用自研不可变事务段与原子根指针。以下为待实现契约，不是已有持久性保证。

## 1. 故障模型与持久性

支持本地文件系统，要求同文件系统 rename 原子、文件与目录 fsync 的语义被平台兑现。
不支持网络共享目录、多进程共同写库或绕过引擎修改文件。
底层设备不兑现刷盘、介质永久损坏不属于 fsync 可以解决的故障，依靠独立备份恢复。

v1 只支持 durable：成功 ACK 之前，权威文件、目录项、CURRENT 均完成同步，
并且进程快照已经发布。不提供“先 ACK、每 64 次再同步”的 batch/none 档位。
未来 group commit 可以合并刷盘，但仍须等同组所有事务持久后才 ACK。

## 2. 文件布局与唯一提交根

    <kb>/sage-db/
      LOCK                        # 进程生命周期持有 OS 排他文件锁
      CURRENT                     # kb_uuid、version、manifest 文件名及其 SHA-256
      manifests/v000042-<uuid>.sgm # 不可变，指向前一已提交 manifest 与本次事务段
      transactions/tx-<uuid>.sgl  # 一次事务的完整有界操作集合
      blobs/<sha256>              # 正文、资产、向量等不可变内容
      checkpoints/cp-<uuid>.sgb   # 可重建的记录检查点，绑定 version + manifest hash
      cache/<key>.sgv             # 可删除的图/检索缓存
      staging/<uuid>              # 未提交文件；不是权威

CURRENT 是唯一提交根。编号最大的 manifest、完整的 tx 文件、写完的 Commit 帧，
都不单独证明事务已提交。每个 manifest 包含上一版本路径和哈希，形成已提交链。
初始化通过同一协议提交包含 KB 元数据的 genesis，之后才认为数据库已创建。
init 只能显式操作空的新目录，并同步新建子目录及其父目录；打开非空目录时缺失
CURRENT 是恢复错误，不能自动初始化。不可变文件名带唯一事务标识，避免崩溃遗留的
未提交 v+1 与重试生成的 v+1 同名；除 CURRENT 外发布不得覆盖已有不同内容。

路径只由引擎生成；加载时拒绝绝对路径、..、目录逃逸与不符合命名规则的引用。
跨设备 staging 不允许进入提交协议。OS 锁用来阻止另一个进程写同一目录；
“单写者”不意味着可以不做跨进程互斥。

## 3. 权威与重建材料

| 对象 | 权威性 | 恢复方式 |
|---|---|---|
| CURRENT + 已提交 manifest 链 + tx 段 | 权威提交历史 | 校验链后重放 |
| 已提交记录引用的 blobs | 权威内容字节 | 哈希验证；缺失不能凭摘要恢复 |
| 记录 checkpoint | 日志前缀的加速投影 | 缺失/损坏时从日志重建 |
| CSR / 闭包 / BM25 | 派生 | 按版本绑定的输入重新计算 |
| staging / 未被提交链引用的 manifest/tx | 非权威 | 确认无在飞写入后回收 |

日志覆盖所有元数据事件：创建/修订/退役 Term、Assertion、Claim、证据链接、
提案状态、会话状态、向量元信息、规则配置、ID 分配及 RequestOutcome。
不能仅记录边操作，却承诺可从日志恢复正文和提案。

## 4. 格式契约

> 状态：**draft 编码，format major 0**。P1 故障注入全部通过前不冻结为 format major 1。
> 本节只保留**跨文档不变式**；字节布局、字段/事件登记表、限值与错误分类的唯一依据是
> [storage-format.md](storage-format.md)，摘要与规范规则字节的唯一依据是
> [storage-digests.md](storage-digests.md)。

### 4.1 契约归属

| 内容 | 唯一所有者 |
|---|---|
| 文件/块/trailer 布局、CURRENT、路径命名、字段 tag 表、事件表、限值、错误分类 | storage-format.md |
| 摘要函数、域分离、覆盖范围、计算顺序、规范规则字节、测试向量 | storage-digests.md |
| 提交协议、恢复判定、权威性、checkpoint/备份/GC | 本文 §2–§3、§5–§8 |
| 记录语义与生命周期 | [data-model.md](data-model.md) |

三份文档冲突时按上表定所有者，并把冲突登记回本文，不允许实现各自解释。
旧版本中位于本节内的字段区间表、事件表、`Field.flags` PRESENT 位、可选压缩与
可选 trailer 都已**撤销**，由 storage-format.md 的逐字段/逐事件登记表取代。

### 4.2 跨文档不变式

1. **三个独立轴**：字段的“是否必须出现”（出现）、“值是否允许缺省”（值）、
   “未知读者能否跳过”（兼容）互不推出。兼容只有 `required` 与 `advisory` 两种；
   major 0 中 advisory 只有 `changed_fields` 与 `extensions`，且它们仍必须原样保留字节。
   “值可缺省”**不**授权跳过或丢弃。
2. **未知即拒绝**：未知 major、未知文件 kind、未知事件/字段 tag、未知枚举值、
   未知块 kind 一律拒绝（CORRUPT 或 UNSUPPORTED_FORMAT 见 storage-format §9.2）。
   不存在“跳过未知 optional 权威字段”的通道。
3. **一种缺省表达**：值缺省 = 整个字段不出现；没有 present 前缀字节，没有 sentinel，
   已登记类型的 `encoded_len = 0` 一律非法。
4. **摘要无环**：任何摘要都不得直接或间接覆盖自身。`payload_digest` 不覆盖
   `RequestOutcome`、`commit_version` 或时间戳；`state_digest` 不覆盖
   `RequestOutcome`（它在 `bookkeeping_digest` 里）；`chain_digest` 不覆盖本版本
   manifest 的文件哈希。顺序见 storage-digests §3.1。
5. **文件哈希 ≠ 域摘要**：blob 与 `rules_ref` 是未加域前缀的 `raw_sha256`（§4.3 的唯一
   例外，为了与 `<kb>/assets/`、外部 `sha256sum` 和迁移 archive 对账）；
   `tx_digest`/`chain_digest`/`state_digest`/`graph_digest`/`bookkeeping_digest` 是域摘要。
   把 blob 摘要当状态摘要复用是错误实现。
6. **规则配置是数据**：`rules_digest` 与 `engine_version` 是两个不同字段
   （`rules_digest` 回答“允许推导什么”，`engine_version` 回答“哪个构建算的”）。
   加载时必须由 `rules_ref` 指向的规范字节重新计算 `rules_digest` 并与事件声明的值比较；
   只与编译期常量比较不合格。
7. **有界解码**：任何长度、计数、加法与解压上限都在**分配之前**校验；超限拒绝整个
   文件，不截断后继续。块 CRC 定位错误，trailer/文件哈希验证内容，二者都不是对恶意
   写者的认证机制。

### 4.3 摘要域分离

域分离的形式、覆盖范围、计算顺序与测试向量见 [storage-digests.md](storage-digests.md)。
本文只固定两条跨文档结论：

- 所有非 blob 摘要使用 `digest(domain, bytes)`，domain 是登记过的 ASCII 名；
- blob 内容摘要是不加域前缀的 `SHA-256(bytes)`（唯一例外），因此 `blobs/<sha256>`、
  `<kb>/assets/<sha256>` 与外部工具可逐字节对账。

`rules_digest` 与 `engine_version` 不得合并：合并会让每次升级看起来像规则迁移，
从而掩盖真正的规则迁移（data-model §4、inference §7）。

### 4.4 draft 编码与 major 1 冻结

draft 文件写 `major=0`。以下条件全部满足后才改为 `major=1`：

1. 每个写入/fsync/rename 点的故障注入通过，含掉电重排模型；
2. golden fixtures 固定为字节级基准，覆盖每个已登记 tag；
3. 未知 required tag/major、截断、位翻转、超长解压均有拒绝测试；
4. 从 CURRENT 到每个被引用历史修订的读取路径有测试；
5. 拒绝路径与迁移工具存在，且其支持范围写在同一份文档里。

冻结之前：不承诺读取 `major=0` 文件，P1 改字段不带迁移；不得对外宣称已有磁盘格式
保证。冻结之后：未知 major 拒绝打开；未知 minor 的块只有标为 optional 且不影响
权威状态时才可跳过；未知 required 操作必须拒绝，不能静默丢数据。

## 5. 提交流程

同 KB 的所有修改由唯一 committer 串行执行；后台维护不能与它竞争 CURRENT。

1. 校验认证与 request_id；先查持久 RequestOutcome，再检查 expect_version/基线。
2. 在最新快照上构造私有候选，应用整组操作并预留内存、磁盘和新旧快照共存预算。
3. 运行引用、字段、图约束校验；构建基础索引和摘要。此时不计算全量闭包。
4. 将新增 blob 写到 staging，校验实际字节、同步文件，再 rename 至 blobs，
   fsync blobs 目录。复用 blob 也要校验其长度与哈希；引用已损坏内容则拒绝。
5. 编码完整 tx（包含 RequestOutcome），同步文件，rename 到 transactions，fsync 目录。
6. 编码新 manifest，包含前一 manifest 的路径/哈希和本次 tx 引用；同步并发布到
   manifests，fsync 目录。
7. 写 CURRENT.tmp，fsync；rename 覆盖 CURRENT，fsync KB 的 sage-db 目录。
8. 原子交换 Arc<Snapshot> 到候选版本，再返回 ACK。进入该步骤不得依赖可失败的大分配。

提案状态、会话记录等修改若未触及图字段或规则，复用原基础图与 graph_digest，
无需重跑图校验或构建 CSR；变更记录的引用校验和完整 state_digest 仍必须更新。
是否影响图由事件类型及字段依赖表决定，不能由客户端声明。

客户端超时不回滚已经提交的事务，也不证明写失败。读者在成功 ACK 后启动的新请求必须
看到该版本或之后版本；ACK 前的并发读允许看到旧版。

### 5.1 COMMIT_UNKNOWN 的产生与解除

第 7 步 rename 之后任何 I/O 错误（含目录 fsync 失败）都进入 COMMIT_UNKNOWN。
**CURRENT 始终是唯一权威**：不引入第二份“恢复日志”作为提交判据，也不假设该日志
在磁盘故障后一定写成功。可选的诊断日志只用于运维观察，不参与判定。

判定只需三件事，全部在持有同一目录锁的前提下完成：

1. **停止写入**：该 KB 的所有写入、后台维护、预热、checkpoint 与 GC 暂停，只允许读
   由 CURRENT 命名的根。不重试提交，不重放原操作，不复用原 request_id 发起新提交。
2. **校验并同步 CURRENT 实际命名的根**：读取 CURRENT 本身，校验其 kb_uuid、version、
   manifest 名与 SHA-256；沿 manifest 祖先链校验哈希；校验被引用的 tx 与 blobs；
   重算 state_digest 与 graph_digest。然后对该根**已存在的文件与目录项**执行 fsync：
   CURRENT 文件、其父目录、该 manifest 及其祖先 manifest、对应 tx 与 blobs 目录。
3. **以“同步成功”作为唯一证据**：只有当上述同步全部返回成功、且校验通过时，该版本
   才算已提交，随后按 request_id 返回首次持久结果（RequestOutcome 在其中）。

不做的三件事：

- **不比较新旧根来挑一个**。CURRENT 指向哪个根就只处理那个根。若新根校验失败，
  结果是 CORRUPT；**不得**因为新根没通过就回退到旧根继续服务——那会用旧状态掩盖
  一次可能已提交的写入，并永久丢失它。
- **不需要人工确认才能恢复**。重启后的判定是确定性的：读 CURRENT → 校验链 →
  尝试同步 → 同步成功即 committed，校验失败即 CORRUPT，同步失败即仍为 unknown。
  没有第三种“等管理员点确认”的状态。
- **不把一次成功的同步说成“已成功但未必持久”**。同步返回成功就是在声明的故障模型
  下的持久性证据；失败就是没有证据。二者之间没有含糊地带。

重复失败时的行为：同步持续失败 → 该 KB 保持只读（`/write-readyz` 报
COMMIT_UNKNOWN，`/readyz` 仍可读 CURRENT 命名且已校验的版本）。写入恢复的唯一条件
是同步最终成功。磁盘本身故障（EIO、ENOSPC 不可恢复）应按 CORRUPT/QUOTA 处理，
不靠反复重试掩盖。

### 5.2 重启时的判定表

| 观察（持有目录锁） | 处理 |
|---|---|
| CURRENT 存在，链校验通过，同步成功 | 提交成立：加载该版本，按 request_id 返回原结果 |
| CURRENT 存在，链校验通过，同步失败 | 仍为 COMMIT_UNKNOWN：只读，继续重试同步 |
| CURRENT 存在，但链或字节校验失败 | CORRUPT：停止该 KB 正常服务，显式恢复备份 |
| CURRENT 缺失但目录非空 | CORRUPT（不是自动 init，见 §2） |
| 只有未被 CURRENT 引用的新 manifest/tx | 未提交孤儿：保留旧版本，可回收 |
| 旧根与写前哈希相同、CURRENT 仍指旧根 | 明确未提交：同 request_id 可以重新提交 |

不得扫描编号最大的 manifest 猜测提交是否发生：孤儿 manifest 不进入已提交链。

## 6. 崩溃与损坏恢复

| 故障点 | 重启处理 |
|---|---|
| 校验/写 staging 中 | CURRENT 未变；不接受任何部分操作 |
| blob/tx/manifest 已发布，CURRENT 未替换 | 未被提交根引用；保留旧版本，可回收孤儿 |
| CURRENT rename 完成，但目录 fsync 未完成或失败 | 磁盘可能旧根或新根；进入 COMMIT_UNKNOWN，按 §5.1 重试同步并校验；未解除前只读 |
| CURRENT 已持久并通过全链校验、Arc 未交换 | 提交成立：重启加载新版本，请求结果可幂等查询 |
| ACK 已发 | 必须恢复完整事务及其 RequestOutcome |
| 缓存损坏 | 丢缓存并按需重算，不改变权威版本 |
| CURRENT/已提交 tx/权威 blob 损坏 | CORRUPT，停止对该 KB 声称正常服务；显式恢复备份 |
| CURRENT 校验通过但同步持续失败 | 该 KB 只读；不得选别的根、不得接受新写、不得重放原 request_id |

正常恢复读取 CURRENT、验证 manifest 祖先关系，选择匹配该链的最新有效 checkpoint，
再按顺序重放尾部事务。checkpoint 自带完整状态、ID 高水位、RequestOutcome 和
所绑定的 manifest 哈希；任何不匹配即放弃它，不能把它接在另一分支上。
全量 verify 重算日志状态与摘要，用于检查 checkpoint 和所有权威字节。

不能自动选择旧 manifest 掩盖损坏，也不能静默降级成 Markdown。
管理员可显式恢复较旧版本到新目录，并获得丢失提交范围报告。

## 7. 记录 checkpoint、备份与 GC

记录 checkpoint 从某个不可变 Snapshot 生成，写临时文件、校验、同步、rename。
它不创建知识 Version，不修改提交根，丢失它只增加重放工作。
存储 checkpoint 与用户的 SessionCheckpoint 在命名/API 中明确区分。

v1 永久保留 CURRENT 祖先链上的全部 manifest、tx 和历史记录引用的 blobs。
在线保留 8 个可直接查询的版本不等于删除更早日志；更老版本通过离线 restore 重放。
存储空间到配额时拒绝新写，不能悄悄删除审计历史。历史归档与截断推迟到后续版本。

GC 只回收：未提交孤儿、无在飞上传引用的过期 staging、无读者/pin/备份引用的
旧 checkpoint，以及可重建缓存。GC 在 committer 的维护屏障下取根集合；
在飞事务/上传文件必须登记，不能因暂时不在 CURRENT 链上而被删。

backup --version v 钉住已提交 v，将其 CURRENT 描述、祖先 manifest/tx、全部
历史引用 blobs 和可选 checkpoint 复制到新目录，完成哈希校验后写 backup.complete。
未完成标记的目录不能 restore。导出的备份清单记录每个文件的大小、哈希和版本。
restore 只写新目录，校验并开库后再显式切换路由；不覆盖正在服务的目录。
密钥、服务配置和部署级 registry 单独备份，不能从知识日志推导。

## 8. 内存与索引

活动基础图按方向、关系建立 CSR；边映射到独立 support 列表，保持多条 Assertion 身份。
CSR 至少包含 offsets、targets、confidence、support_ref；另计 term/slug 索引、
记录元数据、证明、分配器开销与旧版本。不得再以 targets + confidence 的 10 B/边
估算整个数据库内存。

v1 可以全量重建基础图索引，成本 O(V+E)，在候选发布前受预算约束。
不可变记录分块共享；修改块写时复制，正文/资产/向量字节按 blob 引用访问，
不因开库就把所有 blob 放进内存。查询工作区及 blob 读取均有独立字节配额。

### 8.1 历史修订索引

证据、提案基线与审计按 `(record_kind, record_id, revision)` 读取历史修订
（data-model §2.3）。该索引**不**保留整库快照：

    RevisionIndex 条目 = record_kind(u16) | record_id(128b) | revision(u64)
                       | version(u64) | tx_id(128b) | event_seq(u32) | value_ref(32b)

常驻内存的只有热点记录，其余落盘为可重建键值文件，按需读取。资源边界：

| 项 | 约束 |
|---|---|
| 常驻内存 | 只缓存热点记录键，LRU 淘汰；不得整库载入 |
| 单次历史读取 | 只访问该 record 的事件与其 blob；不加载无关记录 |
| 重建 | 从最后一个已确认条目续建；单次重建受查询预算限制 |
| 索引损坏 | 删除并重建，不影响权威事件与 blob |
| 索引落后于日志 | 以日志为准补齐；索引不能把旧修订冒充当前修订 |

按修订定位与按版本整库打开是两条独立路径：历史读取不得要求持有在线 8 个整库快照
之一。一条历史证据所属版本即使早已滑出在线窗口，仍必须可读。

权威字节缺失或哈希不符是 **CORRUPT**，不是 UNAVAILABLE：它意味着这个库已经损坏，
必须停止对该库的正常服务并显式恢复备份（system I3）。UNAVAILABLE 只保留给迁移时
明确标注为“从未归档”的旧审计信息（migration §2），那是记录本身存在的已知缺口。

全局内存账本包括：所有 KB 当前快照 + 所有仍被读者持有的旧快照 + 在建候选 +
缓存 + 历史索引热点 + 查询工作区。预算不足先淘汰可丢缓存，仍不足则拒绝新查询/写入。
取消请求不等于内存立刻释放，只有最后 Arc 释放后才能扣账。

## 9. 兼容与验证

未知 major/schema_version 拒绝打开并提示支持的迁移工具；未知 minor 的块只有
标为 optional 且不影响权威状态时可跳过。未知 required 操作必须拒绝，不能静默丢数据。
未来 major 使用旧格式 reader → 新目录 writer → 全量对账 → 显式切换，永不原地改写。

格式冻结前建立 golden fixtures、截断/翻转/超长解压测试；每个历史 major 保留
读取或明确迁移路径。故障注入覆盖 write 短写、ENOSPC、fsync/rename 失败和所有
崩溃边界。kill -9 只测进程崩溃；另用可模拟未刷盘丢失与写入重排的文件系统模型
验证掉电情况。1 万次正常提交不替代边界故障测试。
