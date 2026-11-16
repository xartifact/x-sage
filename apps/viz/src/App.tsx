import { useEffect, useMemo, useRef, useState } from "react";
import {
  BookOpen,
  Network,
  Search,
  Plus,
  RefreshCw,
  SlidersHorizontal,
  ArrowUpRight,
  AlertCircle,
  Loader2,
  Palette as PaletteIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
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
import { api } from "./api";
import { EDGE_TYPES, LAYERS, type Layer, type EdgeType } from "./types";
import { PALETTES, validPalette, validAppearance, useTheme } from "./theme";
import "./styles.css";

type CatalogEntry = { id: string; name: string; isDefault: boolean };
export default function App() {
  const [entries, setEntries] = useState<CatalogEntry[]>([]),
    [kb, setKb] = useState(""),
    [error, setError] = useState("");
  const [editing, setEditing] = useState(false),
    [attempt, setAttempt] = useState(0),
    [loading, setLoading] = useState(true);
  const { palette, setPalette, appearance, setAppearance } = useTheme();
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    api("", "/knowledge-bases")
      .then((data) => {
        if (!active) return;
        const list = data.knowledgeBases as CatalogEntry[];
        setEntries(list);
        setKb((previous) =>
          list.some((entry) => entry.id === previous)
            ? previous
            : (list.find((entry) => entry.isDefault)?.id ?? list[0]?.id ?? ""),
        );
      })
      .catch((e) => {
        if (active) setError(String(e));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [attempt]);
  return (
    <div className="flex min-h-dvh flex-col lg:h-dvh lg:overflow-hidden">
      <header className="flex shrink-0 flex-wrap items-center justify-between gap-3 border-b bg-card px-4 py-3 md:px-6">
        <div className="flex flex-wrap items-center gap-2 sm:gap-3">
          <div className="flex size-9 items-center justify-center rounded-xl bg-primary text-primary-foreground">
            <Network className="size-5" />
          </div>
          <div className="sm:mr-3">
            <h1 className="text-base font-semibold tracking-tight">
              x-sage{" "}
              <span className="ml-1 font-normal text-muted-foreground">
                贤者
              </span>
            </h1>
            <p className="text-xs text-muted-foreground">让知识彼此连接</p>
          </div>
          <Select
            value={kb}
            onValueChange={setKb}
            disabled={editing || loading || !entries.length}
          >
            <SelectTrigger
              aria-label="选择知识库"
              className="w-32 min-w-28 bg-background sm:w-48 sm:min-w-40"
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
        <div className="flex items-center gap-2">
          <Select
            value={palette}
            onValueChange={(v) => setPalette(validPalette(v))}
          >
            <SelectTrigger
              aria-label="品牌配色"
              className="w-28 border-transparent shadow-none"
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
              className="w-28 border-transparent shadow-none"
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
      {error && (
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
      {!kb ? (
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
          name={entries.find((entry) => entry.id === kb)?.name ?? kb}
          editing={editing}
          setEditing={setEditing}
        />
      )}
    </div>
  );
}
function Workspace({
  kb,
  name,
  editing,
  setEditing,
}: {
  kb: string;
  name: string;
  editing: boolean;
  setEditing: (value: boolean) => void;
}) {
  const { data, loading, error, reload, updated } = useGraphData(kb);
  const [selectedId, setSelectedId] = useState<string | null>(null),
    [editId, setEditId] = useState<string | undefined>();
  const [query, setQuery] = useState(""),
    [showSidebar, setShowSidebar] = useState(false);
  const [layers, setLayers] = useState<Set<Layer>>(() => new Set(LAYERS));
  const [edges, setEdges] = useState<Set<EdgeType>>(() => new Set(EDGE_TYPES));
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "k" && !editing) {
        event.preventDefault();
        input.current?.focus();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [editing]);
  const results = useMemo(() => {
    const q = query.trim().toLocaleLowerCase();
    return (
      data?.nodes.filter(
        (node) =>
          !q ||
          `${node.id} ${node.atom} ${node.triggers.join(" ")}`
            .toLocaleLowerCase()
            .includes(q),
      ) ?? []
    );
  }, [query, data?.nodes]);
  function select(id: string) {
    setSelectedId(id);
    const node = data?.nodes.find((n) => n.id === id);
    if (node && !layers.has(node.layer))
      setLayers((previous) => new Set([...previous, node.layer]));
  }
  function edit(id?: string) {
    setEditId(id);
    setEditing(true);
  }
  const selected = data?.nodes.find((node) => node.id === selectedId) ?? null;
  return (
    <main className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 flex-wrap items-center justify-between gap-3 border-b px-4 py-4 md:px-6">
        <div>
          <div className="flex items-center gap-2">
            <h2 className="font-semibold">{name}</h2>
            <Badge variant="secondary">{data?.nodes.length ?? 0} 个概念</Badge>
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            浏览关联，沉淀定义，持续完善知识。
          </p>
        </div>
        <div className="flex w-full items-center gap-2 sm:w-auto">
          <div className="relative min-w-0 flex-1 sm:w-64">
            <Search className="absolute left-3 top-2.5 size-4 text-muted-foreground" />
            <Input
              ref={input}
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setShowSidebar(true);
              }}
              placeholder="搜索概念或触发词"
              aria-label="搜索概念"
              className="bg-card pl-9"
            />
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
      <div className="flex min-h-9 shrink-0 flex-wrap items-center justify-between gap-2 border-b bg-card px-4 py-2 text-xs text-muted-foreground md:px-6">
        <span className="flex items-center gap-2">
          <span
            className={cn(
              "size-1.5 rounded-full",
              error ? "bg-destructive" : "bg-primary",
            )}
          />
          {error
            ? "同步暂时中断，已保留当前图谱"
            : updated
              ? `最近同步 ${updated} · 自动更新`
              : "正在加载…"}
        </span>
        <div className="flex items-center gap-3">
          <span className="hidden sm:inline">
            点击概念查看详情 · 拖动节点整理视图
          </span>
          <Button
            variant="ghost"
            size="sm"
            className="h-6 text-xs lg:hidden"
            onClick={() => setShowSidebar((x) => !x)}
          >
            <SlidersHorizontal className="size-3.5" />
            {showSidebar ? "收起目录" : "目录与筛选"}
          </Button>
        </div>
      </div>
      {error && (
        <div
          role="alert"
          className="flex items-center gap-2 px-6 py-2 text-sm text-destructive"
        >
          图谱同步失败。
          <Button variant="link" onClick={reload}>
            重试连接
          </Button>
        </div>
      )}
      {loading ? (
        <div className="flex flex-1 items-center justify-center gap-2 p-12 text-muted-foreground">
          <Loader2 className="size-5 animate-spin" />
          正在加载概念图谱…
        </div>
      ) : data ? (
        <div className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[260px_minmax(0,1fr)] xl:grid-cols-[260px_minmax(0,1fr)_340px]">
          <aside
            className={cn(
              "min-h-0 flex-col border-r bg-card lg:flex",
              showSidebar ? "flex" : "hidden",
            )}
            aria-label="概念目录"
          >
            <div className="flex items-center justify-between px-4 pb-2 pt-4">
              <h3 className="text-xs font-semibold tracking-wide text-muted-foreground">
                概念目录
              </h3>
              <span className="text-xs text-muted-foreground">
                {results.length}
              </span>
            </div>
            <div className="max-h-64 min-h-24 overflow-y-auto p-2 lg:max-h-none lg:flex-1">
              {results.length === 0 ? (
                <p className="p-4 text-sm text-muted-foreground">
                  {query
                    ? "没有匹配概念，试试其他关键词。"
                    : "还没有概念，从维护知识开始。"}
                </p>
              ) : (
                results.map((node) => (
                  <button
                    key={node.id}
                    onClick={() => select(node.id)}
                    className={cn(
                      "mb-1 flex w-full items-start gap-2 rounded-md px-3 py-2 text-left transition-colors hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                      selectedId === node.id && "bg-accent",
                    )}
                  >
                    <span className="mt-0.5 shrink-0 rounded border px-1 text-[10px] text-muted-foreground">
                      L{node.layer}
                    </span>
                    <span className="min-w-0">
                      <span className="block truncate text-xs font-medium">
                        {node.id}
                      </span>
                      <span className="mt-1 line-clamp-2 text-xs leading-relaxed text-muted-foreground">
                        {node.atom}
                      </span>
                    </span>
                  </button>
                ))
              )}
            </div>
            <div className="max-h-72 overflow-y-auto border-t">
              <FilterControls
                visibleLayers={layers}
                visibleEdgeTypes={edges}
                layerCounts={data.meta?.layerCounts}
                onToggleLayer={(layer) =>
                  setLayers((previous) => {
                    const next = new Set(previous);
                    if (!next.delete(layer)) next.add(layer);
                    return next;
                  })
                }
                onToggleEdgeType={(type) =>
                  setEdges((previous) => {
                    const next = new Set(previous);
                    if (!next.delete(type)) next.add(type);
                    return next;
                  })
                }
              />
            </div>
          </aside>
          <section
            aria-label="知识图谱"
            className="relative flex min-h-[55vh] min-w-0 flex-col bg-background lg:min-h-0"
          >
            <ConceptGraph
              nodes={data.nodes}
              edges={data.edges}
              visibleLayers={layers}
              visibleEdgeTypes={edges}
              onNodeClick={(node) => setSelectedId(node?.id ?? null)}
              selectedNodeId={selected?.id ?? null}
            />
          </section>
          <section
            className={cn(
              "min-h-0 border-t bg-card lg:col-span-2 xl:col-span-1 xl:border-l xl:border-t-0",
              selected ? "max-h-[70vh] xl:max-h-none" : "hidden xl:block",
            )}
            aria-label="概念详情"
          >
            {selected ? (
              <DetailPanel
                node={selected}
                edges={data.edges}
                allNodes={data.nodes}
                onClose={() => setSelectedId(null)}
                onSelect={select}
                onEdit={edit}
              />
            ) : (
              <div className="flex h-full flex-col items-center justify-center px-8 py-12 text-center">
                <div className="mb-4 rounded-2xl border bg-background p-4">
                  <ArrowUpRight className="size-6 text-muted-foreground" />
                </div>
                <h3 className="font-medium">沿着关联，深入理解</h3>
                <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
                  从图谱或左侧目录选择一个概念，查看定义、依赖与开放问题。
                </p>
                <Button
                  variant="outline"
                  className="mt-6"
                  onClick={() => edit()}
                >
                  记录新的发现
                </Button>
              </div>
            )}
          </section>
        </div>
      ) : null}
      {editing && data && (
        <Workbench
          kb={kb}
          ids={data.nodes.map((node) => node.id)}
          onChange={reload}
          onClose={() => setEditing(false)}
          {...(editId ? { initialConceptId: editId } : {})}
        />
      )}
    </main>
  );
}
