import { MarkdownPreview } from "./MarkdownContent";
import {
  cloneElement,
  isValidElement,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactNode,
  type ReactElement,
} from "react";
import {
  Loader2,
  KeyRound,
  Plus,
  FilePenLine,
  GitPullRequest,
  Compass,
  History,
  Wrench,
  RefreshCw,
  Save,
  Check,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "@/components/ui/dialog";
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogCancel,
  AlertDialogAction,
} from "@/components/ui/alert-dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { api, ApiError } from "./api";
import { EDGE_TYPES, EDGE_LABELS } from "./types";
import OperationResult from "./OperationResult";

interface Props {
  kb: string;
  ids: string[];
  onChange: () => void;
  onClose: () => void;
  initialConceptId?: string;
}
interface Proposal {
  file: string;
  status: string;
  conceptId?: string;
  from?: string;
  target?: string;
}
interface Frontier {
  id: string;
  reason: string;
  priority: string;
}
interface Snapshot {
  session: number;
  date: string;
  established: string[];
  exploredEdges: string[];
  frontier: Frontier[];
  nextEntryPoint?: string;
}
const sections = [
  { id: "create", label: "新增概念", icon: Plus },
  { id: "proposal", label: "修订概念", icon: FilePenLine },
  { id: "review", label: "审阅提案", icon: GitPullRequest },
  { id: "frontier", label: "未决问题", icon: Compass },
  { id: "checkpoint", label: "会话快照", icon: History },
  { id: "tools", label: "查询与校验", icon: Wrench },
];
const newSnapshot = (): Snapshot => ({
  session: 1,
  date: new Date().toISOString().slice(0, 10),
  established: [],
  exploredEdges: [],
  frontier: [],
  nextEntryPoint: "",
});
const split = (value: string) =>
  value
    .split(/[,，]/)
    .map((s) => s.trim())
    .filter(Boolean);
const lines = (value: string) => value.split("\n");
function Field({
  label,
  children,
  hint,
  controlId,
}: {
  label: string;
  children: ReactNode;
  hint?: string;
  controlId?: string;
}) {
  const generatedId = useId();
  const id = controlId ?? generatedId;
  const control =
    !controlId && isValidElement(children)
      ? cloneElement(
          children as ReactElement<{
            id?: string;
            "aria-describedby"?: string;
          }>,
          { id, ...(hint ? { "aria-describedby": `${id}-hint` } : {}) },
        )
      : children;
  return (
    <div className="space-y-2">
      <Label htmlFor={id} className="text-sm">
        {label}
      </Label>
      {control}
      {hint && (
        <p
          id={`${id}-hint`}
          className="text-xs leading-5 text-muted-foreground"
        >
          {hint}
        </p>
      )}
    </div>
  );
}
function Choice({
  label,
  value,
  options,
  onChange,
  disabled = false,
}: {
  label: string;
  value: string;
  options: Array<{ value: string; label: string }>;
  onChange: (value: string) => void;
  disabled?: boolean;
}) {
  const controlId = useId();
  return (
    <Field label={label} controlId={controlId}>
      <Select
        value={value}
        onValueChange={onChange}
        disabled={disabled || !options.length}
      >
        <SelectTrigger id={controlId} aria-label={label} className="w-full">
          <SelectValue placeholder="请选择" />
        </SelectTrigger>
        <SelectContent>
          {options.map((o) => (
            <SelectItem key={o.value} value={o.value}>
              {o.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Field>
  );
}

export default function Workbench({
  kb,
  ids,
  onChange,
  onClose,
  initialConceptId,
}: Props) {
  const [tab, setTab] = useState(initialConceptId ? "proposal" : "create"),
    [token, setToken] = useState(""),
    [busy, setBusy] = useState(false),
    [error, setError] = useState(""),
    [result, setResult] = useState<unknown>(null),
    [dirty, setDirty] = useState(false);
  const [id, setId] = useState(
    initialConceptId && ids.includes(initialConceptId)
      ? initialConceptId
      : (ids[0] ?? ""),
  );
  const [fields, setFields] = useState<Record<string, string>>({
    layer: "2",
    confidence: "hypothetical",
    source: "personal",
    priority: "medium",
    type: "depends-on",
  });
  const [kind, setKind] = useState("body"),
    [body, setBody] = useState(""),
    [revision, setRevision] = useState("");
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [proposals, setProposals] = useState<Proposal[]>([]),
    [listLoaded, setListLoaded] = useState(false),
    [file, setFile] = useState(""),
    [reviewed, setReviewed] = useState("");
  const [confirmation, setConfirmation] = useState<{
    title: string;
    description: string;
    action: string;
  } | null>(null);
  const confirmationResolve = useRef<((answer: boolean) => void) | null>(null),
    inFlight = useRef(false),
    alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      confirmationResolve.current?.(false);
    };
  }, []);
  useEffect(() => {
    if (!ids.includes(id) && !dirty && !busy) {
      setId(ids[0] ?? "");
      setBody("");
      setRevision("");
      setSnapshot(null);
    }
  }, [ids, id, dirty, busy]);
  useEffect(() => {
    if (!dirty) return;
    const guard = (event: BeforeUnloadEvent) => {
      event.preventDefault();
    };
    window.addEventListener("beforeunload", guard);
    return () => window.removeEventListener("beforeunload", guard);
  }, [dirty]);
  function ask(title: string, description: string, action = "继续") {
    return new Promise<boolean>((resolve) => {
      confirmationResolve.current = resolve;
      setConfirmation({ title, description, action });
    });
  }
  function answer(value: boolean) {
    confirmationResolve.current?.(value);
    confirmationResolve.current = null;
    setConfirmation(null);
  }
  async function discard() {
    return (
      !dirty ||
      ask(
        "放弃未保存的修改？",
        "当前输入尚未保存。继续后将清除这些修改。",
        "放弃修改",
      )
    );
  }
  function resetEditor() {
    setFields({
      layer: "2",
      confidence: "hypothetical",
      source: "personal",
      priority: "medium",
      type: "depends-on",
    });
    setBody("");
    setRevision("");
    setSnapshot(null);
    setDirty(false);
    setResult(null);
    setError("");
  }
  async function navigate(next: string) {
    if (next === tab || busy || confirmation) return;
    if (await discard()) {
      resetEditor();
      setTab(next);
    }
  }
  async function close() {
    if (busy || confirmation) return;
    if (await discard()) onClose();
  }
  async function run(
    path: string,
    method = "GET",
    value?: unknown,
    showResult = true,
  ) {
    if (inFlight.current) return null;
    inFlight.current = true;
    setBusy(true);
    setError("");
    try {
      const data = await api(kb, path, method, value, token);
      if (!alive.current) return null;
      if (showResult) setResult(data);
      if (method !== "GET") {
        onChange();
        setDirty(false);
      }
      return data;
    } catch (e) {
      if (alive.current) {
        setError(e instanceof Error ? e.message : String(e));
        if (e instanceof ApiError) setResult(e.payload);
      }
      return null;
    } finally {
      inFlight.current = false;
      if (alive.current) setBusy(false);
    }
  }
  function change(name: string, value: string) {
    setFields((old) => ({ ...old, [name]: value }));
    setDirty(true);
  }
  function input(
    name: string,
    label: string,
    options: {
      required?: boolean;
      multiline?: boolean;
      hint?: string;
      type?: string;
      placeholder?: string;
    } = {},
  ) {
    const props = {
      "aria-label": label,
      value: fields[name] ?? "",
      onChange: (
        e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>,
      ) => change(name, e.target.value),
      required: options.required ?? true,
      placeholder: options.placeholder,
    };
    return (
      <Field label={label} {...(options.hint ? { hint: options.hint } : {})}>
        {options.multiline ? (
          <Textarea
            {...props}
            rows={4}
            {...(name === "atom" ? { maxLength: 200 } : {})}
          />
        ) : (
          <Input
            {...props}
            type={options.type ?? "text"}
            {...(name === "id" ? { pattern: "[a-z0-9][a-z0-9-]*" } : {})}
            {...(name === "atom" ? { maxLength: 200 } : {})}
          />
        )}
      </Field>
    );
  }
  const conceptChoice = (
    <Choice
      label="概念"
      value={id}
      disabled={busy}
      options={ids.map((value) => ({ value, label: value }))}
      onChange={async (next) => {
        if (await discard()) {
          resetEditor();
          setId(next);
        }
      }}
    />
  );
  const submit = (label: string, disabled = false) => (
    <div className="flex justify-end border-t pt-4">
      <Button type="submit" disabled={busy || disabled}>
        {busy ? <Loader2 className="animate-spin" /> : <Save />}
        {label}
      </Button>
    </div>
  );
  const updateSnapshot = (patch: Partial<Snapshot>) => {
    setSnapshot((old) => (old ? { ...old, ...patch } : null));
    setDirty(true);
  };
  async function loadProposals() {
    const data = await run("/proposals", "GET", undefined, false);
    if (data) {
      setProposals([...data.edges, ...data.atoms, ...data.bodies]);
      setListLoaded(true);
    }
  }
  async function inspect(name: string) {
    setFile(name);
    setReviewed("");
    const data = await run(`/proposals/${encodeURIComponent(name)}`);
    if (data?.proposal?.status === "pending" && data.applies !== false)
      setReviewed(name);
  }
  return (
    <>
      <Dialog
        open
        onOpenChange={(open) => {
          if (!open) void close();
        }}
      >
        <DialogContent
          className="flex max-h-[92dvh] w-[calc(100%-1rem)] flex-col gap-0 overflow-hidden p-0 sm:max-w-[880px]"
          onEscapeKeyDown={(e) => {
            if (busy || confirmation) e.preventDefault();
          }}
          onInteractOutside={(e) => {
            e.preventDefault();
          }}
        >
          <DialogHeader className="border-b px-6 py-5 pr-12">
            <DialogTitle className="flex items-center gap-2">
              维护知识库{" "}
              <Badge
                variant="secondary"
                className="max-w-48 truncate font-mono text-xs"
              >
                {kb}
              </Badge>
            </DialogTitle>
            <DialogDescription>
              创建、修订和审阅知识。变更只应用于当前知识库。
            </DialogDescription>
          </DialogHeader>
          <div className="flex min-h-0 flex-1 flex-col sm:flex-row">
            <nav
              aria-label="知识维护操作"
              className="flex shrink-0 gap-1 overflow-x-auto border-b bg-muted/30 p-3 sm:w-40 sm:flex-col sm:border-r sm:border-b-0"
            >
              {sections.map((s) => (
                <Button
                  key={s.id}
                  variant={tab === s.id ? "secondary" : "ghost"}
                  className={cn(
                    "shrink-0 justify-start text-sm",
                    tab === s.id && "text-primary",
                  )}
                  disabled={busy || !!confirmation}
                  aria-current={tab === s.id ? "page" : undefined}
                  onClick={() => void navigate(s.id)}
                >
                  <s.icon className="size-4" />
                  {s.label}
                </Button>
              ))}
            </nav>
            <div className="min-w-0 flex-1 overflow-y-auto px-5 py-5 sm:px-6">
              <details className="mb-5 rounded-lg border px-3 py-2">
                <summary className="flex cursor-pointer items-center gap-2 text-sm text-muted-foreground">
                  <KeyRound className="size-4" />
                  写入凭据 {token && <Badge variant="outline">已填写</Badge>}
                </summary>
                <div className="mt-3">
                  <Input
                    aria-label="写入凭据"
                    type="password"
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                    autoComplete="off"
                    placeholder="服务端启用写保护时填写"
                    disabled={busy}
                  />
                  <p className="mt-2 text-xs text-muted-foreground">
                    仅在当前弹窗内保留，不写入浏览器存储。
                  </p>
                </div>
              </details>
              <fieldset
                disabled={busy || !!confirmation}
                className="min-w-0 space-y-5 border-0 p-0"
              >
                {tab === "create" && (
                  <form
                    className="space-y-5"
                    onSubmit={async (e) => {
                      e.preventDefault();
                      const data = await run("/concepts", "POST", {
                        frontmatter: {
                          id: fields.id,
                          atom: fields.atom,
                          layer: Number(fields.layer),
                          domain: split(fields.domain ?? ""),
                          triggers: split(fields.triggers ?? ""),
                          confidence: fields.confidence,
                          source: fields.source,
                          edges: [],
                        },
                        body: fields.molecule,
                      });
                      if (data) {
                        setFields({
                          layer: "2",
                          confidence: "hypothetical",
                          source: "personal",
                          priority: "medium",
                          type: "depends-on",
                        });
                        setId(data.id);
                      }
                    }}
                  >
                    {input("id", "概念 ID", {
                      hint: "小写英文、数字或连字符；创建后用于引用。",
                      placeholder: "例如 budgeted-concept-activation",
                    })}
                    {input("atom", "一句话摘要", {
                      multiline: true,
                      hint: "最多 200 字符，说明这个概念的独特含义。",
                    })}
                    <div className="grid gap-4 sm:grid-cols-2">
                      <Choice
                        label="概念层级"
                        value={fields.layer ?? "2"}
                        options={[
                          "L0 · 公理",
                          "L1 · 核心",
                          "L2 · 支撑",
                          "L3 · 实例",
                        ].map((label, i) => ({ value: String(i), label }))}
                        onChange={(value) => change("layer", value)}
                        disabled={busy}
                      />
                      <Choice
                        label="置信度"
                        value={fields.confidence ?? "hypothetical"}
                        options={[
                          { value: "hypothetical", label: "待验证" },
                          { value: "confirmed", label: "已确认" },
                          { value: "contested", label: "有争议" },
                        ]}
                        onChange={(value) => change("confidence", value)}
                        disabled={busy}
                      />
                    </div>
                    <div className="grid gap-4 sm:grid-cols-2">
                      {input("domain", "领域", {
                        placeholder: "engineering, product",
                        hint: "多个值用逗号分隔。",
                      })}
                      {input("triggers", "触发词", {
                        placeholder: "上下文, 按需激活",
                        hint: "用于发现与激活此概念。",
                      })}
                    </div>
                    <Field label="正文">
                      <Textarea
                        aria-label="概念正文"
                        required
                        rows={10}
                        value={fields.molecule ?? ""}
                        onChange={(e) => change("molecule", e.target.value)}
                        placeholder="使用 Markdown 描述核心内容、张力与激活建议。"
                      />
                    </Field>
                    <MarkdownPreview content={fields.molecule ?? ""} />
                    <Choice
                      label="知识来源"
                      value={fields.source ?? "personal"}
                      options={[
                        { value: "personal", label: "个人观点" },
                        { value: "domain", label: "领域知识" },
                        { value: "synthesis", label: "综合提炼" },
                      ]}
                      onChange={(value) => change("source", value)}
                      disabled={busy}
                    />
                    {submit("创建概念")}
                  </form>
                )}
                {tab === "proposal" && (
                  <form
                    className="space-y-5"
                    onSubmit={async (e) => {
                      e.preventDefault();
                      if (!ids.includes(id)) {
                        setError("请选择仍存在的概念。");
                        return;
                      }
                      if (kind === "body" && !revision) {
                        setError("请先读取当前正文，建立修订基线。");
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
                              reason: fields.reason,
                            }
                          : kind === "atom"
                            ? {
                                kind,
                                conceptId: id,
                                proposedAtom: fields.atom,
                                reason: fields.reason,
                              }
                            : {
                                kind,
                                from: id,
                                type: fields.type,
                                target: fields.target,
                                confidence: "hypothetical",
                              },
                      );
                    }}
                  >
                    {conceptChoice}
                    <Choice
                      label="修订内容"
                      value={kind}
                      options={[
                        { value: "body", label: "正文" },
                        { value: "atom", label: "摘要" },
                        { value: "edge", label: "概念关联" },
                      ]}
                      onChange={async (next) => {
                        if (await discard()) {
                          resetEditor();
                          setKind(next);
                        }
                      }}
                      disabled={busy}
                    />
                    {kind === "body" && (
                      <>
                        <div className="flex items-center justify-between gap-3">
                          <p className="text-xs text-muted-foreground">
                            先读取最新正文，保存时检查版本冲突。
                          </p>
                          <Button
                            variant="outline"
                            type="button"
                            disabled={busy || !id}
                            onClick={async () => {
                              if (!(await discard())) return;
                              const data = await run(
                                `/concepts/${encodeURIComponent(id)}/body`,
                                "GET",
                                undefined,
                                false,
                              );
                              if (data) {
                                setBody(data.body);
                                setRevision(data.revision);
                                setDirty(false);
                              }
                            }}
                          >
                            <RefreshCw />
                            读取正文
                          </Button>
                        </div>
                        <Textarea
                          aria-label="修订正文"
                          required
                          rows={12}
                          value={body}
                          onChange={(e) => {
                            setBody(e.target.value);
                            setDirty(true);
                          }}
                        />
                        <MarkdownPreview content={body} />
                      </>
                    )}
                    {kind === "atom" &&
                      input("atom", "新的摘要", { multiline: true })}
                    {kind === "edge" && (
                      <div className="grid gap-4 sm:grid-cols-2">
                        <Choice
                          label="关联类型"
                          value={fields.type ?? "depends-on"}
                          options={EDGE_TYPES.map((value) => ({
                            value,
                            label: EDGE_LABELS[value],
                          }))}
                          onChange={(value) => change("type", value)}
                          disabled={busy}
                        />
                        <Choice
                          label="目标概念"
                          value={fields.target ?? ""}
                          options={ids
                            .filter((x) => x !== id)
                            .map((value) => ({ value, label: value }))}
                          onChange={(value) => change("target", value)}
                          disabled={busy}
                        />
                      </div>
                    )}
                    {input("reason", "修订理由", {
                      multiline: true,
                      placeholder: "说明现有内容的问题与本次修改依据。",
                    })}
                    <p className="text-xs text-muted-foreground">
                      提交后进入待审队列，合并后才修改正式概念。
                    </p>
                    {submit(
                      "提交待审提案",
                      !id || (kind === "edge" && !fields.target),
                    )}
                  </form>
                )}
                {tab === "review" && (
                  <section className="space-y-4">
                    <div className="flex items-center justify-between">
                      <h3 className="text-sm font-medium">提案队列</h3>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => void loadProposals()}
                        disabled={busy}
                      >
                        <RefreshCw />
                        加载提案
                      </Button>
                    </div>
                    {!listLoaded && (
                      <p className="rounded-lg border border-dashed p-6 text-center text-sm text-muted-foreground">
                        加载当前知识库的待审与历史提案。
                      </p>
                    )}
                    {listLoaded && !proposals.length && (
                      <p className="p-6 text-center text-sm text-muted-foreground">
                        暂无提案
                      </p>
                    )}
                    <div className="max-h-64 space-y-2 overflow-y-auto">
                      {proposals.map((p) => (
                        <Button
                          key={p.file}
                          variant="outline"
                          className={cn(
                            "h-auto w-full justify-between gap-3 px-3 py-3 text-left",
                            p.file === file && "border-primary bg-accent",
                          )}
                          disabled={busy}
                          onClick={() => void inspect(p.file)}
                        >
                          <span className="min-w-0">
                            <span className="block truncate">
                              {p.conceptId ?? p.from}
                              {p.target ? ` → ${p.target}` : ""}
                            </span>
                            <span className="mt-1 block truncate font-mono text-[11px] text-muted-foreground">
                              {p.file}
                            </span>
                          </span>
                          <Badge
                            variant={
                              p.status === "pending" ? "secondary" : "outline"
                            }
                          >
                            {p.status === "pending"
                              ? "待审"
                              : p.status === "approved"
                                ? "已合并"
                                : p.status}
                          </Badge>
                        </Button>
                      ))}
                    </div>
                    {file && (
                      <div className="flex justify-end">
                        <Button
                          disabled={busy || reviewed !== file}
                          onClick={async () => {
                            if (
                              await ask(
                                "合并此提案？",
                                "请确认已阅读下方差异。合并后，正式概念将使用这份修订内容。",
                                "确认合并",
                              )
                            ) {
                              const data = await run(
                                "/proposals/merge",
                                "POST",
                                { file },
                              );
                              if (data) {
                                setReviewed("");
                                setProposals((old) =>
                                  old.map((p) =>
                                    p.file === file
                                      ? { ...p, status: "approved" }
                                      : p,
                                  ),
                                );
                              }
                            }
                          }}
                        >
                          <Check />
                          合并已审阅提案
                        </Button>
                      </div>
                    )}
                  </section>
                )}
                {tab === "frontier" && (
                  <form
                    className="space-y-5"
                    onSubmit={async (e) => {
                      e.preventDefault();
                      await run("/frontier", "POST", {
                        concept: fields.concept,
                        source: fields.origin,
                        reason: fields.reason,
                        priority: fields.priority,
                      });
                    }}
                  >
                    <Button
                      variant="outline"
                      type="button"
                      onClick={() => void run("/frontier")}
                      disabled={busy}
                    >
                      <Compass />
                      查看未决问题
                    </Button>
                    {input("concept", "待探索概念")}
                    {input("origin", "来源", {
                      placeholder: "文档、讨论或相关概念",
                    })}
                    {input("reason", "为何值得探索", { multiline: true })}
                    <Choice
                      label="优先级"
                      value={fields.priority ?? "medium"}
                      options={[
                        { value: "high", label: "高" },
                        { value: "medium", label: "中" },
                        { value: "low", label: "低" },
                      ]}
                      onChange={(value) => change("priority", value)}
                      disabled={busy}
                    />
                    {submit("添加未决问题")}
                  </form>
                )}
                {tab === "checkpoint" && (
                  <section className="space-y-5">
                    {conceptChoice}
                    <div className="flex flex-wrap gap-2">
                      <Button
                        variant="outline"
                        disabled={busy || !id}
                        onClick={async () => {
                          if (!(await discard())) return;
                          const data = await run(
                            `/checkpoints/${encodeURIComponent(id)}`,
                            "GET",
                            undefined,
                            false,
                          );
                          if (data) {
                            setSnapshot(data);
                            setDirty(false);
                          }
                        }}
                      >
                        <History />
                        读取最新快照
                      </Button>
                      <Button
                        variant="outline"
                        disabled={busy || !id}
                        onClick={async () => {
                          if (await discard()) {
                            setSnapshot(newSnapshot());
                            setDirty(true);
                          }
                        }}
                      >
                        <Plus />
                        填写新快照
                      </Button>
                    </div>
                    {snapshot && (
                      <form
                        className="space-y-5"
                        onSubmit={async (e) => {
                          e.preventDefault();
                          if (
                            await ask(
                              "保存会话快照？",
                              `将保存会话 ${snapshot.session}。同编号快照已存在时，其内容会被替换。`,
                              "保存快照",
                            )
                          )
                            await run(
                              `/checkpoints/${encodeURIComponent(id)}`,
                              "PUT",
                              {
                                ...snapshot,
                                concept: id,
                                established: snapshot.established.filter((s) =>
                                  s.trim(),
                                ),
                                exploredEdges: snapshot.exploredEdges.filter(
                                  (s) => s.trim(),
                                ),
                              },
                            );
                        }}
                      >
                        <div className="grid grid-cols-2 gap-4">
                          <Field label="会话编号">
                            <Input
                              aria-label="会话编号"
                              type="number"
                              min={1}
                              step={1}
                              required
                              value={snapshot.session}
                              onChange={(e) =>
                                updateSnapshot({
                                  session: Number(e.target.value),
                                })
                              }
                            />
                          </Field>
                          <Field label="记录日期">
                            <Input
                              aria-label="记录日期"
                              type="date"
                              required
                              value={snapshot.date}
                              onChange={(e) =>
                                updateSnapshot({ date: e.target.value })
                              }
                            />
                          </Field>
                        </div>
                        <Field
                          label="已建立的结论"
                          hint="每行一条；保留证据与置信度限定。"
                        >
                          <Textarea
                            aria-label="已建立的结论"
                            rows={5}
                            value={snapshot.established.join("\n")}
                            onChange={(e) =>
                              updateSnapshot({
                                established: lines(e.target.value),
                              })
                            }
                          />
                        </Field>
                        <Field label="已探索的关联" hint="每行一条。">
                          <Textarea
                            aria-label="已探索的关联"
                            rows={3}
                            value={snapshot.exploredEdges.join("\n")}
                            onChange={(e) =>
                              updateSnapshot({
                                exploredEdges: lines(e.target.value),
                              })
                            }
                          />
                        </Field>
                        <div className="space-y-3">
                          <div className="flex items-center justify-between">
                            <Label>未决边界</Label>
                            <Button
                              variant="ghost"
                              size="sm"
                              type="button"
                              onClick={() =>
                                updateSnapshot({
                                  frontier: [
                                    ...snapshot.frontier,
                                    { id: "", reason: "", priority: "medium" },
                                  ],
                                })
                              }
                            >
                              <Plus />
                              添加
                            </Button>
                          </div>
                          {snapshot.frontier.map((entry, index) => (
                            <div
                              key={index}
                              className="space-y-3 rounded-lg border p-3"
                            >
                              <Input
                                aria-label={`边界 ${index + 1} 概念`}
                                placeholder="概念 ID"
                                required
                                value={entry.id}
                                onChange={(e) =>
                                  updateSnapshot({
                                    frontier: snapshot.frontier.map((x, i) =>
                                      i === index
                                        ? { ...x, id: e.target.value }
                                        : x,
                                    ),
                                  })
                                }
                              />
                              <Textarea
                                aria-label={`边界 ${index + 1} 理由`}
                                placeholder="待解决的问题"
                                required
                                value={entry.reason}
                                onChange={(e) =>
                                  updateSnapshot({
                                    frontier: snapshot.frontier.map((x, i) =>
                                      i === index
                                        ? { ...x, reason: e.target.value }
                                        : x,
                                    ),
                                  })
                                }
                              />
                              <div className="flex items-end justify-between gap-3">
                                <Choice
                                  label="优先级"
                                  value={entry.priority}
                                  options={[
                                    { value: "high", label: "高" },
                                    { value: "medium", label: "中" },
                                    { value: "low", label: "低" },
                                  ]}
                                  onChange={(value) =>
                                    updateSnapshot({
                                      frontier: snapshot.frontier.map((x, i) =>
                                        i === index
                                          ? { ...x, priority: value }
                                          : x,
                                      ),
                                    })
                                  }
                                  disabled={busy}
                                />
                                <Button
                                  variant="ghost"
                                  type="button"
                                  onClick={() =>
                                    updateSnapshot({
                                      frontier: snapshot.frontier.filter(
                                        (_, i) => i !== index,
                                      ),
                                    })
                                  }
                                >
                                  移除
                                </Button>
                              </div>
                            </div>
                          ))}
                        </div>
                        <Field label="下次进入点">
                          <Input
                            aria-label="下次进入点"
                            value={snapshot.nextEntryPoint ?? ""}
                            onChange={(e) =>
                              updateSnapshot({ nextEntryPoint: e.target.value })
                            }
                          />
                        </Field>
                        {submit("保存快照", !id)}
                      </form>
                    )}
                  </section>
                )}
                {tab === "tools" && (
                  <section className="space-y-6">
                    <div className="flex flex-wrap gap-2">
                      <Button
                        variant="outline"
                        disabled={busy}
                        onClick={() => void run("/validate?graph=true")}
                      >
                        校验知识库
                      </Button>
                      <Button
                        variant="outline"
                        disabled={busy}
                        onClick={() =>
                          void run("/graph", "PUT", { manifest: true })
                        }
                      >
                        重建图与索引
                      </Button>
                      <Button
                        variant="outline"
                        disabled={busy}
                        onClick={() => void run("/manifest")}
                      >
                        查看索引
                      </Button>
                    </div>
                    <form
                      className="space-y-3 border-t pt-5"
                      onSubmit={(e) => {
                        e.preventDefault();
                        void run(
                          `/search?q=${encodeURIComponent(fields.q ?? "")}`,
                        );
                      }}
                    >
                      {input("q", "搜索关键词")}
                      <Button variant="secondary" disabled={busy}>
                        搜索
                      </Button>
                    </form>
                    <form
                      className="space-y-3 border-t pt-5"
                      onSubmit={(e) => {
                        e.preventDefault();
                        void run(
                          `/path?from=${encodeURIComponent(fields.from ?? "")}&to=${encodeURIComponent(fields.to ?? "")}`,
                        );
                      }}
                    >
                      <div className="grid gap-4 sm:grid-cols-2">
                        {input("from", "路径起点 ID")}
                        {input("to", "路径终点 ID")}
                      </div>
                      <Button variant="secondary" disabled={busy}>
                        查询路径
                      </Button>
                    </form>
                  </section>
                )}
              </fieldset>
              {error && (
                <div
                  role="alert"
                  className="mt-5 rounded-lg border border-destructive/40 bg-destructive/5 p-3 text-sm break-words text-destructive"
                >
                  {error}
                </div>
              )}
              {result !== null && (
                <div className="mt-6 border-t pt-5">
                  <OperationResult value={result} />
                </div>
              )}
            </div>
          </div>
          <footer className="flex shrink-0 items-center justify-between border-t bg-muted/20 px-6 py-3 text-xs text-muted-foreground">
            <span role="status" className="flex items-center gap-2">
              {busy ? (
                <>
                  <Loader2 className="size-3 animate-spin" />
                  正在处理…
                </>
              ) : dirty ? (
                "有未保存的修改"
              ) : (
                "所有操作均针对当前知识库"
              )}
            </span>
            <Button
              variant="ghost"
              size="sm"
              disabled={busy}
              onClick={() => void close()}
            >
              关闭
            </Button>
          </footer>
        </DialogContent>
      </Dialog>
      <AlertDialog
        open={!!confirmation}
        onOpenChange={(open) => {
          if (!open) answer(false);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{confirmation?.title}</AlertDialogTitle>
            <AlertDialogDescription>
              {confirmation?.description}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel onClick={() => answer(false)}>
              取消
            </AlertDialogCancel>
            <AlertDialogAction onClick={() => answer(true)}>
              {confirmation?.action ?? "继续"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
