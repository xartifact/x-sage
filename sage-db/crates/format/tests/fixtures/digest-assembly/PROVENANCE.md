# Provenance — digest-assembly fixtures

These bytes are **derived from the accepted D10 golden binaries**, not from any Rust
encoder. `GENERATE.py` (in this directory) re-derives every file from those binaries and
asserts the nine accepted digest values before writing anything; it imports neither
`gen_vectors.py` nor the production crate.

## Sources (immutable, `/tmp` session evidence — bundled here so no test depends on them)

| input | bytes | sha256 |
|---|---|---|
| `golden.genesis.tx.sgl` | 958 | `2a185bd2dc8c4fdefa5d9cbd30260bb1faf84c84ebdd0dea85aed4cceab2f5f4` |
| `golden.v2.tx.sgl` | 821 | `3ffc2989001f694fbc63f0182dc32657cd859697455987f0fc74bbe55237b109` |
| `golden.v3.tx.sgl` | 1330 | `2b0fafeccb14b3c36085aecd34650a83038a45d0287fe49de57dc40be135900a` |
| `docs/storage-digests.md` | 52621 | `c7d4b933f6bca7da63f4688c0fb48d3cb335e066d986dae2d42210f531e06bf4` |
| `evidence/vectors/gen_vectors.py` (compared against, not consumed) | 34259 | `6a9090d81aaeb9b4e49cb12fd1128a99f444861ad4152d24338deca5c1387b82` |
| `evidence/vectors/out4.txt` (compared against, not consumed) | 11844 | `ecd3400755a80d3b2bdeba33df4eb6b65f87eed081945a426a8311bf04391249` |

Path prefix at capture: `/tmp/sage-db-p1.2yjLG4/format-contract/DELIVERY-10/`. The tx
file hashes above are also the `tx_file_sha256` values quoted in §7.4–7.5b, which is an
independent consistency check on the capture.

## Derivation rules applied

| rule | source | effect on the fixtures |
|---|---|---|
| body = event fields in tag order, minus **exactly** tags 1 and 13 | §4.4.3 | every `body.*.bin` |
| Genesis 224 splits into KbMetadata(12) + RuleConfig(13); tag 163 → RuleConfig only | §4.4.6 | `body.genesis.*`, `payload.state.genesis.bin` |
| an ordinary revision carries a **complete after-image** (tags 5/6 included) | §4.4.3, §7.5b | `body.v3.kb-metadata.bin` |
| state order = kind numeric, then **key bytes** | §4.4.2 | all `payload.state.*` |
| graph order = **numeric id**; `revision` is a semantic input | §4.5 | all `payload.graph.*` |
| bookkeeping = outcomes / imports / counters; `high_water_mark` is `u128` | §4.6 | all `payload.bookkeeping.*` |

## File classes

* `payload.*.bin` (no suffix) — canonical payloads for the three accepted generations.
* `body.*.bin` — record bodies as they enter `state_digest`.
* `*.mutant.bin` — **discriminators**, never expected values for production code. Each
  changes exactly one rule so the corresponding test can separate a correct assembler from
  a plausible wrong one (`163` in both bodies, audit tags dropped, tags 1/13 kept, numeric
  key order, bytewise id order, single outcome, `u64` watermark).
* `*.synthetic.bin` — payloads for rules that no D10 vector exercises (two record keys of
  one kind, an import row, a `2^64` watermark). Marked synthetic in the file name so no
  report can present them as accepted vectors.

## Accepted values reproduced (storage-digests §7.4–7.5b)

| domain | genesis | v2 | v3 |
|---|---|---|---|
| `state` | `b6422a4a…aacc88` | `381bd739…89ad4` | `ca16fcb6…b5abf7` |
| `graph` | `35efdf86…2d4e` | `7c9f89cc…210a` | `7c9f89cc…210a` |
| `bookkeeping` | `756daff9…ae902` | `45099a57…bfc68` | `377bf5c1…fdede` |

Full values are asserted in `GENERATE.py` and again in `tests/digest_vectors.rs`, so a
regenerated fixture that drifts fails before any test reads it.

## Fixture manifest

| file | bytes | sha256 |
|---|---|---|
| `GENERATE.py` | 13359 | `97d83c377f141cbd7c96e9190fb8a63b9f77bcddd4d0a63baa00697cace231ab` |
| `body.genesis.kb-metadata.bin` | 175 | `2a2f0bd463e79d88dddbcaa17d08a9d02eeb5fe8b987fbceca0760d28f5d7c85` |
| `body.genesis.rule-config.bin` | 122 | `f8594da2638a842e123103b716b745fefc5fc540de76c88cd045c8b831b56f25` |
| `body.v2.term-1.bin` | 233 | `12f486f95a868d4fc4bae3f18247b5ea439c8de2d51f5ea17fd7ff3fa2922207` |
| `body.v3.kb-metadata.bin` | 248 | `398eb1e4559a4730493e35ff65727779b050f3ba2fb1af67fe22b746546af81c` |
| `body.v3.term-1.bin` | 460 | `63b759a97a791e9fcd6ea3b2365ff179def2062ca917b4d3aa1fa55864f82ab1` |
| `payload.bookkeeping.genesis.bin` | 174 | `7b808f1616bf897b7efe9a7cef5620e92b10c95a9d2f237e1ea3013555073cdc` |
| `payload.bookkeeping.hwm-2p64-u64.mutant.bin` | 22 | `5d363f97d2a7dde9ba8ca4ddeee673c3f767c06a3292bc242d1ce3ec0b21aeae` |
| `payload.bookkeeping.hwm-2p64.synthetic.bin` | 30 | `eaec859b988fb01be7ec9077e7420477ef6ed14b99aed29fddaf523c24a834ee` |
| `payload.bookkeeping.v2.bin` | 267 | `5d9b90889ee5663004602b64bdeb5cb6a54267465469ffbad573d819f41018c6` |
| `payload.bookkeeping.v3.bin` | 360 | `4bb74495ba63fce6627889906373387a40be6777418002c72a7a900d784a45a9` |
| `payload.bookkeeping.v3.one-import.synthetic.bin` | 472 | `3881b72f124a831391ef85ea30355fadb2cb518c8df1f49c99f679da6bcee88f` |
| `payload.bookkeeping.v3.single-outcome.mutant.bin` | 267 | `f8c19513f20edb04afa3cbdfda5d52a8eabdae3c6205a451ef964f4dbbbf677d` |
| `payload.graph.genesis.bin` | 8 | `af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc` |
| `payload.graph.id-bytewise.mutant.bin` | 90 | `12e3760d140f368b2c3a66c554b9759b1d6afe6d06fad95c0d3a0b49d0419cb9` |
| `payload.graph.id-numeric.synthetic.bin` | 90 | `832b9542d3cae120760defb06d3c4871b4f3883434a7e559c8ee4ac8896347ff` |
| `payload.graph.v2.bin` | 13 | `c705fdce94b04877ccbcedcd6c9c660ea5848505b4eb2dd44c1dd4691fc230f7` |
| `payload.graph.v3.bin` | 13 | `c705fdce94b04877ccbcedcd6c9c660ea5848505b4eb2dd44c1dd4691fc230f7` |
| `payload.state.genesis.163-both-bodies.mutant.bin` | 377 | `5fc933891cde1cbab3d39c16aa69d32d59bb49a1fa6ad7b048366a40f4ad975a` |
| `payload.state.genesis.bin` | 337 | `51812b14f583f520c18a479b2e7fcf1185713bc34399dd969058f042631eec94` |
| `payload.state.key-bytewise.synthetic.bin` | 103 | `6d4e9717db080eb53eb783856248681359ef9749c64b2abf3e3c4317a1e19ad9` |
| `payload.state.key-numeric.mutant.bin` | 103 | `59f7acd0503ad2c77bcfee319b09e13b2af73a08f919aa086277943a0beaf8a3` |
| `payload.state.v2.bin` | 592 | `5016fd9f89a14a37ed3c0460a1234d973ce50fbcf4020cb39fde7a7dc6cf811f` |
| `payload.state.v3.bin` | 892 | `8437d65a4e785438049adae4091c0f69f9d464eca8be49c2e8aee677ca143b24` |
| `payload.state.v3.drop-audit-tags.mutant.bin` | 864 | `d5abb7613c6866dc3f95ef6ea6db2c3f4351d495f5c513efec3cf0c58dc37484` |
| `payload.state.v3.keep-tags-1-13.mutant.bin` | 934 | `43c7c0c9a8af96d3ad16013d69c580112bcca2c6e1d6bc84c8291c5023143377` |

This manifest excludes itself, as a file cannot contain its own digest; its identity is
recorded externally in the delivery report.

## What these fixtures do **not** establish

* No `result` or `search` domain payload: §4.9 defers those to P3 and there is no accepted
  vector.
* No platform durability claim: these are pure byte computations, no I/O.
* No per-event encode vector: the bodies come from the golden tx files, so they prove the
  digest's consumption of accepted bytes, not that a writer reproduces every event kind.
