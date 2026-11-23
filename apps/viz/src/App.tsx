import { useEffect, useMemo, useRef, useState } from "react";
import {
  BookOpen,
  Network,
  Search,
  Plus,
  RefreshCw,
  SlidersHorizontal,
  ArrowUpRight,
  ChevronRight,
  CircleHelp,
  ListTree,
  PanelRight,
  X,
  AlertCircle,
  Loader2,
  Palette as PaletteIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { useGraphData } from "./useGraphData";
import ConceptGraph from "./ConceptGraph";
import FilterControls from "./FilterControls";
import DetailPanel from "./DetailPanel";
import Workbench from "./Workbench";
import { ApiError, api, fetchBuildIdentity, type BuildIdentity } from "./api";
import {
  conceptName,
  EDGE_TYPES,
  LAYERS,
  LAYER_COLORS,
  LAYER_LABELS,
  type Layer,
  type EdgeType,
} from "./types";
import { PALETTES, validPalette, validAppearance, useTheme } from "./theme";
import "./styles.css";

type CatalogEntry = { id: string; name: string; isDefault: boolean };
export default function App() {
  const [entries, setEntries] = useState<CatalogEntry[]>([]),
    [kb, setKb] = useState(""),
    [error, setError] = useState("");
  const [readToken, setReadToken] = useState("");
  const [credentialDraft, setCredentialDraft] = useState("");
  const [authRequired, setAuthRequired] = useState(false);
  const [editing, setEditing] = useState(false),
    [attempt, setAttempt] = useState(0),
    [loading, setLoading] = useState(true);
  const [build, setBuild] = useState<BuildIdentity | null>(null);
  const { palette, setPalette, appearance, setAppearance } = useTheme();
  useEffect(() => {
    const controller = new AbortController();
    fetchBuildIdentity(controller.signal)
      .then((identity) => {
        if (!controller.signal.aborted) setBuild(identity);
      })
      .catch(() => {
        if (!controller.signal.aborted) setBuild(null);
      });
    return () => controller.abort();
  }, [attempt]);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    api("", "/knowledge-bases", "GET", undefined, readToken)
      .then((data) => {
        if (!active) return;
        const list = data.knowledgeBases as CatalogEntry[];
        setEntries(list);
        setAuthRequired(false);
        setKb((previous) =>
          list.some((entry) => entry.id === previous)
            ? previous
            : (list.find((entry) => entry.isDefault)?.id ?? list[0]?.id ?? ""),
        );
      })
      .catch((e) => {
        if (!active) return;
        if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
          setAuthRequired(true);
          setEntries([]);
          setKb("");
          if (readToken) setError("凭据无效或没有读取权限。");
        } else {
          setError(String(e));
        }
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [attempt, readToken]);
  return (
    <div className="sage-shell">
      <header className="sage-topbar">
        <div className="sage-brand-group">
          <div className="sage-brand">
            <span className="sage-brand-icon"><Network aria-hidden="true" /></span>
            <h1>x-sage</h1>
          </div>
          <span className="sage-header-divider" aria-hidden="true" />

          <Select
            value={kb}
            onValueChange={setKb}
            disabled={editing || loading || !entries.length}
          >
            <SelectTrigger
              aria-label="选择知识库"
              className="sage-kb-select"
            >
              <BookOpen className="size-4 text-muted-foreground" />
              <SelectValue placeholder="选择知识库" />
            </SelectTrigger>
            <SelectContent>
              {entries.map((entry) => (
                <SelectItem key={entry.id} value={entry.id}>
                  {entry.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            variant="ghost"
            size="icon"
            disabled={editing || loading}
            onClick={() => setAttempt((x) => x + 1)}
            aria-label="刷新知识库列表"
            title="刷新知识库列表"
          >
            <RefreshCw className={cn("size-4", loading && "animate-spin")} />
          </Button>
        </div>
        <div className="sage-preferences">
          {build && (
            <div role="group" aria-label="运行版本" className="inline-flex items-center gap-1.5 whitespace-nowrap px-2 text-xs text-muted-foreground">
              <span>版本 {build.version}</span>
              <code title={build.commitHash}>{build.commitHash === "unknown" ? "未标记" : build.commitHash.slice(0, 7)}</code>
            </div>
          )}
          {readToken && !authRequired && (
            <Button
              variant="ghost"
              disabled={editing}
              onClick={() => {
                setReadToken("");
                setEntries([]);
                setKb("");
              }}
            >
              清除读取凭据
            </Button>
          )}
          <Select
            value={palette}
            onValueChange={(v) => setPalette(validPalette(v))}
          >
            <SelectTrigger
              aria-label="品牌配色"
              className="sage-preference-select"
            >
              <PaletteIcon className="size-4" />
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {Object.entries(PALETTES).map(([key, label]) => (
                <SelectItem key={key} value={key}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={appearance}
            onValueChange={(v) => setAppearance(validAppearance(v))}
          >
            <SelectTrigger
              aria-label="外观"
              className="sage-preference-select"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="system">跟随系统</SelectItem>
              <SelectItem value="light">浅色</SelectItem>
              <SelectItem value="dark">深色</SelectItem>
            </SelectContent>
          </Select>
        </div>
      </header>
      {error && !authRequired && (
        <div
          role="alert"
          className="flex items-center gap-2 border-b px-6 py-2 text-destructive"
        >
          <AlertCircle className="size-4" />
          知识库列表暂不可用。
          <Button variant="link" onClick={() => setAttempt((x) => x + 1)}>
            重试
          </Button>
          <details className="ml-auto text-xs">
            <summary>错误详情</summary>
            {error}
          </details>
        </div>
      )}
      {authRequired ? (
        <div className="flex flex-1 items-center justify-center px-6 py-12">
          <form
            className="w-full max-w-sm space-y-4 rounded-xl border bg-card p-6"
            onSubmit={(event) => {
              event.preventDefault();
              const token = credentialDraft.trim();
              if (!token) return;
              setReadToken(token);
              setCredentialDraft("");
              setAttempt((value) => value + 1);
            }}
          >
            <h2 className="text-base font-semibold">连接知识库</h2>
            <div className="space-y-2">
              <label className="text-sm" htmlFor="read-token">读取令牌</label>
              <Input
                id="read-token"
                type="password"
                autoComplete="off"
                value={credentialDraft}
                onChange={(event) => setCredentialDraft(event.target.value)}
                aria-invalid={Boolean(error)}
                aria-describedby={error ? "read-token-error" : undefined}
              />
              {error && <p id="read-token-error" role="alert" className="text-sm text-destructive">{error}</p>}
            </div>
            <Button type="submit" disabled={loading || !credentialDraft.trim()} className="w-full">
              {loading ? "正在连接…" : "连接"}
            </Button>
          </form>
        </div>
      ) : !kb ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 p-12 text-muted-foreground">
          {loading ? (
            <Loader2 className="size-6 animate-spin" />
          ) : (
            <BookOpen className="size-8" />
          )}
          <p>{loading ? "正在连接知识库…" : "暂无可用知识库"}</p>
          {!loading && (
            <p className="text-xs">配置好知识库后，刷新顶部列表即可开始。</p>
          )}
        </div>
      ) : (
        <Workspace
          key={kb}
          kb={kb}
          readToken={readToken}
          name={entries.find((entry) => entry.id === kb)?.name ?? kb}
          editing={editing}
          setEditing={setEditing}
        />
      )}
    </div>
  );
}
type WorkspacePane = "directory" | "graph" | "detail";

function Workspace({
  kb,
  readToken,
  name,
  editing,
  setEditing,
}: {
  kb: string;
  readToken: string;
  name: string;
  editing: boolean;
  setEditing: (value: boolean) => void;
}) {
  const { data, loading, error, reload, updated } = useGraphData(kb, readToken);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [editId, setEditId] = useState<string | undefined>();
  const [query, setQuery] = useState("");
  const [pane, setPane] = useState<WorkspacePane>("graph");
  const [directoryMode, setDirectoryMode] = useState<"list" | "filters">("list");
  const [layers, setLayers] = useState<Set<Layer>>(() => new Set(LAYERS));
  const [edges, setEdges] = useState<Set<EdgeType>>(() => new Set(EDGE_TYPES));
  const input = useRef<HTMLInputElement>(null);
  const searchRequested = useRef(false);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "k" && !editing) {
        event.preventDefault();
        setPane("directory");
        if (input.current?.getClientRects().length) input.current.focus();
        else searchRequested.current = true;
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [editing]);

  useEffect(() => {
    if (pane === "directory" && searchRequested.current && input.current) {
      input.current.focus();
      searchRequested.current = false;
    }
  }, [pane, data]);

  const results = useMemo(() => {
    const q = query.trim().toLocaleLowerCase();
    return data?.nodes.filter((node) =>
      !q ||
      [conceptName(node), node.id, node.atom, ...node.triggers]
        .join(" ")
        .toLocaleLowerCase()
        .includes(q),
    ) ?? [];
  }, [query, data?.nodes]);

  const selected = data?.nodes.find((node) => node.id === selectedId) ?? null;
  const filtered = layers.size !== LAYERS.length || edges.size !== EDGE_TYPES.length;
  const status = error
    ? data ? "同步中断 · 已保留图谱" : "图谱连接失败"
    : updated ? `已同步 ${updated}` : "正在连接";

  function select(id: string) {
    setSelectedId(id);
    setPane("detail");
    const node = data?.nodes.find((entry) => entry.id === id);
    if (node && !layers.has(node.layer)) {
      setLayers((previous) => new Set([...previous, node.layer]));
    }
  }

  function closeDetail() {
    setSelectedId(null);
    setPane("graph");
  }

  function edit(id?: string) {
    setEditId(id);
    setEditing(true);
  }

  return (
    <main className="sage-workspace" id="workspace">
      <div className="sage-page-heading">
        <div className="min-w-0">
          <div className="sage-crumb">
            <BookOpen size={15} aria-hidden="true" />
            <span>知识库</span><ChevronRight size={12} aria-hidden="true" /><span>{name}</span>
          </div>
          <h2>知识工作台</h2>
        </div>
        <div className="sage-heading-actions">
          <div className={cn("sage-sync", error && "sage-sync-error")}>
            <span className="sage-status-dot" aria-hidden="true" />
            {status}
          </div>
          <Button
            variant="outline"
            size="icon"
            onClick={reload}
            aria-label="刷新图谱"
            title="刷新图谱"
          >
            <RefreshCw className="size-4" />
          </Button>
          <Button onClick={() => edit()} disabled={!data}>
            <Plus className="size-4" />
            维护知识
          </Button>
        </div>
      </div>
      {error && (
        <div role="alert" className="sage-graph-error">
          <AlertCircle className="size-4" />
          图谱同步失败。
          <Button variant="link" onClick={reload}>重试连接</Button>
        </div>
      )}
      {loading ? (
        <div className="sage-workspace-empty" role="status">
          <Loader2 className="size-5 animate-spin" />
          正在加载概念图谱…
        </div>
      ) : data ? (
        <div className="sage-workbench" data-pane={pane}>
          <nav className="sage-pane-nav" aria-label="工作区视图">
            <Button
              variant="ghost"
              aria-current={pane === "directory" ? "page" : undefined}
              onClick={() => setPane("directory")}
            >
              <ListTree />目录
            </Button>
            <Button
              variant="ghost"
              aria-current={pane === "graph" ? "page" : undefined}
              onClick={() => setPane("graph")}
            >
              <Network />图谱
            </Button>
            <Button
              variant="ghost"
              aria-current={pane === "detail" ? "page" : undefined}
              disabled={!selected}
              onClick={() => setPane("detail")}
            >
              <PanelRight />详情
            </Button>
          </nav>

          <aside className="sage-directory" aria-label="概念目录">
            <header className="sage-directory-head">
              <div className="sage-pane-heading">
                <ListTree aria-hidden="true" />
                <h3>概念目录</h3>
                <span aria-label={`${results.length} 个概念`}>{results.length}</span>
              </div>
              <div className="sage-search">
                <Search aria-hidden="true" />
                <Input
                  ref={input}
                  value={query}
                  onChange={(event) => {
                    setQuery(event.target.value);
                    setDirectoryMode("list");
                  }}
                  placeholder="查找概念、摘要或触发词"
                  aria-label="搜索概念"
                />
                {query ? (
                  <Button
                    variant="ghost"
                    size="icon"
                    className="sage-search-clear"
                    aria-label="清除搜索"
                    onClick={() => {
                      setQuery("");
                      input.current?.focus();
                    }}
                  ><X /></Button>
                ) : <kbd aria-hidden="true">⌘/Ctrl K</kbd>}
              </div>
              <nav className="sage-directory-tabs" aria-label="目录与筛选">
                <Button
                  variant="ghost"
                  aria-current={directoryMode === "list" ? "page" : undefined}
                  onClick={() => setDirectoryMode("list")}
                >全部概念</Button>
                <Button
                  variant="ghost"
                  aria-current={directoryMode === "filters" ? "page" : undefined}
                  onClick={() => setDirectoryMode("filters")}
                ><SlidersHorizontal />筛选{filtered && " · 已应用"}</Button>
              </nav>
            </header>
            <div className="sage-directory-scroll">
              {directoryMode === "filters" ? (
                <FilterControls
                  visibleLayers={layers}
                  visibleEdgeTypes={edges}
                  layerCounts={data.meta?.layerCounts}
                  onToggleLayer={(layer) => setLayers((previous) => {
                    const next = new Set(previous);
                    if (!next.delete(layer)) next.add(layer);
                    return next;
                  })}
                  onToggleEdgeType={(type) => setEdges((previous) => {
                    const next = new Set(previous);
                    if (!next.delete(type)) next.add(type);
                    return next;
                  })}
                />
              ) : results.length === 0 ? (
                <p className="sage-directory-empty">
                  {query ? "没有匹配的概念。试试其他关键词。" : "还没有概念。可在维护知识中新增。"}
                </p>
              ) : (
                <ul className="sage-concept-list">
                  {results.map((node) => (
                    <li key={node.id}>
                      <button
                        onClick={() => select(node.id)}
                        className="sage-concept"
                        aria-pressed={selectedId === node.id}
                      >
                        <span className="sage-layer-code" style={{ borderColor: LAYER_COLORS[node.layer] }}>L{node.layer}</span>
                        <span className="sage-concept-copy">
                          <span className="sage-concept-name">{conceptName(node)}</span>
                          <span className="sage-concept-summary">{node.atom}</span>
                        </span>
                        <ChevronRight aria-hidden="true" />
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </aside>

          <section className="sage-graph-pane" aria-label="知识图谱">
            <header className="sage-graph-header">
              <div className="sage-pane-heading">
                <Network aria-hidden="true" />
                <h3>知识图谱</h3>
              </div>
              <div className="sage-graph-counts">
                <span><strong>{data.nodes.length}</strong> 个概念</span>
                <span><strong>{data.meta?.assertedEdgeCount ?? data.edges.filter((edge) => !edge.mirrored).length}</strong> 条关系</span>
              </div>
            </header>
            <div className="sage-canvas">
              <ConceptGraph
                nodes={data.nodes}
                edges={data.edges}
                visibleLayers={layers}
                visibleEdgeTypes={edges}
                onNodeClick={(node) => node ? select(node.id) : closeDetail()}
                selectedNodeId={selected?.id ?? null}
              />
            </div>
            <footer className="sage-graph-footer">
              <div className="sage-layer-legend" aria-label="概念层级图例">
                {LAYERS.map((layer) => (
                  <span key={layer} className={cn(!layers.has(layer) && "sage-layer-hidden")}>
                    <i style={{ background: LAYER_COLORS[layer] }} aria-hidden="true" />
                    {LAYER_LABELS[layer]}
                  </span>
                ))}
              </div>
              <span className="sage-graph-help" title="点击节点查看详情，拖动节点调整位置，在画布上捏合缩放。">
                <CircleHelp aria-hidden="true" />拖动节点 · 捏合缩放
              </span>
            </footer>
          </section>

          <div className="sage-inspector" data-selected={Boolean(selected)}>
            {selected ? (
              <DetailPanel
                node={selected}
                edges={data.edges}
                allNodes={data.nodes}
                onClose={closeDetail}
                onSelect={select}
                onEdit={edit}
              />
            ) : (
              <section aria-label="概念详情" className="sage-inspector-empty">
                <div className="sage-pane-heading"><PanelRight aria-hidden="true" /><h3>概念详情</h3></div>
                <div className="sage-inspector-guide">
                  <span><ArrowUpRight aria-hidden="true" /></span>
                  <h4>选择一个概念</h4>
                  <p>从目录或图谱选择节点，查看摘要、关联和原文。</p>
                </div>
              </section>
            )}
          </div>
        </div>
      ) : null}
      {editing && data && (
        <Workbench
          kb={kb}
          readToken={readToken}
          ids={data.nodes.map((node) => node.id)}
          onChange={reload}
          onClose={() => setEditing(false)}
          {...(editId ? { initialConceptId: editId } : {})}
        />
      )}
    </main>
  );
}
