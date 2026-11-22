# Provenance — record-projection fixtures

18 event envelopes, one event per file. **17 are extracted verbatim from the accepted D10 golden
transactions; 1 is independently hand-built.** The two origins are listed separately because they
carry different authority: the D10 extracts are frozen contract bytes, the hand-built asset is a
legal construction whose only authority is the §7.2 row it follows.

## A. Extracted from D10 (17 files) — source authority

Source prefix at capture: `/tmp/sage-db-p1.2yjLG4/format-contract/DELIVERY-10/evidence/vectors/`

| source file | sha256 | events | fixtures |
|---|---|---|---|
| `golden.genesis.tx.sgl` | `2a185bd2dc8c4fdefa5d9cbd30260bb1faf84c84ebdd0dea85aed4cceab2f5f4` | 10 | `genesis.e0…e9` |
| `golden.v2.tx.sgl` | `3ffc2989001f694fbc63f0182dc32657cd859697455987f0fc74bbe55237b109` | 3 | `v2.e0…e2` |
| `golden.v3.tx.sgl` | `2b0fafeccb14b3c36085aecd34650a83038a45d0287fe49de57dc40be135900a` | 4 | `v3.e0…e3` |

Event tags, in file order (verified by re-parsing each golden file, not from memory):

* genesis: `224, 176 ×9`
* v2: `1, 176, 192`
* v3: `2, 144, 176, 192`

Extraction rule: the transaction body was walked with the D10 §4 framing (header fields, then
`tag:u16 | flags:u16 | field_count:u16` per event), and each event was re-emitted into a
standalone envelope `tag:u16 | flags:u16 | count:u16 | fields…` with its own fields in their
original ascending order. No field value was altered, added or dropped — the extraction is a
re-framing of the same bytes, so a fixture's fields are byte-identical to the D10 event's fields.

| fixture | sha256 |
|---|---|
| `genesis.e0.tag224.bin` | `d2fe591da77236f3810ae9f91de8c130279219f246af024216b04029a39cab97` |
| `genesis.e1.tag176.bin` | `7ccd85364771c634831d9dd135362d49de1a9131aa4e0c8e06370c759d6ded9e` |
| `genesis.e2.tag176.bin` | `c143928f19962978f2d8b6ef45f2e7b6bd9c6ed12295cfa188fb1854474afd17` |
| `genesis.e3.tag176.bin` | `3aff523caefc338b9ea1e654f4c377d1e8ee1b2c6459276256d78ede6063a609` |
| `genesis.e4.tag176.bin` | `0da6b50fbf88c844bb20c7b057365756d8cae3fbab0a08784ee2266ceb8806ef` |
| `genesis.e5.tag176.bin` | `da5aaadca3a1b83113c4ee9ba78dfdadee60b3273e6b80c399885158e19134e7` |
| `genesis.e6.tag176.bin` | `eb5fd55d1c297f3728aba3e266929a4d210d199a57c1382646991c3b2356c718` |
| `genesis.e7.tag176.bin` | `fd314a5fb1434423f106440f206188cfe8f3e71ba9baa3a70a3d58f643b62f93` |
| `genesis.e8.tag176.bin` | `36c0e3c73a43012b74eb2ae036c4bdfa194d398013a11e301e4fe887a772d7f3` |
| `genesis.e9.tag176.bin` | `2bdc53cca2590ff36c4e2bd8f28fa53dac246b435cb277acfc127841dbc58997` |
| `v2.e0.tag1.bin` | `1626dd0627f1eb3620cfeb78054a531e96955ef58d5fb7a7077a17ef10490942` |
| `v2.e1.tag176.bin` | `270dc76fac95976657220b74888f5f4545800873c8e75dd1acad2bc730ad739a` |
| `v2.e2.tag192.bin` | `3beaa460e25741e4c7cbfe2c9f24032e4d157ea95ef23fb7ae88628320eb56d4` |
| `v3.e0.tag2.bin` | `0c800d404d319a3dc977930ae7c8730b1636f9421440bca014c9a9815a5620c9` |
| `v3.e1.tag144.bin` | `74376eb45cc1d9860370e94111a991f535aa8ccba32b831a6a73c73915b0baa6` |
| `v3.e2.tag176.bin` | `e85741f465bf50052ec0fe9918e0d9a0cf9064a490f02ca2b1661ab0c4e9fcb3` |
| `v3.e3.tag192.bin` | `cf4b182b597a5f358813a80a030e7ad2fa10868cacc1ed8cd9345e154b3a3567` |

## B. Independently hand-built (1 file) — NOT D10

| fixture | sha256 | authority |
|---|---|---|
| `asset.e0.tag64.bin` | `463a4c0f08e7d4b2ca9e5bae640fba63205bb067d18918f15aebbc02d48997fd` | legal construction against D10 row 64 |

No D10 golden transaction contains an `AssetDeclared` event, so there was no frozen byte set to
extract. The file was built from §7.2's row 64 field set with fields emitted in ascending tag
order:

| tag | field | value used |
|---|---|---|
| 6 | `actor_origin` | `2` (`unavailable` — no fabricated actor) |
| 8 | `created_at` | `1763424000000000` (the same instant the golden vectors use) |
| 84 | `sha256` | bytes `00 01 … 1f` — the natural key, deliberately not a blob reference |
| 85 | `media_type` | `0` (text) |
| 86 | `byte_size` | `1234` |

Its purpose is one rule only: **Asset's revision is the constant `1`, not an event value** (§4.4.4),
and its key is the raw 32-byte `sha256` (§4.4.2). It is not evidence about any accepted file, and
no digest assertion depends on it.

## C. What the fixtures do and do not establish

* D10 extracts establish the projection's consumption of **accepted contract bytes**: the three
  positive digest chains (`b6422a4a…`, `381bd739…`, `ca16fcb6…`) are computed from these.
* The hand-built asset establishes one rule with independent input, not a golden value.
* Neither class establishes durability, and no fixture is a Store or filesystem input.
