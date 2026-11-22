#!/usr/bin/env python3
"""Generate the digest-assembly fixtures from the frozen D10 golden binaries.

Derivation rules come from docs/design/storage-digests.md only:
  4.4.1-4.4.4  state kinds, natural keys, entry layout, revision convention
  4.4.3        body = the event's fields in tag order, minus EXACTLY tags 1 and 13
  4.4.6        Genesis 224 splits into KbMetadata(12)+RuleConfig(13); 163 -> RuleConfig
  4.5          graph: terms(id,layer) by numeric id, then assertions by numeric id
  4.6          bookkeeping: outcomes / imports / counters, u128 watermarks
  7.4-7.5b     the expected digests, asserted below against the two independent sources

Input is the D10 binary set (frozen, hash-listed in PROVENANCE.md). No Rust encoder and
no import of gen_vectors.py is used; gen_vectors.py's printed digests are only compared
against, never consumed.
"""
import hashlib, json, pathlib, struct

# `D10` is the accepted contract capture (session evidence, not a test dependency).
# `OUT` defaults to this script's own directory, which is also where the tests read from.
import sys
D10 = pathlib.Path(sys.argv[1] if len(sys.argv) > 1
                   else '/tmp/sage-db-p1.2yjLG4/format-contract/DELIVERY-10')
V = D10 / 'evidence' / 'vectors'
OUT = pathlib.Path(__file__).parent
OUT.mkdir(parents=True, exist_ok=True)

u16 = lambda v: struct.pack('<H', v)
u32 = lambda v: struct.pack('<I', v)
u64 = lambda v: struct.pack('<Q', v)
u128 = lambda v: struct.pack('<QQ', v & (2**64 - 1), v >> 64)

def rdu128(b):
    lo, hi = struct.unpack('<QQ', b)
    assert hi == 0, 'every fixture watermark is below 2^64'
    return lo

def dig(domain, data):
    return hashlib.sha256(b"sage-db/v1/" + domain.encode() + b"\x00"
                          + struct.pack('>Q', len(data)) + data).digest()

def body_of(path):
    b = path.read_bytes()
    assert b[:4] == b'SGLB' and b[32:34] == b'\x01\x00'
    n = struct.unpack_from('<I', b, 36)[0]
    body = b[48:48 + n]
    assert hashlib.sha256(b[:-32]).digest() == b[-32:], path
    return body

def fields_at(body, off, n):
    out = []
    for _ in range(n):
        t, fl, ln = struct.unpack_from('<HHI', body, off)
        off += 8
        assert fl == 0 and ln != 0
        out.append((t, body[off:off + ln]))
        off += ln
    return out, off

def parse_tx(body):
    n = struct.unpack_from('<H', body, 0)[0]
    hdr, off = fields_at(body, 2, n)
    events = []
    while off < len(body):
        tag, flags, nf = struct.unpack_from('<HHH', body, off)
        ev, off = fields_at(body, off + 6, nf)
        events.append((tag, ev))
    return hdr, events

def body_bytes(fields, allowed=None):
    keep = (lambda t: t not in (1, 13)) if allowed is None else (lambda t: t in allowed)
    return b''.join(u16(t) + u16(0) + u32(len(v)) + v
                    for t, v in sorted(fields) if keep(t))

def entry(kind, key, rev, body):
    return u16(kind) + u32(len(key)) + key + u64(rev) + u32(len(body)) + body

def state_payload(entries):
    """Canonical payload: kind numeric, then key bytes (4.4.2)."""
    return u32(len(entries)) + b''.join(
        entry(*e) for e in sorted(entries, key=lambda e: (e[0], e[1])))

def state_payload_in_order(entries):
    """Same bytes with the caller's order preserved, for order discriminators only."""
    return u32(len(entries)) + b''.join(entry(*e) for e in entries)

def graph_payload(terms, assertions):
    out = u32(len(terms)) + b''.join(u32(i) + bytes([l]) for i, l in terms)
    out += u32(len(assertions))
    for aid, rev, frm, rel, tgt, conf in assertions:
        out += u128(aid) + u64(rev) + u32(frm) + u16(rel) + u32(tgt) + u16(conf)
    return out

def bookkeeping_payload(outcomes, imports, counters):
    out = u32(len(outcomes))
    for principal, request, pdig, rkind, rref, cver in outcomes:
        out += u32(len(principal)) + principal + u32(len(request)) + request
        out += pdig + u16(rkind) + rref + u64(cver)
    out += u32(len(imports))
    for sid, stage, sdig, plan, counts, complete in imports:
        out += u32(len(sid)) + sid + u16(stage) + sdig + plan
        out += u32(len(counts)) + b''.join(u16(k) + u64(c) for k, c in counts)
        out += bytes([1 if complete else 0])
    out += u32(len(counters))
    for space, hwm in counters:
        out += u16(space) + u128(hwm)
    return out

GENESIS_KB = {2, 7, 8, 14, 144, 145, 146, 147, 148}
GENESIS_RULES = {2, 7, 8, 14, 160, 161, 162, 163, 166, 196}

# ---------------- v1 genesis ----------------
gb = body_of(V / 'golden.genesis.tx.sgl')
ghdr, gev = parse_tx(gb)
gen = next(fs for t, fs in gev if t == 224)
kb_body = body_bytes(gen, GENESIS_KB)
rules_body = body_bytes(gen, GENESIS_RULES)
gen_counters = sorted((struct.unpack('<H', dict(fs)[164])[0], rdu128(dict(fs)[165]))
                      for t, fs in gev if t == 176)
sp1 = state_payload([(12, b'', 1, kb_body), (13, b'', 1, rules_body)])
gp1 = graph_payload([], [])
bk1 = bookkeeping_payload([], [], gen_counters)

# ---------------- v2 ordinary ----------------
v2b = body_of(V / 'golden.v2.tx.sgl')
v2hdr, v2ev = parse_tx(v2b)
term1_fields = next(fs for t, fs in v2ev if t == 1)
term1_body = body_bytes(term1_fields)
term1_layer = struct.unpack('<B', dict(term1_fields)[18])[0]
sp2 = state_payload([(1, u32(1), 1, term1_body), (12, b'', 1, kb_body), (13, b'', 1, rules_body)])
gp2 = graph_payload([(1, term1_layer)], [])

def outcomes(events):
    res = []
    for t, fs in events:
        if t != 192:
            continue
        f = dict(fs)
        res.append((f[176][4:], f[177][4:], f[178], struct.unpack('<H', f[179])[0],
                    f[180], struct.unpack('<Q', f[181])[0]))
    return res

def allocs(events):
    return {struct.unpack('<H', dict(fs)[164])[0]: rdu128(dict(fs)[165])
            for t, fs in events if t == 176}

v2_out = outcomes(v2ev)
counters2 = sorted({**dict(gen_counters), **allocs(v2ev)}.items())
bk2 = bookkeeping_payload(v2_out, [], counters2)

# ---------------- v3 revision + natural-key update ----------------
v3b = body_of(V / 'golden.v3.tx.sgl')
v3hdr, v3ev = parse_tx(v3b)
term3_body = body_bytes(next(fs for t, fs in v3ev if t == 2))
kb3_body = body_bytes(next(fs for t, fs in v3ev if t == 144))
sp3 = state_payload([(1, u32(1), 2, term3_body), (12, b'', 2, kb3_body), (13, b'', 1, rules_body)])
gp3 = graph_payload([(1, term1_layer)], [])
v3_out = outcomes(v3ev)
counters3 = sorted({**dict(counters2), **allocs(v3ev)}.items())
bk3 = bookkeeping_payload(v2_out + v3_out, [], counters3)

# ---------------- discriminators: one rule changed each ----------------
def fields(b):
    off, out = 0, []
    while off < len(b):
        t, fl, ln = struct.unpack_from('<HHI', b, off)
        out.append((t, b[off:off + 8 + ln]))
        off += 8 + ln
    return out

def pack(fs): return b''.join(f for _, f in fs)

kb_f, rules_f = fields(kb_body), fields(rules_body)
f163 = next(f for t, f in rules_f if t == 163)
mut163 = state_payload([(12, b'', 1, pack(sorted(kb_f + [(163, f163)], key=lambda p: p[0]))),
                        (13, b'', 1, rules_body)])
kb3_f = fields(kb3_body)
mut_drop_audit = state_payload([
    (1, u32(1), 2, term3_body),
    (12, b'', 2, pack([p for p in kb3_f if p[0] not in (5, 6)])),
    (13, b'', 1, rules_body)])
rec_key = u16(1) + u16(0) + u32(18) + u16(1) + u128(1)
changed = u16(13) + u16(0) + u32(8) + u32(2) + u16(17) + u16(7)
mut_keep_1_13 = state_payload([
    (1, u32(1), 2, pack(sorted(fields(term3_body) + [(1, rec_key), (13, changed)],
                              key=lambda p: p[0]))),
    (12, b'', 2, kb3_body),
    (13, b'', 1, rules_body)])

def atom_body(atom):
    a = atom.encode()
    return u16(2) + u16(0) + u32(8) + u64(1) + u16(17) + u16(0) + u32(len(a)) + a

lo, hi = 0x00FF, 0x0100
syn_key_bytewise = state_payload([(1, u32(hi), 1, atom_body('high')),
                                  (1, u32(lo), 1, atom_body('low'))])
mut_key_numeric = state_payload_in_order([(1, u32(lo), 1, atom_body('low')),
                                          (1, u32(hi), 1, atom_body('high'))])
assert mut_key_numeric != syn_key_bytewise, 'the order discriminator must discriminate'
syn_graph_id = graph_payload([(lo, 2), (hi, 0)],
                             [(0x00FF, 1, lo, 3, hi, 2), (0x0100, 4, hi, 0, lo, 1)])
mut_graph_bytewise = graph_payload([(hi, 0), (lo, 2)],
                                   [(0x0100, 4, hi, 0, lo, 1), (0x00FF, 1, lo, 3, hi, 2)])

def parse_bk(p):
    off = 0
    n = struct.unpack_from('<I', p, off)[0]; off += 4
    outs, imps, cnts = [], [], []
    for _ in range(n):
        s = off
        a = struct.unpack_from('<I', p, off)[0]; off += 4 + a
        b = struct.unpack_from('<I', p, off)[0]; off += 4 + b
        off += 32 + 2 + 32 + 8
        outs.append(p[s:off])
    m = struct.unpack_from('<I', p, off)[0]; off += 4
    for _ in range(m):
        s = off
        a = struct.unpack_from('<I', p, off)[0]; off += 4 + a
        off += 2 + 32 + 32
        c = struct.unpack_from('<I', p, off)[0]; off += 4 + c * 10 + 1
        imps.append(p[s:off])
    k = struct.unpack_from('<I', p, off)[0]; off += 4
    for _ in range(k):
        s = off
        off += 18
        cnts.append(p[s:off])
    assert off == len(p)
    return outs, imps, cnts

def emit_bk(outs, imps, cnts):
    return (u32(len(outs)) + b''.join(outs) + u32(len(imps)) + b''.join(imps)
            + u32(len(cnts)) + b''.join(cnts))

outs3, imps3, cnts3 = parse_bk(bk3)
outs2, imps2, cnts2 = parse_bk(bk2)
assert len(outs3) == 2 and len(cnts3) == 9 and len(outs2) == 1
mut_single_outcome_v3 = emit_bk(outs3[1:], imps3, cnts3)
mut_single_outcome_v2 = emit_bk(outs2[:1], imps2, cnts2)
assert mut_single_outcome_v2 == bk2, 'v2 has exactly one outcome, so this mutant equals it'

sid = b'import-2026-11-17'
counts = [(1, 12), (2, 3)]
one_import = (u32(len(sid)) + sid + u16(2) + bytes.fromhex('bb' * 32) + bytes.fromhex('cc' * 32)
              + u32(len(counts))
              + b''.join(u16(k) + u64(c) for k, c in counts) + bytes([1]))
syn_bk_import = emit_bk(outs3, [one_import], cnts3)
syn_bk_hwm = u32(0) + u32(0) + u32(1) + u16(1) + u128(1 << 64)
mut_bk_hwm_u64 = u32(0) + u32(0) + u32(1) + u16(1) + u64(0)

files = {
    'body.genesis.kb-metadata.bin': kb_body,
    'body.genesis.rule-config.bin': rules_body,
    'body.v2.term-1.bin': term1_body,
    'body.v3.term-1.bin': term3_body,
    'body.v3.kb-metadata.bin': kb3_body,
    'payload.state.genesis.bin': sp1,
    'payload.state.v2.bin': sp2,
    'payload.state.v3.bin': sp3,
    'payload.graph.genesis.bin': gp1,
    'payload.graph.v2.bin': gp2,
    'payload.graph.v3.bin': gp3,
    'payload.bookkeeping.genesis.bin': bk1,
    'payload.bookkeeping.v2.bin': bk2,
    'payload.bookkeeping.v3.bin': bk3,
    'payload.state.genesis.163-both-bodies.mutant.bin': mut163,
    'payload.state.v3.drop-audit-tags.mutant.bin': mut_drop_audit,
    'payload.state.v3.keep-tags-1-13.mutant.bin': mut_keep_1_13,
    'payload.state.key-bytewise.synthetic.bin': syn_key_bytewise,
    'payload.state.key-numeric.mutant.bin': mut_key_numeric,
    'payload.graph.id-numeric.synthetic.bin': syn_graph_id,
    'payload.graph.id-bytewise.mutant.bin': mut_graph_bytewise,
    'payload.bookkeeping.v3.single-outcome.mutant.bin': mut_single_outcome_v3,
    'payload.bookkeeping.v3.one-import.synthetic.bin': syn_bk_import,
    'payload.bookkeeping.hwm-2p64.synthetic.bin': syn_bk_hwm,
    'payload.bookkeeping.hwm-2p64-u64.mutant.bin': mut_bk_hwm_u64,
}
for name, data in files.items():
    path = OUT / name
    # Refuse to leave an identical-looking but different fixture on disk: rewriting is
    # allowed, silent drift is not.
    if path.exists() and path.read_bytes() != data:
        print(f'REWRITING {name} (content differs from the derivation)', file=sys.stderr)
    path.write_bytes(data)

def domain_of(name):
    return 'state' if '.state.' in name else 'graph' if '.graph.' in name else 'bookkeeping'

# The nine accepted values, quoted from storage-digests 7.4-7.5b. They must match both
# this extractor and gen_vectors.py's out4.txt; a mismatch means this script is wrong.
EXPECTED = {
    ('state', 'genesis'): 'b6422a4ac1ef53598c2259991facb9bf16079b479aee20c97ced428859aacc88',
    ('state', 'v2'): '381bd739b2978e45f457dcb0452fe88d96d8c653a8b620ad018ec4d638c89ad4',
    ('state', 'v3'): 'ca16fcb6337bd3d8e9f2e709a1a74c11f95b39f3bcd440ad7cc7b4d04cb5abf7',
    ('graph', 'genesis'): '35efdf863b38edc1ac307e20f95b7990e35c3f1c5505eff5c9f4b4ddafb42d4e',
    ('graph', 'v2'): '7c9f89cc6e83bb47fdfabaef8ce13b8435eba7c02412ec8f8afae85372f0210a',
    ('graph', 'v3'): '7c9f89cc6e83bb47fdfabaef8ce13b8435eba7c02412ec8f8afae85372f0210a',
    ('bookkeeping', 'genesis'): '756daff9afe2bad1df7caf317f0c58d72a9013e172b0de5e58fc4e5a935ae902',
    ('bookkeeping', 'v2'): '45099a57f1c3f65138f6839dceb802f580f18fb2ada25f61827b861a76bbfc68',
    ('bookkeeping', 'v3'): '377bf5c189b55d0989e9950e30632ea561a0ec0cfef974a767d465a52fbfdede',
}
accept = {}
for (domain, generation), want in EXPECTED.items():
    got = dig(domain, files[f'payload.{domain}.{generation}.bin']).hex()
    assert got == want, (domain, generation, got, want)
    accept[f'{domain}.{generation}'] = got

print(json.dumps({
    'accepted': accept,
    'len': {n: len(d) for n, d in sorted(files.items())},
    'sha256': {n: hashlib.sha256(d).hexdigest() for n, d in sorted(files.items())},
}, indent=2))
