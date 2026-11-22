# 原生导入映射 v1（native import mapping）

> 状态：P5 **导入准备**，不是激活、不是执行器、不是切换。
> 实现：`sage-db/crates/migrate/`。上游：`docs/design/migration.md` §2–§5、
> `data-model.md` §2.1、`TARGET-MAPPING-DECISIONS.md`。

## 1. 它做什么

读取**实际的 v1 源抽取计划**与它所绑定的源归档，产出：

1. 经**规范写入器**（`sage_db_store::typed` 的 typed bridge）构造的原生记录事件；
2. 每条记录真正引用的 blob 声明；
3. determinism 的源身份 → 目标 id 映射；
4. 一个**自包含导入包**（该包字节即 `ImportSession.plan_ref` 的内容）。

它**不打开** `Store`、不写文件系统根、不提交、不声明完成。

## 2. 附件为什么在包里，而不是 73 条记录

`data-model` §2.1 明确 Evidence **不是**源文件路径的别名，`captured_at` 是**声明的采集时间**而非
可借用的源时间戳；`MediaType` 有意只覆盖四种图片格式。真实源是 **57 个 `.md` + 15 个 `.json` + 1 个
`.bak`，零图片**。为这些文件制造 73 条 Evidence/Asset 记录会**错误描述数据**。

因此 73 个文件位于**导入包内**：

- 包由 `ImportSession.plan_ref` 引用，而 tag **185（`plan_ref`）在规范的 required-ref 集合内**，
  所以包字节随重放存活，不是"未引用的孤儿"；
- 每个源路径是包内一条**条目**：`path` + 精确 `length` + `sha256` + **实际字节**（base64）；
- 包内不含任何"只给哈希、让读者去别处找字节"的外部依赖。

**结论**：附件路径数（73）与原生记录行数是**两个不同的量**。包承载 73 个文件，为它们产生
**零条**原生 Evidence/Asset 记录——这是设计，不是丢失。

## 3. 身份、授权与审计

`prepare` 需要目标 KB 上的 **Admin** 能力。授予以 kernel 的真实类型给出:
`Grants::authorize(kb_uuid, Capability::Admin, ledger)`,返回的 `Authorized` 提供 principal。
空 principal、跨 KB 授权、缺 Admin 一律在此拒绝 —— 没有任何「任意非空字符串即认证」的路径。

`ExecutorContext` 给出**导入事件时钟**、原因、session id(不再携带 principal)。这些是
**新目标构造**的事实，绝不冒充源历史：

- `recorded_by` = 执行导入的已认证 principal，`actor_origin = Authenticated`；
- 源声明的作者只出现在它自己的字段（`legacy_id`、`declared_author`），从不改写为认证身份；
- 目标 revision 1 是**新命名空间的事实**；源 revision/date 缺失时记为 unavailable。

## 4. 关系提案:读 payload,不是读投影

`proposals/edges/*.md` 的源文件**明确写出**三元组:

```yaml
- **from**: decision
- **type**: analogous-to
- **target**: risk
- **confidence**: hypothetical
```

计划里 `concept: null` 只说明**投影中没有 slug 字段**,不代表源文件没有主体。按
`data-model.md` §5「关系提案的 subject 必须等于 payload.from」:

- `subject` = payload 的 `from`(经 slug→Term 映射);
- `kind` = `ProposalKind::RelationAssert`,这是源文件**断言的关系**,不是「最接近的种类」;
- `target` 与 `confidence` 保留在原始 payload blob 里,逐字不变;
- **两端** Term 都进入基线(`base_revisions`),排序去重,各自以导入创建的修订 1 入基线;
  自环只算一个身份,这是构造事实,源的原始方向不变;
- 历史 approved 只做显式的 NEW 重述:`Pending@1 → Decided@2`,带 import provenance,
  **绝不重放这条边**。

13 条 Proposal 全部映射,无排除、无「降级为仅包内条目」。

## 5. 资源边界与所有权

顺序是契约的一部分:

1. `max_plan_bytes` 与一个 `checkpoint` **先于**任何解码;
2. `Grants::authorize(KbUuid, Capability::Admin)` 先于读输入 —— 未授权者学不到计划形状;
3. 有界 JSON 解码是**唯一**的具名分配边界,其输出不被信任为任何尺寸;
4. 每个保留缓冲用**同一个** `SharedLedger` 承认,增长走 `try_reserve`;附件总字节用
   `checked_add`;`SourceArchive::blob` 接受同一个 ledger,在真正读入前承认上限;
5. 返回结果的所有权用 **`adopt_retained`**,不是 `reserve_owned`。

### 5.1 前置承认:adopt 只是纯转移

**每一次增长都在内存存在之前付费**,所以最终的 `adopt_retained` 是**纯转移**,没有 top-up:

| 情形 | 结果 |
|---|---|
| 范围已被前置承认 | adopt 成功,`live` 不变,`untransferred` 减少、`held` 增加 |
| 范围未被承认 | 返回 `Internal`,指明这是本 crate 的缺陷 —— 绝不当成预算结果 |

以前那版在最后 `try_charge(shortfall)`:那是在内存**已经存在**之后才承认,方向错了。
现在差额根本不该存在;若存在,就是实现缺陷而不是可修补的预算事件。

### 5.2 增长原语

`admit_and_reserve` 是唯一的增长入口:先按 `size_of::<T>() * additional` 承认槽位与条目,
再用 `try_reserve_exact` 预留**恰好**同样多。`Vec::with_capacity`、`collect`、裸 `push`
一律不用。字符串走 `admitted_copy`/`admitted_string`(先承认容量再分配);
格式化使用借用的 `format_args!`，经计数、承认、预留后写入单一字符串；
没有先分配再复制的诊断或保留字符串路径。

### 5.3 包序列化:计数后精确承认

**不再有任何启发式上界。** 两遍都走**同一个**规范序列化器(`serde_json::to_writer`):

1. 第一遍写入一个**只计数**的 sink(`CountingSink`),不分配任何内存,得到**精确**字节数;
2. 按该精确值 `admit_bytes` + `try_reserve_exact`;
3. 第二遍写入**有界** sink(`BoundedSink`),超过预留立即报错 —— `Vec` 自身会静默增长,
   这个 sink 不会,所以承认是真正的上限而非"首次分配提示";
4. 两遍字节数不一致 => `Internal`,不是静默接受。

同一个不可变 package 视图用于两遍;没有手写 JSON,没有第二个序列化权威。

`Base64View` 通过 `collect_str` **直接写进** serde,不产生每个文件的 `String`。

### 5.3.1 字符串:format_args!,不是 format!

`format!` 返回 `String`,在 helper 被调用**之前**就已经分配。正确形状:

```rust
let args = format_args!("{}.{}", requirement.family, requirement.field); // 借用,零分配
// 计数 fmt::Write -> 精确长度 -> 承认 -> try_reserve_exact -> fmt::write
let value = admitted_format(ledger, args)?;
```

`admitted_format` 是唯一的格式化入口;旧的 `admitted_owned_string(String)` 已删除,
因为"先分配再复制"永远无法弥补原始分配。

### 5.3.2 固定栈值

`hex`/`hex16` 返回固定大小的栈数组(`[u8; 64]` / `[u8; 32]`),零分配;
需要字符串时由调用方承认后再构造。

### 5.4 完整测量(checked,不 saturating)

`complete_output_extent` 度量**实际返回**的每一个缓冲:event 字段表与每个 payload 向量、
blob 槽与 `Provide` 载荷、session 的 counts 与 id/reason/actor 字符串、全部映射条目与
attachment 路径、五段 disposition 字符串、counts、watermarks。全部用
`Vec::capacity()`/`String::capacity()`,并逐步 `checked_add` —— 溢出返回 `Internal`,
不会伪造 `u64::MAX`。

### 5.5 私有所有权

`PreparedImport` 只有一个私有 `owned` 字段,缓冲与 token 同体;访问器都是 `&self` 借用,
安全代码无法把向量移出后单独丢弃 token。包载荷只有一个所有者(blob 的 `Provide`),
`package_bytes()` 借用它。

### 5.6 失败路径:错误类型不含堆数据

`MappingError` 是 `Copy`,**任何变体都不持有 `String`/`Vec`**。这一点在预算耗尽时最关键:
拒绝必须在无法再分配的情况下仍能被描述。`Refusal`/`BudgetError` 本身就是标量;
规范写入器的 `FormatError` 也是 `Copy`,原样保留而不"翻译成字符串"。

| 变体 | 载荷 |
|---|---|
| `Refused(Refusal)` | 标量(`BudgetError` 三个数) |
| `InvalidSource { location, detail }` | 结构位置 + 静态标签 |
| `Unsupported { location, detail }` | 结构位置 + 静态标签 |
| `Unauthorized { detail }` | 静态标签 |
| `CanonicalRejected { location, reason }` | 位置 + **规范 `FormatError` 原值** |
| `Internal { location, invariant }` | 位置 + 静态不变量名 |

`SourceLocation { array, row, field }` 用**数组与行号**标识位置,而不是复制任意长度的
源路径:调用方用自己的 plan 索引即可还原精确路径,既不截断也不伪造。源归属信息仍完整保留:
记录上的 `legacy_source_path` 与包内的文件清单都还在。

实测:`FormatError` 48 字节,`SourceLocation` 32 字节,`MappingError` 恰好 80 字节
(= 载荷 + 位置,无残留指针)。测试以该等式为结构化证明。

### 5.7 授权 principal

`ExecutorContext` **没有** principal 字段。主体只来自 `Grants::authorize` 的返回值。

## 6. 自包含的包

`plan_ref` 的字节在重启后必须足够重建决定,所以包里带着:

| 内容 | 为什么 |
|---|---|
| 实际输入计划的**原始字节** + `plan_digest`(那些字节的哈希) | 独立可校验的计划绑定 |
| `archive_content_digest` / `source_digest` 两个**分开**的字段 | 此前把归档摘要误标成 plan_digest |
| 完整 target 映射(每族 source→target id) | 重启后仍能定位每个对象 |
| **全部** dispositions(policy/constructed/reason) | 不是只留第一条未决项 |
| target 上下文(kb_uuid/session/time/principal/expected_counts/watermarks) | 执行器需要的事实 |
| 全部 73 个源文件的原始字节 | 每个都是 path+length+sha256+bytes |

`ImportSession.source_digest` 是**严格解码后的原值**,不是对十六进制再哈希一次;
非法/缺失的摘要在解码时拒绝,绝不静默变成 0。

## 7. 记录族映射与状态计数

| 源 | 目标 | 说明 |
|---|---|---|
| 41 Term | `TermRecord` + body blob | body = 计划给出的**未裁剪**切片,摘要对照源文件复核 |
| 173 Assertion | `AssertionRecordFull` | 原方向、`RelationId`、`declaration_order`、confidence |
| 13 历史 approved Proposal | `ProposalRecord` Pending@1 + Decided@2 | 显式 NEW 重述,带 import provenance |
| 5 Checkpoint | `SessionCheckpointRecord` + content blob | |
| 30 Frontier 行 | `FrontierEntryRecord` + text blob | 三段都在;priority 取作者列;status 不由它推断 |
| `kb.json` | `KbMetadataRecord` revision 2 over Genesis | 源 kb 的 name/description/domains 真正映射 |
| 每个已用 id 空间 | `IdAllocationEvent` | 标记 = 该空间**已分配的最大 id**,不是行数 |
| 73 源文件 | 包内条目 | **零**条原生 Evidence/Asset |

期望计数由**真实投影器**合并完整身份后得出,不是手工累加:族行 262,加两个 Genesis
单例(KbMetadata 更新、RuleConfig 不变)= **264**。262 与 264 都不是计划的 335 条映射记录。
投影器一致是**一致性检查**,不是独立 golden。

## 8. 非主张

不是激活、不是执行器、不是切换;不声明 73 条附件记录;不声明包本身证明源归档真实性
(包绑定并复用先前已验证的归档/内容摘要,并对每个文件重新哈希)。提交、重启后
`read_committed_blob` 校验与完成标记属于**下一批执行器**。

## Root R8 correction

Both proposal lifecycle images independently admit their baseline and review-note owners
before copying. Endpoint staging uses a fixed stack array. The real-source probe is
explicitly ignored by default and requires `SAGE_DB_REAL_ARCHIVE` and
`SAGE_DB_REAL_PLAN`; invoking it without either input fails. Its result is reported
separately from the self-contained fixture suite. Ledger observations measure admissions,
not arbitrary allocator activity.

## Root R9 allocation audit

The temporary projector field table is admitted before reservation. Mapping entries,
event slots and blob-declaration slots use the same pre-admission helper. Canonical
`prepare_event` already admits its field table and payload copies; moving those buffers
into the output does not charge the payload again. A provided blob's payload is likewise
admitted at its construction site, while insertion admits only the declaration slot.
Allocator capacity is measured at final adoption; `try_reserve_exact` does not promise
allocator-exact capacity. A shortfall remains an internal refusal, never a late top-up.
