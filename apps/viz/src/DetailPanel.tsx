import { LAYER_COLORS, LAYER_LABELS, EDGE_LABELS, EDGE_COLORS } from "./types";
import type { GraphNode, GraphEdge, EdgeType } from "./types";

interface DetailPanelProps {
  node: GraphNode | null;
  edges: GraphEdge[];
  allNodes: GraphNode[];
  onClose: () => void;
}

function getChineseName(node: GraphNode): string {
  // Extract first heading from molecule as Chinese name
  const match = node.molecule.match(/^#\s+(.+?)\s/);
  if (match && match[1]) return match[1];
  return node.id;
}

function getEdgeTypeClass(type: EdgeType): string {
  switch (type) {
    case "enables": return "edge-type-enables";
    case "depends-on": return "edge-type-depends-on";
    case "contradicts": return "edge-type-contradicts";
    case "part-of": return "edge-type-part-of";
    default: return "";
  }
}

function renderMoleculeAsHtml(molecule: string): string {
  // Simple markdown-to-HTML conversion for molecule display
  return molecule
    .replace(/^#\s+(.+)$/gm, "<h1>$1</h1>")
    .replace(/^##\s+(.+)$/gm, "<h2>$1</h2>")
    .replace(/^###\s+(.+)$/gm, "<h3>$1</h3>")
    .replace(/^####\s+(.+)$/gm, "<h4>$1</h4>")
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/\*(.+?)\*/g, "<em>$1</em>")
    .replace(/^-\s+(.+)$/gm, "<li>$1</li>")
    .replace(/(<li>.*<\/li>)/s, "<ul>$1</ul>")
    .replace(/\n\n/g, "<p></p>")
    .replace(/\n/g, "<br>");
}

export default function DetailPanel({ node, edges, allNodes, onClose }: DetailPanelProps) {
  if (!node) return null;

  const chineseName = getChineseName(node);
  const nodeEdges = edges.filter(
    (e) => e.from === node.id || e.target === node.id
  );

  // Group edges by type
  const edgesByType = new Map<EdgeType, GraphEdge[]>();
  nodeEdges.forEach((edge) => {
    const list = edgesByType.get(edge.type) || [];
    list.push(edge);
    edgesByType.set(edge.type, list);
  });

  const getNodeName = (id: string) => {
    const n = allNodes.find((n) => n.id === id);
    return n ? getChineseName(n) : id;
  };

  return (
    <div className={`detail-panel ${node ? "open" : ""}`}>
      <div className="detail-panel-header">
        <div>
          <h2 className="detail-panel-title" lang="zh-CN">
            {chineseName}
          </h2>
          <p className="detail-panel-subtitle">{node.id}</p>
        </div>
        <button className="detail-panel-close" onClick={onClose} aria-label="关闭">
          ×
        </button>
      </div>

      <div className="detail-panel-body">
        {/* Atom */}
        <div className="detail-section">
          <div className="detail-section-title">Atom</div>
          <div className="detail-atom" lang="zh-CN">{node.atom}</div>
        </div>

        {/* Badges */}
        <div className="detail-section">
          <div className="detail-section-title">属性</div>
          <div>
            <span className={`detail-badge layer-l${node.layer}`}>
              {LAYER_LABELS[node.layer]}
            </span>
            <span className="detail-badge confidence">
              {node.confidence}
            </span>
            <span className="detail-badge source">
              {node.source}
            </span>
          </div>
        </div>

        {/* Domains */}
        <div className="detail-section">
          <div className="detail-section-title">领域</div>
          <div>
            {node.domain.map((d) => (
              <span key={d} className="detail-badge domain">
                {d}
              </span>
            ))}
          </div>
        </div>

        {/* Triggers */}
        <div className="detail-section">
          <div className="detail-section-title">触发词</div>
          <div>
            {node.triggers.map((t) => (
              <span key={t} className="detail-badge trigger">
                {t}
              </span>
            ))}
          </div>
        </div>

        {/* Requires */}
        {node.requires.length > 0 && (
          <div className="detail-section">
            <div className="detail-section-title">依赖</div>
            <ul className="detail-edge-list">
              {node.requires.map((id) => (
                <li key={id} className="detail-edge-item">
                  <span
                    className="filter-color-dot"
                    style={{ backgroundColor: LAYER_COLORS[allNodes.find((n) => n.id === id)?.layer ?? 0] }}
                  />
                  {getNodeName(id)} ({id})
                </li>
              ))}
            </ul>
          </div>
        )}

        {/* Enables */}
        {node.enables.length > 0 && (
          <div className="detail-section">
            <div className="detail-section-title">启用</div>
            <ul className="detail-edge-list">
              {node.enables.map((id) => (
                <li key={id} className="detail-edge-item">
                  <span
                    className="filter-color-dot"
                    style={{ backgroundColor: LAYER_COLORS[allNodes.find((n) => n.id === id)?.layer ?? 0] }}
                  />
                  {getNodeName(id)} ({id})
                </li>
              ))}
            </ul>
          </div>
        )}

        {/* Edges */}
        {nodeEdges.length > 0 && (
          <div className="detail-section">
            <div className="detail-section-title">连接 ({nodeEdges.length})</div>
            <ul className="detail-edge-list">
              {nodeEdges.map((edge, idx) => {
                const isOutgoing = edge.from === node.id;
                const otherId = isOutgoing ? edge.target : edge.from;
                const otherName = getNodeName(otherId);
                return (
                  <li key={idx} className="detail-edge-item">
                    <span className={`detail-edge-type ${getEdgeTypeClass(edge.type)}`}>
                      {EDGE_LABELS[edge.type]}
                    </span>
                    <span>
                      {isOutgoing ? "→" : "←"} {otherName} ({otherId})
                    </span>
                  </li>
                );
              })}
            </ul>
          </div>
        )}

        {/* Molecule */}
        <div className="detail-section">
          <div className="detail-section-title">Molecule</div>
          <div
            className="detail-molecule"
            lang="zh-CN"
            dangerouslySetInnerHTML={{ __html: renderMoleculeAsHtml(node.molecule) }}
          />
        </div>
      </div>
    </div>
  );
}
