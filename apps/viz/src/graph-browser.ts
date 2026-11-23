import { conceptName, type EdgeType, type GraphData, type GraphNode, type Layer } from "./types";

const domainNames = new Map([
  ["product", "产品"],
  ["engineering", "工程"],
  ["philosophy", "哲学"],
  ["personal-thought", "个人思考"],
]);

export function domainName(id: string): string {
  return domainNames.get(id) ?? id;
}

export function countDomains(nodes: readonly GraphNode[]) {
  const counts = new Map<string, number>();
  for (const node of nodes) {
    for (const domain of new Set(node.domain)) {
      if (domain) counts.set(domain, (counts.get(domain) ?? 0) + 1);
    }
  }
  return [...counts].map(([id, count]) => ({ id, count }))
    .sort((a, b) => b.count - a.count || a.id.localeCompare(b.id));
}

interface BrowseFilters {
  domain: string;
  query: string;
  layers: ReadonlySet<Layer>;
  relations: ReadonlySet<EdgeType>;
  neighborsOf: string | null;
}

export function browseGraph(data: GraphData | null, filters: BrowseFilters) {
  const { domain, query, layers, relations, neighborsOf } = filters;
  const needle = query.trim().toLocaleLowerCase();
  const scoped = (data?.nodes ?? []).filter((node) =>
    (!domain || node.domain.includes(domain)) &&
    (!needle || [conceptName(node), node.id, node.atom, ...node.triggers]
      .some((text) => text.toLocaleLowerCase().includes(needle))),
  );
  const layerCounts: Record<string, number> = {};
  for (const node of scoped) {
    const key = `L${node.layer}`;
    layerCounts[key] = (layerCounts[key] ?? 0) + 1;
  }
  const matches = scoped.filter((node) => layers.has(node.layer));
  const matchIds = new Set(matches.map((node) => node.id));
  const authored = (data?.edges ?? []).filter((edge) => !edge.mirrored && relations.has(edge.type));
  const neighborIds = new Set<string>();
  if (neighborsOf && matchIds.has(neighborsOf)) {
    for (const edge of authored) {
      if (edge.from === neighborsOf) neighborIds.add(edge.target);
      if (edge.target === neighborsOf) neighborIds.add(edge.from);
    }
  }
  const neighbors = (data?.nodes ?? []).filter((node) =>
    neighborIds.has(node.id) && !matchIds.has(node.id) && layers.has(node.layer),
  );
  const nodes = [...matches, ...neighbors];
  const visibleIds = new Set(nodes.map((node) => node.id));
  const edges = authored.filter((edge) => visibleIds.has(edge.from) && visibleIds.has(edge.target));
  return { matches, matchIds, neighbors, nodes, edges, layerCounts };
}
