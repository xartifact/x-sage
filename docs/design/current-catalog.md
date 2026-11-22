# 当前目录读取(current catalog)

> 状态:**读取接缝**,不是序列化器、渲染器或 HTTP 面。
> 实现:`sage-db/crates/kernel/src/catalog.rs`。
> 上游:`docs/design/data-model.md` §2(slug 稳定身份)、§5(提案基线)。

## 1. 它做什么

为**普通客户端**提供两个只读操作:

1. `visit_catalog` —— 在**一个**持久 basis 上,按借用记录访问当前目录;
2. `resolve_term_slug` —— 精确 slug 解析一个 Term。

两者都是普通的 `Read` 读,不是管理导出。

## 2. 它不做什么

- 不是 `SnapshotView`:那个契约是封闭的四族投影,本模块**不扩展**它。
- 不改造物理管理扫描 `Store::scan_current_records`。那条路径仍属 ADMIN,不借给普通客户端。
- 不铸造权限:唯一的许可来自调用方自己的 `Grants`,并且必须对**实际 KB** 校验。
- 不做序列化、渲染、搜索、HTTP。

## 3. 四道顺序保证

顺序本身就是契约:

1. `Grants::authorize(store.kb_uuid(), Read)` **先于**任何记录访问 —— 没有 `Read` 的调用方
   连"这个 KB 有没有记录"都学不到。
2. `ensure_import_ready` **先于**记录访问 —— 未完成的 import 拒绝,而不是呈现成"很小"或
   "空"的目录。
3. **同一个** `SharedLedger` 支付授权、就绪检查、遍历与 visitor 的工作。
4. **一个** `ViewBasis` —— 结果携带它被读出的那个发布,永不混用两个发布。

访问器收到**同一个** `&mut dyn SharedLedger` 与**借用的** `FoldedRecord`:
不复制记录、不引入平行 DTO,因此没有任何缓冲区能活过支付它的 credit。

## 4. 排序:诚实说明

`Store::scan_identified_records` 遍历 fold 的 current 索引,该索引按**提交顺序**压入。
**没有任何稳定 id 或字典序承诺**,所以本模块也不宣称。需要顺序的调用方必须自己施加;
从 id 推断顺序是错误的。

## 5. 过滤是显式的

retired Term、retracted Assertion、非 approved Proposal 都是**真实存在的源行**。
只有调用方明确要求时才过滤:

| `Activity` | 行为 |
|---|---|
| `Live` | 只保留仍然有效的行 |
| `All` | 全部保留,包括 retired/retracted/rejected 的 after-image |

`CatalogStats` 把 `visited / matched / excluded_family / excluded_inactive` 分开报告,
所以"目录为空"与"过滤掉了全部"可以区分,`visited == matched + excluded_family +
excluded_inactive` 恒成立 —— 没有静默丢弃。

## 6. 精确 slug

- 比较是**逐字节精确**:不做大小写折叠、不做 Unicode 归一化、不 trim。
- retired Term **仍然拥有**它的 slug(`data-model` §2:slug 不复用),所以解析**扫描全部**
  当前 Term(含 retired),并把记录自己的 `retired` 交给调用方判断。把 retired 过滤掉会让一个
  永久保留的名字看起来是空的 —— 正是规则禁止的碰撞。
- 两个当前 Term 声称同一 slug 是**矛盾**,返回 `AmbiguousSlug { claimants, basis }`,
  绝不 first-wins。
- 每次候选比较都向调用方账本收取**约定的内容比较工作量**,约定显式写在 `slugs_equal_charged`:

  | 情形 | 比较对数 | 计费 |
  |---|---|---|
  | 长度不同 | 0 | **0**(长度已决定答案,不读任何字节) |
  | 长度相同,首对即不同 | 1 对 | **2**(stored + probe,读前计费) |
  | 长度相同,全部相同 | n 对 | **2n** |
  | 空对空 | 0 | **0** |

  每对在两个字节被读取**之前**计费;首个不同即停止,其后不再计费。
  **每条路径**都做控制检查点,包括零字节的路径。
- slug 是**借用**的,不复制。`MAX_SLUG_BYTES` 已限制存储长度,计费的意义是精确而非幅度。

## 6.1 统计计数

`CatalogStats` 的计数用 **checked** 进位,溢出返回 `CatalogError::Overflow`,不 saturate:
饱和会静默封顶并破坏 `visited == matched + excluded_*` 不变量,报告一个伪造的总数。
- 缺席只在一次完整、已授权的遍历之后报告 —— 绝不因为某次拒绝就报"不存在"。

## 7. 错误面

| 变体 | 含义 |
|---|---|
| `CatalogError::Command` | 授权、import 就绪、或 Store 读取拒绝(含真实的字节/条目/停止拒绝) |
| `CatalogError::Visitor` | 调用方 visitor 自己的错误,原样保留,且**不返回**部分结果 |
| `CatalogError::AmbiguousSlug` | 同一 slug 有多个当前持有者 |

调用方拿到 `Visitor` 错误时必须丢弃它已经构造的部分输出 —— 本模块刻意不返回部分目录。

## 8. 非主张

不是阶段、不是切换、不是读就绪证明。不推断事实、不渲染、不做 HTTP。四个保证只覆盖本模块
自己添加的边界;Store 自身的持久性与损坏语义原样透传。

## Root R3 correction

The non-ASCII test now uses U+FF11 (not the ASCII U+0031 spelling) and requires
Absent at the actual publication basis. The public slug documentation appears once;
the private comparator documents only its pair-charging convention. Catalog statistics
check each increment and preserve an overflow separately from the visitor's own refusal.
