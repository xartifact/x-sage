#!/usr/bin/env bash
set -euo pipefail
umask 077

: "${SAGE_PROD_SLOT:?set the active native production Compose directory}"
: "${SAGE_RUST_TREE:?set the Git tree for the separately qualified sage-db release}"
: "${SAGE_DB_PROTOC:?set the pinned protocol compiler}"
: "${SAGE_DB_TS_PROTO_PLUGIN:?set the pinned TypeScript generator}"

slot="$SAGE_PROD_SLOT"
state="${XDG_STATE_HOME:-$HOME/.local/state}/x-sage-release"
repo="$state/source.git"
settings="$slot/bun.env"
public=https://x-sage.ts.xartifact.dev
production=http://127.0.0.1:3001
preflight=http://127.0.0.1:3301
remote=https://github.com/xartifact/x-sage.git
container=x-sage-native-production-bun-1
main=refs/remotes/origin/main

mkdir -p "$state"
exec 9>"$(dirname "$slot")/.production-cutover.lock"
flock -n 9
test -f "$settings" && test ! -L "$settings"
test "$(docker inspect --format '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' "$container")" = "$slot"

setting() {
    local key=$1
    awk -F= -v key="$key" '$1 == key { value = substr($0, length(key) + 2); count++ }
        END { if (count != 1 || value == "") exit 1; print value }' "$settings"
}

test "$(setting X_SAGE_DB_MODE)" = active
test "$(setting X_SAGE_NATIVE_BUN_PORT)" = 3001
db_source=$(setting X_SAGE_DB_SOURCE_IDENTITY)
[[ "$db_source" =~ ^[0-9a-f]{64}$ ]]
old_commit=$(setting X_SAGE_DB_EXPECTED_BUN_COMMIT)
old_ref=$(setting X_SAGE_DB_EXPECTED_BUN_REF)
old_image=$(setting X_SAGE_NATIVE_BUN_IMAGE)

compose() {
    local project=$1
    shift
    docker compose --project-name "$project" --env-file "$settings" \
        -f "$slot/compose.yml" -f "$slot/run-user.yml" "$@"
}

database_ready() {
    curl -fsS --max-time 4 http://127.0.0.1:3901/api/health |
        jq -e --arg source "$db_source" '.ok and .readReady and .writeReady and .sourceIdentity == $source' >/dev/null
}

gateway_ready() {
    local address=$1 commit=$2 ref=$3 version=${4:-}
    curl -fsS --max-time 5 "$address/api/health" |
        jq -e --arg commit "$commit" --arg ref "$ref" --arg source "$db_source" --arg version "$version" \
            '.ok and .mode == "active" and .authority == "sage-db" and .databaseReady
             and .databaseSourceIdentity == $source and .commitHash == $commit and .buildRef == $ref
             and ($version == "" or .version == $version)' >/dev/null
}

smoke() {
    local address=$1 commit=$2 ref=$3 version=$4
    gateway_ready "$address" "$commit" "$ref" "$version"
    curl -fsS --max-time 10 "$address/" | grep -F '/assets/' >/dev/null
    curl -fsS --max-time 15 "$address/api/viz" |
        jq -e '(.nodes | type == "array") and (.edges | type == "array")' >/dev/null
}

export GIT_TERMINAL_PROMPT=0
test -d "$repo" || git init --bare -q "$repo"
git --git-dir="$repo" fetch --quiet --no-tags --depth=1 "$remote" "+refs/heads/main:$main"
commit=$(git --git-dir="$repo" rev-parse "$main")
short=${commit:0:7}
if [[ "$old_commit" = "$short" && "$old_ref" = main ]] &&
    test "$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$container")" = "$commit"; then
    database_ready
    gateway_ready "$public" "$short" main
    printf 'Already deployed %s\n' "$commit"
    exit 0
fi
if [[ "$(git --git-dir="$repo" rev-parse "$commit:sage-db")" != "$SAGE_RUST_TREE" ]]; then
    echo "Rust source changed at $commit. Qualify the native release before promoting its Bun gateway." >&2
    exit 1
fi
database_ready
gateway_ready "$production" "$old_commit" "$old_ref"
test "$(docker image inspect --format '{{.Id}}' "$old_image")" = "$(docker inspect --format '{{.Image}}' "$container")"

work=$(mktemp -d "$state/build.XXXXXXXX")
staged=false
promoting=false
clean_up() {
    local status=$?
    trap - EXIT
    if [[ "$status" != 0 && "$promoting" = true ]]; then
        echo "Promotion failed. Restoring the previous Bun gateway." >&2
        if rollback=$(mktemp "$slot/.bun.env.rollback.XXXXXXXX") &&
            cp -p "$slot/bun.env.previous" "$rollback" &&
            mv -f "$rollback" "$settings" &&
            compose x-sage-native-production up --pull never --no-build --wait --wait-timeout 75 bun &&
            gateway_ready "$public" "$old_commit" "$old_ref"; then
            echo "Restored $old_commit"
        else
            echo "Gateway rollback could not be verified." >&2
        fi
    fi
    if [[ "$staged" = true ]]; then
        compose x-sage-native-bun-preflight rm --stop --force bun || status=1
    fi
    rm -rf -- "$work"
    exit "$status"
}
trap clean_up EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$work/source" "$work/projection"
git --git-dir="$repo" archive "$commit" | tar -xf - -C "$work/source"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/x-sage-release/cargo}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
version=$(jq -er '.version | strings | select(length > 0)' "$work/source/package.json")
candidate=x-sage-bun-release:candidate
(
    cd "$work/source"
    bun install --frozen-lockfile --ignore-scripts
    ./scripts/ci.sh
    docker build --tag "$candidate" --label "org.opencontainers.image.revision=$commit" \
        --build-arg "APP_VERSION=$version" --build-arg "GIT_COMMIT_HASH=$short" \
        --build-arg BUILD_REF=main .
)
image=$(docker image inspect --format '{{.Id}}' "$candidate")
[[ "$image" =~ ^sha256:[0-9a-f]{64}$ ]]
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image")" = "$commit"

staged=true
X_SAGE_NATIVE_BUN_IMAGE="$image" X_SAGE_NATIVE_BUN_PORT=3301 \
    X_SAGE_DB_EXPECTED_BUN_COMMIT="$short" X_SAGE_DB_EXPECTED_BUN_REF=main \
    X_SAGE_BUN_PROJECTION_DIR="$work/projection" \
    compose x-sage-native-bun-preflight up --pull never --no-build --wait --wait-timeout 75 bun
smoke "$preflight" "$short" main "$version"
compose x-sage-native-bun-preflight rm --stop --force bun
staged=false

read -r latest _ < <(git ls-remote --exit-code "$remote" refs/heads/main)
if [[ "$latest" != "$commit" ]]; then
    printf 'Main advanced during the build. %s will be considered on the next run.\n' "$latest"
    exit 0
fi
gateway_ready "$production" "$old_commit" "$old_ref"
database_ready

docker image tag "$old_image" x-sage-bun-release:rollback
cp -p "$settings" "$slot/bun.env.previous"
next=$(mktemp "$slot/.bun.env.next.XXXXXXXX")
awk -v image="$image" -v commit="$short" '
    /^X_SAGE_NATIVE_BUN_IMAGE=/ { $0 = "X_SAGE_NATIVE_BUN_IMAGE=" image }
    /^X_SAGE_DB_EXPECTED_BUN_COMMIT=/ { $0 = "X_SAGE_DB_EXPECTED_BUN_COMMIT=" commit }
    /^X_SAGE_DB_EXPECTED_BUN_REF=/ { $0 = "X_SAGE_DB_EXPECTED_BUN_REF=main" }
    { print }
' "$settings" > "$next"
promoting=true
mv -f "$next" "$settings"
compose x-sage-native-production up --pull never --no-build --wait --wait-timeout 75 bun
database_ready
smoke "$production" "$short" main "$version"
smoke "$public" "$short" main "$version"
promoting=false
printf 'Deployed and verified %s (%s)\n' "$commit" "$image"
