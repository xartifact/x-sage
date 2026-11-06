# 边语义形式化 — 实现记录

## 目标

从本体借来"形式语义"这一块能力，但不引入 OWL/DL 推理机：让每条边声明自己的代数性质，
校验器和遍历器按声明执行，可推导的部分一律不手写。

## 关系代数表

单一真源：`packages/core/src/graph/semantics.ts` 的 `RELATION_SPECS`。

| 边类型 | 传递 | 对称 | 无环 | 层向 |
|---|---|---|---|---|
| `is-a` | 是 | 否 | 是 | to-abstract |
| `part-of` | 是 | 否 | 是 | to-abstract |
| `depends-on` | 是 | 否 | 是 | any |
| `enables` | 否 | 否 | 否 | any |
| `instantiates` | 否 | 否 | 是 | to-abstract |
| `contradicts` | 否 | 是 | 否 | any |
| `analogous-to` | 否 | 是 | 否 | any |

## 三处刻意偏离本体

1. **`enables` 不是 `depends-on` 的逆。** 实测把 18 条 `enables` 反推为 `depends-on` 会产生
   `decision → risk → decision` 假环；`risk.md:58-60` 与 `decision.md` 都写明二者互为因果。
2. **`contradicts` 不是 `owl:disjointWith`。** `goal-drift`、`latent-goal`、`pseudo-need`
   同时 is-a 父类又 contradicts 父类；互斥校验会删掉 KB 最有价值的结构。
3. **`confidence` 参与派生。** 闭包边的置信度取路径最弱值，结论不高于链上最弱一环。

## 变更清单

新增：
- `packages/core/src/graph/semantics.ts` — 关系代数表、对称补全、层向判定
- `packages/core/src/graph/closure.ts` — 传递闭包、环检测（迭代 DFS）
- `packages/core/src/graph/derive.ts` — `targetsOf` / `sourcesOf` / `incidentEdges`

删除（双真源收敛）：
- `ConceptFrontmatterSchema` 的 `requires` / `enables` 字段
- 22 个概念文件 + 3 个协议文件的对应两行

改写：
- `validate.ts` — 新增 `edge-cycle` / `layer-violation` / `self-reference`，
  移除旧的 `unreachable-requires`（字段已不存在）
- CLI `REQUIRES` 改读 `depends-on` 传递闭包；`EDGES` 保留手写邻接
- `validate --graph` 新增 `stale-graph` 检查（graph.json 与概念文件不一致即报错）
- viz 支持全部 7 种边，对称边不画箭头

数据修正：
- `goal-drift` / `latent-goal` / `latent-need` / `pseudo-need` 的 `part-of → 父类`
  改为 `is-a → 父类`（molecule 原文均为"……的一种形式"）
- 另外 4 个 L2（`constraint-cascade`、`decision-traceability`、
  `hard-soft-constraint`、`risk-asymmetry`）保留 `part-of`——它们是属性/结构而非子类

## 验证证据

```
bun run typecheck                    → 3/3 packages exit 0
bun apps/cli/src/index.ts validate --graph
                                     → 22/22 concepts valid
                                     → graph.json matches 22 concepts and 94 edges
                                     → no integrity issues
x-sage activate decision         → REQUIRES: agent, tension, constraint, goal, need, context, value
x-sage activate goal-drift       → EDGES: is-a→goal, depends-on→goal, ...
viz (built, served)                  → 101 edges: contradicts 14 (symmetric), depends-on 55,
                                       enables 18, instantiates 5, is-a 4, part-of 5
```

派生统计（真实 KB）：对称补全 +7 条，传递闭包 +36 条（全部是 `depends-on`），0 环。
`part-of` 没有任何派生边——唯一会产出的两条（`latent-goal` / `goal-drift` → `tension`）
依赖 `is-a`∘`part-of` 跨关系复合，而闭包刻意只在同一关系内做。

伪造 graph.json 删一条边后 `validate --graph` 正确报 `stale-graph` 并指出缺失边。

## 查询接口

`activate` 只输出每个节点的出边，因此「谁指向 X」这类问题必须靠扫描整段输出。
`edges` 命令补上这个缺口：

```bash
x-sage edges <id> [--type T] [--dir in|out|both] [--derived] [--json]
```

实测「什么和 goal 冲突」：`activate goal --strategy validation` 输出 10382 字符，
`edges goal --type contradicts --dir in` 输出 688 字符——同一答案，93% 更少的上下文。
对称关系标 `↔` 且只列一次，定向关系区分 `←` / `→`。

## 依赖与资源收敛

两处会导致静默错误的问题已修：

- **zod 双版本**：core 用 zod 3、CLI 声明 zod 4，`ZodError` 的 `instanceof` 跨版本为 false，
  `validate` 因此把整个 Zod 错误对象转储给 agent（约 60 行）而不是一行。修复：删掉 CLI 的
  zod 依赖，报错格式化统一走 `core/src/types/zod-error.ts` 的 `formatZodError`，
  内部含跨版本形状兜底且自身不抛异常。实际效果：
  `Error: layer: Invalid input; domain: Required; confidence: Required; ...`
- **`apps/viz/public/graph.json`**：从 viz 初次提交起就没人读写（`useGraphData` 只取
  `graph-data.json`），内容停在 36 条边。已删除，避免被误当数据源。

## 未处理（发现但不在本次范围）

- `x-sage frontier add` 会重写 `frontier.md`，抹掉"概念内张力"和"L2→L3 待展开方向"两节。
  这是既有 bug，与本次改动无关。已用 `git checkout` 复原文件。
- KB 里 5 个 L3 与 3 个协议文件未被 git 跟踪（目录早于本次会话存在）。
