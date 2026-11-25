import { createHash, randomBytes, randomUUID } from "node:crypto";
import { ConceptFrontmatterSchema, ConceptIdSchema, GraphSchema, ManifestRowSchema } from "@x-sage/core/types";

import type { ReviewProposalRequest, SubmitProposalRequest, RevPair, CommitCommandRequest, ReadGraphSnapshotRequest, ReadProposalFilesRequest, ReadTermTextRequest, ReadTermTextResponse } from "../../../packages/protocol/src/generated.ts";
import { normalizeBody, MAX_BODY_CHARS, conceptRevision, formatConceptFile, parseConceptFile } from "@x-sage/core/manifest";
import { parseAtomProposal, parseBodyProposal, parseEdgeProposal } from "@x-sage/core/proposals";
import { LAYERS, type NativeTermReader, type ProposalLists, type ProposalMergeFile } from "@x-sage/service";

import { ApiError } from "../../../packages/service/src/http.ts";

const SERVICE_PATH = "/sage.v1.SageCommandService/";
const MAX_RESPONSE_BYTES = 128 * 1024;
// Matches MAX_BATCH_RESPONSE in the native HTTP server.
const MAX_BATCH_RESPONSE_BYTES = 512 * 1024;
const MAX_TERM_BODY_BYTES = 256 * 1024;
const MAX_PROPOSAL_RAW_BYTES = 2 * 1024 * 1024;
const MAX_DIRECT_METADATA_BYTES = 256 * 1024;
const TERM_FRAME_PREFIX = 4;
const MAX_INLINE_BODY_BYTES = 64 * 1024;
const TERM_READ_METHODS = new Set(["ReadTermText", "ReadTermDocument", "ReadTermActivation"]);
const MAX_VIZ_BODY_BYTES = 512 * 1024;
const MAX_IMPORTED_SOURCE_BYTES = 64 * 1024;
const TERM_KIND = 1;
const ASSERTION_KIND = 2;
const PROPOSAL_KIND = 7;
const CONFIDENCE = { contested: 0, hypothetical: 1, confirmed: 2 } as const;
const TERM_SOURCE = { personal: 0, domain: 1, synthesis: 2 } as const;
const RELATION = {
  "is-a": 0,
  "part-of": 1,
  instantiates: 2,
  "depends-on": 3,
  enables: 4,
  contradicts: 5,
  "analogous-to": 6,
} as const;

function unavailable(): Error {
  return new Error("x-sage-db term read unavailable");
}

function record(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function positiveDecimal(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]*$/.test(value);
}

function checkBasis(value: unknown, kbUuid: string) {
  const basis = record(value);
  if (
    basis?.kbUuid !== kbUuid ||
    !positiveDecimal(basis.version) ||
    typeof basis.manifestSha256 !== "string" ||
    !/^[0-9a-f]{64}$/.test(basis.manifestSha256)
  ) {
    throw unavailable();
  }
  return { kbUuid, version: basis.version, manifestSha256: basis.manifestSha256 };
}

type Basis = ReturnType<typeof checkBasis>;

function checkResult(data: unknown, kbUuid: string, slug: string): ReadTermTextResponse & { basis: Basis } {
  const row = record(data);
  const basis = checkBasis(row?.basis, kbUuid);
  const key = record(row?.key);
  if (
    key?.kind !== TERM_KIND ||
    !positiveDecimal(key.id) ||
    row?.slug !== slug ||
    !positiveDecimal(row.revision) ||
    typeof row.atom !== "string" ||
    typeof row.body !== "string"
  ) {
    throw unavailable();
  }
  return {
    basis,
    key: { kind: TERM_KIND, id: key.id },
    revision: row.revision,
    slug,
    atom: row.atom,
    body: row.body,
  };
}

function termFrame(metadata: string, body: string): Uint8Array<ArrayBuffer> {
  const metadataSize = Buffer.byteLength(metadata);
  const bodySize = Buffer.byteLength(body);
  if (metadataSize > MAX_DIRECT_METADATA_BYTES || bodySize > MAX_TERM_BODY_BYTES) throw unavailable();
  const encoded = new Uint8Array(TERM_FRAME_PREFIX + metadataSize + bodySize);
  new DataView(encoded.buffer).setUint32(0, metadataSize, true);
  const encoder = new TextEncoder();
  encoder.encodeInto(metadata, encoded.subarray(TERM_FRAME_PREFIX, TERM_FRAME_PREFIX + metadataSize));
  encoder.encodeInto(body, encoded.subarray(TERM_FRAME_PREFIX + metadataSize));
  return encoded;
}

function decodeTermFrame(payload: ArrayBuffer, method: string): unknown {
  if (payload.byteLength < TERM_FRAME_PREFIX) throw unavailable();
  const size = new DataView(payload).getUint32(0, true);
  const start = TERM_FRAME_PREFIX + size;
  if (size > MAX_RESPONSE_BYTES || start > payload.byteLength || payload.byteLength - start > MAX_TERM_BODY_BYTES) {
    throw unavailable();
  }
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  const raw: unknown = JSON.parse(decoder.decode(new Uint8Array(payload, TERM_FRAME_PREFIX, size)));
  const row = record(raw);
  const text = method === "ReadTermText" ? row : record(row?.text);
  if (text === undefined || Object.hasOwn(text, "body")) throw unavailable();
  text.body = decoder.decode(new Uint8Array(payload, start));
  return row;
}

function decodeProposalFrame(payload: ArrayBuffer): unknown {
  if (payload.byteLength < TERM_FRAME_PREFIX) throw unavailable();
  const size = new DataView(payload).getUint32(0, true);
  let start = TERM_FRAME_PREFIX + size;
  if (size > MAX_BATCH_RESPONSE_BYTES || start > payload.byteLength ||
      payload.byteLength - start > MAX_PROPOSAL_RAW_BYTES) throw unavailable();
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  const metadata: unknown = JSON.parse(decoder.decode(new Uint8Array(payload, TERM_FRAME_PREFIX, size)));
  const queue = record(metadata);
  if (!Array.isArray(queue?.nativeFiles)) throw unavailable();
  for (const value of queue.nativeFiles) {
    const file = record(value);
    const body = record(file?.body);
    if (!body || body.rawBytes === undefined) continue;
    if (file?.directory !== "bodies" || Object.hasOwn(body, "proposedBody") ||
        !positiveDecimal(body.rawBytes)) throw unavailable();
    const width = Number(body.rawBytes);
    if (width > MAX_TERM_BODY_BYTES || width > payload.byteLength - start) throw unavailable();
    body.proposedBody = decoder.decode(new Uint8Array(payload, start, width));
    start += width;
  }
  if (start !== payload.byteLength) throw unavailable();
  return queue;
}

function sameBasis(a: Basis, b: Basis): boolean {
  return a.kbUuid === b.kbUuid && a.version === b.version && a.manifestSha256 === b.manifestSha256;
}

function decodeOriginal(value: unknown): string {
  const original = record(value);
  if (
    !positiveDecimal(original?.length) ||
    Number(original.length) > MAX_IMPORTED_SOURCE_BYTES ||
    typeof original.bytesBase64 !== "string" ||
    original.bytesBase64.length !== Math.ceil(Number(original.length) / 3) * 4 ||
    typeof original.sha256 !== "string"
  ) throw unavailable();
  const bytes = Buffer.from(original.bytesBase64, "base64");
  if (
    bytes.length !== Number(original.length) ||
    bytes.toString("base64") !== original.bytesBase64 ||
    createHash("sha256").update(bytes).digest("hex") !== original.sha256
  ) throw unavailable();
  return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
}

function proposalLists(input: Record<string, unknown> | undefined): ProposalLists {
  if (!Array.isArray(input?.files) || !Array.isArray(input.examples)) throw unavailable();
  const lists: ProposalLists = { edges: [], atoms: [], bodies: [] };
  const paths = new Set<string>();
  function add(path: unknown, payload: string, status: string | undefined) {
    if (typeof path !== "string" || paths.has(path)) throw unavailable();
    const match = /^proposals\/(edges|atoms|bodies)\/([A-Za-z0-9][A-Za-z0-9._-]*)$/.exec(path);
    if (!match || match[2]!.includes("..")) throw unavailable();
    const family = match[1]!;
    const file = match[2]!;
    paths.add(path);
    if (family === "bodies") {
      if (status !== "approved" && status !== "pending") throw unavailable();
      const proposal = parseBodyProposal(file, payload);
      proposal.status = status;
      lists.bodies.push(proposal);
      return;
    }
    if (!file.endsWith(".md")) throw unavailable();
    const proposal = family === "edges" ? parseEdgeProposal(file, payload) : parseAtomProposal(file, payload);
    if (status === undefined && (!file.startsWith("example-") || proposal.status !== "example")) return;
    if (status !== undefined) proposal.status = status;
    if ("from" in proposal) lists.edges.push(proposal);
    else lists.atoms.push(proposal);
  }
  for (const value of input.files) {
    const row = record(value);
    if (
      typeof row?.payload !== "string" || typeof row.status !== "string" ||
      !["pending", "approved", "rejected", "conflicted"].includes(row.status)
    ) throw unavailable();
    add(row.path, row.payload, row.status);
  }
  for (const value of input.examples) {
    const row = record(value);
    add(row?.path, decodeOriginal(row), undefined);
  }
  const byFile = (a: { file: string }, b: { file: string }) => a.file < b.file ? -1 : a.file > b.file ? 1 : 0;
  lists.edges.sort(byFile);
  lists.atoms.sort(byFile);
  lists.bodies.sort(byFile);
  return lists;
}

type ProposalDirectory = ProposalMergeFile["directory"];
type ProposalIdentity = {
  directory: ProposalDirectory;
  file: string;
  pair: RevPair;
  status: string;
  concept: string;
  expectedRevision?: string;
};

function byId(a: string, b: string): number {
  return a.length - b.length || (a < b ? -1 : a > b ? 1 : 0);
}

function proposalQueue(input: Record<string, unknown> | undefined) {
  if (!Array.isArray(input?.files) || !Array.isArray(input.examples)
      || (input.nativeFiles !== undefined && !Array.isArray(input.nativeFiles))) throw unavailable();
  const native = new Map<string, { identity: ProposalIdentity; row: Record<string, unknown> }>();
  for (const value of input.nativeFiles ?? []) {
    const row = record(value);
    const key = record(row?.key);
    const directory = row?.directory;
    if ((directory !== "edges" && directory !== "atoms" && directory !== "bodies")
        || typeof row?.filename !== "string"
        || !/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/.test(row.filename) || row.filename.includes("..")
        || key?.kind !== PROPOSAL_KIND || !positiveDecimal(key.id) || !positiveDecimal(row.revision)
        || typeof row.subjectSlug !== "string" || typeof row.status !== "string"
        || !["pending", "approved", "rejected", "conflicted"].includes(row.status)) throw unavailable();
    const file = row.filename;
    const path = `proposals/${directory}/${file}`;
    const existing = native.get(path)?.identity.pair.key?.id;
    if (existing && byId(existing, key.id) === 0) throw unavailable();
    if (existing && byId(existing, key.id) > 0) continue;
    const expected = record(row.body)?.expectedRevision;
    const identity: ProposalIdentity = {
      directory, file, pair: { key: { kind: PROPOSAL_KIND, id: key.id }, revision: row.revision },
      concept: row.subjectSlug, status: row.status,
      ...(typeof expected === "string" ? { expectedRevision: expected } : {}),
    };
    native.set(path, { row, identity });
  }
  const files = input.files.filter((value) => {
    const imported = record(value);
    const path = imported?.path;
    if (typeof path !== "string") throw unavailable();
    const replacement = native.get(path)?.identity.pair.key?.id;
    if (!replacement) return true;
    const oldId = record(imported?.key)?.id;
    if (!positiveDecimal(oldId) || byId(replacement, oldId) === 0) throw unavailable();
    if (byId(replacement, oldId) > 0) return false;
    native.delete(path);
    return true;
  });
  const lists = proposalLists({ ...input, files });
  for (const { row, identity: { directory, file, status, concept, expectedRevision } } of native.values()) {
    if (directory === "edges") {
      const edge = record(row.edge);
      if (!file.endsWith(".md") || typeof edge?.targetSlug !== "string" ||
          typeof edge.relation !== "string" || typeof edge.confidence !== "string") throw unavailable();
      lists.edges.push({ file, status, date: "", from: concept, type: edge.relation,
        target: edge.targetSlug, confidence: edge.confidence });
    } else if (directory === "atoms") {
      const atom = record(row.atom);
      if (!file.endsWith(".md") || typeof atom?.currentAtom !== "string" ||
          typeof atom.proposedAtom !== "string") throw unavailable();
      lists.atoms.push({ file, status, date: "", conceptId: concept, currentAtom: atom.currentAtom,
        proposedAtom: atom.proposedAtom });
    } else {
      const body = record(row.body);
      if (status !== "pending" && status !== "approved") continue;
      if (!file.endsWith(".json") || !expectedRevision || !/^[0-9a-f]{64}$/.test(expectedRevision)
          || typeof body?.proposedBody !== "string" || typeof body.reason !== "string") throw unavailable();
      lists.bodies.push({ file, status, date: file.slice(0, 24), conceptId: concept,
        expectedRevision, proposedBody: body.proposedBody, reason: body.reason });
    }
  }
  const byFile = (a: { file: string }, b: { file: string }) => a.file < b.file ? -1 : a.file > b.file ? 1 : 0;
  lists.edges.sort(byFile);
  lists.atoms.sort(byFile);
  lists.bodies.sort(byFile);
  return { lists, native };
}

function proposalId(slug: string): string {
  if (!ConceptIdSchema.safeParse(slug).success) {
    throw new ApiError(400, `Invalid concept id: ${JSON.stringify(slug)}. Must be a lowercase slug [a-z0-9-]`);
  }
  return slug;
}

function textFilename(slug: string, body: boolean): string {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const nonce = randomBytes(3).toString("hex");
  return body ? `${stamp}-${slug}-body-${nonce}.json` : `${stamp}-${slug}-${nonce}.md`;
}

/** A private connection for one configured KB, using the service's own Read credential. */
export function connectNativeTermReader(options: {
  url: string;
  kbUuid: string;
  kbSlug: string;
  token: string;
  writeConceptToken?: string;
  proposalProposeToken?: string;
  proposalReviewToken?: string;
}): NativeTermReader {
  const target = new URL(options.url);
  if (
    target.protocol !== "http:" ||
    !["127.0.0.1", "[::1]"].includes(target.hostname) ||
    target.pathname !== "/" ||
    target.username || target.password || target.search || target.hash
  ) {
    throw new Error("x-sage-db connection requires a private loopback origin");
  }
  if (!/^[0-9a-f]{32}$/.test(options.kbUuid) || !options.kbSlug || !/^[0-9a-f]{64}$/.test(options.token)) {
    throw new Error("Invalid x-sage-db connection identity or read credential");
  }
  const writeToken = options.writeConceptToken;
  if (writeToken !== undefined && (!/^[0-9a-f]{64}$/.test(writeToken) || writeToken === options.token)) {
    throw new Error("Concept write credential must be distinct from the read credential");
  }

  const proposeToken = options.proposalProposeToken;
  const reviewToken = options.proposalReviewToken;
  const tokens = [options.token, writeToken, proposeToken, reviewToken]
    .filter((value): value is string => value !== undefined);
  if (tokens.some((value) => !/^[0-9a-f]{64}$/.test(value)) || new Set(tokens).size !== tokens.length) {
    throw new Error("Proposal service requires distinct scoped private credentials");
  }

  async function exchange(
    method: string,
    payload: ReadTermTextRequest | ReadGraphSnapshotRequest | ReadProposalFilesRequest |
      CommitCommandRequest | SubmitProposalRequest | ReviewProposalRequest,
    token: string,
    signal = AbortSignal.timeout(5000),
    directBody?: string,
  ) {
    const acceptsTermFrame = TERM_READ_METHODS.has(method);
    const acceptsProposalFrame = method === "ReadProposalFiles";
    const metadata = JSON.stringify(payload);
    const response = await fetch(new URL(SERVICE_PATH + method, target), {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": directBody === undefined ? "application/json" : "application/octet-stream",
        ...((acceptsTermFrame || acceptsProposalFrame) ? { accept: "application/octet-stream" } : {}),
      },
      body: directBody === undefined ? metadata : termFrame(metadata, directBody),
      redirect: "error",
      signal,
    });
    const length = response.headers.get("content-length");
    const framed = (acceptsTermFrame || acceptsProposalFrame) && response.ok && response.headers.get("content-type") === "application/octet-stream";
    const tailLimit = acceptsProposalFrame ? MAX_PROPOSAL_RAW_BYTES : MAX_TERM_BODY_BYTES;
    const replyLimit = acceptsProposalFrame || method === "ReadGraphSnapshot" ? MAX_BATCH_RESPONSE_BYTES : MAX_RESPONSE_BYTES;
    const maxBytes = framed ? TERM_FRAME_PREFIX + replyLimit + tailLimit : replyLimit;
    if (!length || !/^[0-9]+$/.test(length) || Number(length) > maxBytes) {
      await response.body?.cancel();
      throw unavailable();
    }
    let data: unknown;
    if (framed) {
      const bytes = await response.arrayBuffer();
      if (bytes.byteLength !== Number(length)) throw unavailable();
      data = acceptsProposalFrame ? decodeProposalFrame(bytes) : decodeTermFrame(bytes, method);
    } else {
      data = await response.json();
    }
    return { ok: response.ok, status: response.status, data };
  }

  async function read(
    method: "ReadTermText" | "ReadTermDocument" | "ReadTermActivation" | "ReadGraphSnapshot" | "ReadProposalFiles",
    slug?: string,
    signal?: AbortSignal,
  ) {
    const payload: ReadTermTextRequest | ReadGraphSnapshotRequest | ReadProposalFilesRequest = {
      envelope: {
        protocolVersion: 1,
        kbUuid: options.kbUuid,
        requestId: randomUUID(),
        deadlineMs: 4500,
        readSet: [],
      },
      ...(slug === undefined ? {} : { slug }),
    };
    const response = await exchange(method, payload, options.token, signal);
    const { data } = response;
    const code = record(record(data)?.error)?.code;
    if (response.status === 404 && code === "ERROR_CODE_INVALID_ARGUMENT") {
      return null;
    }
    if (response.status === 429 && code === "ERROR_CODE_BUDGET") {
      throw new ApiError(503, "Database read exceeded its resource budget");
    }
    if (!response.ok) throw unavailable();
    return { data };
  }

  async function readGraph(signal: AbortSignal) {
    const row = record((await read("ReadGraphSnapshot", undefined, signal))?.data);
    const basis = checkBasis(row?.basis, options.kbUuid);
    if (!Array.isArray(row?.terms) || !Array.isArray(row.assertions)) throw unavailable();
    const paths: string[] = [];
    const terms = new Map<string, { id: string; revision: string }>();
    const ids = new Set<string>();
    const manifest = row.terms.map((value: unknown) => {
      const term = record(value);
      const entry = ManifestRowSchema.parse({
        id: term?.slug,
        atom: term?.atom,
        layer: term?.layer,
        domain: term?.domains,
        confidence: term?.confidence,
        source: term?.source,
        triggers: term?.triggers,
      });
      const path = term?.legacySourcePath ?? `concepts/${LAYERS[entry.layer]}/${entry.id}.md`;
      if (typeof path !== "string") throw unavailable();
      paths.push(path);
      if (writeToken !== undefined || proposeToken !== undefined) {
        const key = record(term?.key);
        if (key?.kind !== TERM_KIND || !positiveDecimal(key.id)
            || !positiveDecimal(term?.revision) || ids.has(key.id)) throw unavailable();
        terms.set(entry.id, { id: key.id, revision: term.revision });
        ids.add(key.id);
      }
      return entry;
    });
    const slugs = new Set(manifest.map((term) => term.id));
    if (slugs.size !== manifest.length) throw unavailable();
    const graph = GraphSchema.parse({
      nodes: manifest.map((term, i) => ({
        id: term.id,
        atom: term.atom,
        layer: term.layer,
        domain: term.domain,
        confidence: term.confidence,
        filePath: paths[i],
      })),
      edges: row.assertions.map((value: unknown) => {
        const assertion = record(value);
        if (
          typeof assertion?.fromSlug !== "string" ||
          typeof assertion.targetSlug !== "string" ||
          !slugs.has(assertion.fromSlug) ||
          !slugs.has(assertion.targetSlug)
        ) throw unavailable();
        return {
          type: assertion.relation,
          target: assertion.targetSlug,
          confidence: assertion.confidence,
          from: assertion.fromSlug,
        };
      }),
    });
    return {
      basis, graph, manifest, terms, version: basis.version,
      ...(writeToken === undefined ? {} : { writeBasis: { version: basis.version, terms } }),
    };
  }

  async function readDocument(slug: string, signal?: AbortSignal) {
    const response = await read("ReadTermDocument", slug, signal);
    if (response === null) return null;
    const row = record(response.data);
    const text = checkResult(row?.text, options.kbUuid, slug);
    if (!Array.isArray(row?.assertions)) throw unavailable();
    const edges = row.assertions.map((value: unknown) => {
      const assertion = record(value);
      const key = record(assertion?.key);
      if (
        key?.kind !== 2 ||
        !positiveDecimal(key.id) ||
        !positiveDecimal(assertion?.revision) ||
        typeof assertion.declarationOrder !== "number" ||
        !Number.isInteger(assertion.declarationOrder) ||
        assertion.declarationOrder < 0 ||
        assertion.declarationOrder > 0xffffffff
      ) {
        throw unavailable();
      }
      return { type: assertion.relation, target: assertion.targetSlug, confidence: assertion.confidence };
    });
    const frontmatter = ConceptFrontmatterSchema.parse({
      id: text.slug,
      atom: text.atom,
      layer: row.layer,
      domain: row.domains,
      confidence: row.confidence,
      source: row.source,
      triggers: row.triggers,
      edges,
    });
    return { basis: text.basis, key: text.key!, revision: text.revision, body: text.body, frontmatter };
  }

  async function readProposalSnapshot(signal: AbortSignal) {
    const row = record((await read("ReadProposalFiles", undefined, signal))?.data);
    const basis = checkBasis(row?.basis, options.kbUuid);
    return { basis, ...proposalQueue(row) };
  }

  function proposalEnvelope(basis: Basis, readSet: RevPair[]) {
    return {
      protocolVersion: 1, kbUuid: options.kbUuid, requestId: randomUUID(), deadlineMs: 4500,
      readSet, expectVersion: basis.version,
    };
  }

  async function submit(request: SubmitProposalRequest, filename: string, token: string, rawBody?: string): Promise<string> {
    const method = rawBody === undefined ? "SubmitProposal" : "SubmitLargeBodyProposal";
    const result = await exchange(method, request, token, undefined, rawBody).catch(() => undefined);
    if (!result || result.status >= 500) throw new ApiError(503, "Database unavailable");
    const reply = record(result.data);
    const outcome = record(reply?.outcome);
    if (record(outcome?.unknown)) throw new ApiError(503, "Database unavailable");
    if (result.status === 409) throw new ApiError(409, "Knowledge base changed; retry proposal submission");
    if (result.status === 400 || result.status === 422) throw new ApiError(400, "Proposal refused by database");
    const committed = record(outcome?.committed ?? outcome?.alreadyCommitted);
    const recorded = record(committed?.recorded);
    const affected = Array.isArray(recorded?.affected) ? recorded.affected : [];
    const row = record(affected[0]);
    const key = record(row?.key);
    if (!result.ok || affected.length !== 1 || key?.kind !== PROPOSAL_KIND || !positiveDecimal(key.id)) {
      throw new ApiError(503, "Database unavailable");
    }
    return filename;
  }

  async function proposalDocument(slug: string) {
    const current = await readDocument(proposalId(slug)).catch(() => {
      throw new ApiError(503, "Database unavailable");
    });
    if (current === null) throw new ApiError(404, `Concept not found: ${slug}`);
    return current;
  }

  const connection: NativeTermReader = {
    kbSlug: options.kbSlug,
    ...(proposeToken === undefined ? {} : {
      submitProposal: {
        async edge(input) {
          const from = proposalId(input.from);
          const target = proposalId(input.target);
          const graph = await readGraph(AbortSignal.timeout(5000)).catch(() => {
            throw new ApiError(503, "Database unavailable");
          });
          const sourceRow = graph.terms.get(from);
          const targetRow = graph.terms.get(target);
          if (!sourceRow) throw new ApiError(404, `Concept not found: ${from}`);
          if (targetRow?.id === sourceRow.id) throw new ApiError(400, "A relation cannot target its source");
          const sourceKey = { kind: TERM_KIND, id: sourceRow.id };
          const sourcePair = { key: sourceKey, revision: sourceRow.revision };
          const filename = `${new Date().toISOString().slice(0, 10)}-${from}-${input.type}-${target}.md`;
          const authored = {
            from: sourceKey, relation: RELATION[input.type], confidence: CONFIDENCE[input.confidence],
            declarationOrder: 0, declaredAuthor: "CLI", reason: "(待填写)",
          };
          if (!targetRow) {
            return submit({
              envelope: proposalEnvelope(graph.basis, [sourcePair]),
              payload: { legacyRelationBySlug: { ...authored, filename, targetSlug: target } },
            }, filename, proposeToken);
          }
          const targetKey = { kind: TERM_KIND, id: targetRow.id };
          const readSet = [sourcePair, { key: targetKey, revision: targetRow.revision }]
            .sort((a, b) => byId(a.key.id, b.key.id));
          return submit({
            envelope: proposalEnvelope(graph.basis, readSet),
            payload: { legacyRelation: { filename, edge: { ...authored, target: targetKey } } },
          }, filename, proposeToken);
        },
        async atom(input) {
          const current = await proposalDocument(input.conceptId);
          const atom = input.proposedAtom.trim();
          if (!atom) throw new ApiError(400, "proposedAtom must not be empty");
          if (atom.length > 200) {
            throw new ApiError(400, `proposedAtom must be at most 200 characters (got ${atom.length})`);
          }
          const filename = textFilename(input.conceptId, false);
          return submit({
            envelope: proposalEnvelope(current.basis, [{ key: current.key, revision: current.revision }]),
            payload: { legacyConcept: {
              filename, currentAtom: current.frontmatter.atom,
              edit: { subject: current.key, atom, declaredAuthor: "CLI", reason: input.reason ?? "" },
            } },
          }, filename, proposeToken);
        },
        async body(input) {
          if (!input.reason.trim()) throw new ApiError(400, "A body proposal needs a reason");
          const current = await proposalDocument(input.conceptId);
          const revision = conceptRevision(current.frontmatter, current.body);
          if (revision !== input.expectedRevision) {
            throw new ApiError(409, `Concept ${input.conceptId} is at revision ${revision.slice(0, 12)}, not ${input.expectedRevision.slice(0, 12)}. Re-read it and resubmit.`);
          }
          const body = normalizeBody(input.proposedBody);
          if (!body.trim()) throw new ApiError(400, "Proposed body is empty");
          if (body.length > MAX_BODY_CHARS) {
            throw new ApiError(400, `Proposed body is ${body.length} characters, limit is ${MAX_BODY_CHARS}`);
          }
          const filename = textFilename(input.conceptId, true);
          const raw = Buffer.byteLength(body) > MAX_INLINE_BODY_BYTES ? body : undefined;
          return submit({
            envelope: proposalEnvelope(current.basis, [{ key: current.key, revision: current.revision }]),
            payload: { legacyConcept: {
              filename, publicRevision: revision,
              edit: { subject: current.key, body: raw === undefined ? body : "", declaredAuthor: "CLI", reason: input.reason },
            } },
          }, filename, proposeToken, raw);
        },
      },
    } satisfies Pick<NativeTermReader, "submitProposal">),
    ...(reviewToken === undefined ? {} : {
      proposalMerge: {
        async list() { return (await readProposalSnapshot(AbortSignal.timeout(5000))).lists; },
        async decide({ directory, file }) {
          const signal = AbortSignal.timeout(5000);
          const { basis, native, lists } = await readProposalSnapshot(signal);
          const found = native.get(`proposals/${directory}/${file}`)?.identity;
          if (!found) {
            const imported = lists[directory].find((item) => item.file === file);
            if (imported?.status === "approved") return { kind: "alreadyApplied" };
            if (imported?.status === "example") {
              return { kind: "failed", message: `Proposal ${file} has status "example", expected "pending"` };
            }
            return {
              kind: "failed",
              message: imported ? "Imported proposal requires conversion before approval" : `No such proposal: ${file}`,
            };
          }
          if (found.status === "approved") return { kind: "alreadyApplied" };
          if (found.status !== "pending") {
            return { kind: "failed", message: `Proposal ${file} has status ${JSON.stringify(found.status)}, expected "pending"` };
          }
          let publicRevision: string | undefined;
          if (directory === "bodies") {
            const current = await readDocument(found.concept, signal);
            if (current === null) return { kind: "failed", message: `Concept not found: ${found.concept}` };
            if (!sameBasis(current.basis, basis)) {
              return { kind: "conflict", message: "Knowledge base changed; retry proposal merge" };
            }
            publicRevision = conceptRevision(current.frontmatter, current.body);
          }
          const review = {
            filename: file,
            action: { proposal: found.pair.key, decision: 1 as const, note: "Approved by legacy proposal merge" },
            ...(publicRevision === undefined ? {} : { publicRevision }),
          };
          const request: ReviewProposalRequest = {
            envelope: proposalEnvelope(basis, [found.pair]),
            payload: directory === "edges" ? { legacyRelation: review } : { legacyConcept: review },
          };
          const result = await exchange("ReviewProposal", request, reviewToken, signal).catch(() => undefined);
          if (!result || result.status >= 500) return { kind: "unknown" };
          const reply = record(result.data);
          const outcome = record(reply?.outcome);
          const error = record(reply?.error ?? outcome?.error);
          if (result.status === 409 && error?.code === "ERROR_CODE_LEGACY_PROPOSAL_ATOM_MISMATCH") {
            return { kind: "conflict", message: `Atom proposal ${file} was written against a different atom for ${found.concept}` };
          }
          if (result.status === 409 && error?.code === "ERROR_CODE_LEGACY_PROPOSAL_BODY_MISMATCH") {
            return { kind: "conflict", message: `Body proposal ${file} was written against revision ${found.expectedRevision?.slice(0, 12)}, current is ${publicRevision?.slice(0, 12)}` };
          }
          if (result.status === 422 && error?.code === "ERROR_CODE_LEGACY_PROPOSAL_TARGET_UNRESOLVED") {
            const target = lists.edges.find((edge) => edge.file === file)?.target;
            return { kind: "failed", message: target ? `Unresolved proposal target: ${target}` : "Proposal target is still unresolved" };
          }
          if (result.status === 409) return { kind: "conflict", message: "Knowledge base changed; retry proposal merge" };
          if (result.status === 400 || result.status === 422) {
            return { kind: "failed", message: "Proposal could not be applied by database" };
          }
          if (result.ok && (record(outcome?.committed) || record(outcome?.alreadyCommitted))) return { kind: "applied" };
          return { kind: "unknown" };
        },
      },
    } satisfies Pick<NativeTermReader, "proposalMerge">),
    async readGraphSnapshot(includeBodies) {
      const signal = AbortSignal.timeout(5000);
      const { basis, ...snapshot } = await readGraph(signal);
      const { manifest } = snapshot;
      if (!includeBodies) return snapshot;
      let total = 0;
      const bodies = new Map<string, string>();
      for (const term of manifest) {
        const reply = await read("ReadTermText", term.id, signal);
        if (reply === null) throw unavailable();
        const doc = checkResult(reply.data, options.kbUuid, term.id);
        if (!sameBasis(doc.basis, basis)) {
          throw unavailable();
        }
        total += Buffer.byteLength(doc.body);
        if (total > MAX_VIZ_BODY_BYTES) throw unavailable();
        bodies.set(term.id, doc.body);
      }
      return { ...snapshot, bodies };
    },
    async readActivation(slug) {
      const signal = AbortSignal.timeout(5000);
      const { basis, graph, manifest } = await readGraph(signal);
      const term = manifest.find((candidate) => candidate.id === slug);
      if (term === undefined) return null;
      const node = graph.nodes.find((candidate) => candidate.id === slug);
      const response = await read("ReadTermActivation", slug, signal);
      // The earlier graph already established this term's presence.
      if (node === undefined || response === null) throw unavailable();
      const row = record(response.data);
      const doc = checkResult(row?.text, options.kbUuid, slug);
      if (!sameBasis(doc.basis, basis)) throw unavailable();

      const edges = graph.edges.filter((edge) => edge.from === slug).map(({ type, target, confidence }) => ({
        type, target, confidence,
      }));
      const frontmatter = ConceptFrontmatterSchema.parse({ ...term, edges });
      const molecule = doc.body.trim();
      let raw: string | undefined;
      if (row?.original !== undefined) {
        const archive = decodeOriginal(row.original);
        const parsed = parseConceptFile(archive, node.filePath);
        // This source spelling is usable only if it still describes current
        // content and current authored edges (edge edits may not revise Term).
        if (parsed.molecule === molecule && JSON.stringify(parsed.frontmatter) === JSON.stringify(frontmatter)) {
          raw = archive;
        }
      }
      raw ??= formatConceptFile({ frontmatter, molecule, filePath: node.filePath });
      return { graph, raw, molecule, triggers: term.triggers };
    },
    async readTermText(slug) {
      const response = await read("ReadTermText", slug);
      return response === null ? null : checkResult(response.data, options.kbUuid, slug).body;
    },
    async readProposalFiles() {
      const signal = AbortSignal.timeout(5000);
      const { basis, lists } = await readProposalSnapshot(signal);
      return {
        lists,
        async readBody(conceptId) {
          const current = await readDocument(conceptId, signal);
          if (current === null || !sameBasis(current.basis, basis)) throw unavailable();
          const { body, frontmatter } = current;
          return { body, revision: conceptRevision(frontmatter, body) };
        },
      };
    },
    async readTermDocument(slug) {
      const document = await readDocument(slug);
      return document === null ? null : { body: document.body, frontmatter: document.frontmatter };
    },
  };
  if (writeToken !== undefined) {
    connection.createConcept = async (concept, body, basis) => {
      const targets = new Map<string, { key: { kind: number; id: string }; revision: string }>();
      const assertions = concept.edges.map((edge) => {
        const target = basis.terms.get(edge.target);
        if (!target) throw unavailable();
        const key = { kind: TERM_KIND, id: target.id };
        targets.set(target.id, { key, revision: target.revision });
        return { target: key, relation: RELATION[edge.type], confidence: CONFIDENCE[edge.confidence] };
      });
      const readSet = [...targets.values()].sort((a, b) =>
        a.key.id.length - b.key.id.length || (a.key.id < b.key.id ? -1 : a.key.id === b.key.id ? 0 : 1),
      );
      const requestId = randomUUID();
      const rawBody = Buffer.byteLength(body) > MAX_INLINE_BODY_BYTES ? body : undefined;
      const request: CommitCommandRequest = {
        envelope: {
          protocolVersion: 1,
          kbUuid: options.kbUuid,
          requestId,
          deadlineMs: 4500,
          readSet,
          expectVersion: basis.version,
        },
        payload: {
          directConceptCreate: {
            term: {
              slug: concept.id,
              atom: concept.atom,
              body: rawBody === undefined ? body : "",
              layer: concept.layer,
              domains: concept.domain,
              confidence: CONFIDENCE[concept.confidence],
              source: TERM_SOURCE[concept.source],
              triggers: concept.triggers,
              declaredAuthor: "",
              reason: "Created through POST /api/concepts",
            },
            assertions,
          },
        },
      };
      // A lost private response may follow a durable publication.
      const method = rawBody === undefined ? "CommitCommand" : "CommitDirectConcept";
      const response = await exchange(method, request, writeToken, undefined, rawBody).catch(() => undefined);
      if (!response) return "unknown";
      const reply = record(response.data);
      const outcome = record(reply?.outcome);
      const code = record(reply?.error ?? outcome?.error)?.code;
      if (!response.ok) {
        if (response.status === 409 && code === "ERROR_CODE_CONCEPT_SLUG_EXISTS") return "duplicate";
        if (response.status === 409 && code === "ERROR_CODE_CONFLICT") return "conflict";
        if ((response.status === 400 || response.status === 422) && code === "ERROR_CODE_INVALID_ARGUMENT") {
          return "invalid";
        }
        if (code === "ERROR_CODE_COMMIT_UNKNOWN") return "unknown";
        throw unavailable();
      }
      if (record(outcome?.unknown)?.requestId === requestId) return "unknown";
      const commit = record(outcome?.committed ?? outcome?.alreadyCommitted);
      const recorded = record(commit?.recorded);
      if (!positiveDecimal(recorded?.commitVersion) ||
          !Array.isArray(recorded.affected) ||
          recorded.affected.length !== assertions.length + 1) throw unavailable();
      for (const [index, affected] of recorded.affected.entries()) {
        const pair = record(affected);
        const key = record(pair?.key);
        if (key?.kind !== (index === 0 ? TERM_KIND : ASSERTION_KIND) ||
            !positiveDecimal(key.id) || pair?.revision !== "1") throw unavailable();
      }
      return "committed";
    };
  }
  return connection;
}
