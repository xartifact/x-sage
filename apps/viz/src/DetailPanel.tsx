import MarkdownContent from "./MarkdownContent";
import { X, Pencil, ArrowRight, ArrowLeft, ArrowLeftRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { conceptName, LAYER_LABELS, EDGE_LABELS } from "./types";
import type { GraphNode, GraphEdge } from "./types";
import { domainName } from "./graph-browser";

interface DetailPanelProps {
  node: GraphNode | null;
  edges: GraphEdge[];
  allNodes: GraphNode[];
  onClose: () => void;
  onSelect?: (id: string) => void;
  onEdit?: (id: string) => void;
}

export default function DetailPanel({
  node,
  edges,
  allNodes,
  onClose,
  onSelect,
  onEdit,
}: DetailPanelProps) {
  if (!node) return null;
  const connected = edges.filter(
    (e) => e.from === node.id || e.target === node.id,
  );
  // Symmetric API relationships may contain both directions; show one connection.
  const seen = new Set<string>();
  const links = connected.filter((edge) => {
    const other = edge.from === node.id ? edge.target : edge.from;
    const symmetric =
      edge.type === "contradicts" || edge.type === "analogous-to";
    const key = symmetric
      ? `${edge.type}:${other}`
      : `${edge.from}:${edge.type}:${edge.target}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  return (
    <aside
      aria-label="概念详情"
      className="sage-detail"
    >
      <header className="sage-detail-header">
        <div className="min-w-0 flex-1">
          <h2 className="sage-detail-title">
            {conceptName(node)}
          </h2>
          <p className="mt-1 break-all font-mono text-xs text-muted-foreground">
            {node.id}
          </p>
        </div>
        <Button
          variant="ghost"
          size="icon"
          onClick={onClose}
          aria-label="关闭概念详情"
        >
          <X className="size-4" />
        </Button>
      </header>
      <div className="sage-detail-scroll space-y-5">
        <section className="space-y-3">
          <div className="flex flex-wrap gap-1.5">
            <Badge variant="secondary">{LAYER_LABELS[node.layer]}</Badge>
            <Badge variant="outline">{node.confidence}</Badge>
            <Badge variant="outline">{node.source}</Badge>
          </div>
          <p className="sage-atom">{node.atom}</p>
          {onEdit && (
            <Button variant="outline" size="sm" onClick={() => onEdit(node.id)}>
              <Pencil className="size-3.5" />
              修订概念
            </Button>
          )}
        </section>
        <Separator />
        <section className="space-y-3">
          <h3 className="text-sm font-semibold">
            领域与触发词
          </h3>
          <div className="flex flex-wrap gap-1.5">
            {[...new Set(node.domain)].map((d) => (
              <Badge key={`domain-${d}`} variant="secondary" title={d}>
                {domainName(d)}
              </Badge>
            ))}
            {node.triggers.map((t) => (
              <Badge key={`trigger-${t}`} variant="outline">
                {t}
              </Badge>
            ))}
          </div>
        </section>
        <Separator />
        <section className="space-y-2">
          <h3 className="text-sm font-semibold">
            关联概念 · {links.length}
          </h3>
          {links.length === 0 && (
            <p className="text-sm text-muted-foreground">
              尚未建立关联。可通过修订概念添加。
            </p>
          )}
          {links.map((edge) => {
            const out = edge.from === node.id,
              other = out ? edge.target : edge.from,
              target = allNodes.find((n) => n.id === other),
              symmetric =
                edge.type === "contradicts" || edge.type === "analogous-to";
            const Icon = symmetric
              ? ArrowLeftRight
              : out
                ? ArrowRight
                : ArrowLeft;
            return (
              <Button
                key={`${edge.from}:${edge.type}:${edge.target}`}
                variant="ghost"
                className="h-auto w-full justify-start gap-2 whitespace-normal px-2 py-2 text-left"
                disabled={!target || !onSelect}
                onClick={() => onSelect?.(other)}
              >
                <Icon className="size-3.5 shrink-0 text-muted-foreground" />
                <span className="min-w-0">
                  <span className="block text-xs font-normal text-muted-foreground">
                    {EDGE_LABELS[edge.type]}
                  </span>
                  <span className="block break-words text-sm">
                    {target ? conceptName(target) : other}
                  </span>
                </span>
              </Button>
            );
          })}
        </section>
        <Separator />
        <section className="space-y-3">
          <h3 className="text-sm font-semibold">
            完整概念
          </h3>
          <MarkdownContent content={node.molecule} />
        </section>
      </div>
    </aside>
  );
}
