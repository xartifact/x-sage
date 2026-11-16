import OperationResult from "./OperationResult";
import { useState, useEffect } from "react";
import { api } from "./api";
import { EDGE_TYPES } from "./types";
interface Props {
  kb: string;
  ids: string[];
  onChange: () => void;
  onClose: () => void;
}
export default function Workbench({ kb, ids, onChange, onClose }: Props) {
  const [tab, setTab] = useState("create"),
    [token, setToken] = useState(""),
    [busy, setBusy] = useState(false),
    [error, setError] = useState(""),
    [result, setResult] = useState<unknown>(null),
    [dirty, setDirty] = useState(false);
  const [proposals, setProposals] = useState<
    Array<{
      file: string;
      status: string;
      conceptId?: string;
      from?: string;
      target?: string;
    }>
  >([]);
  const [checkpoint, setCheckpoint] = useState("");
  const [kind, setKind] = useState("body");
  const [id, setId] = useState(ids[0] ?? ""),
    [revision, setRevision] = useState(""),
    [body, setBody] = useState(""),
    [file, setFile] = useState(""),
    [reviewed, setReviewed] = useState("");
  useEffect(() => {
    if (!ids.includes(id)) {
      setId(ids[0] ?? "");
      setRevision("");
      setBody("");
      setCheckpoint("");
    }
  }, [ids, id]);
  const discard = () =>
    !dirty || confirm("此操作会替换未保存的输入，是否继续？");
  async function run(path: string, method = "GET", value?: unknown) {
    setBusy(true);
    setError("");
    try {
      const data = await api(kb, path, method, value, token);
      setResult(data);
      if (method !== "GET") {
        onChange();
        setDirty(false);
      }
      return data;
    } catch (e) {
      setError(String(e));
      return null;
    } finally {
      setBusy(false);
    }
  }
  const field = (name: string, label: string, required = true) => (
    <label>
      {label}
      <input
        name={name}
        required={required}
        {...(name === "id" ? { pattern: "[a-z0-9][a-z0-9-]*" } : {})}
        {...(name === "atom" ? { maxLength: 200 } : {})}
      />
    </label>
  );
  return (
    <aside className="workbench" role="dialog" aria-label="知识库维护">
      <header>
        <h2>维护知识库 · {kb}</h2>
        <button
          disabled={busy}
          onClick={() => {
            if (!dirty || confirm("放弃未保存的输入？")) onClose();
          }}
        >
          关闭
        </button>
      </header>
      <label>
        写入凭据
        <input
          type="password"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="服务端启用保护时填写，仅保留在当前面板"
          autoComplete="off"
        />
      </label>
      <nav>
        {[
          ["create", "新增概念"],
          ["proposal", "修订概念"],
          ["review", "审阅提案"],
          ["frontier", "未决问题"],
          ["checkpoint", "会话快照"],
          ["tools", "查询与校验"],
        ].map(([key, label]) => (
          <button
            aria-pressed={tab === key}
            key={key}
            disabled={busy}
            onClick={() => {
              if (!dirty || confirm("放弃未保存的输入？")) {
                setTab(key!);
                setDirty(false);
                setResult(null);
                setError("");
              }
            }}
          >
            {label}
          </button>
        ))}
      </nav>
      <fieldset disabled={busy} onChange={() => setDirty(true)}>
        {tab === "create" && (
          <form
            onSubmit={async (e) => {
              e.preventDefault();
              const form = e.currentTarget;
              const f = new FormData(form);
              const split = (key: string) =>
                String(f.get(key))
                  .split(/[,，]/)
                  .map((s) => s.trim())
                  .filter(Boolean);
              const data = await run("/concepts", "POST", {
                frontmatter: {
                  id: f.get("id"),
                  atom: f.get("atom"),
                  layer: Number(f.get("layer")),
                  domain: split("domain"),
                  triggers: split("triggers"),
                  confidence: f.get("confidence"),
                  source: "personal",
                  edges: [],
                },
                body: f.get("body"),
              });
              if (data) form.reset();
            }}
          >
            {field("id", "概念 ID（小写英文、数字、连字符）")}
            {field("atom", "一句话摘要（最多 200 字符）")}
            <label>
              层级
              <select name="layer" defaultValue="2">
                {[0, 1, 2, 3].map((n) => (
                  <option key={n} value={n}>
                    L{n}
                  </option>
                ))}
              </select>
            </label>
            <label>
              置信度
              <select name="confidence">
                <option>hypothetical</option>
                <option>confirmed</option>
                <option>contested</option>
              </select>
            </label>
            {field("domain", "领域（逗号分隔）")}
            {field("triggers", "触发词（逗号分隔）")}
            <label>
              概念正文
              <textarea name="body" required rows={10} />
            </label>
            <button disabled={busy}>创建概念</button>
            <p>创建后立即进入图谱；关联可通过“修订概念”提交。</p>
          </form>
        )}
        {tab === "proposal" && (
          <form
            onSubmit={async (e) => {
              e.preventDefault();
              const f = new FormData(e.currentTarget);
              if (kind === "body" && !revision) {
                setError("请先读取当前正文");
                return;
              }
              await run(
                "/proposals",
                "POST",
                kind === "body"
                  ? {
                      kind,
                      conceptId: id,
                      expectedRevision: revision,
                      proposedBody: body,
                      reason: f.get("reason"),
                    }
                  : kind === "atom"
                    ? {
                        kind,
                        conceptId: id,
                        proposedAtom: f.get("atom"),
                        reason: f.get("reason"),
                      }
                    : {
                        kind,
                        from: id,
                        type: f.get("type"),
                        target: f.get("target"),
                        confidence: "hypothetical",
                      },
              );
            }}
          >
            <label>
              概念
              <select
                value={id}
                onChange={(e) => {
                  if (!dirty || confirm("切换概念会清除当前编辑，是否继续？")) {
                    setId(e.target.value);
                    setRevision("");
                    setBody("");
                    setDirty(false);
                  }
                }}
              >
                {ids.map((x) => (
                  <option key={x}>{x}</option>
                ))}
              </select>
            </label>
            <label>
              修订类型
              <select
                name="kind"
                value={kind}
                onChange={(e) => {
                  if (
                    !dirty ||
                    confirm("切换修订类型会清除未提交的字段，是否继续？")
                  ) {
                    setKind(e.target.value);
                    setDirty(false);
                  }
                }}
              >
                <option value="body">正文</option>
                <option value="atom">摘要</option>
                <option value="edge">关联</option>
              </select>
            </label>
            {kind === "body" && (
              <>
                <button
                  type="button"
                  disabled={busy}
                  onClick={async () => {
                    if (!discard()) return;
                    const data = await run(
                      `/concepts/${encodeURIComponent(id)}/body`,
                    );
                    if (data) {
                      setBody(data.body);
                      setRevision(data.revision);
                      setDirty(false);
                    }
                  }}
                >
                  读取当前正文
                </button>
                <label>
                  正文
                  <textarea
                    value={body}
                    onChange={(e) => setBody(e.target.value)}
                    rows={10}
                  />
                </label>
              </>
            )}
            {kind === "atom" && field("atom", "新摘要")}
            {kind === "edge" && (
              <>
                <label>
                  关联类型
                  <select name="type">
                    {EDGE_TYPES.map((t) => (
                      <option key={t}>{t}</option>
                    ))}
                  </select>
                </label>
                <label>
                  关联目标
                  <select name="target">
                    {ids
                      .filter((x) => x !== id)
                      .map((x) => (
                        <option key={x}>{x}</option>
                      ))}
                  </select>
                </label>
              </>
            )}
            {field("reason", "修订理由")}
            <button disabled={busy || !id}>提交待审提案</button>
          </form>
        )}
        {tab === "review" && (
          <section>
            <button
              disabled={busy}
              onClick={async () => {
                const data = await run("/proposals");
                if (data)
                  setProposals([...data.edges, ...data.atoms, ...data.bodies]);
              }}
            >
              加载提案列表
            </button>
            <ul className="proposal-list">
              {proposals.map((p) => (
                <li key={p.file}>
                  <button
                    disabled={busy}
                    onClick={async () => {
                      setFile(p.file);
                      setReviewed("");
                      if (
                        await run(`/proposals/${encodeURIComponent(p.file)}`)
                      ) {
                        if (p.status === "pending") setReviewed(p.file);
                      }
                    }}
                  >
                    {p.conceptId ?? p.from} · {p.status}
                    <small>{p.file}</small>
                  </button>
                </li>
              ))}
            </ul>
            <label>
              提案文件名
              <input
                value={file}
                onChange={(e) => {
                  setFile(e.target.value);
                  setReviewed("");
                }}
              />
            </label>
            <button
              disabled={busy || !file}
              onClick={async () => {
                const data = await run(
                  `/proposals/${encodeURIComponent(file)}`,
                );
                if (data?.proposal?.status === "pending") setReviewed(file);
              }}
            >
              查看差异
            </button>
            <button
              disabled={busy || reviewed !== file || !file}
              onClick={() => {
                if (confirm("确认已审阅此提案并合并到当前知识库？"))
                  void run("/proposals/merge", "POST", { file });
              }}
            >
              合并已审阅提案
            </button>
          </section>
        )}
        {tab === "frontier" && (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              const f = new FormData(e.currentTarget);
              void run("/frontier", "POST", {
                concept: f.get("concept"),
                source: f.get("source"),
                reason: f.get("reason"),
                priority: f.get("priority"),
              });
            }}
          >
            <button
              type="button"
              disabled={busy}
              onClick={() => run("/frontier")}
            >
              查看未决问题
            </button>
            {field("concept", "待探索概念")}
            {field("source", "来源")}
            {field("reason", "探索理由")}
            <label>
              优先级
              <select name="priority">
                <option>medium</option>
                <option>high</option>
                <option>low</option>
              </select>
            </label>
            <button disabled={busy}>添加未决问题</button>
          </form>
        )}
        {tab === "checkpoint" && (
          <section>
            <label>
              概念
              <select
                value={id}
                onChange={(e) => {
                  if (discard()) {
                    setId(e.target.value);
                    setCheckpoint("");
                    setDirty(false);
                  }
                }}
              >
                {ids.map((x) => (
                  <option key={x}>{x}</option>
                ))}
              </select>
            </label>
            <button
              disabled={busy}
              onClick={async () => {
                if (!discard()) return;
                const data = await run(
                  `/checkpoints/${encodeURIComponent(id)}`,
                );
                if (data) setCheckpoint(JSON.stringify(data, null, 2));
              }}
            >
              读取最新快照
            </button>
            <button
              disabled={busy}
              onClick={() => {
                if (discard()) {
                  setDirty(true);
                  setCheckpoint(
                    JSON.stringify(
                      {
                        concept: id,
                        session: 1,
                        date: new Date().toISOString().slice(0, 10),
                        established: [],
                        exploredEdges: [],
                        frontier: [],
                        nextEntryPoint: "",
                      },
                      null,
                      2,
                    ),
                  );
                }
              }}
            >
              填写新快照
            </button>
            <label>
              快照内容（JSON，session 指定会话编号）
              <textarea
                rows={16}
                value={checkpoint}
                onChange={(e) => setCheckpoint(e.target.value)}
              />
            </label>
            <button
              disabled={busy || !checkpoint}
              onClick={() => {
                try {
                  const value = JSON.parse(checkpoint);
                  if (confirm("保存此会话编号的快照？已有同编号内容将被替换。"))
                    void run(
                      `/checkpoints/${encodeURIComponent(id)}`,
                      "PUT",
                      value,
                    );
                } catch {
                  setError("快照 JSON 格式无效");
                }
              }}
            >
              保存快照
            </button>
          </section>
        )}
        {tab === "tools" && (
          <section>
            <button disabled={busy} onClick={() => run("/validate?graph=true")}>
              校验知识库
            </button>
            <button
              disabled={busy}
              onClick={() => run("/graph", "PUT", { manifest: true })}
            >
              重建图与索引
            </button>
            <button disabled={busy} onClick={() => run("/manifest")}>
              查看索引
            </button>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const f = new FormData(e.currentTarget);
                void run(`/search?q=${encodeURIComponent(String(f.get("q")))}`);
              }}
            >
              {field("q", "搜索关键词")}
              <button disabled={busy}>搜索</button>
            </form>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const f = new FormData(e.currentTarget);
                void run(
                  `/path?from=${encodeURIComponent(String(f.get("from")))}&to=${encodeURIComponent(String(f.get("to")))}`,
                );
              }}
            >
              {field("from", "路径起点 ID")}
              {field("to", "路径终点 ID")}
              <button disabled={busy}>查询路径</button>
            </form>
          </section>
        )}
      </fieldset>
      {busy && <p role="status">正在处理…</p>}
      {error && (
        <p role="alert" className="operation-error">
          {error}
        </p>
      )}
      {result !== null && <OperationResult value={result} />}
    </aside>
  );
}
