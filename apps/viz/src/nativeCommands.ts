import type { CommitPayload, Envelope, RecordKey, RevPair, SubmitPayload, TermCreate } from "../../../packages/protocol/src/generated";
import { ApiError, api } from "./api";

export type NativeTerm = {
  conceptId: string;
  kbUuid: string;
  termId: string;
  revision: string;
  atom: string;
  body: string;
};

export type NativeProposal = {
  id: string;
  kind: string;
  subject: string;
  revision: string;
  status: string;
  legacyId?: string;
  reason: string | null;
};

export type NativePreview = {
  kbUuid: string;
  payloadSha256: string;
  proposal: NativeProposal;
  preview: { kind: "concept-markdown"; atom: string | null; body: string | null } |
    { kind: "term-create"; slug: string; atom: string; body: string; layer: number;
      domains: string[]; triggers: string[]; confidence: string; source: string; reason: string } |
    { kind: "relation-assert"; from: RecordKey; target: RecordKey; targetSlug: string; relation: string; confidence: string };
};

const service = "/sage.v1.SageCommandService/";
const termKey = (term: NativeTerm): RecordKey => ({ kind: 1, id: term.termId });
const termPair = (term: NativeTerm): RevPair => ({ key: termKey(term), revision: term.revision });

export async function readTerm(kb: string, slug: string, token: string): Promise<NativeTerm> {
  const term = await api(kb, `/concepts/${encodeURIComponent(slug)}/body`, "GET", undefined, token);
  if (term.conceptId !== slug || !/^[a-f\d]{32}$/.test(term.kbUuid) ||
      !/^[1-9]\d*$/.test(term.termId) || !/^[1-9]\d*$/.test(term.revision) ||
      typeof term.atom !== "string" || typeof term.body !== "string") {
    throw new Error("服务器未返回有效的概念修订基线。");
  }
  return term;
}

export function textSubmission(term: NativeTerm, kind: "atom" | "body", value: string) {
  const payload: SubmitPayload = {
    conceptText: { subject: termKey(term), declaredAuthor: "", [kind]: value },
  };
  return { kbUuid: term.kbUuid, readSet: [termPair(term)], payload };
}

const relations = ["is-a", "part-of", "depends-on", "enables", "contradicts", "instantiates", "analogous-to"];

export function relationSubmission(from: NativeTerm, target: NativeTerm, relation: string, reason: string) {
  const code = relations.indexOf(relation);
  if (code < 0 || from.kbUuid !== target.kbUuid || from.termId === target.termId) {
    throw new Error("请为关联选择同一知识库中的两个不同概念。");
  }
  const readSet = BigInt(from.termId) < BigInt(target.termId) ?
    [termPair(from), termPair(target)] : [termPair(target), termPair(from)];
  const payload: SubmitPayload = {
    relationAssert: {
      from: termKey(from), target: termKey(target), relation: code, confidence: 1,
      declarationOrder: 0, declaredAuthor: "", reason,
    },
  };
  return { kbUuid: from.kbUuid, readSet, payload };
}

export function reviewProposal(detail: NativePreview, decision: "DECISION_APPROVE" | "DECISION_REJECT", note: string) {
  const { id, revision, status } = detail.proposal;
  if (!note.trim()) throw new Error("请输入审阅说明。");
  if (status !== "pending" || !/^prp_[1-9]\d*$/.test(id) || !/^[1-9]\d*$/.test(revision)) {
    throw new Error("请重新读取待审提案。");
  }
  const proposal = { kind: 7, id: id.slice(4) };
  const family = {
    "concept-markdown": "conceptText", "relation-assert": "relationAssert", "term-create": "termCreate",
  }[detail.preview.kind];
  return {
    kbUuid: detail.kbUuid,
    readSet: [{ key: proposal, revision }],
    payload: { [family]: { proposal, decision, note } },
  };
}

export type NativeCheckpoint = {
  conceptId: string;
  kbUuid: string;
  checkpointId: string;
  revision: string;
  session: number;
  content: string;
};

export async function readCheckpoint(kb: string, slug: string, token: string): Promise<NativeCheckpoint> {
  const value = await api(kb, `/knowledge/checkpoints/${encodeURIComponent(slug)}`, "GET", undefined, token);
  if (value.conceptId !== slug || !/^[a-f\d]{32}$/.test(value.kbUuid) ||
      !/^[1-9]\d*$/.test(value.checkpointId) || !/^[1-9]\d*$/.test(value.revision) ||
      !Number.isInteger(value.session) || value.session < 1 || typeof value.content !== "string") {
    throw new Error("服务器未返回有效的会话快照。");
  }
  return value;
}

export function checkpointCommand(term: NativeTerm, sessionNo: number, content: string, existing: NativeCheckpoint | null) {
  if (!Number.isInteger(sessionNo) || sessionNo < 1 || sessionNo > 0xffffffff ||
      (existing && (existing.kbUuid !== term.kbUuid || existing.conceptId !== term.conceptId || existing.session !== sessionNo))) {
    throw new Error("会话编号无效或与已读取的快照不一致。");
  }
  const state = { concept: termKey(term), sessionNo, content };
  const readSet = [termPair(term)];
  let payload: CommitPayload;
  if (existing) {
    const checkpoint = { kind: 9, id: existing.checkpointId };
    readSet.push({ key: checkpoint, revision: existing.revision });
    payload = { reviseSessionCheckpoint: { checkpoint, state } };
  } else {
    payload = { createSessionCheckpoint: state };
  }
  return { kbUuid: term.kbUuid, readSet, payload };
}

async function readKbIdentity(kb: string, token: string): Promise<string> {
  const identity = await api(kb, "/meta?identity=true", "GET", undefined, token);
  if (identity.kb !== kb || !/^[a-f\d]{32}$/.test(identity.kbUuid)) {
    throw new Error("服务器未返回有效的知识库标识。");
  }
  return identity.kbUuid;
}

export async function createTermCommand(kb: string, token: string, termCreate: TermCreate) {
  const kbUuid = await readKbIdentity(kb, token);
  const payload: SubmitPayload = { termCreate };
  return { kbUuid, readSet: [], payload };
}

export async function frontierCommand(kb: string, token: string, known: boolean, entry: {
  concept: string; source: string; reason: string; priority: string;
}) {
  const priority = ["high", "medium", "low"].indexOf(entry.priority);
  if (priority < 0 || !entry.concept.trim() || !entry.reason.trim()) {
    throw new Error("请输入待探索概念、原因和优先级。");
  }
  const kbUuid = await readKbIdentity(kb, token);
  const term = known ? await readTerm(kb, entry.concept, token) : null;
  if (term && term.kbUuid !== kbUuid) throw new Error("知识库标识已变化。");
  const payload: CommitPayload = {
    createFrontier: {
      conceptRefs: term ? [termKey(term)] : [],
      status: 0, priority,
      text: JSON.stringify({ concept: entry.concept, source: entry.source, reason: entry.reason }),
    },
  };
  return { kbUuid, readSet: term ? [termPair(term)] : [], payload };
}

type Command = { kbUuid: string; readSet: RevPair[]; payload: unknown };
type WriteMethod = "SubmitProposal" | "ReviewProposal" | "CommitCommand";
type Pending = { method: WriteMethod; identity: string; token: string; envelope: Envelope; payload: unknown };

export class NativeWriter {
  private pending: Pending | undefined;
  get hasPending() { return this.pending !== undefined; }

  async write(method: WriteMethod, identity: string, token: string, prepare: () => Promise<Command>) {
    if (!token) throw new Error("请先填写写入凭据。");
    const previous = this.pending;
    if (previous && (previous.method !== method || previous.identity !== identity || previous.token !== token)) {
      throw new Error("上一条请求的结果尚未确认。请恢复原输入和凭据后重试。");
    }
    if (!previous) {
      const { kbUuid, readSet, payload } = await prepare();
      const requestId = crypto.randomUUID();
      const hello = await api("", service + "Handshake", "POST",
        { envelope: { kbUuid, protocolVersion: 1, requestId } }, token);
      if (!hello.capabilities?.includes("read")) {
        throw new Error("写入凭据需要读取权限，才能在响应丢失时查询原请求。");
      }
      this.pending = {
        method, identity, token, payload,
        envelope: { kbUuid, readSet, protocolVersion: 1, requestId },
      };
    }
    const request = this.pending!;
    try {
      if (previous) {
        const { kbUuid, protocolVersion, requestId } = request.envelope;
        const lookup = await api("", service + "GetRequestOutcome", "POST",
          { envelope: { kbUuid, protocolVersion, requestId } }, token);
        if (lookup.found?.recorded) {
          this.pending = undefined;
          return { alreadyCommitted: true, recorded: lookup.found.recorded, status: lookup.found.originalStatus };
        }
        if (!lookup.absent) throw new Error("服务器暂时无法确认该请求的结果。");
      }
      let result;
      try {
        result = await api("", service + method, "POST",
          { envelope: request.envelope, payload: request.payload }, token);
      } catch (error) {
        if (error instanceof ApiError && error.status >= 400 && error.status < 500) {
          this.pending = undefined;
        }
        throw error;
      }
      const success = result.outcome?.committed ?? result.outcome?.alreadyCommitted;
      if (!success?.recorded) throw new Error("服务器暂时无法确认该请求的结果。");
      this.pending = undefined;
      return {
        recorded: success.recorded,
        status: success.review?.status ?? success.originalStatus,
        proposal: success.submit?.proposal,
      };
    } catch (error) {
      if (!this.pending) throw error;
      throw new Error(`请求结果尚未确认（${request.envelope.requestId}）。请保留原输入并重试。`);
    }
  }
}
