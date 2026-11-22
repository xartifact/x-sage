# 源归档导出器 v1（source archive v1）

> 状态：P5 **输入工具**，不是导入器，也不是切换声明。
> 实现：`tools/sage-db-source-archive.ts`；测试：`tools/sage-db-source-archive.test.ts`。
> 上游契约：[migration.md](migration.md) §1–§3。

## 1. 它做什么、不做什么

把一个**已经部署的旧 KB 源树**冻结成一个新目录，供后续 mapper 读取。

| 做 | 不做 |
|---|---|
| 逐字节导出源树中的每个文件（含 `AI.md`、`MANIFEST.md`、`frontier.md`、`protocols/`、`checkpoints/`、`proposals/`、`assets/`） | 不把记录映射成 sage-db 对象 |
| 在结构化权威下导出 `knowledge.db` 的**全部**已登记表/列/行 | 不声称「无损目标映射」已被证明 |
| 保留原始 `knowledge.db`（含 `-wal`/`-shm`）字节 | 不声称 P5 完成、不声明可切换/已就绪 |
| 提供只读 read+verify，重算每个 blob 与 NDJSON 计数 | 不读取凭据，不连接线上服务 |
| 用 `COMPLETE.json` 区分「已完成并自检」与「半成品」 | 不覆盖任何已存在的归档 |

迁移契约要求「由旧服务或一次性导出工具先产生中立 archive」；本工具就是那个一次性导出工具的第一段。
它产出的记录被归类为 **source records（待映射的源记录）**，不是目标侧的 after-image。

## 2. 权威判定（fail-closed）

判定只看**实际数据库状态**，不看配置、不看目录名：

| 状态 | 结果 |
|---|---|
| 没有 `knowledge.db` | `markdown` / `no-database` |
| 可读、schema 已知、`meta.imported_at` 是真实时间戳 | `structured` / `imported` |
| 可读、schema 已知、`meta.imported_at` 缺失 | `markdown` / `database-not-imported`，**仍保留原始数据库字节**，但不声称任何结构化内容 |
| 损坏 / 打不开 / `quick_check` 失败 | **错误** |
| `user_version ≠ 1`、表或列超出 v1、未知 `meta` 键、`imported_at` 非时间戳 | **错误** |

关键点：**损坏绝不能退化成 markdown**。否则一个崩溃的库会被当成「从未导入」，导出过期的概念文件并冒充最新知识。
同理，非空但语义非法的 `imported_at` 是错误而不是「未导入」。

### 2.1 不打开线上数据库本体

`knowledge.db`、`-wal`、`-shm` 是服务进程拥有的**活动协调文件**。导出器既不打开也不修改它们：
写者随时可能提交，这些文件也不归导出器处置。因此把字节复制到 OS 临时目录下的私有 scratch，
SQLite 只在**那份副本**上打开，它可以在那里自由创建/重建自己的 `-shm`/`-wal`：

1. 把 `knowledge.db` → `-wal` → `-shm` 按序复制到 OS 临时目录的**私有 scratch**；
2. 在 scratch 上用 `safeIntegers: true` 打开并立刻 `PRAGMA query_only=ON`（scratch 是可丢弃副本，不是源，也不是生产写路径）；
3. 一个 `BEGIN` 内读取 schema、`meta` 与所有表的行；`PRAGMA data_version` 前后比对只是**静态一致性护栏**
   （该副本上只有本连接，且为 `query_only`，正常不会变化），**不是**对线上写者的检测；
4. 读完后再从源读一次这些文件，与 scratch 副本逐字段比对，不一致即 `E_SOURCE_CHANGED`。

`knowledge.db` 及其 `-wal`/`-shm` 本身就是源树里的普通文件：它们在 `files.ndjson` 里各有 `source_file` 记录，
字节在 `blobs/` 里。归档**不再**另存一份 `source-db/`——两处副本只会产生两个需要核对的地方。
导出的行来自**归档实际保留的那份字节**，验证器还会核对 `authority.database.files` 与文件流逐项一致。

> 复制期间若服务正在提交，副本可能不一致。真正的并发拒绝来自**源字节证据**：数据库文件重新读取并与行来源的
> 快照逐字节比对，整棵源树重新遍历并与冻结清单比对；不一致即失败，从不静默接受。复制顺序
> （`db` → `-wal` → `-shm`）只是让撕裂副本更接近 SQLite 可重放的状态，并不使撕裂副本正确。
> 真正的导出仍应按 migration.md §3 先停写或使用一致性快照。

## 3. 归档布局

```
<out>/
  archive.json        # 清单（manifest），canonical JSON
  files.ndjson        # 源文件流，每个文件一条 source_file 记录
  rows.ndjson         # 源行流（仅结构化权威），每行一条 source_row 记录
  blobs/<sha256>      # 源文件原始字节，按内容寻址
  COMPLETE.json       # 完成标记，最后写入
```

`files.ndjson` 覆盖源树中的**每一个常规文件**（含二进制资产与元数据文件），每条记录给出该文件的确切字节。

**范围限定**：这是**文件数据**归档，不是文件系统快照。目录本身不是记录，**空目录不会被重建**，
目录命名空间也不保证可复原。遍历工作仍计入 `maxEntries` / `maxDepth`——空目录不能用来换取无界遍历。
迁移需要的是文件字节与记录行，不是空的目录结构。同名内容只存一份 blob，记录里仍逐文件保留 `sha256`/`bytes`。

§3.1 的 `ino` 是 **64 位 inode 的十进制字符串**（`lstat(..., { bigint: true })`），不经 `Number` 转换：
`ino_t` 超过 2^53，转换会四舍五入，使两个不同文件比较相等，"同内容、文件被替换" 的信号因此失效。

### 3.1 文件记录

```json
{"record_type":"source_file","path":"concepts/L1-core/review-decision.md",
 "bytes":1234,"sha256":"…","blob":"blobs/…","mode":33188,"mtime_ms":1763…,"ino":123456}
```

`ino`/`mtime_ms`/`mode` 让「内容没变但被替换」与「内容变了」都能被识别。

### 3.2 行记录（中性、类型精确）

```json
{"record_type":"source_row","table":"vectors","row_index":0,
 "columns":["id","…","norm","created_at"],
 "values":[{"t":"text","v":"v1"},…,{"t":"real","bits":"3fd3333333333334"},…]}
```

| `t` | 载荷 | 说明 |
|---|---|---|
| `null` | — | 与空字符串、空 blob 区分开 |
| `integer` | `v`: 十进制字符串 | signed 64-bit 精确值；`> 2^53` 不经过 `Number` |
| `real` | `bits`: 16 位十六进制 IEEE-754 | 位精确，避免 `0.1+0.2` 之类被重新格式化 |
| `text` | `v` | 原样，不 trim；非 UTF-8 直接拒绝 |
| `blob` | `bytes_base64` + `bytes` | 0 长度合法 |

没有归一化、没有列重命名、没有「空表跳过」：行序由该表主键列（`pragma` 顺序）决定，
`row_index` 从 0 连续递增，由验证器强制。

`rows.ndjson` 流上还有 `tables: [{table, rows}]` 声明——零行表不会产生记录，
所以「全部表都枚举了」只能由声明 + 计数核对来证明，而不是从记录里推断。

## 4. 边界

所有边界都在**读取之前**生效，且 CLI 无任何生产默认值：

| 选项 | 约束的对象 |
|---|---|
| `--max-files` | 已接纳的常规文件数 |
| `--max-bytes` | 源文件 / 数据库字节总量（同时是 registry 与每个 blob 的读取上限） |
| `--max-archive-bytes` | **校验**时 manifest + 所有流 + 所有 blob 的累计字节上限（调用方提供） |
| `--max-records` | 校验时所有流的记录数上限（调用方提供） |
| `--max-line-bytes` | 校验时单条 NDJSON 行的字节上限（调用方提供） |
| `--verify-max-entries` | 校验时枚举的 blob 目录项上限（调用方提供） |
| `--max-rows` | 导出的行数（超出在流中途失败，与「表不存在」区分） |
| `--max-entries` | 走过的目录项总数（含空目录），限制 `readdir` 工作量 |
| `--max-depth` | 相对源根的目录深度，根为 0 |

目录用 `opendir` 逐项流式读取，不 `readdirSync` 整目录分配；只有被接纳的结果最后排序以获得确定性顺序。
文件读取走 `open` + 定长读 + **一字节越界探测**：`stat` 与 `read` 之间文件变大不会被缓冲，直接 `E_SOURCE_CHANGED`。
schema 内省（`pragma_table_list`、`table_info`、`sqlite_master`、`meta`）同样先按 `--max-entries` 计数/限流再收集。

## 5. 拒绝的情形

| 情形 | 代码 |
|---|---|
| 输出已存在 | `E_OUTPUT_EXISTS` |
| 输出在源内 / 源在输出内 | `E_NESTED_OUTPUT` |
| 源内出现符号链接、FIFO/socket/设备 | `E_SYMLINK` / `E_SPECIAL_FILE` |
| 路径不安全（`.`/`..`/NUL/非 UTF-8） | `E_UNSAFE_PATH` |
| 任一预算超限 | `E_BUDGET_*` |
| 冻结后源发生变化（文件、新增、删除、DB 写入） | `E_SOURCE_CHANGED` |
| 数据库损坏 / schema 不受支持 | `E_DB_CORRUPT` / `E_DB_UNSUPPORTED` |

**不静默排除任何东西。** 无法安全导出的条目会让整次导出失败，而不是被漏掉。

## 6. 完成标记与验证

1. 写 `files.ndjson`、`rows.ndjson` 与 blobs；
2. 组装 `archive.json`（`complete:false`）并写入；
3. 重走源树，与冻结清单逐字段比对（含 `-wal`/`-shm`，任何并发写都会失败）；
4. 全量读回自检：重算流与每个 blob 的哈希，核对每条记录的形状、列序、`row_index` 连续性、表的声明与实际计数；
5. 通过后写 `complete:true`；
6. 写 `COMPLETE.json`，再自检一次；**这一步失败必须撤回标记**。

> **标记语义（F1）**：`COMPLETE.json` 是「本次运行成功」的唯一发布物。任何在它写入之后的失败都会
> 尝试删除它并把清单重置为 `complete:false`。若删除本身失败，错误信息必须**如实报告清理未完成**，
> 不能声称标记已移除。消费方应以**校验器成功 / 退出码 0**为准，而不是只看标记是否存在。

`content_digest` 是清单去掉 `content_digest` 与 `complete` 后的 canonical JSON 的 `sha256:` 摘要——不含自身，无自引用哈希。
`source.snapshot.digest` 是源清单（路径/大小/哈希/模式/mtime/inode）的摘要；
`authority.database.logical_snapshot_digest` 是 schema + 行流 + 每表计数的摘要，即**逻辑快照身份**。

### 6.1 失败时的目录

失败**保留**输出目录，但不留下 `COMPLETE.json`：若失败发生在标记写入之后，标记会被撤回；
撤回失败则如实报错（见上）。`archive.json` 里的 `complete` 重置为 `false`（若可写）。
删除目录会抹掉「这次运行到底写了什么」的证据。恢复方式：重新导出到新目录——不覆盖旧归档。

### 6.2 自检拒绝什么

- blob 缺失、长度不符、内容哈希不符、目录里有未被引用的 blob；
- 流字节数/哈希与清单不符；
- 未知 `archive_version`、未知值类型、非 canonical 的 base64/整数；
- 行引用了 schema 未声明的表，或列序与 schema 不符，或 `row_index` 断号；
- 声明为结构化的归档缺少某张已登记表，或声明计数与实际记录数不符；
- markdown 归档却带有行流；
- `complete:true` 但缺 `COMPLETE.json`，或标记与清单的 `content_digest`/`source_digest` 不一致。

## 7. 用法

```bash
bun tools/sage-db-source-archive.ts \
  --source /path/to/deployed-kb \
  --out /path/to/new-archive-dir \
  --kb default \
  --registry /path/to/registry.json \
  --max-files 20000 --max-bytes 2000000000 --max-rows 2000000 \
  --max-entries 40000 --max-depth 32
```

```bash
bun tools/sage-db-source-archive.ts --verify /path/to/new-archive-dir \
  --max-archive-bytes 2000000000 --max-records 2000000 \
  --max-line-bytes 1048576 --verify-max-entries 40000
```

校验的**上限由调用方给出**，绝不是归档自己声明的 `manifest.limits`：归档不能抬高校验它的预算。
缺少任一上限即 `E_USAGE`。`verifySourceArchive(outDir, limits)` 的签名同样要求 `limits`。
导出器自己的回读使用**自身已写范围**推导出的预算（带溢出检查），而不是不可信的 manifest。

退出码：`0` 成功；`64` 用法错误；`65` 导出被拒绝（源/边界/数据库/并发）；`66` 校验失败（归档损坏或不完整）。

## 8. 明确的非主张

- 本工具**不**证明 P5 完成，**不**声明可切换。
- 它**不**把源记录映射成 sage-db 对象，因此**不**声称无损目标映射。
- 它**不**为源中不存在的历史编造内容：没有结构化记录时就是 markdown 记录，没有导入过就保留原始数据库字节并标 `database-not-imported`。
- 它**不**读取凭据，不连接线上服务；`--source` 是调用方显式给出的路径，registry 里的 `path` 只作为部署上下文记录，从不用于解析源。
