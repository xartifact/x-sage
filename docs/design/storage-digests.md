# 摘要与规范字节规范（draft, format major 0）

> 上游：[storage.md](storage.md) §4.3/§4.4；字节布局：[storage-format.md](storage-format.md)。
> 本文定义：摘要函数、域分离、每个摘要的**精确输入**、计算的**无环顺序**、规则配置的
> 规范字节，以及可逐字节比对的测试向量。
> 本文取代 storage.md §4.3 的摘要表；冲突以本文为准。

状态：**draft，major 0**。major 0 不构成兼容承诺；数值仍是实现契约。

## 1. 两个不可互换的摘要函数

| 函数 | 定义 | 用途 | 是否加域前缀 |
|---|---|---|---|
| `raw_sha256(bytes)` | `SHA-256(bytes)` | 内容寻址：`blobs/<sha256>`、`Asset.sha256`、`rules_ref`、`tx_file_sha256`、`manifest_sha256`、CURRENT 的 `manifest_sha256` | **否** |
| `digest(domain, bytes)` | `SHA-256("sage-db/v1/" ‖ domain ‖ 0x00 ‖ u64be(len(bytes)) ‖ bytes)` | 语义身份：`tx`、`manifest-chain`、`state`、`graph`、`rules`、`bookkeeping`、`request`、`result`、`search` | 是 |

两者不可互换，且不允许“把 raw 结果当域摘要用”。理由：`blobs/<sha256>` 必须与
`<kb>/assets/<sha256>` 和外部 `sha256sum` 逐字节对账（storage.md §4.3 的唯一例外），
而语义摘要必须防止同一段字节在不同用途下算出可比较的错误相等。

**禁止**：把 blob 摘要当 `state`/`graph` 摘要复用；把 `tx_digest` 当文件哈希写进
`tx_file_sha256`（后者必须能由外部工具独立重算）；把 `rules_digest` 当 `engine_version`。

## 2. 域分离细节

```
domain        := ASCII 名（下表），不含 NUL，长度 1..=32
prefix        := "sage-db/v1/" ‖ domain ‖ 0x00
length        := u64 大端（不是本文件的 LE 约定：这是哈希前缀，与文件字节无关）
digest_input  := prefix ‖ length ‖ payload
```

`len_u64_be` 消除 `("ab","c")` 与 `("a","bc")` 的拼接歧义。域名字符串参与哈希，
因此新增域不会与旧域碰撞。

已登记域（major 0 全部固定）：

| domain | 覆盖 | 计算的时点 |
|---|---|---|
| `tx` | 一个 tx 的 `body`（`tx_header` + 事件序列的编码字节） | 写 tx 文件后、写 manifest 前 |
| `manifest-chain` | 前一链摘要 + 本 tx 的 `tx` 摘要（§4.3） | 写 manifest 前 |
| `state` | 折叠后的知识记录全集（§4.4） | 写 manifest 前 |
| `graph` | 活动图投影（§4.5） | 写 manifest 前 |
| `rules` | 规范规则字节（§5） | genesis / 规则迁移；加载时重算校验 |
| `bookkeeping` | RequestOutcome + ImportSession + ID 高水位（§4.6） | 写 manifest 前 |
| `request` | 请求负载的规范字节（§4.7） | 提交前（客户端或服务层） |
| `result` | 规范排序后的事实行（§4.9） | 查询完成时（运行时，不写入 tx） |
| `search` | 检索投影（§4.9） | 索引重建时（运行时，不写入 tx） |

未登记的域名不得使用。新增域必须同时给出：覆盖内容的规范字节定义、计算时点、
以及它**不**包含哪些摘要（无环证明）。

字段 153 `legacy_frontier_ref` 参与 KbMetadata 的规范记录正文与状态摘要，
并作为事务中必须声明的 blob 引用进入历史引用集。引用值是 Frontier 文档原始字节的
`raw_sha256`。Genesis 不携带此字段，既有无字段事件的编码与摘要保持不变。

## 3. 无环性是硬约束

摘要依赖必须构成 DAG：不存在任何摘要直接或间接包含自身。这不是设计偏好，而是
`RequestOutcome` 幂等与重试语义的前提。三条最容易犯的错：

1. **`payload_digest` 不能覆盖含它自己的 `RequestOutcome`。** `RequestOutcome` 事件
   (192) 就在同一 tx 内并携带 `payload_digest`；若请求摘要覆盖该记录，则计算
   `payload_digest` 需要先知道 `payload_digest`。
2. **`result_ref` 不能指向包含 `state_digest` 的字节，而 `state_digest` 也不能覆盖
   `RequestOutcome`/`result_ref`。** 首次持久结果在提交前尚不可知（它包含
   `commit_version`），因此 `RequestOutcome` 不能进入 `state_digest`；它进入
   `bookkeeping_digest`。
3. **`chain_digest(N)` 不能覆盖 `manifest_sha256(N)`。** 文件哈希覆盖整份 manifest
   文件（其中包含 `chain_digest(N)`）；若 `chain_digest(N)` 再覆盖文件哈希，两者互相
   依赖。`chain_digest(N)` 只覆盖 `N-1` 的链摘要（已固定）与 `N` 的 `tx_digest`。
   注意：`manifest_sha256` **确实**覆盖四个摘要（这是它的用途），只是没有任何摘要
   反过来读它。

### 3.1 版本索引的有序计算

摘要不是一张“谁覆盖谁”的对角线矩阵，而是一条**按版本推进的计算序列**。同一版本内
不存在环；跨版本只向后引用（版本 N 只读取版本 < N 已固定的值）。

设提交版本为 `N`，前一版本为 `N-1`（genesis 时 `N = 1`，无 `N-1`）：

| 步 | 产物 | 输入（全部是已完成步骤或已固定历史） |
|---|---|---|
| 1 | 候选状态折叠 | 日志前缀 `1..N-1` 的全部事件 |
| 2 | `rules_digest` | 规范规则字节（profile 常量生成）；与日志无关 |
| 3 | `payload_digest` | 调用方请求规范字节（**不含** RequestOutcome/IdAllocation/version/时间，§4.7）；genesis 取哨兵 32×`0x00` 且 `206=0` |
| 4 | `result_ref` | 结果负载字节：`result_kind`（恒 `0`）、`commit_version = N`、`affected`、`counters` |
| 5 | `bookkeeping_digest` | 候选状态的 outcome/import/counter 集合，其中 outcome 含第 3、4 步的值 |
| 6 | `state_digest` | 候选状态的记录集合（**不含** outcome/import/counter、不含 `changed_fields`、含 `extensions`） |
| 7 | `graph_digest` | 候选状态的活动图投影 |
| 8 | `tx_digest` | `tx_header`（含第 3 步的 `payload_digest`）+ 事件（含 `RequestOutcome`，其 `result_ref` 是第 4 步值） |
| 9 | `tx_file_sha256` | tx 文件字节（含 `tx_digest` 所在的 body） |
| 10 | `chain_digest` | `0x00 ‖ prev_chain_digest(N-1) ‖ tx_digest(N)` |
| 11 | `manifest_sha256` | manifest 文件字节（含第 5–7、9、10 步的值） |
| 12 | CURRENT | `manifest_sha256`、`version`、`name`、`kb_uuid` |

不变量（可逐条断言）：

- **无环**：表中每步只读它上方的行，加上版本 `< N` 的 `prev_chain_digest`。第 10 步读的
  `prev_chain_digest` 是 `N-1` 的 manifest 里已固定的字段，不是 `N` 的任何值。
- **`chain_digest` 不含本版本文件哈希**：它只含 `N-1` 的链摘要与 `N` 的 `tx_digest`；
  `manifest_sha256(N)` 在它之后计算。若把 `manifest_sha256(N)` 塞进 `chain_digest(N)`，
  第 10、11 步就互为依据。
- **`tx_digest` 确实传递依赖 `payload_digest` 与 `rules_digest`**：后者经
  `tx_header.163`，前者经 `tx_header.178` 与 `RequestOutcome.178`。所以
  `tx_digest → payload_digest`、`tx_digest → rules_digest` 都是真实依赖，不是“无”。
- **`bookkeeping_digest` 传递依赖 `payload_digest`**：outcome 记录携带它。这正是把它
  从 `state_digest` 拆出的原因（否则 `payload_digest` 与状态摘要成环，§3 第 1 条）。
- **`state_digest` 与 `bookkeeping_digest` 平行**：两者都由 `N` 的 manifest 发布，
  谁都不读对方。
- **文件哈希覆盖摘要**：`tx_file_sha256` 覆盖含 `tx_digest` 的字节；
  `manifest_sha256` 覆盖含 `chain_digest`/`state_digest`/`graph_digest`/`bookkeeping_digest`
  的字节。方向恒为“文件哈希 → 摘要”，从不反向，所以没有环。
- **`engine_version` 不在任何一步的输入里**（§4.8）。

同一版本内的禁止依赖（每条都是实现缺陷，测试必须能拒绝）：

| 禁止 | 为什么 |
|---|---|
| `state_digest` 读 `bookkeeping_digest` 或反方向 | 两者平行，互读即环 |
| `chain_digest` 读 `manifest_sha256(N)` | 第 10↔11 步互依据 |
| `payload_digest` 读 `RequestOutcome`/`commit_version`/任何时间戳 | 自指 + 重试不可重复 |
| `result_ref` 指向含 `state_digest`/`graph_digest` 的字节 | 结果负载必须在 manifest 之前确定 |
| `tx_digest` 读 `chain_digest` 或 `manifest_sha256` | 反向依赖（链引用 tx，不是反过来） |

## 4. 逐个摘要的规范定义

所有“记录全集”“行集合”都在**同一提交后的候选状态**上计算。

### 4.1 `tx_digest`

```
payload  := tx 文件的 body 字节（§storage-format 4）
tx_digest := digest("tx", payload)
```

覆盖：`tx_header` 全部字段 + 全部事件（含 `event_seq` 的编码顺序）。
不含：文件头、块头、trailer（这些由 `tx_file_sha256` 与块 CRC 覆盖），
不含 manifest 的任何字段（manifest 引用 `tx_digest`，不是反过来）。

`tx_digest` 同时**固定事件顺序**：重排事件改变字节，因此改变 `tx_digest`。
这让 storage-format §4.1 的“位置即 `event_seq`”成为可验证声明。

### 4.2 `RequestOutcome` 与 `result_ref`

首次持久结果 = 该请求的事务结果。关键区别是“**准备期可确定**”与“**manifest 之后才可确定**”：

| 字段 | 内容 | 准备期可确定？ |
|---|---|---|
| `result_kind`(179) | major 0 中恒为 `committed`（见下） | 是 |
| `commit_version`(181) | 本事务的 `version` | **是**：`version = prev_version + 1`，写 tx 之前已知；同步失败时不改变该值（仍记 unknown） |
| `result_ref`(180) | `raw_sha256(结果负载字节)` | **是**：负载只含准备期字段，见下 |

只有 `state_digest`/`graph_digest`/`bookkeeping_digest`/`chain_digest`/`manifest_sha256`
是“manifest 之后才确定”的值。它们全部位于 manifest 或 CURRENT，**不在**结果负载里，
因此结果负载可以在写 tx 之前构造完。

**`result_kind` 在 major 0 中只有 `0 committed`。** 已提交事务的持久结果就是“已提交”；
`rejected`/`conflict` 是写前类型化错误，不产生事务与 outcome；`COMMIT_UNKNOWN` 是传输期
状态，必须由重试解析为 committed，不得落盘为终态（storage-format §7.3）。
因此 `ResultPayload` 里的该字段是当前唯一合法值，不是“将来可能扩展的枚举”——
扩展必须先补它的原子性、版本绑定与幂等语义。

`result_ref` 指向的负载是**结果负载 blob**，只含提交前可确定的字段：

```
result payload :=
  result_kind:u16
  commit_version:u64
  affected:  [RevPair] 升序（本事务写入/终结的记录与修订）
  counters:  [Watermark] 升序（本事务推进的 ID 高水位；`space:u16 | hwm:u128`）
```

它**不含**：`state_digest`、`graph_digest`、`bookkeeping_digest`、`chain_digest`、
`manifest_sha256`、`tx_file_sha256`、任何时间戳。理由分两类：

- 四个摘要与两个文件哈希在第 5–11 步之后才有值（§3.1），而 `RequestOutcome` 必须在
  第 8 步（`tx_digest`）之前构造完，否则 `tx_digest` 覆盖它时得到的是未定义值；
- 时间戳来自提交者且每次尝试可能不同，放进结果负载会让“同一请求重试返回同一结果”
  无法逐字节成立。时间戳只出现在 `RequestOutcome` 事件 `8` 与 tx 头 `8`，属于事务，
  不属于结果内容。

**推论**：`state_digest` 不得覆盖 `RequestOutcome`；否则 `payload_digest`
（在 `RequestOutcome` 内）的规范字节一旦提到状态摘要就成环。这就是 §4.6 拆分
`bookkeeping_digest` 的唯一理由，也是它必须与 `state_digest` 平行发布的原因。

因此对同一 `(kb_uuid, principal_id, request_id, payload_digest)`，**结果负载在重试时
逐字节相同**（重试命中已持久 outcome，不再重新构造）。若结果是查询类（`result` 域），
其 `result_digest` 是运行时值，不写入 tx；`RequestOutcome` 只记录 `result_ref` 指向的
提交结果，不记录查询行。

### 4.3 `chain_digest`

```
payload := domain_byte(0x00) ‖ prev_chain_digest:32 ‖ tx_digest:32
chain_digest := digest("manifest-chain", payload)
```

- genesis（`version = 1`）：`prev_chain_digest` = 32 个 `0x00`。
- 其余：`prev_chain_digest` 是**被 `prev_manifest_sha256` 命名的那份 manifest** 的
  `chain_digest` 字段值（已在上一版本固定，不会被本版本改写）。
- 前导 `domain_byte(0x00)` 是版本标记：将来若要改链摘要的输入，用 `0x01` 而不是让
  旧链在新语义下被误判为有效。

**不含**：本 manifest 的任何字段、本 manifest 文件哈希、`state_digest`、`graph_digest`、
`bookkeeping_digest`、`engine_version`。链只证明“事务序列未被改写”，与状态摘要正交：
后者能被独立重算并比较，前者保证无法丢弃/替换中间版本。

### 4.4 `state_digest`

覆盖**折叠后的全部知识记录**，按 `(state kind, natural key)` 排序：

```
payload := count:u32 ‖ Entry[count]
Entry   := kind:u16 ‖ key_len:u32 ‖ key ‖ revision:u64 ‖ body_len:u32 ‖ body

key     := 该记录的自然键字节，见 §4.4.2（空键即长度 0，仅单例使用）
body    := 规范记录字节，见 §4.4.3
revision:= 该记录当前修订（不可变记录固定为 1，见 §4.4.4）
排序     := kind 数值升序，kind 内按 key 的逐字节无符号序（单例键为空，故每 kind 唯一）
```

### 4.4.1 `state kind` 取值范围

`state kind` **不是** `RecordKey.kind`。前者是折叠后的记录集合划分（包含没有数值
身份的记录），后者只用于有数值 ID 的记录。两者共享 1..9 的数值但必须分开定义，
因为 Asset/Model/KB/Rules 没有 `RecordKey`：

| state kind | 记录 | 数值身份 | 自然键（§4.4.2） | 可修订 |
|---|---|---|---|---|
| 1 | Term | `RecordKey{1, id}` | `u32 id` LE | 是 |
| 2 | Assertion | `RecordKey{2, id}` | `u128 id` LE | 是 |
| 3 | Claim | `RecordKey{3, id}` | `u128 id` LE | 是 |
| 4 | Evidence | `RecordKey{4, id}` | `u128 id` LE | 是 |
| 5 | EvidenceLink | `RecordKey{5, id}` | `u128 id` LE | 是 |
| 6 | Vector | `RecordKey{6, id}` | `u128 id` LE | 是 |
| 7 | Proposal | `RecordKey{7, id}` | `u128 id` LE | 是 |
| 8 | FrontierEntry | `RecordKey{8, id}` | `u128 id` LE | 是 |
| 9 | SessionCheckpoint | `RecordKey{9, id}` | `u128 id` LE | 是 |
| 10 | Asset | 无（内容寻址） | `32 B sha256` 原字节 | 否 |
| 11 | Model | 无 | `model_key` 的 UTF-8 字节 | 是 |
| 12 | KbMetadata | 无（单例） | 空（长度 0） | 是 |
| 13 | RuleConfig | 无（单例） | 空（长度 0） | 是 |

自然键记录的完整折叠区间（哪些事件、哪些字段、状态从何而来）必须只有一处定义，
见 §4.4.8 的表；本节只定义摘要如何消费它。

14 及以上未登记。`state kind` 是封闭集合：未登记值使**重放**失败（CORRUPT），因为
一个无法归类的记录不可能被折叠。

**RequestOutcome、ImportSession、IdAllocation 不在此表**：它们不是知识记录，进入
`bookkeeping_digest`（§4.6）。

### 4.4.2 自然键的唯一性与排序

- 每个 `(state kind, key)` 在折叠结果中**恰好一条**。两条即 CORRUPT（
  `DuplicateRecordKey`）。
- 排序只比较 `kind`（数值）与 `key`（逐字节无符号），**不**比较 revision 或 body。
  因此排序是记录集合的纯函数，不依赖折叠顺序。
- 单例（12、13）的键为空字节串，`key_len = 0`；每 kind 只能有一条。
- 数值键用**小端**编码（与本文件所有整数一致），因此“键的字节序”不等于“数值序”；
  排序规则显式指定为逐字节无符号，实现必须按该规则排序，不得依赖 `u128` 的小端
  表示恰好有序。

### 4.4.3 规范记录字节（`body`）

`body` 是该记录**最新事件的字段序列**，按 `tag` 升序，使用 `Field` 信封
（`tag:u16 | flags:u16(0) | encoded_len:u32 | value`），但**省略且仅省略**：

| 省略的 tag | 理由 |
|---|---|
| `1 record_key` | 已作为 `Entry.kind` + `Entry.key` 出现；重复编码会产生第二个可矛盾来源 |
| `13 changed_fields` | 可由相邻修订差分唯一重建（§4.4.5） |

其余字段全部保留，包括 `2 revision`（记录的版本身份）、`4 status`、
`10/11/12 legacy_*`、`14 extensions`（无法重建，必须进入摘要），以及全部值可缺省的
字段（缺省就是不出现，空值也照常编码）。

**这与“完整 after-image”是同一件事，不是两套规则。** 每个 revise/retire 事件必须携带
该修订的全部记录值（storage-format §7.2），因此“最新事件的字段”与“该修订的完整记录”
在合法文件上必然相等。若某实现让 `status`/`legacy_*`/`extensions` 依赖其他修订继承，
它能算出某个摘要，但那个摘要不属于“按修订可自足”的字节——这正是本规范要排除的第
二种合法折叠。

一个事件若要同时产出两条记录（仅 `Genesis`），其字段被**划分**进两个 body，
划分表见 §4.4.6。该表**故意共享**少数事务级字段（`2 revision`、`8 created_at`、`7 reason`、`14 extensions` 与 `163 rules_digest` 的归属由该表逐项指定）；除此之外不允许字段进入两个 body。共享不等于第二个真源：同一事件只有一份字段字节。

### 4.4.4 不可变记录与修订约定

| 记录 | `revision` 取值 | 理由 |
|---|---|---|
| 1..9（有 `RecordKey`） | 事件里的 `2 revision` | 修订链单调递增 |
| 10 Asset | 恒为 `1` | 内容寻址，声明一次，永不修改 |
| 11 Model | 事件里的 `2 revision` | 注册/更新/退役是同一记录的修订 |
| 12 KbMetadata | 事件里的 `2 revision` | 每次更新一个新修订 |
| 13 RuleConfig | 事件里的 `2 revision` | 每次显式迁移一个新修订 |

不可变记录的固定 `1` 不是“编造的修订号”，而是“存在且不会变”的显式声明：它让
`Entry.revision` 字段对所有 kind 都是必现的 `u64`，无需 `Option`。

### 4.4.5 `changed_fields` 为什么可以省略

`changed_fields` 的语义是“与前一修订相比哪些 tag 变了”。给定某记录的两个相邻
after-image（都已在日志里），`changed_fields` 可**唯一**重建为“字段值不同的 tag 集合”。
因此它不承载额外信息：省略它不减少可验证内容。

这与 `extensions` 相反：后者无法从任何其他字段推导，必须进入 `body`。
判据是“能否重建”，不是“重不重要”。

### 4.4.6 Genesis 的字段划分（两个 body）

`Genesis` 事件同时确立 KbMetadata（kind 12）与 RuleConfig（kind 13）。字段划分是
**规范表**，不是实现选择：

| tag | 进入 | 说明 |
|---|---|---|
| 2 `revision` | 两个 body（同值 1） | 两条记录各自的首次修订 |
| 8 `created_at` | 两个 body | |
| 144 `kb_uuid` | KbMetadata | |
| 145 `kb_slug` | KbMetadata | |
| 146 `kb_name` | KbMetadata | |
| 147 `kb_description` | KbMetadata | |
| 148 `kb_domains` | KbMetadata | |
| 163 `rules_digest` | **RuleConfig only** | 规则摘要属于规则记录；KB 的当前规则通过 state kind 13 读取 |
| 166 `rules_ref` | RuleConfig | |
| 196 `rules_profile` | RuleConfig | |
| 7 `reason` | 两个 body（若出现） | 同一个理由解释这一笔 |
| 14 `extensions` | 两个 body（若出现） | 附注可能同时涉及两方 |

被两个 body 共享的 tag（`2`、`8`、`7`、`14`）必须逐字节相同——同一事件只有一份值，
所以“共享”不是第二个真源，而是“一次编码、两处引用”。测试判别：若实现让两侧取不同值，
该事件根本无法编码。

**`163 rules_digest` 只进入 RuleConfig（kind 13），不进入 KbMetadata。** KB 的当前规则
通过 state kind 13 读取（`196 rules_profile` + `166 rules_ref` 已完整描述该配置）。
把 `163` 也放进 KbMetadata 会做两件错事：一是在 `state_digest` 里出现同一事实的两份
拷贝，二是把“知识元数据变化”与“规则内容变化”耦合到同一个 body——那会让一次纯
描述更新看起来像规则变更。

这个选择有数值判别式（t63 独立复现，我用自己的生成器复核过）：同一 Genesis 事件下，
`163` 只进 RuleConfig 得到 `state_digest = b6422a4a…`（与 golden manifest `198` 一致），
`163` 同时进两个 body 得到 `aace0923…`。实现若按后者编码，会把自己读到的合法文件判为
CORRUPT。

### 4.4.8 自然键记录：状态来源与折叠区间

有 `RecordKey` 的记录，状态来自其最新事件的 `4 status`。四类自然键记录没有
`RecordKey`，状态必须逐类型定义，否则“折叠后记录集”不可算。**本表是唯一权威**，
其他文档只引用它，不重复列：

| state kind | 记录 | 产生/修改它的事件 | 状态字段来源 | 当前值来源 |
|---|---|---|---|---|
| 10 | Asset | `64 AssetDeclared` | **无状态**（不可变） | 该 `sha256` 的唯一 `64` 事件 |
| 11 | Model | `145 ModelRegistered` / `146 ModelUpdated` / `147 ModelRetired` | 事件 `4 status`，**每个事件都必现**：`145 = 0 active`、`146` 显式给出（`0` 或 `1`，不省略）、`147 = 1 retired` | 该 `model_key` 的最后一个事件 |
| 12 | KbMetadata | `224 Genesis` / `144 KbMetadataUpdated` | **无状态**（每 KB 一条，总有值） | 最后一个 `224`/`144` 事件 |
| 13 | RuleConfig | `224 Genesis` / `160 RuleProfileChanged` | **无状态**（当前生效配置） | 最后一个 `224`/`160` 事件 |

补充规则（同样只在此处定义）：

1. **Asset 没有状态值**：它不参与 `4 status` 的取值域检查。为它写 `4` 是 CORRUPT
   （字段表把它列为“Asset 无 `4`”）。
2. **Model 的 `4` 取值域是 `0 active` / `1 retired`**，与 Term 相同但作用于
   `model_key` 而非数值 id。`147` 必须 `status = 1`；`145` 必须 `status = 0`；
   `146` 若给出必须是 `0` 或 `1`（复活已退役模型是合法的显式事件，不是隐式状态）。
3. **KbMetadata 与 RuleConfig 的 `2 revision`** 从 `1` 起，每次更新 +1；
   `Genesis` 提供两者的 `revision = 1`（§4.4.6）。
4. **自然键唯一性**：同一 `(state kind, key)` 在折叠结果中恰好一条。多个事件是
   *修订*，不是多条记录；折叠取最后一个事件的 after-image。
5. **`ImportSession` 不在本表**：它没有记录 body，只进 `bookkeeping_digest`。
6. **`RequestOutcome`、`IdAllocation` 也不在本表**：同上。
7. **未在本版本变更的自然键记录，按前一版本折叠结果原样继承**：revision 与 body 都
   沿用前值，**不重新编号**。因此一个只写了 `TermCreated` + `RequestOutcomeRecorded`
   的 v2 事务，其 `state_digest` 仍包含 revision 为 1 的 KbMetadata 与 RuleConfig
   （它们从 genesis 继承而来）。把继承值重新编号为“本版本的新修订”会让两个同样正确的
   实现算出不同摘要；把继承记录整个省略会让 `state_digest` 只覆盖“本版本写过的东西”，
   与“当前状态”的定义矛盾。

   同一规则适用于所有自然键记录与有 `RecordKey` 的记录：记录集合是**当前状态**，
   不是“本版本的事件投影”。唯一例外是记录从未存在过——那时它根本不进集合。

### 4.4.7 可执行性声明

本节给出的是**可算出字节**的定义：kind 编号、键编码、`Entry` 布局、body 投影、
Genesis 划分、不可变记录的 revision 约定全部固定，没有“实现时决定”。
`storage-digests §7` 的 golden 向量由独立脚本按本节实现计算，因此一条不符的实现
会在与 golden 比对时立即失败，而不是"看起来合理"。

### 4.5 `graph_digest`

只覆盖**参与推理的图字段**，因此“提案状态变化但图未变”不改变它：

```
payload := term_count:u32 ‖ Term[term_count] ‖ assertion_count:u32 ‖ Assertion[assertion_count]
Term      := id:u32 ‖ layer:u8                       // 仅活动 Term，id 升序
Assertion := id:u128 ‖ revision:u64 ‖ from:u32 ‖ relation:u16 ‖ target:u32 ‖ confidence:u16
                                                     // 仅活动 Assertion，id 升序
```

`revision` 是**语义输入**，不是防御性的缓存成本（D2 已批准）：证明叶节点绑定
“该 assertion 的具体修订”，所以同一端点的两个修订产生不同的可回放证明；若
`graph_digest` 忽略它，两个可区分状态会共享键，缓存命中时会返回绑定到旧修订的证明。

**不含**：`atom`、`slug`、`domains`、`triggers`、`body_ref`、证据、提案、时间戳、
`extensions`、`rules_digest`、`engine_version`。理由：这些字段不影响闭包/最宽路径结果；
把它们混进来会让“只改了正文”使全部查询缓存失效，也会掩盖真正影响语义的变更。
`rules_digest` 与 `engine_version` 分别是缓存键的独立分量（inference §7），不是图数据。

这与 P0 `GraphDigest` 的语义一致：两个快照 `graph_digest` 相等 ⇒ 活动图相同 ⇒ 缓存可复用
（`sage-db/crates/types/src/digest.rs`）。

### 4.6 `bookkeeping_digest`

```
payload := outcome_count:u32 ‖ Outcome[outcome_count]
        ‖ import_count:u32  ‖ Import[import_count]
        ‖ counter_count:u32 ‖ Counter[counter_count]

Outcome := principal_id:str ‖ request_id:str ‖ payload_digest:32 ‖ result_kind:u16(=0)
        ‖ result_ref:32 ‖ commit_version:u64                       // 按 (principal_id, request_id)
Import  := import_session_id:str ‖ stage:u16 ‖ source_digest:32 ‖ plan_ref:32
        ‖ expected_counts:[CountPair] ‖ import_complete:bool        // 按 import_session_id
Counter := counter:u16 ‖ high_water_mark:u128                       // 按 counter；u128 见 §4.4
```

`Counter` 的高水位宽度必须与 `165 high_water_mark` 一致（`u128`）。二者若不同宽，
`bookkeeping_digest` 就会在两个“同一状态”的实现间产生不同值——这正是 delivery-4 的
缺陷：字段表写 `u128`（且 golden 字节按 16 字节发射），而本节写 `u64`，于是照字段表
实现的读取器算出的 `bookkeeping_digest` 与 golden 不符。

把 RequestOutcome 与 ID 高水位从 `state_digest` 拆出，是为了让无条件自指可证明：
`RequestOutcome` 携带 `payload_digest`，而 `payload_digest` 属于请求输入；若它在
`state_digest` 内，则 `payload_digest` 的规范字节只要提到 `state_digest` 就成环（§3 第 1 条）。
拆开后 `bookkeeping_digest` 与 `state_digest` 互相独立，都由 manifest 发布，都可由日志
独立重算。丢失 `bookkeeping_digest` 的校验会让幂等重试无法被证明，因此它也是必现字段。

### 4.7 `payload_digest`（domain `request`）

`payload_digest` 是**调用方请求**的摘要。D3 已批准的作用域切分是：

| 层 | 负责什么 |
|---|---|
| **P1（本规范）** | 请求的**有界帧结构**与其哈希：字段顺序、长度前缀、上限、读集合、期望版本，全部在此固定 |
| P2 | 每个操作的**语义**（`assert-relation` 到底改什么、需要哪些校验） |

P1 因此定义“如何把调用方给的不透明操作负载装进一个可哈希的帧”，**不**定义任何
操作的内容。P1 的合成夹具有边界用途：它们是**帧结构**的测试数据，
**不得**被声明为 P2 生产操作编解码器。

```
canonical request bytes :=
  canon_version:u16 = 1
  op_count:u32                          // 1..=max_ops（§4.7.2）
  Op[op_count]
  read_set:u32 ‖ RevPair[read_set]      // 按 RevPair 数值升序，无重复
  expected_version:u8 ‖ u64?            // 0 = 不出现；1 = 后随 u64

Op :=
  op_kind:u16 | op_version:u16
  payload_len:u32 | payload[payload_len]   // 调用方自有的不透明规范字节

request_digest := digest("request", canonical request bytes)
```

`Op` 的三个字段都是**不透明**的：格式只校验宽度与上限，不解释
`op_kind`/`op_version`/`payload` 的内容。P2 的登记表给这些值分配含义；未登记的组合
在 P1 是“合法帧、未知语义”，由受理层在 P2 拒绝，而不是由格式层猜测。

**为什么不是“P1 实现版本 1 的操作集”**：那会把 P2 的语义提前固化进持久契约，
而且 P1 没有操作语义可测，只能测自己编的夹具——这正是 D3 禁止的“把未定义的 P2
操作表当成已实现”。

### 4.7.2 上限与排序（P1 可强制）

| 项 | 值 | 违反 |
|---|---|---|
| `op_count` | `1..=256` | 0 → `EMPTY_REQUEST`；>256 → `BUDGET_EXCEEDED` |
| 单 `payload_len` | `1..=65536` | 超出 → `BUDGET_EXCEEDED` |
| `read_set` 元素数 | `0..=1024` | 超出 → `BUDGET_EXCEEDED` |
| 请求帧总字节 | `<= 1 MiB` | 超出 → `BUDGET_EXCEEDED` |
| `expected_version` | 0 或 1（出现时后随 `u64 >= 1`） | 其他 → `INVALID_ARGUMENT` |

**`Op` 序列不排序**，按其声明顺序参与哈希：操作顺序是请求的一部分，重排是另一个请求。
`read_set` **必须**按 `RevPair` 数值升序且无重复，因为它是集合；乱序或重复即
`INVALID_ARGUMENT`（不是静默排序——静默排序会让两个不同的客户端字节串算出同一摘要）。

### 4.7.3 三个“不是请求”的对象

#### 4.7.1 genesis 与 outcome 不是请求（显式例外）

两处最容易把作用域搞错，因此单独固定：

1. **genesis 没有 `payload_digest`。** 它是初始化，不是调用方请求。tx 头 `178` 写
   32 个 `0x00`，`206 request_canon_version = 0`。这两个值是**哨兵**，语义是
   “本事务不来自调用方请求”，不是“空请求的摘要”。
   实现规则（不是概率论证）：**全零 `payload_digest` 是保留哨兵，任何真实请求都不得
   使用它**——即使某次计算恰好得到全零，也必须当作 `INTERNAL` 拒绝，而不是静默接受。
   这样哨兵与真实摘要不会因为偶然相等而产生歧义。
2. **`RequestOutcome` 不是请求的一部分。** 它由提交者在准备期生成并写入同一事务，
   携带 `payload_digest`。把它纳入请求字节会立刻自指（§3 第 1 条），也会让重试
   因为新的 `commit_version` 而算出不同摘要，直接破坏幂等。

`206 request_canon_version` 的取值域因此是三个值，各有唯一含义：

| 值 | 含义 | 谁写 |
|---|---|---|
| 0 | 非请求事务（genesis）：`178` 为哨兵 32×`0x00` | store 初始化 |
| 1 | 调用方请求，规范版本 1 | 受理层计算，store 校验 |
| ≥2 | 保留：未来规范版本 | 必须同时给出新字节定义 |

`>= 2` 在本轮是 UNSUPPORTED_FORMAT，不是“向前兼容地接受”。

**明确排除**（否则自指或重试不同值）：

| 排除项 | 理由 |
|---|---|
| `RequestOutcome` 记录（含 `payload_digest` 本身） | 自指（§3 第 1 条） |
| `IdAllocation`、`ImportBookkeeping` | 提交期/运维产物，不是调用方操作 |
| `version` / `commit_version` | 提交时才确定，重试时会是新版本 |
| `created_at` / `updated_at` / 任何提交时间 | 重试会取新时间，破坏幂等比对 |
| `principal_id` | 不在请求正文里；它来自认证结果（server §3）。身份在幂等键中单独出现 |
| `manifest_sha256` / `tx_file_sha256` / `state_digest` / `chain_digest` | 提交后才有值 |
| `engine_version` | 实现版本不是请求内容 |

`payload_digest` 出现在两个位置且必须相等：tx 头 `178` 与 `RequestOutcome.178`
（genesis 只有 tx 头，且为哨兵）。

把 `principal_id` 排除在 `payload_digest` 之外是**有意**的：幂等键是
`(kb_uuid, principal_id, request_id)`（server §3），身份已在键里；若再把身份混进负载摘要，
则“同一负载、不同主体”会被判为 `IDEMPOTENCY_MISMATCH` 而不是 `PERMISSION_DENIED`，
错误分类就错了。

### 4.8 `engine_version` 不进入任何持久摘要

`engine_version` 是构建身份，不是数据。它不进入 `rules_digest`、`graph_digest`、
`state_digest`、`bookkeeping_digest`、`chain_digest`，也不写入 manifest 或 tx。
理由：写进文件会让 golden 字节依赖构建，且把“升级二进制”伪装成“数据变了”。
它只出现在：
- 查询结果信封（`ResultIdentity`, P0 已如此）；
- 缓存键（inference §7）；
- 运维指标与日志。

### 4.9 `result` 与 `search`（运行时域，不落盘）

```
result payload := kb_uuid:16 ‖ version:u64 ‖ rules_profile:u16 ‖ rules_digest:32
               ‖ graph_digest:32 ‖ relation:u16 ‖ source:u32
               ‖ row_count:u32 ‖ Row[row_count]          // Row 按规范键排序
Row            := from:u32 ‖ relation:u16 ‖ to:u32 ‖ annotation:u8
                ‖ flags:u8        // bit0 has_asserted, bit1 has_derived, bit2 disputed
```

`Row` 与 P0 的 `sage_db_types::CanonicalRow` 字段一一对应：`from`、`relation`、`to`、
`annotation`，以及 `has_asserted`、`has_derived`、`disputed`（见
`sage-db/crates/types/src/query.rs` 的 `CanonicalRow`）。三个布尔量打包成 `flags:u8`
是**编码选择**，不是语义简化：漏掉它们会把“本方向有作者断言”与“只有派生证明”的
两行算成同一行，从而让差分比较失去意义。保留位 `3..=7` 必须为 0。

`result` 不含 `engine_version`：两个实现版本必须给出相同的规范行才能声称语义一致
（inference §6.1），因此把引擎版本混进结果摘要会让“语义一致”无法被表达。
它不含执行统计、proof 字节、行到达顺序。

P1 **不实现** `result` 域的最终编码：它的输入是 `CanonicalRow`，而查询在 P3 才实现。
本节的字节定义只是把“必须包含哪些字段”固定下来（防止 P3 用少字段的行集合重新定义
摘要）；在 P3 落地前不得声称已有 `result_digest` 的 golden 向量。

`search` 域覆盖检索投影的类型/ID/修订/状态/scope 与文本 blob 哈希（storage.md §4.3），
在 P3 检索实现时按其登记表固定；P1 不计算它，也不把它写进任何持久文件。

## 5. 规范规则字节（两个 profile）

`rules` 域覆盖**允许推导什么**：profile 名字、7 条关系的完整模板、以及 profile 全局
的校验规则。只 hash “profile 名 + transitive/symmetric” 不能称为完整规则配置：那样
formal-v1 的 `instantiates` 基数约束、L3 覆盖约束、定义边与 `depends-on` 并集无环都不在
版本化配置里，磁盘上的规则摘要就无法证明这些语义确实被启用。

### 5.1 字节布局（`canon_version = 1`）

```
Rules :=
  format_version:u16 = 1
  profile:u16                         // 0 legacy-v1, 1 formal-v1
  profile_name:str                    // "legacy-v1" / "formal-v1"，冗余但人类可读
  relation_count:u16 = 7
  Relation[relation_count]            // 按 relation id 升序
  validation_count:u16 = 12
  Validation[validation_count]        // 按 rule id 升序

Relation :=
  id:u16                              // 0..=6，与 sage_db_types::RelationId::get 一致
  name:str                            // wire 名，如 "is-a"
  is_definition:u8
  transitive:u8
  symmetric:u8
  acyclic:u8
  layer_rule:u16                      // 0 unrestricted / 1 target<=source / 2 target<source
  implies_dependency:u8               // R3

Validation :=
  id:u16                              // 1..=12
  enforcement:u16                     // 0 off / 1 on （未知值非法）
  params_len:u32 | params             // 按 id 的既定布局，见 §5.2
```

### 5.2 校验规则登记表（`id` 与 `params` 布局）

| id | 规则 | legacy-v1 | formal-v1 | `params` |
|---|---|---|---|---|
| 1 | R1 传递复合 | on | on | `u32 n` + `u16 relation_id[n]` = `[0,1,3]` |
| 2 | R2 对称补全 | on | on | `u32 n` + `u16[]` = `[5,6]` |
| 3 | R3 定义蕴含 `depends-on` | **off** | **on** | `u16[]` = `[0,1,2]` |
| 4 | V2 自引用禁止 | on | on | 空 |
| 5 | V3 层向（逐关系携带 `layer_rule`） | on | on | 空 |
| 6 | V4 `instantiates` 每主体至多 1 个不同目标 | **off** | **on** | `u32 subject_count` = 1，`u16 relation_id` = 2 |
| 7 | V5 活动 L3 必须有实例化目标 | **off** | **on** | `u8 source_layer` = 3，`u16 relation_id` = 2，`u8 min_targets` = 1 |
| 8 | V6 每个 `acyclic` 关系无环 | on | on | `u16[]` = `[0,1,2,3]` |
| 9 | V6b 定义边 ∪ `depends-on` 无环 | **off** | **on** | `u16[]` = `[0,1,2,3]` |
| 10 | 注解代数（bottom < contested < hypothetical < confirmed，路径 min / 选择 max） | on | on | `u16[]` = `[0,1,2]` |
| 11 | 最宽路径稳定访问序 | on | on | `str` = `"relation_id,target_id,assertion_id"` |
| 12 | `asserted`/`derived`/`disputed` 来源标记规则 | on | on | 空 |

`enforcement = 0` 的规则**仍在字节里**（不是省略）：这样“off”是显式配置，而不是
“旧版本没这条规则”。两者语义不同：前者可被新的加载器解释为“明确关闭”，后者是“未知”。

规则 5 的参数为空，因为层向是逐关系的 `layer_rule` 字段；把逐关系值再列一遍会产生
第二个可矛盾的真源。

### 5.3 与 P0 类型的一致性

`RelationSpec { id, is_definition, transitive, symmetric, acyclic, layer_rule,
implies_dependency }` 与 `relation.rs` 的 `RelationId::spec` 完全对应：

| relation | is_def | trans | sym | acyclic | layer legacy | layer formal | implies dep legacy/formal |
|---|---|---|---|---|---|---|---|
| is-a (0) | 1 | 1 | 0 | 1 | 1 | 2 | 0 / 1 |
| part-of (1) | 1 | 1 | 0 | 1 | 1 | 2 | 0 / 1 |
| instantiates (2) | 1 | 0 | 0 | 1 | 1 | 2 | 0 / 1 |
| depends-on (3) | 0 | 1 | 0 | 1 | 0 | 0 | 0 / 0 |
| enables (4) | 0 | 0 | 0 | 0 | 0 | 0 | 0 / 0 |
| contradicts (5) | 0 | 0 | 1 | 0 | 0 | 0 | 0 / 0 |
| analogous-to (6) | 0 | 0 | 1 | 0 | 0 | 0 | 0 / 0 |

（`layer_rule`: 0 unrestricted / 1 target<=source / 2 target<source；与 `RelationId::spec`
的 `LayerRule` 映射一致。）上表是**取值的第二个表述**，因此必须由测试对照
`sage_db_types::RuleProfile::spec` 逐项比较，防止两处漂移。

### 5.4 计算与校验顺序

```
rules_bytes        = encode_rules(profile)              // 由 profile 常量生成，无外部输入
rules_ref          = raw_sha256(rules_bytes)            // blob 路径
rules_digest       = digest("rules", rules_bytes)
```

写者：把 `rules_bytes` 作为 blob 写入 `blobs/<rules_ref>`，在事件中写 `166 rules_ref`
与 `163 rules_digest`。
读者：`read(blobs[166])` → `raw_sha256 == 166` → `digest("rules", bytes) == 163` →
`bytes.profile == 196`。四步全过才算规则可用；任一步失败是 CORRUPT。

**不使用编译期常量代替**：`163` 是数据，可能来自更早的版本或迁移事务。只与
“当前二进制里的常量”比较会漏掉“磁盘上的规则被替换成另一套而同 profile 名不变”。

## 6. 规定的计算顺序（每笔提交）

固定顺序，任一环失败即整体不提交：

1. 候选状态折叠完成（内存），已应用全部操作。
2. `artifact` 写入 staging：新增 blob（正文/资产/向量/提案 payload/结果负载/规则字节）。
3. `rules_digest` ← 由 `(profile, rules_bytes)` 重算（若本事务未改规则，仍重算一次，
   防止内存中的配置与磁盘 blob 不一致）。
4. `payload_digest` ← 已由客户端/服务层在受理时确定，本步骤只**读取**，不重算。
5. `result payload` ← `result_kind`、`commit_version = prev + 1`、`affected`、`counters`。
   `result_ref ← raw_sha256(result payload)`。**此时 `RequestOutcome` 的六个字段全部确定。**
6. `state_digest` ← 折叠记录（§4.4），**不含** RequestOutcome/ImportSession/高水位。
7. `graph_digest` ← 活动图投影（§4.5）。
8. `bookkeeping_digest` ← outcome + import + counters（§4.6）。
9. 编码 tx：`tx_header`（含 `178 payload_digest`、`206`）+ 事件（含 `192 RequestOutcome`）
   → `tx_digest` → 文件 → `tx_file_sha256`。
10. `chain_digest` ← `(prev_chain_digest, tx_digest)`（§4.3）。
11. 编码 manifest（`198/199/200/205`、`204 tx_file_sha256`）→ 文件 → `manifest_sha256`。
12. 写 `CURRENT`（引用 `manifest_sha256`）。
13. 发布快照 → ACK。

第 5 步为什么在 `state_digest` 之前：结果负载只含提交前可确定的字段，而 `state_digest`
需要折叠后的完整记录。两者互不依赖，顺序固定只是为了确定性。
第 9 步必须在第 5–8 步之后：`tx` 覆盖 `payload_digest` 与全部事件。
第 10 步必须在第 9 步之后：链摘要覆盖 `tx_digest`。
第 11 步必须在第 10 步之后：manifest 携带 `chain_digest`。

第 6 步 `state_digest` 显式**排除** RequestOutcome——这正是 §3 第 2 条要求的：
`RequestOutcome.result_ref` 指向结果负载，而如果结果负载或 `RequestOutcome` 进入
`state_digest`，则 `payload_digest`（在 `RequestOutcome` 内）的规范字节只要提到
`state_digest` 就成环。拆到 `bookkeeping_digest` 后两个摘要平行且都被 manifest 发布。

## 7. 测试向量

全部由 `/tmp/sage-db-p1.2yjLG4/format-contract/vectors/gen_vectors.py` 独立生成：
手写自本文件与 storage-format.md，**不**来自任何 Rust 编码器。脚本自检
`CRC32C("123456789") = 0xE3069283`，并在退出前校验每个 golden 文件的头部 CRC、
trailer、`payload_len` 与块 CRC。完整日志：同目录 `out4.txt`。

固定输入：`kb_uuid = 00112233445566778899aabbccddeeff`、`tx1_id = 0f1e2d3c…e1f0`、
`tx2_id = 1a2b3c4d…f809`、`created_at = 1763424000000000`、`rules_profile = formal-v1`。

### 7.1 CRC-32C

| 输入 | 值 |
|---|---|
| `"123456789"` | `0xE3069283` |
| `""` | `0x00000000` |
| 32 × `0x00` | `0x8A9136AA` |

### 7.2 域分离（空负载）

| 表达式 | 值 |
|---|---|
| `digest("tx", "")` | `a0fd46c377db2c75146dcb081a0034fadf8419dcdafa9ff2171c5a375ea14707` |
| `digest("manifest-chain", "")` | `cee19577ff68120b219c8115900447e93cbc68b6be794e1c0b2f1927fd9fea68` |
| `digest("state", "")` | `8cfbe0a9e6d94103337e2130b2c36e60d6b608026db7dc4b25ac8f56f91f1f90` |
| `digest("graph", "")` | `64b35ca9523627121dfc06edd0ecfc0e8bba6636772cbba3dcd3ebde2808a0ec` |
| `digest("rules", "")` | `af60b5273db61dd76e15ea9db6d2c8897a93b5cdf63ffdcf0e3fdbf921e39bc1` |
| `digest("bookkeeping", "")` | `2fc6fab885abb972663adb19553ea5feafed8fb91ce3b17a8a4ab922ff898f62` |
| `digest("request", "")` | `dda8bdbda2ce15b05d3b67aa13625589dd03b70dd86de8be570144cfd4f2fa51` |
| `raw_sha256("")` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |

七个域在同一空负载上互不相同，且都不等于 `raw_sha256("")`。

### 7.3 规范规则字节

| profile | 长度 | `rules_ref`（raw SHA-256） | `rules_digest`（domain） |
|---|---|---|---|
| legacy-v1 | 381 B | `b6f92f3c3806be34b9343cae9524e8b7796395942e2d7d21e99ad1b960a2ec16` | `b4f0366e9ba49f5984d0f10e142c8ce6854d5b321ebe89065878156b88aef741` |
| formal-v1 | 381 B | `04342325f9c6733f4dc100ee8d310b1d862f82f894b23589c303d87f25654f88` | `19fe486994acf70283c1055ffa8a27057fe7b764146b1c9c7ac0b3a3f3176ded` |

两个 profile 长度相同，但 profile 字节、三个定义关系的 `layer_rule`、三个关系的
`implies_dependency`、以及规则 3/6/7/9 的 `enforcement`/参数不同。**这是判别式**：
只 hash “profile 名 + transitive/symmetric” 会给两者相同摘要，正确实现必须不同。

### 7.4 Golden genesis（v1）

| 项 | 值 |
|---|---|
| 事件序列 | `Genesis(224)` + 9 × `IdAllocation(176)`，counter 0..8，`hwm = 0` |
| `Genesis` 字段 tag | `2, 8, 144, 145, 146, 147, 148, 163, 166, 196`（含 REQUIRED `2 revision = 1`） |
| `tx_header` tag | `8, 144, 163, 176, 177, 178, 192, 193, 194, 195, 196, 206` |
| body / file | 878 B / 958 B |
| `header_crc32c` | `0x0C6D5AE6`（整 32 B 头、`24..28` 置零） |
| 块 `crc32c` | `0xD23A4005` |
| `tx_digest` | `789427a27e6934592dbdc004fd052c891da3f2bcc284c22960917c2fa2988c0e` |
| `tx_file_sha256` | `2a185bd2dc8c4fdefa5d9cbd30260bb1faf84c84ebdd0dea85aed4cceab2f5f4` |
| `state_digest` | `b6422a4ac1ef53598c2259991facb9bf16079b479aee20c97ced428859aacc88` |
| `graph_digest`（无节点无边） | `35efdf863b38edc1ac307e20f95b7990e35c3f1c5505eff5c9f4b4ddafb42d4e` |
| `bookkeeping_digest` | `756daff9afe2bad1df7caf317f0c58d72a9013e172b0de5e58fc4e5a935ae902` |
| `chain_digest`（prev = 32×`0x00`） | `54c68fe5d89508acb2022c07cb4b53642a6bc0a88e64e8f5ebb63aa2f88999b7` |
| manifest body / 文件 | 403 B / 439 B |
| `manifest_sha256` | `8be1beb5fd3f453e62d703f7061a11f52dc7a61586bf2c01b808355f9dec3f5b` |
| CURRENT（n = 44） | 118 B，`payload_len = 106`，`crc32c = 0x080A46B5` |

**`state_digest` 的输入是可算的**：两条记录，`key` 均为空（单例），`revision = 1`：

| state kind | 记录 | body 长度 |
|---|---|---|
| 12 | KbMetadata | `af` = 175 B |
| 13 | RuleConfig | `a6` = 166 B |

`body` 分别是 `Genesis` 字段按 §4.4.6 划分后、去掉 tag 1/13 的 `Field` 序列。

**头部 CRC 判别式**：同一文件若按“跳过 24..28 的 28 字节”计算会得到
`0x3288BFF3`，而正确值（整 32 B、该 4 字节置零）是 `0x0C6D5AE6`。两个数不同，
因此这是一个能区分两种实现的测试，而不是措辞问题。

**CURRENT 的 `payload_len` 必须包含尾部 CRC。** 偏离曾在 delivery-2 的真实字节里出现：
文件 118 B、名字 44 B，按 §3 应为 `payload_len = 62+44 = 106 = 118-12`，而旧的 golden
写了 `102`（把 CRC 排除在外）。这类错误**无法**由“重算规范常量”的检查发现，只能由
解析真实字节发现——因此本节的验证由 `verify_golden.py` 完成：它独立重实现信封规则、
直接解析 `golden.*` 二进制，并断言 `payload_len == len - 12`。

### 7.5 Golden 普通事务（v2）

内容：`TermCreated(1)`（Term id 1，revision 1）+ `IdAllocation(176, counter 0, hwm 1)`
+ `RequestOutcomeRecorded(192)`。

`IdAllocation` 是必需的：本事务消费了一个 Term ID，高水位因此推进；推进必须留事件，
否则只读事件流的实现算不出本事务的 `bookkeeping_digest`（正是 delivery-4 v2 的缺陷：
`6ce900d8…` vs golden `37f91c0b…`）。

| 项 | 值 |
|---|---|
| 调用方请求字节 | `0100 01000000 0100 0100 10000000 6f703a6372656174652d7465726d3a31 00000000 00`（35 B：canon 1、1 op、kind 1、ver 1、payload 16 B、read_set 0、expected_version 缺省 `00`） |
| `payload_digest` | `7f37fecacf9bea01fa4621e3bb24ad5c0d798dc3677916a2eb20ebf821e940bb` |
| 结果负载 | `0000 0200000000000000 01000000 <RevPair(Term1,rev1)> 01000000 <counter 0, hwm 1>` |
| `result_ref` | `299e6e881bcb39d49e7cd83aecb7180f7297e223901165e478b5c901c114251d` |
| body / file | 723 B / 803 B |
| `tx_digest` | `0e14c566b5ebafc4e5f8efd0f46b6474271c779edd0a49bbcf7f3caa8bc54d4b` |
| `tx_file_sha256` | `3ffc2989001f694fbc63f0182dc32657cd859697455987f0fc74bbe55237b109` |
| `state_digest` | `381bd739b2978e45f457dcb0452fe88d96d8c653a8b620ad018ec4d638c89ad4` |
| `graph_digest`（1 Term，无断言） | `7c9f89cc6e83bb47fdfabaef8ce13b8435eba7c02412ec8f8afae85372f0210a` |
| `bookkeeping_digest` | `45099a57f1c3f65138f6839dceb802f580f18fb2ada25f61827b861a76bbfc68` |
| `chain_digest` | `29715e1c1cb80c724cb3e1e3bd9c854b4cf84b29e48c7aff241297ef6ceb96dc` |
| manifest 文件 | 535 B，`manifest_sha256 = 46e718c8703b95565888fe7668b5c6b07d6a8bb0f64d856adc3a5d2651ec5ec7` |
| CURRENT（version 2） | 118 B，`payload_len = 106`，`crc32c = 0xA615AC37` |

v2 的 `chain_digest` 引用 v1 的 `chain_digest`（§4.3），因此它同时证明“链前进”
与“`manifest_sha256` 不进入链”。

### 7.5b Golden 修订事务（v3）——after-image 完整性与自然键更新

内容：`TermRevised(2)` + `KbMetadataUpdated(144)` + `IdAllocation(176, counter 0, hwm 3)`
+ `RequestOutcomeRecorded(192)`。这个夹具专门覆盖两件 prose 无法证明的事：

1. **修订的完整 after-image**：`TermRevised` 携带 Created 时的全部记录字段，外加只有修订
   才会出现的内容——`10 legacy_id`、`11 legacy_source_path`、`12 legacy_content_digest`
   （非空）、`14 extensions`（两个键 `legacy.provider` / `legacy.dimensions`）、
   `9 updated_at`、`13 changed_fields`、`7 reason`。若某个字段被允许“从上一修订继承”，
   这一条就会失败。
2. **自然键更新**：`KbMetadataUpdated` 使 state kind 12 的 `revision` 从 1 变为 2，
   而折叠结果中**仍然只有一条** kind-12 记录（键为空）。这证明“同键多事件是修订、不是
   多条记录”，也证明未在本版本变更的 kind 13 原样继承。

| 项 | 值 |
|---|---|
| `tx_digest` | `6cd76367424772b2ee57fd9dc4c9b5c102ffe52af113777488472904a5d08b8a` |
| `tx_file_sha256` | `2b0fafeccb14b3c36085aecd34650a83038a45d0287fe49de57dc40be135900a` |
| `state_digest` | `ca16fcb6337bd3d8e9f2e709a1a74c11f95b39f3bcd440ad7cc7b4d04cb5abf7` |
| `bookkeeping_digest` | `377bf5c189b55d0989e9950e30632ea561a0ec0cfef974a767d465a52fbfdede` |
| `chain_digest` | `e365cf9cb0b9bb0b106455d2d5772accc3ee5136d60d71c5df52cecc20f4f6a7` |
| `manifest_sha256` | `ffac55c932183e094579f1f24e88be954aede72c117b778138c5a94776d522da` |

`bookkeeping_digest` 是**累积**的：v2 的 `req-1` outcome 仍在库中，v3 追加 `req-3`。
把它当作“本事务 outcome”实现会得到不同的值——这是该夹具的第二个判别式。

### 7.6 不提供向量的项

| 项 | 原因 |
|---|---|
| `result` / `search` 域 | 输入是运行时查询结果，P3 才实现；域名字符串与字段清单已固定（§4.9），但无 golden |
| 其他事件的 after-image | 布局由 §4.4.3 与字段表完全确定；P1-2 的 golden fixture 必须逐事件补齐 |

## 7.7 与 store 的共享预算接缝（R1 / store SEAM §6b）

摘要与编解码必须能在一个**跨整条链**的账本下运行，因为“每文件限额 × 无限链长”不是边界。
事实来源是**一个**共享账本 trait（store v4 与本文采用同一形状），不另立第二套术语：

```rust
pub trait SharedLedger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError>;        // 先准入，再分配
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError>; // 目录项/元素
    fn checkpoint(&mut self) -> Result<(), ControlStop>;                     // 调用方策略
}
```

`try_charge_entries` 必须与 `try_charge` 分开：目录项不是字节。把“一项折合多少字节”
塞进字节账本，会让一个**策略常量**冒充**实测用量**，任何报告都无法再区分两者。

`format` 侧因此必须满足四条（缺一不可）：

| # | 要求 | 为什么 |
|---|---|---|
| 1 | 每个解码/摘要入口接收 `&mut dyn SharedLedger`，并在**分配或复制之前** `try_charge` | 事后累加 `Vec::len()` 已经分配过了，正是 I10 禁止的形态 |
| 2 | 不返回无界的自有集合：`blob_refs` 走调用方提供的 sink，遍历走 visitor | `blob_refs(..) -> Vec` 会在整链验证中逐事件分配 |
| 3 | `state_digest` 接收 store 折叠后**已排序、无重复**的 `RecordSet`，排序失败即拒绝 | 静默重排会掩盖折叠缺陷；同时避免 format 自己分配排序缓冲 |
| 4 | 预算拒绝是类型化结果（`FORMAT_LIMIT` 或 `BudgetExceeded`），绝不“先做一半再返回” | 与 `ControlStop` 一样，中断不是结果 |

`Limits` 仍然存在，但它只表达**单文件结构上限**，不能替代 `SharedLedger`：两者职责
不同，一个是“这个文件最大多少”，一个是“这次打开/提交总共还能用多少”。同理
`FormatError::Limit` 与 `BudgetError` 是两种拒绝：前者是输入或配置上限的性质，
后者是本请求剩余预算的性质；合并会让同一条超限无法判断该改数据还是该加预算。

取消/中断（`ControlStop::Cancelled | DeadlineExceeded`，P0 类型）与预算拒绝必须分开：
前者是调用方策略，后者是资源上限；两者都返回类型化错误，都不返回部分结果。

## 8. 变更控制

- 域名字符串是兼容契约：改名等于改格式。
- `canon_version`（规则字节的 `1`、请求字节的 `1`）是各自域的版本开关；改输入范围必须
  递增，且不得对同一已提交对象混用两个版本。
- 新增摘要必须给出覆盖矩阵的一行（§3.1）并证明无自环。
- 冻结 major 1 时，本文每个摘要的输入集合、排序键、省略字段、域名字符串都成为契约。
