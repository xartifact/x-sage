# 存储字节格式规范（draft, format major 0）

> 上游：[storage.md](storage.md)（提交协议、恢复、权威性）；摘要规则：[storage-digests.md](storage-digests.md)。
> 记录语义：[data-model.md](data-model.md)；求值语义：[inference.md](inference.md)。
> 本文是 P1 编解码器的**唯一格式依据**。它取代 storage.md §4 的字段区间表与事件摘要表；
> 两者冲突时以本文为准，并把冲突登记回 storage.md，而不是各自解释。

状态：**draft 编码，major = 0**。以下每一条都是可实现的规范值，不留“实现时再选”。
major 0 不构成兼容承诺；`major 1` 的冻结条件见 storage.md §4.4。

## 1. 通用约定

| 项 | 规则 |
|---|---|
| 字节序 | 所有定宽整数小端（LE）；没有例外，没有 varint |
| 对齐/填充 | 无隐式填充，无对齐要求，结构之间不补零 |
| `bool` | `u8`，只允许 `0x00` / `0x01` |
| 枚举 | `u16`，**未知值解码失败**，不回退为第一个变体 |
| 字符串 `str` | `u32 字节长度` + UTF-8 字节；禁止 NUL 字节；长度只描述 `value`，不补零 |
| 定长字节 `bytes32` | 32 字节原文（摘要或 blob 引用） |
| 列表 `[T]` | `u32 数量` + 逐元素 `T`；`0` 合法（事件表另有限制时除外） |
| map | `u32 数量` + 按规范键排序的 `K, V` 交替；重复键即 CORRUPT |
| 浮点 | 只允许出现在向量 blob 内（IEEE-754 float32 LE）；任何文件结构中出现浮点即格式错误 |
| 时间 | `i64`，UTC 微秒（Unix epoch 起），由提交者写入；重放不得重新取时间 |
| 压缩 | 见 §2.3；major 0 只允许 `codec = 0 (none)` |

### 1.1 复合类型

```
RecordKey      := kind:u16 | id:u128           // 18 B；两者都按数值比较
RevPair        := RecordKey | revision:u64     // 26 B
ExtMap         := { str(≤128 B, §8.3) : bytes }
CountPair      := kind:u16 | count:u64         // expected_counts 的元素（行数，不是 ID）
Watermark      := space:u16 | hwm:u128          // ID 高水位（与 CountPair 是不同的空间）
```

`RecordKey` 只用于**有数值身份**的记录。`id` 编码宽度恒为 16 字节，按 kind 解释：

| kind | 记录 | `id` 合法值 | 修订 |
|---|---|---|---|
| 1 | Term | `<= u32::MAX`，高 12 字节必须为 0 | `2` 必现 |
| 2 | Assertion | 任意 u128 | `2` 必现 |
| 3 | Claim | 任意 u128 | `2` 必现 |
| 4 | Evidence | 任意 u128 | `2` 必现 |
| 5 | EvidenceLink | 任意 u128 | `2` 必现 |
| 6 | Vector | 任意 u128 | `2` 必现 |
| 7 | Proposal | 任意 u128 | `2` 必现 |
| 8 | FrontierEntry | 任意 u128 | `2` 必现 |
| 9 | SessionCheckpoint | 任意 u128 | `2` 必现 |

`0` 与 `>= 10` 在本表中非法：`RecordKey.kind` 是**封闭集合**，未知值即
UNSUPPORTED_FORMAT（不是“新记录类型，先跳过”）。

下列记录**没有** `RecordKey`，用自然键标识。“自然键唯一”指的是**记录身份**唯一，
不是“日志里只能出现一条事件”：有修订的记录沿自然键形成修订链，每个事件是新修订。

| 记录 | 自然键 | 修订 | 同键多事件 |
|---|---|---|---|
| Asset | `sha256`（84） | 无修订 | **禁止**：内容寻址记录只声明一次 |
| Model | `model_key`（149） | `2` 必现，链式递增 | 允许（注册/更新/退役 = 同一记录的多个修订） |
| KbMetadata | 单例（每 KB 一条） | `2` 必现，链式递增 | 允许（每次更新一个新修订） |
| RuleConfig | 单例（当前生效配置） | `2` 必现，每次显式迁移递增 | 允许（每次迁移一个新修订） |
| RequestOutcome | `(principal_id, request_id)` | 无修订 | **禁止**：首次结果不可改写 |
| ImportSession | `import_session_id` | 无修订 | 允许：同一导入会话的阶段推进，按 `8 created_at` 严格递增定序 |

`ImportSession` 与 `Model` 的“同键多事件”不是修订：前者是同一会话的阶段序列，
后者是修订链。两者的读法不同，写进日志的形式相同（同键事件按出现顺序折叠）。

`Genesis`、`IdAllocation` 不是记录事件：前者定义单例元数据，后者推进分配高水位。

### 1.2 三种顺序

“按规范排序”一律指**按数值**排序，不是按编码字节：LE 编码的 `u128` 字节序与数值序
不一致，按字节排序是错误实现。

| 类别 | 成员 | 规则 |
|---|---|---|
| 集合/映射（排序） | 字段/事件内 tag、`ExtMap` 键、`base_revisions`、`expected_counts`、`changed_fields`、`concept_refs` | 数值升序；重复即 CORRUPT |
| 作者序列（保序） | `domains`(19)、`kb_domains`(148)、`triggers`(22) | 原样保序，**不排序**；重排是内容变更（新修订） |
| 位置序列（无显式顺序） | `events` | 由出现位置定义 `event_seq`（§4.1） |

字符串比较（用于 `ExtMap` 键）为逐字节无符号比较（即 UTF-8 字节序）。

### 1.3 限值（major 0 固定值）

超限一律**拒绝整个文件**（`FORMAT_LIMIT`），不截断、不跳过、不部分受理。

| 限值 | 值 |
|---|---|
| 单文件：transaction | 64 MiB |
| 单文件：manifest | 8 MiB |
| 单文件：CURRENT | 4096 B |
| 单块 `raw_len` | 1 B ..= 16 MiB |
| 单块 `encoded_len` | 1 B ..= 64 MiB |
| 通用字符串 | 1 MiB（逐字段更小，见 §6） |
| 单事件字段数 | 64 |
| 单 tx 事件数 | 65536 |
| tx/manifest 头部字段数 | 32 |
| `[str]` / `RevPair` 列表 / `ExtMap` / `CountPair` 列表元素数 | 65536（逐字段更小，见 §6） |
| 定宽复合类型字节数 | `u8` 1、`u16` 2、`u32` 4、`u64`/`i64` 8、`u128` 16、`bytes32` 32、`RecordKey` 18（`u16`+`u128`）、`RevPair` 26 |
| 文件名长度 | 64 B |
| 文件嵌套层 | 1（事件不嵌套文件） |

## 2. 文件外层

### 2.1 公共文件头（32 B，所有 kind 共用）

| 偏移 | 字段 | 宽度 | 规则 |
|---|---|---|---|
| 0 | `magic` | 4 | ASCII `SGLB`；不匹配 → UNSUPPORTED_FORMAT |
| 4 | `major` | 2 | 本文为 0；未知值 → UNSUPPORTED_FORMAT |
| 6 | `minor` | 2 | major 0 必须为 0 |
| 8 | `kind` | 2 | 1 manifest / 2 transaction；其余 → UNSUPPORTED_FORMAT（§2.5） |
| 10 | `flags` | 2 | 必须为 0；非 0 → CORRUPT（major 0 没有可选头特性） |
| 12 | `header_len` | 4 | 必须恰为 32；不用于“跳过未知扩展头” |
| 16 | `payload_len` | 8 | 文件头之后剩余字节数，**含块与 trailer** |
| 24 | `header_crc32c` | 4 | 对**整个 32 B 头**计算 CRC-32C，计算前把本字段 4 字节置零（即覆盖 `header[0..24]‖00 00 00 00‖header[28..32]`）；不是“跳过”这 4 字节 |
| 28 | `reserved` | 4 | 必须为 0 |

CRC-32C = Castagnoli（反射多项式 `0x82F63B78`，正规 `0x1EDC6F41`），初值
`0xFFFFFFFF`，输入输出反射，末异或 `0xFFFFFFFF`。校验值：
`CRC32C("123456789") = 0xE3069283`。实现必须使用成熟库，不自写查表。

**“整头零字段”与“跳过字段”是两个不同输入**，必须选一个并全规范一致。本规范选：
输入长度**恒为 32 字节**，偏移 24..28 写 0。等价伪码：

```
buf = header[0..32]
buf[24..28] = 0x00 * 4
crc  = crc32c(buf)            // 32 bytes in
```

写成“hash `header[0..24] ‖ header[28..32]`”（28 字节）会得到**不同**的值，
是错误实现。测试向量必须能区分二者：同一 golden 文件上两种算法给出不同结果即为判别式
（见 storage-digests §7.4 的 `stored` 与 `sliced-variant` 两个数）。

`payload_len` 必须等于 `文件长度 - 32`，否则 CORRUPT（截断与拼接由此定位）。

### 2.2 块

`payload` 是 1..N 个块的紧密拼接：

```
Block := kind:u16 | flags:u16 | encoded_len:u32 | raw_len:u32 | crc32c:u32 | data[encoded_len]
```

| 字段 | 规则 |
|---|---|
| `kind` | 1 = body。major 0 只允许 1 |
| `flags` | 位 0 = REQUIRED（未知 kind 必须拒绝）；位 1–2 = codec：`0` none、`1` zstd（**保留**）；位 3–15 必须为 0 |
| `encoded_len` | 不得越过剩余 `payload_len`；不得为 0 |
| `raw_len` | 解压后长度，解码前用于拒绝超限块；不得为 0 |
| `crc32c` | 对**已编码** `data` 计算 |

`codec = 1 (zstd)` 在 major 0 中是**保留值**：写者不得写出，读者必须返回
UNSUPPORTED_FORMAT。这使“声明支持 zstd 却静默跳过”成为可测试的错误。

多个 kind=1 块按出现顺序拼接为 `body` 后再解析；块边界可以落在任意字段中间
（解码器必须先完整拼接，不能假定块边界对齐字段）。

### 2.3 Trailer

`payload` 的最后 32 B 固定为 `trailer_sha256 = SHA-256(header[0..32] || 全部块字节)`，
即覆盖 32 B 文件头与每个 `Block`（含块头），**不含** trailer 自身。major 0 中 trailer
恒存在：旧文 storage.md §4.1 的 “flags 位 0 = 含 trailer” 已撤销，可省略的校验和是
可降级攻击面。

块 `crc32c` 定位错误；trailer 验证内容；两者都不是对恶意写者的认证机制。

### 2.4 路径与命名

路径只由引擎生成；加载时按下列规则校验，不匹配即 CORRUPT（不“尝试修复”）。

| 文件 | 相对 `<kb>/sage-db/` | 规则 |
|---|---|---|
| 锁 | `LOCK` | 固定名 |
| 提交根 | `CURRENT` | 固定名 |
| manifest | `manifests/v000042-<mid>.sgm` | `^v[0-9]{6,20}-[0-9a-f]{32}\.sgm$`；数字段是 `192 version` 的十进制、至少 6 位零填充、值 `>= 1` |
| transaction | `transactions/tx-<tid>.sgl` | `^tx-[0-9a-f]{32}\.sgl$` |
| blob | `blobs/<sha256>` | `^[0-9a-f]{64}$` |
| staging | `staging/<tid>/` | 目录；内容不进提交协议 |
| reserve | `checkpoints/`、`cache/` | major 0 不写不读（§2.5） |

名字字节只允许 ASCII `[0-9a-z.-]`；拒绝 `/`、`\`、`..`、绝对路径、NUL、非 ASCII、
长度 > 64。`<mid>`/`<tid>` 是 16 字节标识的 32 位小写十六进制，不是摘要。

### 2.5 文件种类登记

| kind | 名称 | major 0 状态 |
|---|---|---|
| 1 | manifest | 实现（§5） |
| 2 | transaction | 实现（§4） |
| 3 | checkpoint | **保留**：写者不写，读者返回 UNSUPPORTED_FORMAT |
| 其他 | — | UNSUPPORTED_FORMAT |

保留 3 而不“先定义待补布局”：被接受的格式不能有未规定的正文。checkpoint 会在后续
minor 中带完整布局登记；P1 恢复不依赖它（storage.md §6）。

## 3. CURRENT 布局

`CURRENT` 不是 `SGLB` 文件：它是唯一可变、原子替换的根指针，必须极小且单次写入。
以下偏移是规范值，任何实现逐项比对；叙述性总长公式只作为交叉校验。

| 偏移 | 字段 | 宽度 | 规则 |
|---|---|---|---|
| 0 | `magic` | 4 | ASCII `SGLC`；不匹配 → UNSUPPORTED_FORMAT |
| 4 | `layout` | 2 | 必须为 1 |
| 6 | `flags` | 2 | 必须为 0 |
| 8 | `payload_len` | 4 | 偏移 12 之后的字节数 = **`62 + name_len`** |
| 12 | `kb_uuid` | 16 | 不可变 KB 身份 |
| 28 | `version` | 8 | 本版本号（= 该 manifest 的 `192 version`） |
| 36 | `name_len` | 2 | `1..=64` |
| 38 | `name` | n | ASCII，满足 §2.4 的 manifest 正则 |
| `38+n` | `manifest_sha256` | 32 | 整份 manifest 文件的 SHA-256（文件哈希，非域摘要） |
| `70+n` | `crc32c` | 4 | 对偏移 `[0, 70+n)`（`magic` 到 `manifest_sha256` 末字节）计算 |

算术不变式（测试断言这四条，而不是复述一句总长）：

1. 固定前缀 `[0,12)` 共 12 B；其后 `16+8+2 = 26` B 定长、`n` B 名字、`32` B 摘要、`4` B CRC。
2. `payload_len = 16+8+2+n+32+4 = 62 + name_len`。
3. `总长 = 12 + payload_len = 74 + name_len`；末偏移 `70+n` 的 4 B CRC 与之吻合。
4. `crc32c` 覆盖 `[0, 70+n)`，**包含** `payload_len` 字段；而 `payload_len` 只由
   `name_len` 决定（`name_len` 位于其覆盖范围之内但在 `payload_len` 之后），
   因此“CRC 覆盖长度字段”不产生循环：先定 `n`，再算 `payload_len`，再算 CRC。

数值向量（与 `vectors/out.txt` 的 golden CURRENT 同输入）：
`kb_uuid = 00112233445566778899aabbccddeeff`、`version = 1`、
`name = "v000001-0f1e2d3c4b5a69788796a5b4c3d2e1f0.sgm"`（**44 B**）、
`manifest_sha256 = 32 × 0xAB`。

| 项 | 值 |
|---|---|
| `name_len` | 44（`0x002C`） |
| `payload_len` | `62 + 44` = **106**（`0x0000006A`） |
| 总长 | `74 + 44` = **118 B** |
| `crc32c` 覆盖长度 | `70 + 44` = 114 B |
| `crc32c` | `0x914FBD87`（勘误，见下） |

> **勘误（2026-11-17）：本行的 `crc32c` 曾写作 `0x735D7266`。**
>
> 用上表四个输入（`kb_uuid = 0011…eeff`、`version = 1`、44 B 名字、
> `manifest_sha256 = 32 × 0xAB`）对偏移 `[0, 114)` 计算 CRC-32C 得到
> **`0x914FBD87`**，与规范自述的算术 `62 + 44 = 106` 一致。
> `0x735D7266` 的成因是**只把 `payload_len` 这一个字段写错**：保持上表其他字节
> （含 `name_len = 44` 及那 44 B 名字）不变，仅把 `payload_len` 误写为 **102**，
> 才复现该值；这不是另一种合法布局，而是同一文件上的单字段笔误
> （实测：`payload_len = 102` 且 `name_len = 40` 会得到第三个值 `0x7C41CD60`，
> 所以"少 4 B 名字"的解释连数值都对不上）。
>
> 独立复算：CRC-32C（Castagnoli，反射，多项式 `0x82F63B78`）在已知答案
> `b"123456789" → 0xE3069283` 的对照下通过，随后用上表字节得到 `0x914FBD87`。
>
> 修订范围：**仅此数值列**，冻结的 D10 delivery 不动。本勘误较早的一版曾把
> `0x735D7266` 归因于 `vectors/out.txt`，该归因已撤回——我没有对该文件做过字节级
> 复核，不应把它当作证据。三份实际 D10 `.current` 二进制在自身字节上 CRC 校验
> 正确，但它们**不是**本行这个 `32 × 0xAB` 示例，因此不为它作证；本节结论只来自
> 上表输入的直接复算。任何按 `0x735D7266` 生成的 golden 都需按正确输入重算。

规则：

- 内容完全由 `(kb_uuid, version, name, manifest_sha256)` 决定：**重新编码**同一版本产生
  逐字节相同的字节。**不存在原地重写**：写入只能
  `CURRENT.tmp`（新建）→ fsync → `rename` 覆盖 `CURRENT` → fsync 目录。
  “幂等”指重复提交同一版本得到同一字节序列，**不是** `pwrite` 覆盖同一 inode。
- CURRENT 没有自摘要：它是链的根；它的 `manifest_sha256` 指向上一环。
- 缺失 CURRENT 而目录非空不是“可初始化”，见 storage.md §2 / §5.2。
- 不允许多份 CURRENT、不允许多个 `.tmp` 名字（固定 `CURRENT.tmp`）。
- `name_len > 64` 或总长超过 §1.3 的 4096 B 拒绝阈值都是 FORMAT_LIMIT；`name` 不匹配
  §2.4 正则是 CORRUPT。

## 4. 事务文件（kind = 2）

```
body      := tx_header events
tx_header := field_count:u16 | Field[field_count]     // 按 tag 升序
events    := Event[event_count]                       // event_count = tx_header tag 195
```

`tx_header` 使用与记录字段相同的 `Field` 信封（§7.1）。

### 4.1 `event_seq` 的唯一定义

`Event` 信封固定 6 B：`tag:u16 | flags:u16 | field_count:u16`。信封里没有序号字段。
`event_seq` 就是 `events` 序列的 **0 基稠密下标**，即 `RevisionIndex` 中
`(version, tx_id, event_seq)` 的分量。规则：

1. 下标 `0 .. event_count-1`，无空洞、无重复；`event_count` 必须与实际解析出的事件数
   相等，否则 CORRUPT。**`event_count = 0` 一律非法**：一个没有任何事件的事务没有可
   重放的内容，却会占用一个 `version`，并使 `chain_digest` 覆盖空序列。合法的“无操作
   但已受理”请求仍然写出事件——至少一条 `RequestOutcomeRecorded`（tag 192），
   因此 `event_count >= 1` 恒成立（ID 高水位推进写 `IdAllocation`）。
2. 顺序是语义：`tx_digest`（storage-digests §4.1）覆盖 `body` 的编码字节，
   manifest 引用 `tx_digest`，CURRENT 引用 manifest。因此重排事件必然改变哈希，漏事件
   必然改变 `event_count` 与 `tx_file_sha256`。`event_seq` 不需要单独存字节，也无法
   在不被发现的情况下漂移。
3. 事件 tag **不要求升序**（同一 tx 可出现多条 `IdAllocation`），只有事件内**字段**必须
   严格升序。
4. 同一记录的修订在日志中严格递增且每次加一；跨 tx 的修订空洞由重放判定为 CORRUPT，
   不由 `event_seq` 表达。
5. 读者不得从文件名、块顺序或字段内容推断 `event_seq`；只有解析顺序能定义它。

不加 8 字节序号的原因：信封从 6 B 变 10 B 会让既有信封作废，且序号与位置必须互相
校验，反而引入一个可矛盾的冗余值。位置 + 被提交的 `tx_digest` 已唯一确定它。

### 4.2 `tx_header` 字段

| tag | 名称 | 必现 | 说明 |
|---|---|---|---|
| 8 | `created_at` | 是 | 提交者写入的提交时刻（与 manifest 相同） |
| 144 | `kb_uuid` | 是 | 与目录/链一致；不一致 CORRUPT |
| 163 | `rules_digest` | 是 | 与本事务物理用的 profile 配对，须按 storage-digests §5 重算校验 |
| 176 | `principal_id` | 是 | 执行者；genesis 用 `system` |
| 177 | `request_id` | 是 | genesis 用 `genesis` |
| 178 | `payload_digest` | 是 | 调用方请求负载摘要；genesis 用保留哨兵 32 个 `0x00`（`206=0`） |
| 192 | `version` | 是 | 上一版本 + 1；genesis 为 1 |
| 193 | `tx_id` | 是 | 16 B，必须与文件名 `<tid>` 相同 |
| 194 | `prev_version` | 是 | genesis 为 0 |
| 195 | `event_count` | 是 | 必须等于实际事件数 |
| 196 | `rules_profile` | 是 | 本事务求值所用 profile |
| 206 | `request_canon_version` | 是 | `0` = 非请求事务（genesis，`178` 为哨兵）；`1` = 调用方请求规范 v1；`>=2` 本轮 UNSUPPORTED_FORMAT |
| 197 | `expect_version` | 否 | 出现则必须等于 `prev_version` |
| 14 | `extensions` | 否 | §8.3 |

`tx_header` 禁止出现：`1 record_key`、`2 revision`、`4 status`、`5 recorded_by`、
`6 actor_origin`、`9 updated_at`、`10-13`、记录专属字段（16–191 中除 144、163、176–178 外）、
其余摘要字段（198–205）。出现即 CORRUPT。

**勘误（2026-11-17）：`144 kb_uuid` 是例外。** 本节上表把 144 列为 `tx_header` 必现，
旧文的禁现区间却写成“16–191 中除 176–178、163”，把 144 也一并禁止——同一段自相矛盾。
144 是事务头的真实字段（genesis 与每笔事务都带），禁现的是**记录专属**区间里除
144/163/176/177/178 之外的部分。

### 4.3 genesis

`version = 1` 的 tx 恰好含一个 `Genesis` 事件（tag 224），后接 9 条 `IdAllocation`
（tag 176，`counter` = 0..8 各一条，按数值升序）：

- `Genesis` 携带 `2 revision = 1`（KB 元数据是有修订的记录）与 §7.4 的元数据/规则配置；
  不携带 `1 record_key`（单例无数值身份）、`4 status`、`5 recorded_by`、`6 actor_origin`。
- `IdAllocation.counter` 覆盖 §7.5 的全部 ID 空间，**每个空间恰好一条**。
  `high_water_mark` 是“已分配的最大值”，初始 0；新 ID = `hwm + 1`。

**每一次高水位推进都必须在同一事务里留下一条 `IdAllocation` 事件。** 这是
`bookkeeping_digest` 可独立重算的唯一前提：它由**事件流**折叠得出，不能由“本事务创建了
哪些记录”反推。delivery-4 的 v2 golden 违反了这条——它把 term 高水位记成 1，却没有
对应的 `IdAllocation`，于是任何一个只读事件流的实现都会算出不同的
`bookkeeping_digest`（`6ce900d8…` vs golden `37f91c0b…`）。凡推进高水位而不写事件的
实现，其 manifest 都无法被独立验证。
- genesis 的 manifest 缺省 `prev_manifest_name` / `prev_manifest_sha256`；
  `chain_digest` 的“前一链摘要”取 32 个 `0x00`（storage-digests §4.3）。
- 初始化中断（CURRENT 缺失但目录非空）不构成 genesis，见 storage.md §2 / §5.2。

## 5. Manifest 文件（kind = 1）

```
body := field_count:u16 | Field[field_count]
```

| tag | 名称 | 必现 | 说明 |
|---|---|---|---|
| 8 | `created_at` | 是 | 与 tx 相同 |
| 144 | `kb_uuid` | 是 | 与所引用 tx 相同 |
| 163 | `rules_digest` | 是 | 与 tx 相同 |
| 192 | `version` | 是 | 与 tx 相同 |
| 196 | `rules_profile` | 是 | 与 tx 相同 |
| 198 | `state_digest` | 是 | 本版本后的知识状态摘要（storage-digests §4.4） |
| 199 | `graph_digest` | 是 | 本版本后的活动图摘要（§4.5） |
| 200 | `chain_digest` | 是 | 覆盖前一链摘要与本事务（§4.3） |
| 201 | `prev_manifest_name` | 否 | genesis 缺省；否则满足 §2.4 正则 |
| 202 | `prev_manifest_sha256` | 否 | 与 201 同现或缺省 |
| 203 | `tx_file_name` | 是 | `tx-<tid>.sgl` |
| 204 | `tx_file_sha256` | 是 | 整份 tx 文件的 SHA-256（文件哈希） |
| 205 | `bookkeeping_digest` | 是 | RequestOutcome / ImportSession / ID 高水位的摘要（§4.6） |

一致性：manifest 与 tx 的 `8,144,163,192,196` 必须逐字节相等，否则 CORRUPT
（`created_at` 是 i64 值比较）。

manifest **不含**：`payload_digest`、`principal_id`、`request_id`（属于 tx 头部）、
`engine_version`（构建版本不是数据，写进文件会让 golden 依赖构建，也无法证明语义未变，
storage-digests §4.8）、自身文件哈希（自指）。`204` 引用的是**另一份**文件的哈希，
不构成自指。

## 6. 字段注册表

字段 tag 空间与事件 tag 空间**相互独立**，都从 1 开始；tag `0` 在两层都非法。

列含义：`出现` = 写者在事件中何时写出（§8.1）；`值` = 值本身是否可缺省（§8.2）；
`兼容` = 旧读者遇到未知 tag 的义务（§8.3）。除 §6.4 登记的两个 advisory tag 外，
**全部字段兼容 = required**。

### 6.1 通用（1–15）

| tag | 名称 | 编码 | 限值 | 出现 | 值 | 兼容 |
|---|---|---|---|---|---|---|
| 1 | `record_key` | RecordKey | kind 1..9 | must（识别记录） | 必有 | required |
| 2 | `revision` | u64 | ≥ 1 | must（所有有修订的记录事件，含 Created = 1）；禁现于 64/176/192/208 | 必有 | required |
| 3 | `declaration_order` | u32 | — | 仅 Assertion 事件 must | 必有 | required |
| 4 | `status` | u16 | 取值域见 §7.3 | must（Created/Created-类与状态变更事件） | 必有 | required |
| 5 | `recorded_by` | str | ≤ 200 B | conditional（依 `actor_origin`） | 可缺省 | required |
| 6 | `actor_origin` | u16 | 0 authenticated / 1 legacy_declared / 2 unavailable | must（除 Genesis/IdAllocation） | 必有 | required |
| 7 | `reason` | str | ≤ 8000 B | conditional（必须出现的事件见 §7.2） | 可缺省 | required |
| 8 | `created_at` | i64 | — | must | 必有 | required |
| 9 | `updated_at` | i64 | — | must（除 Created 外的状态变更事件） | 必有 | required |
| 10 | `legacy_id` | str | ≤ 200 B | conditional（导入记录） | 可缺省 | required |
| 11 | `legacy_source_path` | str | ≤ 2000 B | conditional（导入记录） | 可缺省 | required |
| 12 | `legacy_content_digest` | bytes32 | — | conditional（导入记录） | 可缺省 | required |
| 13 | `changed_fields` | [u16] | ≤ 64 项，数值升序 | conditional（仅 revise 事件） | 可缺省 | **advisory**（§6.4） |
| 14 | `extensions` | ExtMap | ≤ 32 项 | conditional | 可缺省 | **advisory**（§8.3） |
| 15 | 保留 | — | — | — | — | — |

`actor_origin` × `recorded_by` 联合约束（违反即 CORRUPT）：

| `actor_origin` | `recorded_by` |
|---|---|
| 0 authenticated | 必须出现 |
| 1 legacy_declared | 可出现（导入者）或可缺省 |
| 2 unavailable | 必须缺省（不得编造执行者） |

`2 revision`（u64）与 `12 legacy_content_digest`（旧系统内容哈希）不可互换：前者是
P1 起的单调修订，后者只在导入记录上保留原值（data-model §2）。

### 6.2 记录字段（16–191）

| tag | 名称 | 编码 | 限值 | 出现 | 值 | 记录 |
|---|---|---|---|---|---|---|
| 16 | `slug` | str | ≤ 64 B | must | 必有 | Term |
| 17 | `atom` | str | ≤ 800 B | must | 必有 | Term |
| 18 | `layer` | u8 | 0..=3 | must | 必有 | Term |
| 19 | `domains` | [str] | ≤ 64 项，每项 ≤ 128 B，保序 | must | 必有 | Term |
| 20 | `source` | u16 | 0 personal / 1 domain / 2 synthesis | must | 必有 | Term |
| 21 | `confidence` | u16 | 0 contested / 1 hypothetical / 2 confirmed | must | 必有 | Term, Assertion, Claim |
| 22 | `triggers` | [str] | ≤ 128 项，每项 ≤ 200 B，保序 | must | 必有 | Term |
| 23 | `body_ref` | bytes32 | — | must | 必有 | Term |
| 32 | `assertion_from` | RecordKey | kind 必须为 1 | must | 必有 | Assertion |
| 33 | `assertion_target` | RecordKey | kind 必须为 1 | must | 必有 | Assertion |
| 34 | `relation` | u16 | 0..=6 | must | 必有 | Assertion |
| 35 | — | — | **禁现**（注解是派生值，不持久化） | — | — | — |
| 36–47 | 保留 | — | — | — | — | — |
| 48 | `concept` | RecordKey | kind 必须为 1 | must | 必有 | Claim, SessionCheckpoint |
| 49 | `claim_kind` | u16 | 0 premise … 5 requirement | must | 必有 | Claim |
| 50 | `text_ref` | bytes32 | — | must | 必有 | Claim |
| 51 | 保留 | — | — | — | — | — |
| 52–63 | 保留 | — | — | — | — | — |
| 64 | `evidence_kind` | u16 | 0 text / 1 image | must | 必有 | Evidence |
| 65 | `title` | str | ≤ 1200 B | must | 必有 | Evidence |
| 66 | `locator` | str | ≤ 8000 B | conditional | 可缺省 | Evidence |
| 67 | `excerpt_ref` | bytes32 | — | conditional | 可缺省 | Evidence |
| 68 | `asset_sha256` | bytes32 | — | conditional（`64=1` 时必现） | 必有 | Evidence |
| 69 | `captured_at` | i64 | — | must | 必有 | Evidence |
| 70 | `declared_captured_by` | str | ≤ 800 B | conditional | 可缺省 | Evidence |
| 71–79 | 保留 | — | — | — | — | — |
| 80 | `evidence` | RecordKey | kind 必须为 4 | must | 必有 | EvidenceLink |
| 81 | `evidence_revision` | u64 | ≥ 1 | must | 必有 | EvidenceLink |
| 82 | `link_target` | RecordKey | kind 1/2/3 | must | 必有 | EvidenceLink |
| 83 | `target_revision` | u64 | ≥ 1 | must | 必有 | EvidenceLink |
| 84 | `sha256` | bytes32 | — | must | 必有 | Asset |
| 85 | `media_type` | u16 | 0 png / 1 jpeg / 2 webp / 3 gif | must | 必有 | Asset |
| 86 | `byte_size` | u64 | ≥ 1 | must | 必有 | Asset |
| 87–95 | 保留 | — | — | — | — | — |
| 96 | `subject` | RecordKey | kind 1/2/3/4 | must | 必有 | Vector |
| 97 | `subject_revision` | u64 | ≥ 1 | must | 必有 | Vector |
| 98 | `model_id` | str | ≤ 120 B | must | 必有 | Vector |
| 99 | `dimensions` | u32 | 1..=4096 | must | 必有 | Vector **only**（Model 用 152） |
| 100 | `input_digest` | bytes32 | — | must | 必有 | Vector |
| 101 | `values_ref` | bytes32 | — | must | 必有 | Vector |
| 102–111 | 保留 | — | — | — | — | — |
| 112 | `proposal_kind` | u16 | 0..=5（§7.3） | must | 必有 | Proposal |
| 113 | `proposal_subject` | RecordKey | kind 1/3 | must | 必有 | Proposal |
| 114 | `base_revisions` | [RevPair] | ≤ 256 项，升序，无重复 | must | 必有 | Proposal |
| 115 | `payload_ref` | bytes32 | — | must | 必有 | Proposal |
| 116 | `review_note` | str | ≤ 8000 B | conditional | 可缺省 | Proposal |
| 117–127 | 保留 | — | — | — | — | — |
| 128 | `concept_refs` | [RecordKey] | ≤ 64 项，kind 1，升序 | must | 必有 | FrontierEntry |
| 129 | `text_ref` | bytes32 | — | must | 必有 | FrontierEntry |
| 130 | `priority` | u16 | 0 high / 1 medium / 2 low | must | 必有 | FrontierEntry |
| 131 | 保留 | — | — | — | — | — |
| 132 | `session_no` | u32 | ≥ 1 | conditional | 原生定位与 48 同现 | SessionCheckpoint |
| 133 | `content_ref` | bytes32 | — | must | 必有 | SessionCheckpoint |
| 134–143 | 保留 | — | — | — | — | — |
| 144 | `kb_uuid` | 16 B | — | must（Genesis、KbMetadata、tx/manifest 头） | 必有 | KbMetadata |
| 145 | `kb_slug` | str | ≤ 64 B | must | 必有 | KbMetadata |
| 146 | `kb_name` | str | ≤ 600 B | must | 必有 | KbMetadata |
| 147 | `kb_description` | str | ≤ 8000 B | must | 必有 | KbMetadata |
| 148 | `kb_domains` | [str] | ≤ 64 项，每项 ≤ 128 B，保序 | must | 必有 | KbMetadata |
| 149 | `model_key` | str | ≤ 120 B | must | 必有 | Model |
| 150 | `model_identifier` | str | ≤ 120 B | must | 必有 | Model |
| 151 | `model_provider` | str | ≤ 120 B | conditional | 可缺省 | Model |
| 152 | `model_dimensions` | u32 | 1..=4096 | must | 必有 | Model |
| 153 | `legacy_frontier_ref` | 32 B | — | opt（KbMetadataUpdated） | 有值必现 | 旧 Frontier 文档原始字节的 SHA-256 |
| 154 | 公共 session 定位 | str + u64 | slug 和正的 JS 安全整数 | conditional | 与 48、132 互斥 | SessionCheckpoint |
| 155–159 | 保留 | — | — | — | — | — |
| 160 | `from_profile` | u16 | 0 legacy-v1 / 1 formal-v1 | must（RuleProfileChanged） | 必有 | RuleConfig |
| 161 | `to_profile` | u16 | 同上 | must（RuleProfileChanged） | 必有 | RuleConfig |
| 162 | `dry_run_report_ref` | bytes32 | — | conditional | 可缺省 | RuleConfig |
| 163 | `rules_digest` | bytes32 | — | must | 必有 | RuleConfig, Genesis, tx/manifest 头 |
| 164 | `counter` | u16 | 0..=8（§7.5） | must（IdAllocation） | 必有 | IdAllocation |
| 165 | `high_water_mark` | u128 | ≤ 该空间上界 | must（IdAllocation） | 必有 | IdAllocation |
| 166 | `rules_ref` | bytes32 | — | must（Genesis、RuleProfileChanged） | 必有 | RuleConfig |
| 167 | 保留 | — | — | — | — | — |
| 168 | `declared_author` | str | ≤ 800 B | must（Proposal）；conditional（其他声明作者的记录） | 必有 | required |
| 169 | `legacy_source_file_digest` | bytes32 | — | conditional（导入记录） | 可缺省 | required |
| 170 | `legacy_source_revision` | bytes32 | — | conditional（导入记录） | 可缺省 | required |
| 171 | `embedding_revision` | u64 | ≥ 1 | must（Vector） | 必有 | required |
| 172 | `declared_by` | str | ≤ 800 B | conditional（Asset） | 可缺省 | required |
| 173 | `declared_at` | i64 | — | conditional（Asset） | 可缺省 | required |
| 174–175 | 保留 | — | — | — | — | — |
| 176 | `principal_id` | str | ≤ 120 B | must | 必有 | RequestOutcome, tx 头 |
| 177 | `request_id` | str | ≤ 128 B | must | 必有 | RequestOutcome, tx 头 |
| 178 | `payload_digest` | bytes32 | — | must | 必有 | RequestOutcome, tx 头 |
| 179 | `result_kind` | u16 | **只有 `0 committed`**（`1/2/3` 未登记：业务拒绝写在事务内的事件里，瞬时 unknown 不落盘） | must | 必有 | RequestOutcome |
| 180 | `result_ref` | bytes32 | — | must | 必有 | RequestOutcome |
| 181 | `commit_version` | u64 | ≥ 1 | must | 必有 | RequestOutcome |
| 182 | `import_session_id` | str | ≤ 64 B | must | 必有 | ImportSession |
| 183 | `import_stage` | u16 | 0 prepare / 1 validate / 2 import / 3 verify / 4 activate | must | 必有 | ImportSession |
| 184 | `source_digest` | bytes32 | — | must | 必有 | ImportSession |
| 185 | `plan_ref` | bytes32 | — | must | 必有 | ImportSession |
| 186 | `expected_counts` | [CountPair] | ≤ 32 项，升序，无重复 | must | 必有 | ImportSession |
| 187 | `import_complete` | bool | — | must | 必有 | ImportSession |
| 188–191 | 保留 | — | — | — | — | — |

`35` 与 `51` 显式标为禁现/保留，避免把派生值（`annotation`）或历史草案 tag 当成可用编号。

### 6.3 文件结构字段（192–223）

| tag | 名称 | 编码 | 限值 | 出现 |
|---|---|---|---|---|
| 192 | `version` | u64 | ≥ 1 | must（tx/manifest 头） |
| 193 | `tx_id` | 16 B | — | must（tx 头） |
| 194 | `prev_version` | u64 | genesis 为 0 | must（tx 头） |
| 195 | `event_count` | u32 | 1..=65536 | must（tx 头） |
| 196 | `rules_profile` | u16 | 0/1 | must（tx/manifest 头、Genesis） |
| 197 | `expect_version` | u64 | — | conditional（tx 头） |
| 198 | `state_digest` | bytes32 | — | must（manifest） |
| 199 | `graph_digest` | bytes32 | — | must（manifest） |
| 200 | `chain_digest` | bytes32 | — | must（manifest） |
| 201 | `prev_manifest_name` | str | §2.4 正则 | conditional（genesis 缺省） |
| 202 | `prev_manifest_sha256` | bytes32 | — | 与 201 同现或缺省 |
| 203 | `tx_file_name` | str | §2.4 正则 | must（manifest） |
| 204 | `tx_file_sha256` | bytes32 | — | must（manifest） |
| 205 | `bookkeeping_digest` | bytes32 | — | must（manifest） |
| 206 | `request_canon_version` | u16 | 0（非请求事务，仅 genesis）/ 1（调用方请求） | must（tx 头） |
| 207–223 | 保留 | — | — | — |

### 6.4 advisory tag 登记表

`兼容 = advisory` 只说明一件事：**不认识的读者必须接受该 tag 并原样保留其值字节**，
不得拒绝文件、不得丢弃。它**不**说明该字段可以不出现在摘要里。

两个正交属性，逐 tag 登记：

| tag | 名称 | 未知读者可拒绝？ | 未知读者可丢弃字节？ | 可推导性 | 是否参与 `state_digest` |
|---|---|---|---|---|---|
| 13 | `changed_fields` | 否 | 否（必须原样写回） | 是：由相邻两次 after-image 差分得到 | **否**（唯一例外，见下） |
| 14 | `extensions` | 否 | 否（必须原样写回） | 否 | **是** |

**冻结决定（语义无关性与字节恒等分开）**：

1. 一个字段“对语义无关”与“对字节无关”是两件事。`tx_digest` 覆盖整份 `body`，
   `chain_digest` 覆盖 `tx_digest`，所以**任何**持久字节都与链摘要相关——
   不可能存在“加入一个字段而链摘要不变”。
2. 因此 major 0 的规则是：**所有存储字节照常进入各自的摘要**；未知 tag 一律拒绝
   （§8.3）。`extensions` 与 `changed_fields` 是被**认识**的 tag，其字节进入
   `tx_digest`；`state_digest` 的输入是规范记录字节（storage-digests §4.4.1），
   其中包含 `extensions` 原字节，不包含 `changed_fields`（后者见下）。
3. 不存在“加入扩展字段后旧链仍验证通过”的承诺。要引入一个不改变链的结构性扩展，
   那是 major/minor 升级 + 迁移工具的事，不是在本 major 内绕开摘要。

`changed_fields` 的摘要归属：它可由 after-image 差分**唯一重建**（差分的定义就是
“与前一修订比较哪些 tag 变化”），因此它不进 `state_digest`；这是“可推导”的唯一一处
例外，不是“我们觉得它不重要”。`extensions` 无法这样重建，所以它**必须**进摘要。
两者的判据都是“能否重建”，表格最后一列据此填写，而不是按“是否 advisory”填写。
如果将来某个字段无法这样重建，它就必须进 `state_digest`。

`extensions` 的摘要归属：无法从其他字段推导，因此必须进入 `state_digest`
（见 storage-digests §4.4）。旧文曾把“advisory ⇒ 不进摘要”当规则，那会让“含未解释
附注”与“不含附注”两条记录摘要相等——正是“未知 optional 权威字段被隐式丢弃”的失效方式。

准入条件（加入本表必须同时满足）：

1. **保留性**：解码器把值当不透明字节，重编码写回逐字节相同；
2. **不接受性义务**：未知该 tag 时接受文件，不产生 CORRUPT/UNSUPPORTED；
3. **不解释性**：不改变记录字段、查询结果行、引用关系或 ID 分配；
4. **摘要归属明确**：写明是否进入 `state_digest`；写“否”必须附可重建证明。

## 7. 事件注册表

### 7.1 事件与字段信封

```
Event := tag:u16 | flags:u16 | field_count:u16 | Field[field_count]
Field := tag:u16 | flags:u16 | encoded_len:u32 | value[encoded_len]
```

- `Event.flags`：位 0 = REQUIRED，位 1–15 必须为 0。major 0 中所有已登记事件都要求
  位 0 = 1；未知事件 tag 无论 flags 如何都返回 UNSUPPORTED_FORMAT（§8.4）。位 0 = 0
  的语义要等出现“advisory 事件登记表”才启用，major 0 该表为空。
- `Field.flags`：**必须为 0**。旧文 storage.md §4.2 的位 0 REQUIRED / 位 1 PRESENT
  已撤销：兼容性由静态注册表（§6）决定，值缺省由“整个字段不出现”表达（§8.1）。
  非 0 即 CORRUPT。
- `encoded_len` 是 `value` 的字节数，不含 tag/flags/len；它是跳过未知字段的唯一依据。
- 事件内字段按 `tag` 严格升序，重复即 CORRUPT；`field_count` 必须与实际一致。
- 未登记字段 tag 的处理见 §8.3。

### 7.2 事件表

三类事件的公共规则：

| 类别 | 公共必现 | 公共条件 |
|---|---|---|
| 识别记录事件 | `1 record_key`、`6 actor_origin`、`8 created_at` | `2`（Created 之外）、`5`（依 `actor_origin`）、`7`、`9`（Created 之外）、`13`（仅 revise）、`14` |
| 自然键记录事件 | `6 actor_origin`、`8 created_at` | `2`（仅 Model）、`5`、`7`、`9`、`14` |
| 非记录事件（Genesis、IdAllocation） | 见各行 | `7`、`14` |

`7 reason` 在状态终结/管理类事件中**必须出现**：3 TermRetired、18 AssertionRetracted、
34 ClaimSuperseded、35 ClaimRejected、51 EvidenceLinkRetracted、81 VectorRetired、
147 ModelRetired、97 ProposalDecided、160 RuleProfileChanged、208 ImportBookkeeping。
其余事件中可出现、可缺省。

下表只列**记录专属**字段；`必` = 必须出现，`条` = 条件出现（条件在备注列或脚注）。

| tag | 事件 | 键/记录 | 专属必现 | 专属条件 |
|---|---|---|---|---|
| 1 | TermCreated | kind 1 | 2,4,16,17,18,19,20,21,22,23 | 10,11,12,169,170（导入）；7 |
| 2 | TermRevised | kind 1 | 2,4,9,16,17,18,19,20,21,22,23 | 10,11,12,14（有值必现）；13；7 |
| 3 | TermRetired | kind 1 | 2,4,9,16,17,18,19,20,21,22,23 | 10,11,12,14（有值必现）；13；7（必） |
| 16 | AssertionCreated | kind 2 | 2,3,4,21,32,33,34 | 10,12；7 |
| 17 | AssertionRevised | kind 2 | 2,3,4,9,21,32,33,34 | 10,12,14（有值必现）；13；7 |
| 18 | AssertionRetracted | kind 2 | 2,3,4,9,21,32,33,34 | 10,12,14（有值必现）；13；7（必） |
| 32 | ClaimCreated | kind 3 | 2,4,48,49,50,21 | 10,12；7 |
| 33 | ClaimRevised | kind 3 | 2,4,9,48,49,50,21 | 10,12,14（有值必现）；13；7 |
| 34 | ClaimSuperseded | kind 3 | 2,4,9,48,49,50,21 | 10,12,14（有值必现）；7（必） |
| 35 | ClaimRejected | kind 3 | 2,4,9,48,49,50,21 | 10,12,14（有值必现）；7（必） |
| 48 | EvidenceCreated | kind 4 | 2,4,64,65,69 | 66,67,68,70；7 |
| 49 | EvidenceRevised | kind 4 | 2,4,9,64,65,69 | 66,67,68,70,14（有值必现）；13；7 |
| 50 | EvidenceLinkCreated | kind 5 | 2,4,80,81,82,83 | 7 |
| 51 | EvidenceLinkRetracted | kind 5 | 2,4,9,80,81,82,83 | 7（必） |
| 64 | AssetDeclared | 自然键 `sha256` | 84,85,86 | 172,173,7；禁现 `2`、`4` |
| 80 | VectorUpserted | kind 6 | 2,4,96,97,98,99,100,101,171 | 7 |
| 81 | VectorRetired | kind 6 | 2,4,9,96,97,98,99,100,101,171 | 7（必） |
| 96 | ProposalSubmitted | kind 7 | 2,4,112,113,114,115,168 | 10；7 |
| 97 | ProposalDecided | kind 7 | 2,4,9,112,113,114,115,116,168 | 7（必） |
| 112 | FrontierEntryChanged | kind 8 | 2,4,128,129,130 | 14（有值必现）；7 |
| 128 | SessionCheckpointSaved | kind 9 | 2,48,132,133 | 7 |
| 144 | KbMetadataUpdated | 单例 | 2,144,145,146,147,148 | 153（有值必现）；7 |
| 145 | ModelRegistered | 自然键 `model_key` | 2,4,149,150,152 | 151；7 |
| 146 | ModelUpdated | 自然键 `model_key` | 2,4,9,149,150,152 | 151,14（有值必现）；13；7 |
| 147 | ModelRetired | 自然键 `model_key` | 2,4,9,149,150,152 | 151,14（有值必现）；7（必） |
| 160 | RuleProfileChanged | 单例 | 2,160,161,163,166 | 162；7（必） |
| 176 | IdAllocation | 非记录 | 164,165 | 禁现 `1`、`2`、`4`、`6`；**普通非 genesis 事务中，每个真正被消费的 ID 空间恰好一条**，未推进的空间不写（genesis 是显式例外，见下） |
| 192 | RequestOutcomeRecorded | 自然键 `(176,177)` | 5,6,176,177,178,179,180,181 | 7；禁现 `2`、`4` |
| 208 | ImportBookkeeping | 自然键 `import_session_id` | 182,183,184,185,186,187 | 7（必）；禁现 `2`、`4` |
| 224 | Genesis | 单例 | 2,8,144,145,146,147,148,163,166,196 | 7；禁现 `1`、`4`、`5`、`6` |

规范要求（不是建议）：

- **after-image 完整性**：所有 revise/retire 类事件（2,3,17,18,33,34,35,49,51,81,97,
  112,146,147）必须携带该修订的**全部**记录字段：按修订读取单条记录时不重放整库
  （data-model §2.3）。
  - **表格自检规则**：任一 revise/retire 行的必现字段集合，必须**包含**同记录 Created 行的
  全部记录字段（`record_key` 与 `status` 除外，前者由事件身份给出、后者由该行显式列出）。
  `AssertionRevised` 曾漏掉 `3 declaration_order` 而 Created 行有它——这正是该自检要拦的
  错误，因为 `declaration_order` 是记录值，不随修订消失。
- **完整 after-image 指“该修订的全部记录值”，包括 `4 status`、`10/11/12 legacy_*`、
    `14 extensions`。** 它们在每个 revise/retire 事件里都必须出现（值缺省时整字段不出现，
    但“因为上一修订有值而继承”是不允许的）。
  - 只有**纯事务元数据**随类别变化：`9 updated_at`（Created 可省略，其余必现）、
    `13 changed_fields`（可省略，纯展示）。
  - **不得隐式继承**：按修订读取一条记录时，不得要求读取另一修订才能补全它的字段。
    这正是“完整 after-image”要排除的失败模式——若 `status` 或 `legacy_*` 可以继承，
    那么单个修订的字节就不再自足，“按修订读取”退化为“重放前缀”。
  - 允许在 **P2 准备层**先用 delta 表达编辑，但**编码前**必须展开成完整 after-image。
    delta 是实现便利，不是持久形式。
- **`record_key` 与事件一致**：`record_key.kind` 必须等于本节“键/记录”列的 kind，否则
  CORRUPT（廉价跨字段校验，防止把 Term 字段塞进 Assertion 事件）。
- **修订单调**：`revision = 1` 只允许出现在 Created 类事件；其余必须
  `revision = 前一修订 + 1`，且同记录的前一修订必须已在日志前缀中出现。
- **不可变记录**：Asset、RequestOutcome、ImportSession 没有修订；major 0 不存在为它们
  写 revise 的 tag，也不需要“删除”语义。
- **每笔非 genesis 事务恰好一条 `RequestOutcomeRecorded`**，且其
  `176 principal_id`、`177 request_id`、`178 payload_digest` 必须与 tx 头逐字节相等，
  `181 commit_version` 必须等于 tx 头 `192 version`，`206 request_canon_version`
  必须是 tx 头同值的来源。零条或两条以上即 CORRUPT。这样“事务头部”与“事件流”不是
  两个可独立漂移的真源：头部字段被事件重复，重复值必须相等。
> **勘误（2026-11-17）：`224 Genesis` 的专属必现列曾漏写 `8 created_at`。**
>
> `§6.1` 把 tag 8 定为无条件 `must`，`§8.2` 第 2 步要求读者检查全部 `must` 字段；
> 冻结的 DELIVERY-10 `golden.genesis.tx.sgl` 的 `Genesis` 事件实际携带
> `2,8,144,145,146,147,148,163,166,196`（tag 8 = 8 字节，值 `00c01b25d3430600`），
> `storage-digests §4.4.6` 也把 8 放进两个 genesis body。因此 genesis **要求** tag 8，
> 行内漏写是表格缺陷，不是"genesis 可无创建时间"。
>
> 同一份 golden 也界定了 `7 reason` 与 `176 IdAllocation`：genesis 事件不带 7
> （逐事件 conditional，无值即整字段不出现），而 9 条 `IdAllocation` 事件只携带
> `164,165`。所以 `176` 的"未列出 8"是正确的，不得为它补 8 —— 接受一个没有槽位的
> 字段就是 accept-and-drop。
> **勘误（2026-11-17）：`176` 的"未推进不写"只约束普通事务。**
>
> 正文上一条要求“每一次高水位推进都必须留下一条 `IdAllocation`”，而冻结的
> DELIVERY-10 `golden.genesis.tx.sgl` 里恰好有 **9 条** `IdAllocation`，全部
> `high_water_mark = 0`——即九个空间一条都没推进，却都写了一条事件。
> 两者看似矛盾，实际是两层规则：
>
> - **普通非 genesis 事务**：只写真正推进了的空间；没推进的空间不写。
> - **genesis**：`§4.3` 规定显式写出 §7.5 全部九个空间各一条，`hwm` 初始为 0。
>   genesis 的例外由 `192 version == 1` 唯一确定，不需要额外标志位。
>
> 因此“9 条零值 IdAllocation”是 genesis 的正确形态，不是违规；对 version ≥ 2 的事务
> 写出零值 `IdAllocation` 才是多余的。
- **genesis 是显式例外**：`version = 1` 的事务只含 `Genesis` + 9 条 `IdAllocation`，
  不携带 `RequestOutcomeRecorded`。它没有客户端请求，也没有可返回的首次结果；
  这与“每笔普通事务都有 outcome”不矛盾，因为例外在版本号上唯一确定，不需要额外标志位
  （COORDINATOR-DECISIONS-01 §8）。
- **genesis 的空 outcome 集合不构成一条更便宜的解析路径。** “`version = 1` 的 KB 没有
  outcome”只有在**完成全量校验与重放、并对同一根完成同步之后**才是确定的 `ABSENT`；
  同步失败或重放不完整时，答案仍是 `INCOMPLETE`，不得因为“反正 genesis 没有 outcome”
  就跳过校验直接回答 absent。换句话说：genesis 的例外只说明**结果集合为空**，
  不说明**判定可以省步骤**（store Gate G0）。
- **`result_kind` 只有一种可持久值：`0 committed`。** 见 §7.3 与下方的结果语义。
- **`178` 的全零值是保留哨兵**，只允许与 `206 = 0` 同时出现（即 genesis）。真实请求
  即使是全零也必须被拒绝为 INTERNAL，不得与哨兵混淆（storage-digests §4.7.1）。
- **`206` 是派生冗余，不是第二个“是否 genesis”的权威。** “genesis”的唯一定义是
  `192 version == 1`；`206` 必须由它派生：`version == 1` ⇔ `206 = 0` ⇔ `178` 为全零；
  `version >= 2` ⇔ `206 = 1` ⇔ `178` 非全零。三个条件里任意一个与其余两个矛盾即
  CORRUPT（与 `RequestOutcome` 与 tx 头相等是同一种模式：冗余被允许，漂移不被允许）。
  实现不得用 `206` 判断是否 genesis —— 那会让一处损坏产生两种解释。
- **证据字段不做产品校验**：`67 excerpt_ref`、`68 asset_sha256`、`70 declared_captured_by`
  在 `major 0` 全部为“可出现、值可缺省”。现有 HTTP 写入契约要求“文本证据必须有摘录、
  图像证据必须有资产、`capturedBy` 必填”（`packages/core/src/knowledge/contracts.ts` 的
  `superRefine`），那是**受理层**的准入规则；把它移植进持久化会让一条历史证据因为缺少
  声明采集者而无法无损保存，直接违反 data-model §2.1 的“`declared_captured_by` 是来源
  属性，缺失时原样保留”。产品准入由 P2 在提交前执行，格式层只记录发生了什么。

### 7.3 枚举取值域

**`179 result_kind` 的持久取值域在 major 0 中只有 `0 committed`。**
一个“业务上被拒绝但事务已提交”的动作（例如批准流程把提案标为 rejected）仍然是
`0 committed`：被拒绝这件事写在它自己的 `ProposalDecided` 事件里，不是 `result_kind`。
需要长期记住“这次请求本身没成功”的信息一律来自**未提交**的类型化错误；不提交就没有
outcome，重试会用同一 `request_id` 重新判断。 理由：读到一条
已提交事务的 `RequestOutcome` 就意味着该事务的持久结果已成立，因此“结果种类”对已提交
记录只能是成功。`rejected` / `conflict` 是**写前的类型化错误**，不产生事务，因而不产生
`RequestOutcome`；`COMMIT_UNKNOWN` 是**传输期的未知**，不是持久结果，重试必须最终解析为
committed（storage.md §5.1）。把瞬时 unknown 落盘会把一次可恢复的不确定性固化成终态，
并使同一 `request_id` 永远无法返回真实结果。

一个 *已提交的提案被拒绝*（业务动作成功、知识未变）仍是成功事务，`result_kind = 0`；
被拒绝的**事实**记录在该事务写入的 `ProposalDecided` 事件里，而不是 `result_kind` 里。

若要新增其他持久 `result_kind`，必须先定义它的原子性、版本绑定与幂等语义
（同一 `request_id` 重复返回什么、与 `commit_version` 如何对应），否则不得登记。

`4 status` 的取值域按记录 kind 封闭，**唯一来源是下表**：

| kind | 记录 | 取值 |
|---|---|---|
| 1 | Term | `0 active` / `1 retired` |
| 2 | Assertion | `0 active` / `1 retracted` |
| 3 | Claim | `0 accepted` / `1 superseded` / `2 rejected` |
| 4 | Evidence | `0 active` |
| 5 | EvidenceLink | `0 active` / `1 retracted` |
| 6 | Vector | `0 active` / `1 retired` |
| 7 | Proposal | `0 pending` / `1 approved` / `2 rejected` / `3 conflicted` |
| 8 | FrontierEntry | `0 open` / `1 resolved` |
| 11 | Model | `0 active` / `1 retired` |

越过本表的值是 UNSUPPORTED_FORMAT，不是“未知但可接受”。四类自然键记录中，
**Asset 与 KbMetadata/RuleConfig 没有 `4` 字段**（写了即 CORRUPT）；Model 的状态折叠与
“每类记录由哪些事件产生”只在 storage-digests §4.4.8 定义，本节不重复。

`112 proposal_kind`：0 concept-markdown / 1 claim-create / 2 claim-edit /
3 evidence-attach / 4 relation-assert / 5 term-create。旧读者遇到值 5 时报告不支持。

`49 claim_kind`：0 premise / 1 constraint / 2 decision / 3 risk / 4 observation /
5 requirement。

`34 relation`：0 is-a / 1 part-of / 2 instantiates / 3 depends-on / 4 enables /
5 contradicts / 6 analogous-to（与 `sage_db_types::RelationId::get` 一致）。

`20 source`、`21 confidence`、`18 layer`、`183 import_stage`、`130 priority`、
`179 result_kind`、`196 rules_profile`、`6 actor_origin` 的取值范围见 §6 各行。

### 7.4 规则配置的绑定

`Genesis` 与 `RuleProfileChanged` 携带 `163 rules_digest`、`166 rules_ref`，
`Genesis` 另带 `196 rules_profile`。规则配置因此是**数据**，不是编译期常量。加载时必须：

1. 读出 `rules_ref` 指向的 blob；
2. 校验 `SHA-256(blob) == rules_ref`（文件哈希规则，storage-digests §2）；
3. 按 storage-digests §5 的规范字节重新计算 `digest("rules", blob)`；
4. 要求第 3 步结果等于 `163`，且 blob 内的 profile 字段等于 `196`；否则 CORRUPT。

只比较 digest 而不重算是不合格的实现：那样 `rules_digest` 只是不透明标签，无法证明
“允许推导什么”与磁盘上的规则一致（data-model §4、storage §4.3）。

规则校验的**错误分类必须分开**（把两者合成一个“规则不匹配”会让可迁移问题表现为损坏）：

| 观察 | 类别 | 理由 |
|---|---|---|
| `raw_sha256(blob) != 166` | **CORRUPT** | 声明的内容字节与实际字节不符：这是存储损坏 |
| `digest("rules", blob) != 163` | **CORRUPT** | 规范字节与声明的规范摘要不符：同上 |
| `blob` 内 `profile` 字段 != `196` | **CORRUPT** | 同一事务的两个字段互相矛盾 |
| `blob` 不是合法的 Rules 编码（`format_version`、长度、排序、`enforcement` 取值域） | **CORRUPT** | 字节无法成为任何合法规则配置 |
| `blob` 合法，但其 `profile` / 模板 / 校验规则本二进制不支持 | **UNSUPPORTED_FORMAT** | 文件没问题，是读者需要更新：这类必须可迁移，不能归为损坏 |

不得用“模板与内置常量不一致”这种启发式把第四行归入 CORRUPT：`166/163` 已经证明了
字节完整性与一致性，剩下的只是本实现是否支持该规则集。

### 7.5 ID 空间（`164 counter`）

| 值 | 空间 | 记录键宽度 | `high_water_mark` 宽度 | 上界（超出即 Exhausted） |
|---|---|---|---|---|
| 0 | term_id | u32 | u128 | `u32::MAX` |
| 1 | assertion_id | u128 | u128 | `u128::MAX` |
| 2 | claim_id | u128 | u128 | `u128::MAX` |
| 3 | evidence_id | u128 | u128 | `u128::MAX` |
| 4 | evidence_link_id | u128 | u128 | `u128::MAX` |
| 5 | vector_id | u128 | u128 | `u128::MAX` |
| 6 | proposal_id | u128 | u128 | `u128::MAX` |
| 7 | frontier_entry_id | u128 | u128 | `u128::MAX` |
| 8 | session_checkpoint_id | u128 | u128 | `u128::MAX` |

**`high_water_mark` 是 `u128`（`165`），不是 `u64`。** 早期修订曾把计数器收窄到 64 位，
那会让一个声明为 u128 的 ID 空间实际只能分配 `2^64` 个值——这是**规范层面的容量缩水**，
不能由格式单方面决定。现在：

- 计数器宽度与记录键宽度一致（term 用 `u32` 的取值上界，编码仍 16 B）；
- 空间耗尽时写者必须拒绝（`Exhausted`），不得回绕、不得截断到 u64；
- P0 的 `sage_db_types::Counter`/`Exhausted` 是**错误载荷**，其数值不定义分配宽度；
  P1 新增 `IdSpace` + `Watermark{space, hwm: u128}` 承载本表。

**`count` 与 `hwm` 是两个不同的数**，不得互相套用：

| 类型 | 字段 | 宽度 | 语义 |
|---|---|---|---|
| `Watermark` | `164 + 165` | `u16 + u128` | 已分配的最大 **ID 值** |
| `CountPair` | `186` 的元素 | `u16 + u64` | 期望的**行数**（导入对账） |

用 `u64` 存行数是正确的（行数不可能接近 `2^64`）；用 `u64` 存 ID 高水位是错的。

`KbVersion` 不需要分配事件（等于 manifest `192 version`）；每记录 `revision` 也不需要
全局高水位（由该记录修订链决定）。P0 的 `sage_db_types::Counter` 只有 4 个变体，与本表
不同；P1 新增独立的 `IdSpace` 枚举承载本表，**不改动** P0 的 `Counter`（它是错误载荷，
不是分配空间）。该决策登记在 `CONTRACT.md` 的决策表。

## 8. 三个独立轴：出现、值、兼容

三个问题必须分开回答，任何一个都不能推出另一个：

| 轴 | 问题 | 取值 | 由谁决定 |
|---|---|---|---|
| **出现** Appearance | 这个事件里是否写出该字段？ | `must` / `conditional` / `禁现` | 事件表 §7.2（写者义务） |
| **值** Value | 该字段的值本身是否允许缺省？ | `必有` / `可缺省` | 字段表 §6（记录语义） |
| **兼容** Compat | 不认识的读者能否跳过？ | `required` / `advisory` | §6.4（读者义务） |

必须区分的合法组合：

| 字段 | 出现 | 值 | 兼容 | 含义 |
|---|---|---|---|---|
| `66 locator` | conditional | 可缺省 | **required** | 无定位串的文本证据合法；但懂它的读者不得丢它 |
| `70 declared_captured_by` | conditional | 可缺省 | **required** | 导入记录可能没有声明采集者；不得把“没有”改写成“编造” |
| `5 recorded_by` | conditional | 可缺省 | **required** | `actor_origin=1/2` 时缺省是审计事实本身 |
| `7 reason` | conditional | 可缺省 | **required** | 终结类事件必须有；其他事件无理由也合法 |
| `13 changed_fields` | conditional | 可缺省 | advisory | 纯展示，可由 after-image 差分得到 |
| `14 extensions` | conditional | 可缺省 | advisory | 附注容器，不参与语义 |

**禁止的推理**：“值可缺省 ⇒ 兼容可跳过”。可缺省说的是记录可以没有这个值，不是
“不懂它的代码可以忽略它”。

**勘误（2026-11-17）：摘要归属与兼容类别是两个正交轴，不能互相推导。**
本节旧文写过“凡参与 `state_digest` 的字段一律 required；advisory 一律不参与任何摘要”，
两句都错，且与 §6.4 的登记表直接矛盾：

- **`14 extensions` 是 advisory，却必须参与 `state_digest`。** 它无法从其他字段重建，
  所以 §6.4 把它列入摘要；而它同时是“未知读者须接受并原样保留”的 advisory tag。
  若按旧文“advisory 不进摘要”，一条带附注的记录与一条不带的记录会算出相同
  `state_digest`，这正是“未知 optional 权威字段被隐式丢弃”的失效方式。
- **`13 changed_fields` 是 advisory，且确实不进 `state_digest`**，理由是它可由
  after-image 差分唯一重建——这是“可推导”的唯一例外，不是“advisory 所以不进”。
- 反方向同样不成立：一个字段**参与摘要**并不使它成为 `required`。是否 `required`
  由 §6.4 的“未知该 tag 能否拒绝”决定，与摘要无关。

因此：摘要归属只看 §6.4 的最后一列（判据是“能否重建”），兼容类别只看“未知读者可否
拒绝”。两者都不得由对方推出。

### 8.1 规范编码对“缺省”的唯一表达

**值缺省 = 整个字段不出现。** 没有“出现但长度为 0”，没有 sentinel（`0`、空串、
全 0 摘要），没有 `present` 前缀字节。因此：

- 写者按 §7.2 决定是否写该字段；写了就必须有完整值；值缺省时整条 `Field` 省略。
- **空值不是零字节**：所有已登记类型的最小编码长度都 ≥ 1——`u8`/`bool` 为 1、
  `u16` 为 2、`u32` 为 4、`u64`/`i64` 为 8、`bytes32` 为 32、`RecordKey` 为 18、
  `str` 与 `[T]`/map 为 4（它们总是带 `u32` 长度前缀，空串/空列表编码为 4 字节的 `0`）。
  因此 **`encoded_len = 0` 在任何已登记字段上都非法，一律 CORRUPT**。
  这消除“空值 = 缺省”与“空值 = 零字节”两种读法的分歧。
- 旧文 storage.md §4.1 的“optional 写成 `u8 present(0/1)` 再跟值”与 §4.2 的
  “`Field.flags` 位 1 = PRESENT” 都已撤销：两套机制并存会让同一记录有两种合法编码，
  使 `tx_digest` / `state_digest` 依赖写者习惯，这是不可接受的分歧源。

### 8.2 条件出现的判定顺序

写者与读者使用同一顺序，任何一步失败即拒绝：

1. 由事件 tag 选出记录类别与该事件的字段集；
2. 检查所有 `must` 字段存在、所有 `禁现` 字段不存在；
3. 检查 `conditional` 字段的条件（条件只引用本事件内字段）；
4. 检查跨字段一致性（`record_key.kind`、`actor_origin` × `recorded_by`、修订单调、
   `prev_manifest_*` 同现、`64` × `67/68/70`）。

### 8.3 未知 tag 的四种行为

| 情形 | 行为 |
|---|---|
| 未知字段 tag（本节表外） | **拒绝整个文件**：UNSUPPORTED_FORMAT |
| §6.4 的 advisory tag（13/14） | **是被认识的 tag**：接受并原样保留字节；它们不是“未知 tag”的例外通道 |
| `extensions` 内的未知键 | 保留原字节；不解释、不排序猜测、不丢弃；不得据此改变语义 |
| 未知事件 tag | 拒绝：UNSUPPORTED_FORMAT（不因 `flags.REQUIRED=0` 而跳过） |
| 未知块 kind | 拒绝（major 0 只允许 kind 1） |
| 未知枚举值 / 未知 `major` | 拒绝：UNSUPPORTED_FORMAT |
| `minor != 0` | 拒绝：UNSUPPORTED_FORMAT（major 0 无 minor 能力） |

### 8.4 为什么 advisory 不能扩大

若把 `locator` 标成 advisory，不认它的旧 reader 会把证据写成“无来源”，再提交一个
`EvidenceRevised`，从而**用一次合法写入永久抹掉来源**。因此 `locator`、
`declared_captured_by`、`reason`、`recorded_by` 都是 **required**：不是“理解不了可以
跳过”，而是“不理解就必须拒绝写入”。

advisory 只登记两个 tag，且都不承载权威状态（§6.4）。即使对它们，读者也必须原样保留：
`extensions` 参与 `state_digest`，所以“保留但丢字节”会立刻让摘要校验失败；`changed_fields`
不进摘要，但它是可重建的展示提示，丢了不改变任何语义。

同理，`reason`/`recorded_by` 缺省并不授权“丢字段后照常写回”：缺省是**值**的状态，
由写者在提交时决定；读者若丢弃一个出现的 required 字段，它重编码出的 after-image
就不等于原事件，`state_digest` 必然不符。

**推论（对 `state_digest` 的影响）**：任何未被本规范显式列入“不参与摘要”的字段，
都必须参与 `state_digest`。新增字段时，若实现想把它排除在摘要之外，必须在 §6.4 登记并
给出重建证明；否则默认参与。这样“未知 optional 权威字段”无法在摘要里悄悄消失。

## 9. 解码边界与错误分类

四类错误互不混用（混用会让“磁盘坏了”与“这个版本我不认识”变成同一个可重试错误）：

| 类别 | 判定 | 调用方动作 |
|---|---|---|
| `UNSUPPORTED_FORMAT` | 可能是合法文件，但不是本实现支持的版本/种类/特性 | 提示迁移工具；不重试 |
| `CORRUPT` | 字节无法成为任何合法文件（自相矛盾、校验失败） | 停止该 KB 正常服务，显式恢复（system I3） |
| `FORMAT_LIMIT` | 结构合法但超过本实现声明的资源上限 | 拒绝；不截断、不部分受理 |
| `INTERNAL` | 本实现的不可达分支/不变量被破坏 | 拒绝并记录缺陷；不得转成上面三类 |

### 9.1 有界解码顺序（先验证再分配）

1. 读 32 B 头；不足 → CORRUPT。
2. `magic`/`major`/`minor`/`kind`/`header_len`/`flags`/`reserved` 按 §2.1 分类。
3. 校验 `header_crc32c`。
4. `payload_len` 与实际长度比较。
5. 逐块头：`encoded_len` 不得越过剩余；`raw_len` 在 `1..=16 MiB`；`codec` 必须 none；
   **此时尚未分配任何 payload 缓冲**。
6. trailer 位置必须恰为 `32 + payload_len - 32`；块总长必须精确填满该区域。
7. 流式校验 `trailer_sha256`。
8. 逐块校验 `crc32c`，拼接得到 `body`。
9. 先读 `field_count` / `event_count`，与 §1.3 上限及“剩余字节是否可行”（每事件至少
   6 B、每字段至少 8 B）比较，不可行即 CORRUPT，**再**分配。
10. 逐字段/逐事件解析，每个长度与计数在读取时立即校验。

任何一步都不得“先读进内存再检查大小”。

### 9.2 判定表

| 观察 | 类别 |
|---|---|
| `magic != "SGLB"` / 未知 `major` / `minor != 0` / 未知 `kind`（含 3） / `header_len != 32` | UNSUPPORTED_FORMAT |
| `flags != 0` / `reserved != 0` / 块 `flags` 保留位非 0 / `Field.flags != 0` | CORRUPT |
| `header_crc32c` 不符 | CORRUPT |
| `payload_len != 文件长 - 32` | CORRUPT |
| 块 `encoded_len` 越界或为 0 / 块总长不足或超出 | CORRUPT |
| `raw_len == 0` | CORRUPT |
| `raw_len > 16 MiB` / `encoded_len > 64 MiB` | FORMAT_LIMIT |
| `codec = 1` | UNSUPPORTED_FORMAT |
| `trailer_sha256` 不符 / 块 `crc32c` 不符 | CORRUPT |
| `field_count`/`event_count` 与实际不符 | CORRUPT |
| 字段 tag 非升序或重复 | CORRUPT |
| 未知字段 tag（§6.4 表外） / 未知枚举值 | UNSUPPORTED_FORMAT |
| 字符串非 UTF-8 / 含 NUL | CORRUPT |
| 字符串/列表/映射超字段限值 | FORMAT_LIMIT |
| 映射重复键；要求排序的列表乱序 | CORRUPT |
| `record_key.kind` 与事件不符 | CORRUPT |
| 修订非 `前值 + 1`（跨 tx 需重放判定） | CORRUPT |
| `tx_id` 与文件名不符 / `tx_file_sha256` 与文件不符 | CORRUPT |
| manifest 与 tx 的 `8,144,163,192,196` 不等 | CORRUPT |
| `chain_digest` / `rules_digest` / `state_digest` / `graph_digest` / `bookkeeping_digest` 重算不符 | CORRUPT |
| `rules_ref` blob 哈希或 `rules` 摘要不符、blob 内 profile 与 `196` 不符、Rules 编码非法 | CORRUPT |
| Rules 编码合法但本实现不支持该 profile/模板/规则集 | UNSUPPORTED_FORMAT |
| `event_count == 0`（任何 kind=2 文件） | CORRUPT |
| 非 genesis tx 的 `RequestOutcomeRecorded` 条数 ≠ 1 | CORRUPT |
| `RequestOutcomeRecorded` 的 `176/177/178` 与 tx 头不等，或 `181 != 192` | CORRUPT |
| `179 result_kind != 0`（major 0） | UNSUPPORTED_FORMAT |
| `version`、`206`、`178` 三者不满足 §7.2 的派生等式（含 `version=1` 却 `206=1`） | CORRUPT |
| `206 >= 2` | UNSUPPORTED_FORMAT |
| 路径名不符 §2.4 | CORRUPT |

“未知事件 tag 之后仍有可解析字节”不是继续的理由：不认识的事件可能分配 ID、写 blob
或改变状态；跳过它会让重放产生与提交者不同的状态。

## 10. 校验流水线

三个阶段，输出不同结论，不得合并：

| 阶段 | 输入 | 检查 | 失败结论 |
|---|---|---|---|
| ① decode | 单文件字节 | §9 全部；`tx_digest` 重算 | CORRUPT / UNSUPPORTED / LIMIT |
| ② link | CURRENT + manifest 链 + tx | `manifest_sha256`、`tx_file_sha256`、`chain_digest` 逐版本重算、`kb_uuid`、`version` 连续、`prev_manifest_name` 与祖先一致 | CORRUPT |
| ③ replay | 事件序列 | 记录折叠、修订单调、ID 高水位、blob 存在性（按需）、五类摘要重算 | CORRUPT（结构性）或 findings（语义） |

③ 是**必做**步骤，不是可选的“verify 子命令”：打开/恢复时就在持有目录锁的前提下完成
完整折叠与五类摘要重算，全部匹配才算该根可用。不得为了缩短启动时间而跳过
`state_digest`/`graph_digest` 校验，也不得“先提供服务、后台再校验”——那会把一个
已损坏的库伪装成可用（system I3）。

③ 的**语义**问题（悬空引用、退役仍被引用、证据目标不存在）在 P1 只作为 findings
报告，不使日志变成 CORRUPT：日志忠实记录了提交者写入的事件；判它“损坏”会让一个业务
约束违反变成不可恢复的存储故障。P2 的 V1–V7 在提交前拒绝这些写入。

**取消/中断**：③ 的每一步都在提交预算与取消检查之内；被取消时该 KB 保持
“已锁定但未发布”状态，不得对外提供读服务（一个只折叠了一半的状态不能当作快照）。

## 11. P1 / P2 分工

| 内容 | P1 | P2 |
|---|---|---|
| 全部登记事件的编码/解码/往返/重放 | 是 | — |
| `tx_digest`、`chain_digest`、`state_digest`、`graph_digest`、`bookkeeping_digest`、`rules_digest`、`payload_digest` | 是 | — |
| 规范规则字节的生成与校验（两个 profile） | 是 | — |
| 从 CURRENT 恢复完整记录集（含正文/资产/向量 blob 引用） | 是 | — |
| 按 `(record, revision)` 定位历史修订 | 是（有界扫描） | 高效索引（P3） |
| `RequestOutcome` 持久化、重放、重复请求判定 | 是 | — |
| ID 高水位与配额账本 | 是 | — |
| 引用存在性、退役被引用、提案基线等业务校验（V1–V7） | 否（只报告 findings） | 是 |
| 提案/审阅/CAS 授权与生命周期 | 否 | 是 |
| 媒体类型字节签名、资产内容校验 | 否 | 是 |
| 字符集/字数等 schema 校验（slug 正则、atom 长度） | 否（只做字节上限） | 是 |

**往返义务**：每个登记事件在 P1 必须满足 `decode(encode(e)) == e` 且
`encode(decode(encode(e))) == encode(e)`（规范字节固定点）。仅“编码器与解码器彼此一致”
不是正确性证据：golden 字节必须由独立实现或本文档的测试向量产生并固定
（storage-digests §7）。

## 12. 变更控制

- major 0 期间：字段可增删改，不带迁移；不承诺读取旧文件，也不得对外宣称已有磁盘格式
  保证（storage.md §4.4）。
- `minor` 只在 major 1 之后启用；`minor != 0` 的文件在 major 0 中一律 UNSUPPORTED_FORMAT。
- 新增事件/字段必须同时更新：§6 字段表、§7.2 事件表、§6.4（若为 advisory，附四条
  准入证明）、golden 向量与拒绝测试。
- 冻结 major 1 时，本文每个数值（宽度、限值、正则、排序、域名字符串）都成为兼容契约；
  不得把本文中的值改成“实现时决定”。
