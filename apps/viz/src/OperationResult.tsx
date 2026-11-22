import MarkdownContent, { MarkdownPreview } from "./MarkdownContent";
import { CheckCircle2, AlertCircle, FileText } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

const object = (value: unknown): Record<string, unknown> =>
  value && typeof value === "object" ? (value as Record<string, unknown>) : {};
function Raw({ value }: { value: unknown }) {
  return (
    <details className="mt-3 text-xs text-muted-foreground">
      <summary className="cursor-pointer">查看完整数据</summary>
      <pre className="mt-2 max-h-72 overflow-auto rounded-md bg-muted p-3 text-xs leading-5 whitespace-pre-wrap break-words">
        {JSON.stringify(value, null, 2)}
      </pre>
    </details>
  );
}
function Notice({
  children,
  warning = false,
}: {
  children: React.ReactNode;
  warning?: boolean;
}) {
  return (
    <div
      role="status"
      className={cn(
        "flex items-start gap-2 rounded-lg border bg-muted/30 p-3 text-sm",
        warning && "border-destructive/30 text-destructive",
      )}
    >
      {warning ? (
        <AlertCircle className="mt-0.5 size-4 shrink-0" />
      ) : (
        <CheckCircle2 className="mt-0.5 size-4 shrink-0" />
      )}
      <div className="min-w-0 break-words">{children}</div>
    </div>
  );
}
export default function OperationResult({ value }: { value: unknown }) {
  if (!value || typeof value !== "object")
    return <p className="text-sm text-muted-foreground">{String(value)}</p>;
  const result = object(value);
  if (result.recorded && typeof object(result.recorded).commitVersion === "string") {
    const statuses: Record<string, string> = {
      PROPOSAL_STATUS_APPROVED: "提案已合并",
      PROPOSAL_STATUS_REJECTED: "提案已拒绝",
      PROPOSAL_STATUS_CONFLICTED: "提案基线已过期，未应用修改",
    };
    const message = statuses[String(result.status)] ?? "请求已提交";
    return (
      <Notice warning={result.status === "PROPOSAL_STATUS_CONFLICTED"}>
        {message}（版本 {String(object(result.recorded).commitVersion)}）
      </Notice>
    );
  }
  if (typeof result.diff === "string")
    return (
      <section className="space-y-3">
        <h3 className="flex items-center gap-2 text-sm font-semibold">
          <FileText className="size-4" />
          正文差异
        </h3>
        {result.applies === false && (
          <Notice warning>
            概念已更新，当前提案的基线已过期。请重新读取正文并提交。
          </Notice>
        )}
        <div className="max-h-96 overflow-auto rounded-lg border bg-muted/20 font-mono text-xs leading-6">
          {result.diff ? (
            result.diff.split("\n").map((line, index) => (
              <div
                key={index}
                className={cn(
                  "min-w-fit px-3 whitespace-pre",
                  line.startsWith("+")
                    ? "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                    : line.startsWith("-")
                      ? "bg-red-500/10 text-red-700 dark:text-red-300"
                      : line.startsWith("@@")
                        ? "bg-primary/10 text-primary"
                        : "text-muted-foreground",
                )}
              >
                {line || " "}
              </div>
            ))
          ) : (
            <p className="p-4">正文没有变化</p>
          )}
        </div>
        {typeof object(result.proposal).proposedBody === "string" && (
          <MarkdownPreview content={String(object(result.proposal).proposedBody)} />
        )}
      </section>
    );
  if (result.proposal) {
    const proposal = object(result.proposal);
    return (
      <section className="space-y-3">
        <h3 className="text-sm font-semibold">
          {result.kind === "atom" ? "摘要修订" : "关联提案"}
        </h3>
        {result.kind === "atom" ? (
          <>
            <div className="rounded-lg border p-3">
              <Badge variant="outline">当前摘要</Badge>
              <p className="mt-2 text-sm leading-6">
                {String(proposal.currentAtom ?? "")}
              </p>
            </div>
            <div className="rounded-lg border border-primary/30 bg-primary/5 p-3">
              <Badge variant="secondary">提议摘要</Badge>
              <p className="mt-2 text-sm leading-6">
                {String(proposal.proposedAtom ?? "")}
              </p>
            </div>
          </>
        ) : (
          <p className="rounded-lg border p-4 text-sm break-words">
            {String(proposal.from)}{" "}
            <span className="mx-2 text-muted-foreground">
              {String(proposal.type)} →
            </span>{" "}
            {String(proposal.target)}
          </p>
        )}
        <Raw value={value} />
      </section>
    );
  }
  if (typeof result.markdown === "string")
    return (
      <section>
        <h3 className="mb-3 text-sm font-semibold">知识库索引</h3>
        <div className="max-h-96 min-w-0 overflow-y-auto rounded-lg border p-4">
          <MarkdownContent content={result.markdown} />
        </div>
      </section>
    );
  if (
    Array.isArray(value) &&
    value.every((row) => row && typeof row === "object" && "concept" in row)
  )
    return (
      <section className="space-y-3">
        <h3 className="text-sm font-semibold">
          未决问题 <Badge variant="secondary">{value.length}</Badge>
        </h3>
        {!value.length && (
          <p className="text-sm text-muted-foreground">当前没有未决问题。</p>
        )}
        {value.map((value, index) => {
          const row = object(value);
          return (
            <article key={index} className="space-y-2 rounded-lg border p-4">
              <div className="flex items-center justify-between gap-3">
                <h4 className="text-sm font-medium">{String(row.concept)}</h4>
                <Badge variant="outline">
                  {(
                    { high: "高", medium: "中", low: "低" } as Record<
                      string,
                      string
                    >
                  )[String(row.priority)] ?? String(row.priority)}
                </Badge>
              </div>
              <p className="text-sm leading-6 text-muted-foreground">
                {String(row.reason)}
              </p>
              <p className="text-xs text-muted-foreground">
                来源：{String(row.source)}
              </p>
            </article>
          );
        })}
      </section>
    );
  if (typeof result.id === "string")
    return (
      <div className="space-y-2">
        <Notice>
          已创建概念 <strong className="font-mono">{result.id}</strong>
          ，图谱会自动更新。
        </Notice>
        {typeof result.warning === "string" && (
          <Notice warning>{result.warning}</Notice>
        )}
      </div>
    );
  if (typeof result.file === "string")
    return (
      <Notice>
        提案已保存。请在“审阅提案”查看差异并合并。
        <span className="mt-1 block font-mono text-xs text-muted-foreground">
          {result.file}
        </span>
      </Notice>
    );
  if (result.ok === true) return <Notice>保存成功</Notice>;
  if (typeof result.valid === "boolean")
    return (
      <div>
        <Notice warning={!result.valid}>
          {result.valid
            ? "知识库校验通过"
            : "发现需要处理的问题，请查看校验详情。"}
        </Notice>
        <Raw value={value} />
      </div>
    );
  if (Array.isArray(result.merged))
    return (
      <div>
        <Notice
          warning={Boolean(
            (result.failed as unknown[] | undefined)?.length ||
            (result.conflicts as unknown[] | undefined)?.length,
          )}
        >
          已合并 {result.merged.length} 项提案。
        </Notice>
        <Raw value={value} />
      </div>
    );
  if (typeof result.nodes === "number" && typeof result.edges === "number")
    return (
      <Notice>
        已重建 {result.nodes} 个概念、{result.edges} 条关联
        {result.manifest ? "及原子索引" : ""}。
      </Notice>
    );
  if (Array.isArray(result.results))
    return (
      <section className="space-y-3">
        <h3 className="text-sm font-semibold">
          搜索结果 · {result.results.length}
        </h3>
        {result.results.map((value, index) => {
          const row = object(value);
          return (
            <article className="rounded-lg border p-3" key={index}>
              <h4 className="font-mono text-sm">{String(row.id ?? "")}</h4>
              <p className="mt-1 text-sm text-muted-foreground">
                {String(row.atom ?? "")}
              </p>
            </article>
          );
        })}
      </section>
    );
  if (Array.isArray(result.path))
    return (
      <section>
        <h3 className="text-sm font-semibold">概念路径</h3>
        <p className="mt-3 rounded-lg border p-3 font-mono text-sm break-words">
          {result.path
            .map((value) =>
              typeof value === "string"
                ? value
                : String(object(value).id ?? JSON.stringify(value)),
            )
            .join(" → ") || "未找到连接路径"}
        </p>
        <Raw value={value} />
      </section>
    );
  if (typeof result.concept === "string" && typeof result.reason === "string")
    return <Notice>已添加未决问题：{result.concept}</Notice>;
  return (
    <section>
      <p className="text-sm text-muted-foreground">操作已返回结果。</p>
      <Raw value={value} />
    </section>
  );
}
