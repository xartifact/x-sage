#!/usr/bin/env bash
# x-sage CLI + Skill 安装脚本（联网，无需克隆仓库）。
#
#   curl -fsSL https://raw.githubusercontent.com/xartifact/x-sage/main/scripts/install-cli.sh | bash
#
# 设计约束（改这里之前先读 apps/cli/src/build-info.ts 的布局约定）：
#
#   1. 先把 REF 解析成明确的 40 位 commit，再从该 commit 下载所有文件。
#      按浮动 REF 逐文件下载会在上游推送时混版：index.ts 来自旧 commit、
#      build-info.ts 来自新 commit，两者不兼容也不报错。
#   2. 解析失败即安装失败，旧版本原样保留。不写 `unknown`，不做伪成功。
#   3. 每个 commit 装进自己的目录，`current` 是唯一的切换点。切换 = 重指软链接，
#      因此「改名 → 写 wrapper → 装 skill」任一步失败都能整体回滚。
#   4. `bin/x-sage` 永不通过 `>` 重定向写入。目标是软链接时，`>` 会写穿它并
#      覆盖链接指向的源码（开发机上 ~/.bun/bin/x-sage 正指向工作树）。
#      改为写同目录临时文件再 `mv`：替换目录项，不跟随链接。
#   5. Skill 目录同样不写穿外链：目标是软链接时先移走链接本身，再建真实目录。
#   6. 不写、不改 `~/.config/x-sage/config.toml`。
#
# 安装布局：
#   <prefix>/bin/x-sage                              wrapper（真实文件）
#   <prefix>/lib/x-sage/current -> versions/<sha>    当前版本（原子切换点）
#   <prefix>/lib/x-sage/versions/<sha>/cli/src/*.ts
#   <prefix>/lib/x-sage/versions/<sha>/skills/x-sage-kb/
#   <prefix>/lib/x-sage/versions/<sha>/build-manifest.json
#
# 可选环境变量：
#   X_SAGE_REF          分支、tag 或完整 commit（默认 main）
#   X_SAGE_PREFIX       安装前缀（默认 $HOME/.local）
#   X_SAGE_BASE         源地址前缀（默认 GitHub raw），用于内网镜像
#   X_SAGE_API          GitHub API 前缀，用于解析 commit
#   X_SAGE_REPO_URL     解析 commit 用的仓库地址（默认 GitHub）
#   X_SAGE_SKILLS_DIR   Skill 安装目录（默认 $HOME/.agents/skills）
set -euo pipefail

REF="${X_SAGE_REF:-main}"
PREFIX="${X_SAGE_PREFIX:-$HOME/.local}"
BASE="${X_SAGE_BASE:-https://raw.githubusercontent.com/xartifact/x-sage}"
API="${X_SAGE_API:-https://api.github.com/repos/xartifact/x-sage}"
REPO_URL="${X_SAGE_REPO_URL:-https://github.com/xartifact/x-sage.git}"
SKILLS_DIR="${X_SAGE_SKILLS_DIR:-$HOME/.agents/skills}"

CLI_FILES="index.ts config.ts build-info.ts"
SKILL_FILES="SKILL.md"

ROOT="$PREFIX/lib/x-sage"
VERSIONS="$ROOT/versions"
CURRENT="$ROOT/current"
BIN_DIR="$PREFIX/bin"
BIN_PATH="$BIN_DIR/x-sage"
SKILL_TARGET="$SKILLS_DIR/x-sage-kb"

die() { echo "x-sage install: $*" >&2; exit 1; }

# Replace a symlink atomically with the link stored at $1.
#
# `mv src dst` is not usable here: when dst is a symlink to a directory, both
# BSD and GNU mv move src *into* the directory it points at instead of replacing
# the link. `mv -T` is GNU-only. Bun is already a prerequisite, and node's
# rename(2) is exactly "replace this directory entry", so the link itself is
# what moves — on Linux and macOS alike. Paths are passed as argv, never
# interpolated into JavaScript.
replace_link() { # $1 = staged symlink path, $2 = final path
  bun -e 'require("node:fs").renameSync(process.argv[1], process.argv[2])' "$1" "$2"
}

command -v bun >/dev/null 2>&1 || die "bun is required: https://bun.sh/docs/installation"
command -v curl >/dev/null 2>&1 || die "curl is required"

# ---------------------------------------------------------------- rollback state
STAGING=""
SWITCHED_LINK=0
SWITCHED_WRAPPER=0
OLD_LINK=""
OLD_BIN_SAVED=""
WRAPPER_TMP=""
OLD_SKILLS_BACKUP=""
SKILL_SWITCHED=0

rollback() {
  local status=$?
  [ "$status" -eq 0 ] && return 0
  echo "x-sage install: failed, rolling back" >&2

  # Skill first: it is the only step that moved a user-owned tree.
  if [ "$SKILL_SWITCHED" -eq 1 ] && [ -n "$OLD_SKILLS_BACKUP" ] && [ -e "$OLD_SKILLS_BACKUP" ]; then
    rm -rf "$SKILL_TARGET"
    mv "$OLD_SKILLS_BACKUP" "$SKILL_TARGET" 2>/dev/null || true
  fi

  # The entry point must be restored, not merely deleted: deleting it would leave
  # the previous version installed but unreachable.
  if [ "$SWITCHED_WRAPPER" -eq 1 ]; then
    if [ -n "$OLD_BIN_SAVED" ] && [ -e "$OLD_BIN_SAVED" ]; then
      mv -f "$OLD_BIN_SAVED" "$BIN_PATH" 2>/dev/null || true
    else
      rm -f "$BIN_PATH" 2>/dev/null || true
    fi
  fi

  if [ "$SWITCHED_LINK" -eq 1 ]; then
    if [ -n "$OLD_LINK" ]; then
      ln -sfn "$OLD_LINK" "$CURRENT.new" && replace_link "$CURRENT.new" "$CURRENT" || true
    else
      rm -f "$CURRENT" 2>/dev/null || true
    fi
  fi
  [ -n "$WRAPPER_TMP" ] && rm -f "$WRAPPER_TMP" 2>/dev/null
  [ -n "$STAGING" ] && rm -rf "$STAGING" 2>/dev/null
  echo "x-sage install: rolled back; the previous install is still in place" >&2
}
trap rollback ERR

# ---------------------------------------------------------------- resolve commit
# A full sha needs no lookup; anything else is resolved to one. `git ls-remote`
# is tried first: no token, no rate limit. The API is the fallback for machines
# without git.
resolve_commit() {
  local ref="$1" sha=""

  if printf '%s' "$ref" | grep -Eq '^[0-9a-f]{40}$'; then
    printf '%s' "$ref"
    return 0
  fi

  if command -v git >/dev/null 2>&1; then
    for candidate in "refs/heads/$ref" "refs/tags/$ref"; do
      sha="$(git ls-remote "$REPO_URL" "$candidate" 2>/dev/null | awk 'NR==1 {print $1}')"
      [ -n "$sha" ] && break
    done
    if printf '%s' "$sha" | grep -Eq '^[0-9a-f]{40}$'; then
      printf '%s' "$sha"
      return 0
    fi
  fi

  sha="$(curl -fsSL "$API/commits/$ref" 2>/dev/null \
    | sed -n 's/.*"sha": *"\([0-9a-f]\{40\}\)".*/\1/p' | head -1)"
  if printf '%s' "$sha" | grep -Eq '^[0-9a-f]{40}$'; then
    printf '%s' "$sha"
    return 0
  fi

  return 1
}

echo "==> resolving $REF"
COMMIT="$(resolve_commit "$REF")" || die "cannot resolve $REF to a commit; nothing was installed"
echo "    commit $COMMIT"

# ---------------------------------------------------------------- stage download
# A per-commit directory: an installed version is immutable, so `current` can be
# pointed at either one without moving files around.
STAGING="$VERSIONS/.staging.$$"
trap 'rm -rf "${STAGING:-}"' EXIT
mkdir -p "$STAGING/cli/src" "$STAGING/skills/x-sage-kb"

fetch() { # url dest
  curl -fsSL "$1" -o "$2" || die "cannot download $1; nothing was installed"
  [ -s "$2" ] || die "$1 was empty; nothing was installed"
  # A proxy or captive portal answers 200 with HTML; Bun would then fail with a
  # syntax error pointing at the wrong cause.
  head -c 1 "$2" | grep -q '[^<]' || die "$1 did not return source (got HTML?); nothing was installed"
}

for file in $CLI_FILES; do
  fetch "$BASE/$COMMIT/apps/cli/src/$file" "$STAGING/cli/src/$file"
done
for file in $SKILL_FILES; do
  fetch "$BASE/$COMMIT/skills/x-sage-kb/$file" "$STAGING/skills/x-sage-kb/$file"
done

# ---------------------------------------------------------------- verify staged
VERSION="$(curl -fsSL "$BASE/$COMMIT/apps/cli/package.json" 2>/dev/null \
  | sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' | head -1 || true)"
[ -n "$VERSION" ] || die "cannot read apps/cli/package.json at $COMMIT; nothing was installed"

# Written inside the staged tree; build-info.ts reads `<root>/build-manifest.json`
# where <root> is the directory above `cli/`.
cat > "$STAGING/build-manifest.json" <<MANIFEST
{
  "version": "$VERSION",
  "commit": "$COMMIT",
  "ref": "$REF",
  "installedAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "root": "$VERSIONS/$COMMIT"
}
MANIFEST

# Smoke 1: the CLI loads. It prints its banner and exits 1 with no arguments
# (usage is an error), so check the banner, not the status. Captured rather than
# piped into `grep -q`: under `pipefail`, grep exiting early kills bun with
# SIGPIPE and turns a working install into a failure.
SMOKE="$(bun "$STAGING/cli/src/index.ts" 2>&1 || true)"
case "$SMOKE" in
  *"x-sage CLI"*) ;;
  *) die "staged CLI does not run; nothing was installed" ;;
esac

# Smoke 2: the CLI reports the commit we resolved. This is the check that catches
# a mixed-version install, which is what a floating REF invites.
REPORTED="$(bun "$STAGING/cli/src/index.ts" --version 2>&1 || true)"
case "$REPORTED" in
  *"commit $COMMIT"*) ;;
  *) die "staged CLI reports '$REPORTED', expected commit $COMMIT; nothing was installed" ;;
esac

# Smoke 3: the skill is the one we think it is, and documents the shipped CLI.
grep -q '^name: x-sage-kb' "$STAGING/skills/x-sage-kb/SKILL.md" \
  || die "staged SKILL.md has no 'name: x-sage-kb' frontmatter; nothing was installed"
for command in 'concept body' 'refine-body' 'frontier add'; do
  grep -q "$command" "$STAGING/skills/x-sage-kb/SKILL.md" \
    || die "staged SKILL.md does not document '$command'; nothing was installed"
done

# ---------------------------------------------------------------- preflight
# Everything that can fail for permission reasons is checked before the live
# install changes: a half-switched install is worse than no install.
mkdir -p "$VERSIONS" "$BIN_DIR" "$SKILLS_DIR" || die "cannot create install directories"
[ -w "$VERSIONS" ] || die "$VERSIONS is not writable"
[ -w "$BIN_DIR" ] || die "$BIN_DIR is not writable"
[ -w "$SKILLS_DIR" ] || die "$SKILLS_DIR is not writable"

if [ -L "$SKILL_TARGET" ] && [ ! -e "$SKILL_TARGET" ]; then
  # Dangling symlink: nothing to back up, but it must not be written through.
  rm -f "$SKILL_TARGET"
fi

# ---------------------------------------------------------------- switch (atomic)
# Keep a copy of the previous version rather than deleting it: rollback is a
# single link update.
if [ -e "$CURRENT" ] && [ ! -L "$CURRENT" ]; then
  die "$CURRENT exists and is not a symlink; refusing to replace it"
fi
OLD_LINK="$(readlink "$CURRENT" 2>/dev/null || true)"

# Move the staged tree into its final, immutable location.
FINAL="$VERSIONS/$COMMIT"
if [ -e "$FINAL" ]; then
  rm -rf "$FINAL"     # same commit reinstalled: replace its own directory
fi
mv "$STAGING" "$FINAL"
STAGING=""

# Re-point `current`. The staged link is renamed over the live one, so the
# replacement is a single directory-entry update and never follows the old link.
ln -sfn "$FINAL" "$CURRENT.new"
replace_link "$CURRENT.new" "$CURRENT"
SWITCHED_LINK=1

# Wrapper: written to a temp file in the same directory, then renamed over the
# target. `> "$BIN_PATH"` would follow an existing symlink and overwrite whatever
# it points at — on a development machine that is the working tree's source.
# Save the existing entry before replacing it. It may be a symlink into another
# install (or a source tree), so it is moved, never written through.
if [ -e "$BIN_PATH" ] || [ -L "$BIN_PATH" ]; then
  OLD_BIN_SAVED="$BIN_DIR/.x-sage.previous.$$"
  mv -f "$BIN_PATH" "$OLD_BIN_SAVED"
fi
WRAPPER_TMP="$BIN_DIR/.x-sage.$$.tmp"
printf '%s\n' '#!/usr/bin/env bash' "exec bun \"$CURRENT/cli/src/index.ts\" \"\$@\"" > "$WRAPPER_TMP"
chmod +x "$WRAPPER_TMP"
mv -f "$WRAPPER_TMP" "$BIN_PATH"
WRAPPER_TMP=""
SWITCHED_WRAPPER=1

# ---------------------------------------------------------------- skill install
# The skill is installed from the same commit, so CLI and Skill cannot drift.
# A symlinked target is moved aside rather than written through: writing into it
# would edit whatever repository it points at.
if [ -e "$SKILL_TARGET" ] || [ -L "$SKILL_TARGET" ]; then
  # A symlinked target counts as "customised": writing through it would edit the
  # repository it points at.
  needs_backup=0
  if [ -L "$SKILL_TARGET" ] || ! cmp -s "$FINAL/skills/x-sage-kb/SKILL.md" "$SKILL_TARGET/SKILL.md" 2>/dev/null; then
    needs_backup=1
  fi
  if [ "$needs_backup" -eq 1 ]; then
    OLD_SKILLS_BACKUP="$SKILL_TARGET.backup-$(date -u +%Y%m%dT%H%M%SZ)"
    mv "$SKILL_TARGET" "$OLD_SKILLS_BACKUP"
    SKILL_SWITCHED=1
  fi
fi
mkdir -p "$SKILL_TARGET"
cp "$FINAL/skills/x-sage-kb/SKILL.md" "$SKILL_TARGET/.SKILL.md.tmp"
mv -f "$SKILL_TARGET/.SKILL.md.tmp" "$SKILL_TARGET/SKILL.md"

trap - ERR
trap - EXIT

# Committed: the saved entry point is no longer needed.
[ -n "$OLD_BIN_SAVED" ] && rm -f "$OLD_BIN_SAVED"

# ---------------------------------------------------------------- report
echo "==> installed: $BIN_PATH"
echo "    $("$BIN_PATH" --version 2>/dev/null || echo 'version probe failed')"
echo "    current -> $FINAL"
if [ -n "$OLD_LINK" ] && [ "$OLD_LINK" != "$FINAL" ]; then
  echo "    previous: $OLD_LINK"
  echo "    roll back with: ln -sfn \"$OLD_LINK\" \"$CURRENT.new\" &&"
  echo "                    bun -e 'require(\"node:fs\").renameSync(process.argv[1], process.argv[2])' \"$CURRENT.new\" \"$CURRENT\""
fi
[ -n "$OLD_SKILLS_BACKUP" ] && echo "    your previous skill was moved to: $OLD_SKILLS_BACKUP"

# PATH shadowing: reporting "installed" while an older entry wins is how a
# machine ends up running a stale client that looks freshly installed.
resolve_first() { # binary name
  local name="$1" dir
  local IFS=:
  for dir in $PATH; do
    [ -x "$dir/$name" ] && { printf '%s' "$dir/$name"; return 0; }
  done
  return 1
}
FIRST="$(resolve_first x-sage || true)"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo
     echo "    $BIN_DIR is not on PATH. Add it, e.g.:"
     echo "      echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.bashrc   # or ~/.zshrc"
     ;;
esac
if [ -n "$FIRST" ] && [ "$FIRST" != "$BIN_PATH" ]; then
  echo
  echo "    WARNING: '$FIRST' comes first on PATH, so 'x-sage' still runs that one."
  if [ -L "$FIRST" ]; then
    echo "             it is a symlink to: $(readlink "$FIRST")"
    echo "             Do not edit it in place; remove or re-point it after verifying."
  fi
  echo "             Put $BIN_DIR earlier on PATH, or remove the old entry."
fi

if [ -f "$HOME/.config/x-sage/config.toml" ]; then
  echo "    config kept as-is: ~/.config/x-sage/config.toml"
else
  echo "    next: write ~/.config/x-sage/config.toml"
  echo '      server = "https://x-sage.example.com"'
  echo '      token  = "<write token, only needed for writes>"'
fi
echo
echo "    verify: x-sage validate --graph"
