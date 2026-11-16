---
name: x-sage-kb
description: 通过 x-sage REST 服务读写「AI-Native 概念知识库」：检索/激活概念、查边与路径、校验、写回提案、管理快照与图产物。当需要查询某个领域的概念结构、追溯概念间的依赖或矛盾、或向知识库贡献新关联时使用。
---

# x-sage 概念知识库

x-sage 是一个**概念知识库**：不是文档库，也不是向量检索。概念以 markdown 文件为唯一真源，
工具在读取时计算传递闭包与对称边，所以「问一个概念和什么冲突」这类问题不需要你自己遍历。

**架构是标准 C/S**：所有客户端（含 `x-sage` CLI）都是 REST 客户端。服务端是唯一读得到
知识库的组件，客户机没有任何 KB 文件。因此**必须先有一个可达的服务端**。

## 它是什么：概念模型

先用两段话建立心智模型，后面的命令才有意义。

**分层按「定义依赖」排序，不是按重要性。** 越底层越少依赖其他概念，越高层越依赖下层：

| 层 | 名称 | 判定标准 | 当前 |
|---|---|---|---|
| L0 | 公理 | 无法用系统内其他概念定义，只能自然语言描述 | 4（agent / value / context / tension） |
| L1 | 核心 | 用 L0 定义的主干词汇，大多数讨论都会出现 | 5（goal / need / constraint / decision / risk） |
| L2 | 支撑 | 用 L0+L1 定义的细化，只在特定语境相关 | 11（goal-drift / pseudo-need / xartifact-product-matrix …） |
| L3 | 实例 | 某个 L1/L2 在具体场景的落地，会随实践变化 | 21（adr-template / x-herald-gateway / vivid-render-engine …） |

**推理含义**：推演时必须先加载 L0 再展开 L1——因为 L1 的定义依赖 L0。向上是归纳
（从 L3 案例找原则），向下是演绎（从 L0 推具体）。

L3 不应作为其他概念的 `depends-on` 前置：实例是可替换的实现细节，不是理解前提。
**这是约定，不是系统强制**——校验器目前不拦这种边（实测 L2 `depends-on` L3 仍返回 valid），
所以要自己守住。

**置信度决定这个断言能不能当地基。** 三档不是质量评分，是推理许可：

| 值 | 含义 | 能否作为推理基础 |
|---|---|---|
| `confirmed` | 经过验证 | 可以 |
| `hypothetical` | 待验证 | 可用于探索，**不可当公理** |
| `contested` | 存在对立观点 | 需辩证处理，不能单边采信 |

看到 `hypothetical` 就把它当「假设」，在结论里保持这个限定词，不要静默升级成事实。

## 该用哪种模式：从措辞判断

用户的问法决定展开策略。选错模式会得到偏题的子图。

| 用户说 | 模式 | 策略 | 展开方向 |
|---|---|---|---|
| 「告诉我关于 X」「X 和 Y 有什么关系」 | 探索 | 沿 `enables` 广度优先 | 向外辐射，找没想到的关联 |
| 「X 真的对吗」「X 有什么问题」 | 验证 | 优先 `contradicts`，再沿 `depends-on` 回溯 | 向内拆解，找盲点与隐性假设 |
| 「如何设计 X」「在 X 条件下怎么办」 | 生成 | 从 L0 出发沿 `instantiates` 向下 | 从抽象推演到具体 |

拿不准就用探索。对应 CLI 的 `--strategy exploration|validation|generation`。

**验证模式的关键差异**：它**不**沿 `enables` 展开（那是探索的事）。要从矛盾入手，
检查系统的脆弱点——所以它对 `contradicts` 的权重最高，且在 `--depth` 上默认更深（2 层）。

> ⚠️ **验证模式必须补一条入向查询。** `activate` 只沿**出边**遍历，而多数概念的
> `contradicts` 是别人指向它的。实测 `activate goal --strategy validation --depth 1`
> 的前几个结果是 agent / context / value，**一个矛盾都没有**。所以验证时必须紧接着：
>
> ```bash
> x-sage edges <id> --type contradicts --dir in
> ```
>
> 漏了这步，「验证」就退化成又一次探索。

## 激活协议：关键词 → 子图 → 推理

每次处理问题都走这五步，不要跳步：

1. **提取关键词** — 从用户输入取实词
2. **对照 triggers** — `x-sage search <关键词>`，匹配概念 id / atom / triggers
3. **声明激活列表** — 在推理里明确「我要加载哪几个概念」，让判断可审查
4. **加载 molecule** — `x-sage activate <id>`，拿到完整正文
5. **子图内推理** — 只在已加载的概念之间推理；边指向未加载概念时，声明「需要扩展激活」而不是凭印象补充

**上下文预算：同时持有 ≤ 5 个概念的完整 molecule。** 超出时按优先级取舍：

| 优先级 | 加载什么 | 理由 |
|---|---|---|
| P0 | 起点概念 | 用户直接关注的入口 |
| P1 | `enables` 1-hop 邻居 | 直接支撑理解起点 |
| P2 | `depends-on` 邻居 | 起点依赖的前置知识 |
| P3 | `contradicts` 邻居 | 揭示张力对立面 |
| P4 | `analogous-to` 邻居 | 跨域类比，最低优先 |

深度每加一层，上下文显著增长（实测 `activate goal`：1 层 6.9k 字符 → 2 层 10.3k）。
先 1 层，不够再扩。

## 何时必须写回

写回不是「有空就做」，而是有明确触发条件。四种操作对应四种发现：

| 触发条件 | 操作 | 落到哪 |
|---|---|---|
| 发现两个概念间有未记录的关联 | `propose-edge` | `proposals/edges/` |
| 当前 atom 不足以区分两个相似概念 | `refine-atom` | `proposals/atoms/` |
| 正文结论已被推翻，摘要改了但正文还写着旧结论 | `refine-body` | `proposals/bodies/` |
| 发现两个概念存在未记录的矛盾 | 直接编辑 frontier.md | `frontier.md`「概念内张力」表 |
| 会话结束、有值得固化的结论 | `checkpoint` | `checkpoints/` |

**推理中的自检**：如果你在某个概念上做了「边里没写」的推断，那就是一次 `propose-edge`
候选——但标 `hypothetical`，因为你还没验证。

---

## 第零步：确认服务端可达

```bash
curl -s "$X_SAGE_SERVER/api/health"
# {"ok":true,"kb":"/app/kb/default","version":"...","commitHash":"...","buildRef":"..."}
```

不可达时先问用户服务地址，不要猜测、不要试图读本地文件代替。

**地址与 token 的优先级**：`--server` / `--token` > 环境变量 > 配置文件
`~/.config/x-sage/config.toml`（遵循 `XDG_CONFIG_HOME`）。

```toml
# ~/.config/x-sage/config.toml  (chmod 600)
server = "https://x-sage.example.com"
token = "..."          # 仅写操作需要
```

优先用配置文件：一次写好，所有会话生效，也不把 token 留在 shell rc 里被备份带走。
环境变量适合临时覆盖，例如对比两个部署：

```bash
X_SAGE_SERVER=http://127.0.0.1:3001 x-sage activate decision
```

## 读：从问题到概念

```bash
# 1. 定位：关键词匹配 id / atom / triggers
x-sage search 决策

# 2. 激活：目标概念的完整正文 + 邻居摘要
x-sage activate decision
x-sage activate decision --strategy validation --depth 1
x-sage activate decision --format human      # 原样 markdown，给用户看的场合
```

`activate` 的输出格式：

```
[CONCEPT: decision]
ATOM: <一句精华>
LAYER: 1 | SOURCE: product | CONFIDENCE: confirmed
EDGES: depends-on→agent, enables→risk, ...
REQUIRES: [agent, tension, constraint, goal, need, context, value]
TRIGGERS: [...]
LOAD-NEXT: [risk]
---
<完整 molecule>
```

### 读输出的三条规矩

1. **`EDGES` 只含该概念自己写的出边。** 指向它的边不在里面。`goal` 的 `EDGES` 里
   一条 `contradicts` 都没有，但实际有 5 个概念与它对立。要问「谁指向它」必须用
   `edges <id> --dir in`，不要用 `--strategy validation` 拉整张子图
   （实测 688 字符 vs 10382 字符）。
2. **`REQUIRES` 含派生的间接前置。** 想只看作者手写的直接依赖，看 `EDGES` 里的 `depends-on`。
3. **邻居只有 atom，没有正文。** 要深入哪个就再 `activate` 它一次。

## 查边与路径

```bash
x-sage edges goal --type contradicts --dir in     # 谁与 goal 冲突（in = 入向）
x-sage edges decision --type depends-on --derived # 含传递闭包推出的边，标 (derived)
x-sage edges decision --json                      # 结构化，可 jq
x-sage path risk value                            # 两个概念怎么连上的
```

七种边各有代数性质，决定它们怎么被解释：

| 边 | 含义 | 传递 | 对称 | 无环 |
|---|---|---|---|---|
| `is-a` | A 是 B 的一种 | 是 | 否 | 是 |
| `part-of` | A 是 B 的一部分 | 是 | 否 | 是 |
| `depends-on` | 理解 A 前须先懂 B | 是 | 否 | 是 |
| `enables` | 懂 A 有助于懂 B（导航，非前提） | 否 | 否 | 否 |
| `instantiates` | A 是 B 的具体实例 | 否 | 否 | 是 |
| `contradicts` | 辩证张力（**不是**逻辑互斥） | 否 | 是 | 否 |
| `analogous-to` | 跨域类比 | 否 | 是 | 否 |

两个常见误读：

- `contradicts` 是辩证对立，不是 `owl:disjointWith`。一个概念可以是 X 的特例同时又与 X
  张力对立（`goal-drift`、`latent-goal`、`pseudo-need` 都是）。
- `enables` 不是 `depends-on` 的逆。`risk` 与 `decision` 互相 enable，但没有先后。

对称关系在输出里标 `↔`，只列一次。

## 策略的边优先级（补充）

模式决定**为什么**展开，这里给出**具体权重**，用于理解 `activate` 的输出顺序：

| 策略 | 边优先级 |
|---|---|
| `exploration` | enables → depends-on → contradicts → analogous-to |
| `validation` | contradicts → depends-on → enables |
| `generation` | instantiates → depends-on → enables |

## 写：贡献回知识库

写操作需要 `--token`（服务端启用写保护时）。**不要直接改概念文件**，走提案：

```bash
# 发现两个概念间有未记录的关联
x-sage propose-edge agent analogous-to context --confidence hypothetical

# 当前 atom 不足以区分相似概念
x-sage refine-atom decision --atom "新的精华描述" --reason "为什么更好"

x-sage proposals list        # 看待审
x-sage proposals merge all   # 合并回概念文件（人类 review 后才应执行）
```

### 改正文（不只 atom）

atom 纠正之后正文往往还留着旧结论，此时激活会重新引入它。正文走独立的提案路径，
因为它必须原样保留 frontmatter：

```bash
x-sage concept body <id> --json                 # 取 body + revision
x-sage refine-body <id> --file candidate.md \
  --expected-revision <revision> --reason "为什么改"
x-sage proposals show <file>                    # unified diff，只含变更区
x-sage proposals merge <file>
x-sage concept body <id> --format markdown      # 读回，应与候选逐字节一致
```

- `revision` 是提交与合并的双重基线：期间有人改了 atom、边或正文都会让合并返回 409，
  提案保持 `pending`，重新读取后重提即可。
- 只替换 frontmatter 之后的字节，元数据不会被重写。
- 正文不参与 `graph.json` / `MANIFEST.md` 的生成，合并后**不需要**重建产物。
- 提案文件是 JSON（`proposals/bodies/`），不是 Markdown——正文以 `---` 开头时
  后者会被 YAML 解析吃掉。

`confidence` 三档：`confirmed`（可作推理基础）、`hypothetical`（可探索，不可当公理）、
`contested`（存在对立观点，需辩证处理）。**不要把自己的推测标成 confirmed。**

## 校验与产物重建

```bash
x-sage validate --graph      # 文件 schema + 图完整性 + graph.json 是否落后
x-sage graph generate        # 概念文件 → graph.json
x-sage manifest --write      # 概念文件 → MANIFEST.md
```

`graph.json` 和 `MANIFEST.md` 都是生成物。提交前跑 `validate --graph`，它会在产物落后于
概念文件时报 `stale-graph`。

## 会话快照

```bash
x-sage checkpoint create tension 2      # 建空骨架（session 2）
x-sage checkpoint read tension          # 读最新
x-sage checkpoint read tension 1        # 读指定 session
x-sage checkpoint save - < cp.json      # 从 stdin 写完整快照
```

`checkpoint create` 写的是**空骨架**（`established: []`）。内容需要你自己填：
读出骨架、补字段、再 `save -` 写回。

## 禁止事项

1. **不要用 `frontier` 记录「概念内张力」或「L2→L3 展开方向」** —— `frontier add` 只向
   「未决概念」表插入一行，不碰另两节；这两节仍只能直接编辑文件。不要用 `add` 去表达张力。
2. **不要把 hypothetical 当 confirmed 用** —— 未验证的关联不可作为推理基础。
3. **不要消除张力的两端** —— 张力是真实结构，只能管理不能消除。
4. **不要跳过 `depends-on` 前置** —— 加载顺序影响推理结论。
5. **不要猜服务地址** —— 不可达就问用户。

## 快速参照：命令 → REST 端点

CLI 只是薄传输层，等价调用：

| CLI | REST |
|---|---|
| `search <kw>` | `GET /api/search?q=` |
| `activate <id>` | `GET /api/concepts/<id>?format=text` |
| `edges <id> --dir in` | `GET /api/concepts/<id>/edges?dir=in` |
| `path a b` | `GET /api/path?from=a&to=b` |
| `validate --graph` | `GET /api/validate?graph=true` |
| `concept body <id>` | `GET /api/concepts/<id>/body` |
| `frontier add <c> --source --reason` | `POST /api/frontier` |
| `propose-edge ...` | `POST /api/proposals` (`kind:"edge"`) |
| `refine-body ...` | `POST /api/proposals` (`kind:"body"`) |
| `proposals show <file>` | `GET /api/proposals/<file>` |
| `graph generate` | `PUT /api/graph` |
| `manifest --write` | `PUT /api/manifest` |

`format` 可协商：`json`（默认）、`text`（AI 文本）、`markdown`（原文）、`manifest`（表格）。
