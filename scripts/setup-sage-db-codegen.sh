#!/usr/bin/env bash
# Build tools only. No application, credential or knowledge-base configuration is read.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:?usage: setup-sage-db-codegen.sh ABSOLUTE_DIR [--compiler-only]}"
MODE="${2:-full}"
[[ "$DEST" = /* && "$DEST" != *$'\n'* && "$DEST" != *$'\r'* ]] || { echo 'absolute single-line tooling directory required' >&2; exit 2; }
[[ "$MODE" = full || "$MODE" = --compiler-only ]] || { echo 'unknown setup mode' >&2; exit 2; }
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) PLATFORM=linux-x86_64; SHA=bc8211ce760bd43ee21ddc145d6d9dbaeeabae205267a79d9054a240e367d4b4 ;;
  Linux/aarch64) PLATFORM=linux-aarch_64; SHA=4a00ec5e256d20a3deadd9e77d56da0ac04c72367c3c959f6d08e110a368400a ;;
  Darwin/arm64) PLATFORM=osx-aarch_64; SHA=b6bc4afdcb880124bf342851d05155b6e3d9b6e661236d87b9c614250d26ae00 ;;
  Darwin/x86_64) PLATFORM=osx-x86_64; SHA=2847d952ecd1c466769ae3ca319c9cd34c3613542eba335dc9b02c49537f6c70 ;;
  *) echo 'no pinned compiler asset for this build host' >&2; exit 2 ;;
esac
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd -P)"
ZIP="$DEST/protoc-36.0-$PLATFORM.zip"
curl --fail --location --silent --show-error --max-filesize 32000000 \
  "https://github.com/protocolbuffers/protobuf/releases/download/v36.0/protoc-36.0-$PLATFORM.zip" --output "$ZIP"
python3 - "$ZIP" "$SHA" "$DEST" <<'PY'
import sys,pathlib,hashlib,zipfile
z=pathlib.Path(sys.argv[1]); data=z.read_bytes()
if len(data)>32_000_000 or hashlib.sha256(data).hexdigest()!=sys.argv[2]: raise SystemExit('pinned protoc checksum mismatch')
root=pathlib.Path(sys.argv[3]); total=0
with zipfile.ZipFile(z) as archive:
 if len(archive.infolist())>1024: raise SystemExit('too many compiler archive entries')
 for item in archive.infolist():
  p=pathlib.PurePosixPath(item.filename)
  if p.is_absolute() or '..' in p.parts: raise SystemExit('invalid compiler archive path')
  total+=item.file_size
  if item.file_size>16_000_000 or total>64_000_000: raise SystemExit('compiler archive extent exceeded')
  if item.is_dir():continue
  out=root/p;out.parent.mkdir(parents=True,exist_ok=True);out.write_bytes(archive.read(item))
(root/'bin/protoc').chmod(0o755)
print('PROTOC_ARCHIVE_SHA256='+sys.argv[2])
PY
PROTOC="$DEST/bin/protoc"
[[ "$("$PROTOC" --version)" = 'libprotoc 36.0' ]] || { echo 'wrong protoc version' >&2; exit 1; }
printf 'export SAGE_DB_PROTOC=%q\n' "$PROTOC" > "$DEST/env.sh"
if [[ "$MODE" = full ]]; then
  mkdir -p "$DEST/plugin"
  cp "$ROOT/tools/protocol-codegen/package.json" "$ROOT/tools/protocol-codegen/pnpm-lock.yaml" "$DEST/plugin/"
  pnpm --dir "$DEST/plugin" install --frozen-lockfile --ignore-scripts
  PLUGIN="$DEST/plugin/node_modules/.bin/protoc-gen-ts_proto"
  [[ -f "$PLUGIN" ]] || { echo 'pinned TypeScript generator missing' >&2; exit 1; }
  printf 'export SAGE_DB_TS_PROTO_PLUGIN=%q\n' "$PLUGIN" >> "$DEST/env.sh"
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
  printf 'SAGE_DB_PROTOC=%s\n' "$PROTOC" >> "$GITHUB_ENV"
  if [[ "$MODE" = full ]]; then printf 'SAGE_DB_TS_PROTO_PLUGIN=%s\n' "$PLUGIN" >> "$GITHUB_ENV"; fi
fi
printf 'CODEGEN_ENV=%s\n' "$DEST/env.sh"
