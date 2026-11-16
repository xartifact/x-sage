# 结构化知识存储（Structured Knowledge Store）

> 状态：已实现首版（2026-11-16）。Markdown 从「真源」降级为「人的编辑与审阅格式」；
> 权威记录是每个知识库一个的 SQLite 文件（`<kb>/knowledge.db`）。

## 为什么要这一步

Markdown 文件真源在概念数量少、只有概念+边时是合适的：一个文件就是一个概念，
`git diff` 就是审阅。但它有三个结构性缺口，都随知识增长而放大：

| 缺口 | 表现 |
|---|---|
| 一条断言无处安放 | 概念的正文是散文。哪一句是「被支持的断言」，哪一句是作者的修辞，机器无法区分；证据只能靠 `locator` 字符串指向文件。 |
| 关系之外没有「关联」 | 边是断言（`depends-on`、`contradicts`）。相似但不是断言的联系没有位置，于是只能塞进边，把两种强弱不同的东西混成一种。 |
| 多模态无法表达 | 图片证据无处存放；把截图放进仓库也没有摘要、没有哈希、没有「它支持哪个断言」。 |

这一步把权威记录搬进 SQLite，并保留 Markdown 作为**人的格式**：导出→编辑→
以版本基线审阅写回。同一次搬迁也引入了断言（claim）、证据（evidence）与外部
向量（vector）三类记录。

## 权威切换（唯一开关）

```
<kb>/knowledge.db 存在且写入过 meta.imported_at   ⇒  权威 = 结构化
否则                                              ⇒  权威 = Markdown（原行为）
```

- **读不创建数据库。** 没有导入过的知识库行为与之前完全一致。
- **导入是显式动作**：`POST /api/knowledge/import { mode: "authority" }`。
- **回滚是删文件**：删除 `knowledge.db` 即回到文件权威，概念文件从未被移动或改写。
- 导入后旧的写路径（`POST /api/concepts`、`POST /api/proposals`、
  `POST /api/proposals/merge`、`PUT /api/graph`）返回 **409** 并点名替代端点；
  旧读路径（`/api/manifest`、`/api/graph`、`/api/viz`、`/api/search`、`/api/path`、
  `/api/validate`、`/api/concepts/:id`、`/api/concepts/:id/edges`）保持相同 DTO，
  数据改从数据库派生。

## 显式、可恢复的导入

`POST /api/knowledge/import`

```json
{ "mode": "authority", "reimport": false }
```

1. 把 `concepts/`、`graph.json`、`MANIFEST.md` 复制到 `archive/markdown-<timestamp>/`
   （附 README 说明如何回滚）。文件保留在原位，`git diff` 显示为新增而非删除。
2. 逐文件解析并写入 `concepts`、`relations`、`claims` 表。文件里的受管 claims 块
   （见下）会被导入为断言；没有该块的纯 Markdown 概念导入为零断言。
3. 每个概念记录两个哈希：`source_hash`（导入时文件的字节哈希）与 `source_revision`
   （导入时权威记录的版本）。

重复导入的判定（`reimport: true` 时必须显式声明）：

| 文件 | 权威 | 结果 |
|---|---|---|
| 未变 | 未变 | `unchanged` |
| 变了 | 未变 | `updated` |
| 未变 | 变了（有审阅通过的断言/正文） | 保留权威，不报告问题 |
| 变了 | 变了 | **`conflict`**：两边都改动，人工裁定 |
| 文件消失 | 有记录 | **`conflict`**：保留权威记录，绝不因文件重命名而删除知识 |
| id 重复 | — | **`conflict`**：先出现者胜 |

有冲突时导入返回 **207**，响应体给出逐条结果；不抛异常、不中断其余文件。

## 记录模型

```
concepts(id, atom, layer, domain, confidence, source, triggers, body, source_path,
         revision, source_hash, source_revision, created_at, updated_at)
relations(from_id, type, target, confidence, created_at)          -- 断言的关系
claims(id, concept_id, kind, status, text, confidence, revision)  -- 断言的内容
evidence(id, kind, title, locator, excerpt, asset_*, captured_at, captured_by)
evidence_targets(evidence_id, target_type, target_id)             -- concept | claim | relation
vectors(id, subject_type, subject_id, model, dimensions, revision, provider, values_blob, norm)
assets(sha256, media_type, byte_size, path)
proposals(id, kind, subject, expected_revision, payload, reason, author, status, …)
```

三条设计约束：

1. **边只有一张表。** `relations` 是唯一的关系真源；概念记录里没有 `edges` 副本。
   这正是 Markdown 时代 `requires`/`enables` 与 `edges` 漂移的根因。
2. **证据是记录，不是字符串。** 一条证据指向 concept / claim / relation 中的任意
   一个或多个目标，图片证据带内容寻址的资产。
3. **向量与边不同表、不同字段、不可互转。** 检索结果把 `assertedRelations` 与
   `vectorAssociations` 分开返回，任何消费者都不会把相似度当成某人断言过的事实。

## 断言（claims）

```bash
curl -s -X POST $S/api/knowledge/proposals -H 'content-type: application/json' -d '{
  "kind": "claim-create",
  "subject": "review-premise",
  "expectedRevision": "<conceptRevision>",
  "reason": "把容量假设写下来",
  "author": "external-agent",
  "payload": { "kind": "premise", "text": "峰值 QPS 在 2027 年前不超过 3 倍", "confidence": "hypothetical" }
}'
```

- `kind`：`premise | constraint | decision | risk | observation | requirement`。
  它不是分类目录，而是决定审阅者怎么读这句话；一句话要两个 kind 就是两句话。
- `confidence`：复用概念的 `confirmed | hypothetical | contested`。
- 只有审阅通过（`status = accepted`）的断言进入权威与检索。`superseded` 表示
  从文档中移除但保留追溯，`rejected` 表示审阅否决。
- 删除断言的唯一方式是审阅通过的文档写回把它从受管块中移除——它变成
  `superseded`，而不是被删除。

## 证据

```bash
# 图片先上传，得到内容地址
curl -s -X POST $S/api/knowledge/assets \
  -H 'content-type: application/json' \
  -d "{\"data\":\"$(base64 -w0 chart.png)\"}"

# 再把证据挂到断言上
curl -s -X POST $S/api/knowledge/proposals -H 'content-type: application/json' -d '{
  "kind": "evidence-attach", "subject": "review-premise",
  "expectedRevision": "<conceptRevision>", "reason": "引用容量评审",
  "author": "external-agent",
  "payload": {
    "kind": "text", "title": "容量评审记录 2026-11-04",
    "locator": "evidence/review-2026-11-04-capacity.md",
    "excerpt": "SRE 指出：3 倍假设没有经过压测。",
    "capturedBy": "sre-lead",
    "targets": [{ "type": "claim", "id": "clm_…" }]
  }
}'
```

- 资产按 `sha256` 存于 `<kb>/assets/<sha256>`，媒体类型由**字节签名**判定，
  与上传者声明的类型不一致即拒绝（415）。SVG 不在允许列表：它会带来脚本执行面。
- `GET /api/knowledge/assets/<sha256>` 返回原字节与检测到的类型。
- 证据目标必须在提交时存在，否则 404——审阅者无法修正作者写错的 id。

## 审阅与版本基线

所有权威写入都是「提案 + 审阅」。没有直接编辑端点。

```
POST /api/knowledge/proposals            → 201 { id, status: "pending", … }
GET  /api/knowledge/proposals?status=…
GET  /api/knowledge/proposals/<id>
POST /api/knowledge/proposals/<id>/review { decision: approve|reject, reviewer, note? }
```

基线由服务端判定，不是由作者自选：

| 提案 kind | `expectedRevision` 的含义 |
|---|---|
| `claim-create` | 概念的**审阅基线**：概念记录 + 其 accepted 断言 |
| `claim-edit` | 该断言自身的版本 |
| `evidence-attach` | 概念的审阅基线 |
| `relation-assert` | 该概念的**关系集合**版本（正文改动不应让它失效） |
| `concept-markdown` | 导出文档的字节哈希（`GET …/document` 返回的 `revision`） |

- 提交时基线不符 → **409**，什么都不写。
- 审阅时基线已变 → 提案标记为 `conflicted`（**保留**，可重新提交），返回 **409**。
- 批准是单事务：复核基线 → 应用 → 置 `approved`。中途失败则提案仍是 `pending`。
- 关系提案在事务内跑图校验：成环、层向违例直接拒绝，不会写进一个让闭包不收敛的边。

## Markdown 导出 / 写回

`GET /api/knowledge/concepts/<id>/document` 返回可编辑文档：

```markdown
---
id: review-risk
atom: "…"
…
---

# 人类正文，原样保留

<!-- x-sage:claims:begin -->
## 断言 · claims (managed by x-sage; text after the last `::` is yours to edit)
- clm_9f3a2b7c :: premise :: confirmed :: 峰值 QPS 不超过 3 倍
<!-- x-sage:claims:end -->

<!-- x-sage:evidence:begin -->
## 证据 · evidence (read-only projection; change it through the structured API)
- evd_1a2b3c4d :: text :: 容量评审记录 2026-11-04 :: evidence/review-…md
<!-- x-sage:evidence:end -->
```

规则：

- 人类正文在导出与导入之间**逐字节保持**（只规范化首尾空行）。正文若包含
  `<!-- x-sage:claims:begin -->` 这类受管标记，导出会被拒绝（400）而不是被静默截断。
- claims 块是唯一可编辑的机器区：行首 id 锚定断言，最后一个 `::` 之后是正文。
  文本里的 `::` 没问题（从右往左重组），换行会被转义成 `\n` 因此一条断言始终是一行；
  字面量 `\n` 会转义成 `\\n`，回读不会变成换行。
- evidence 行的 title / locator 里的 `::` 会被转义，位置字段不会互相移位。
- evidence 块是只读投影：图片字节与哈希无法在 Markdown 里编辑，所以证据变更
  必须走结构化 API。
- 写回走 `kind: "concept-markdown"` 提案，携带导出的 `revision`；基线不符即 409。
- 文档里引用一个不存在的 claim id 会被拒绝（400），而不是静默新建——断言的创建
  有自己的提案 kind，路径分开才有审计。

CLI：

```bash
x-sage knowledge document review-risk                  # markdown
x-sage knowledge document review-risk --json           # 带 revision / conceptRevision
x-sage knowledge propose --kind concept-markdown --subject review-risk \
  --expected-revision <rev> --reason "澄清约束与前提" --author me \
  --document edited.md
```

## 外部向量

嵌入由外部 Agent 计算，服务端只存储与校验，绝不自己生成、也不伪造随机向量。

```bash
curl -s -X POST $S/api/knowledge/vectors -H 'content-type: application/json' -d '{
  "vectors": [{
    "subjectType": "concept",
    "subjectId": "review-constraint",
    "model": "openai/text-embedding-3-small",
    "provider": "openai",
    "revision": "<sha256 of embeddingText>",
    "values": [0.013, -0.004, …]
  }]
}'
```

- **必须带 `revision`**：被嵌入文本的哈希。计算方式公开在 core：
  `embeddingTextForConcept`（atom + 空行 + body）、`embeddingTextForClaim`（text）、
  `embeddingTextForEvidence`（title + 空行 + excerpt），`embeddingRevision(text)` 取哈希。
  不一致 → **409**，并报告两边的前 12 位。上传时就拒绝，比检索时静默过滤更能让
  Agent 发现自己的嵌入管道过期了。
- 每个 `(subjectType, subjectId, model)` 唯一：重新上传即替换。
- `model` 与 `dimensions` 随行存储。查询必须同时给出 model 与向量长度，只有完全
  匹配的行参与打分——768 维向量不会被当作 1536 维的零填充版本。

## 混合检索

```bash
curl -s -X POST $S/api/knowledge/search -H 'content-type: application/json' -d '{
  "query": "延迟 约束",
  "limit": 10,
  "vector": { "model": "openai/text-embedding-3-small", "values": [ … ] }
}'
```

- **词法轴**：BM25。CJK 无空格，按字符 bigram 索引（`知识` 命中 `结构化知识`），
  拉丁词按 token。结果里带命中的 term 与频次作为匹配依据。
- **向量轴**：余弦相似度。只比较 model 与维度都相同的行。
- **融合**：按名次（reciprocal rank fusion），不按分数相加——BM25 无上界、余弦在
  [-1,1]，没有共同单位。
- **过期向量不参与**：`vectors.revision` 与当前文本哈希不符即排除，并在
  `vector.excludedStale` 与 `notes` 中报告数量。缺向量时计 `vector.missing`；
  存量行含非有限值（旧版本写入的 float32 溢出）时计 `vector.invalid` 并跳过该行，
  而不是让整次查询失败。
- 每条结果返回：`signals`（哪条轴、什么分数、命中哪些词）、`assertedRelations`
  （入/出向断言关系）、`vectorAssociations`（向量关联，明确与断言分开）、
  `claims`、`evidence`。
- 边界：query ≤ 512 字符，limit ≤ 50，向量 ≤ 4096 维；索引只取文本前 20 000 字符
  （`MAX_INDEX_CHARS`），因此超长正文不会把一次检索变成全库扫描。

`conceptId` 可把检索限定在一个概念的范围内（含其断言与证据）。

## API 一览

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/knowledge/status` | 权威类型、导入时间、记录计数 |
| POST | `/api/knowledge/import` | 显式迁移（`mode=authority`，`reimport`） |
| GET | `/api/knowledge/concepts` | 概念列表（id/atom/layer/revision） |
| GET | `/api/knowledge/concepts/:id/document` | Markdown 导出（`format=markdown`） |
| GET | `/api/knowledge/concepts/:id/claims` | accepted 断言 + 其证据 |
| GET | `/api/knowledge/concepts/:id/evidence` | 该概念的证据（含支持其断言的证据） |
| GET | `/api/knowledge/claims/:id/evidence` | 某条断言的证据 |
| GET | `/api/knowledge/revisions?kind=&subject=` | 该提案类型当前接受的基线 |
| GET | `/api/knowledge/evidence/:id` | 单条证据 |
| POST | `/api/knowledge/proposals` | 提交提案 |
| GET | `/api/knowledge/proposals[?status=]` | 提案列表 |
| GET | `/api/knowledge/proposals/:id` | 提案详情 |
| POST | `/api/knowledge/proposals/:id/review` | 批准 / 否决 |
| POST | `/api/knowledge/search` | 混合检索 |
| POST | `/api/knowledge/vectors` | 上传外部嵌入 |
| POST | `/api/knowledge/assets` | 上传图片（base64） |
| GET | `/api/knowledge/assets/:sha256` | 取回图片字节 |

## 迁移步骤（运维）

```bash
# 1. 备份（导入会自己再复制一份到 archive/，这里是为了整库快照）
tar czf kb-backup.tgz kb/default

# 2. 显式导入；207 表示有需要人工裁定的冲突
curl -s -X POST "$S/api/knowledge/import" -H 'content-type: application/json' \
  -d '{"mode":"authority"}' | jq .

# 3. 校验：读路径全部来自数据库
curl -s "$S/api/meta" | jq .          # authority: "structured"
curl -s "$S/api/validate?graph=true" -o /dev/null -w '%{http_code}\n'

# 4. 需要回滚时
rm kb/default/knowledge.db
```

导入时 `MANIFEST.md` 与 `graph.json` 会被替换为一行指针（指向
`archive/markdown-<timestamp>/`）。它们不再由服务端读写：`GET /api/manifest` 与
`GET /api/graph` 从数据库生成，`PUT` 返回 409。用编辑器直接打开这两个文件不会
看到「看起来像最新」的旧内容。

注意：数据库与 `archive/` 都在 `<kb>/` 下。部署时该目录必须是可写卷（容器里
已经如此挂载）。WAL 模式会额外产生 `knowledge.db-wal` / `-shm`。

## 与导入前输出的差异

导入后 DTO 形状不变，但有两处可见差异，都是刻意的：

- `GET /api/concepts/:id?format=markdown` 返回的是**投影文档**（YAML 块式列表、
  未加引号的 atom），不是原始文件字节。语义相同、可重新解析；`molecule` 与 `requires` /
  `loadNext` 与导入前逐字一致（图的节点/边顺序也一致：按层、按 id、按声明顺序）。
- `MANIFEST.md` / `graph.json` 磁盘文件被替换为指针说明（见上）。

## 已知限制

- 没有内置的嵌入模型；检索的向量轴质量取决于外部 Agent 提供的向量。
- 证据资产只支持 PNG / JPEG / WebP / GIF；PDF 与 SVG 尚未支持（SVG 有脚本面，
  需要先决定净化策略）。
- 提案没有并发配额与速率限制；写 token 之外没有按主体授权。
- `evidence` 的只读投影意味着「在 Markdown 里改证据」不生效——它会被忽略，而不是报错。
- **跨知识库引用被显式拒绝**。Markdown schema 仍接受 `kb-id::concept-id` 语法，
  但结构化存储一库一文件：导入时报 `invalid`（逐文件），`relation-assert` 与
  `evidence-attach` 提交时报 400。这样契约与实现一致，而不是写入一条永远悬空的边。
  跨库检索需要先决定按 `(kb_id, id)` 建键，尚未实现。
