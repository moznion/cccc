#!/bin/sh
# Publish every publishable workspace crate to crates.io, dependencies first.
# Run by the `publish` job of .github/workflows/tagpr.yml after tagpr has tagged
# a release (and re-runnable by hand: versions already on crates.io are skipped).
#
# Usage: publish-crates.sh [--dry-run]
#   Authenticates with crates.io Trusted Publishing: needs the GitHub Actions
#   OIDC environment (`id-token: write`). Outside of Actions, set
#   CARGO_REGISTRY_TOKEN instead (e.g. to publish a brand-new crate by hand,
#   which Trusted Publishing cannot do). Neither is needed with --dry-run.
#
# cccc-kt is `publish = false` (its Kotlin grammar is a git dependency, which
# crates.io rejects), so before publishing, every line of cccc-cli's manifest
# marked `crates.io: strip` is removed. That drops the optional cccc-kt
# dependency and the default `kotlin` feature: the crates.io cccc-cli is built
# without Kotlin, while the GitHub Releases binaries keep it.
set -eu

dry_run=
if [ "${1:-}" = "--dry-run" ]; then
    dry_run=1
fi

cd "$(git rev-parse --show-toplevel)"

cli_manifest=crates/cccc-cli/Cargo.toml
perl -ni -e 'print unless /# crates\.io: strip$/' "$cli_manifest"
perl -pi -e 's/Kotlin, // if /^description = /' "$cli_manifest"
if grep -qE '^(cccc-kt|kotlin|default) ' "$cli_manifest"; then
    echo "error: $cli_manifest still references cccc-kt after stripping" >&2
    exit 1
fi

if [ -n "$dry_run" ]; then
    # Packages and verify-builds every crate against a local overlay of the
    # not-yet-published workspace crates.
    cargo publish --workspace --dry-run --allow-dirty
    exit 0
fi

# Publishable workspace members in dependency order (normal and build
# dependencies; dev-dependencies are stripped by cargo on publish).
crates=$(cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys
meta = json.load(sys.stdin)
pkgs = {p["name"]: p for p in meta["packages"] if p["publish"] != []}
deps = {
    n: {d["name"] for d in p["dependencies"] if d["kind"] != "dev" and d["name"] in pkgs}
    for n, p in pkgs.items()
}
done = []
def visit(n):
    if n in done:
        return
    for d in sorted(deps[n]):
        visit(d)
    done.append(n)
for n in sorted(pkgs):
    visit(n)
print("\n".join(n + " " + pkgs[n]["version"] for n in done))
')

# True if crates.io already has $1 at version $2 (sparse index lookup).
published() {
    case ${#1} in
        1) path="1/$1" ;;
        2) path="2/$1" ;;
        3) path="3/$(printf %s "$1" | cut -c1)/$1" ;;
        *) path="$(printf %s "$1" | cut -c1-2)/$(printf %s "$1" | cut -c3-4)/$1" ;;
    esac
    curl -fsSL "https://index.crates.io/$path" 2>/dev/null | grep -q "\"vers\":\"$2\""
}

tokens_endpoint=https://crates.io/api/v1/trusted_publishing/tokens
# crates.io rejects API requests without a descriptive User-Agent (curl's
# default gets 403), so every call to its API identifies this script.
user_agent="cccc-release (https://github.com/moznion/cccc)"

# Revoke the current Trusted Publishing token, if any.
revoke_token() {
    if [ -n "${tp_token:-}" ]; then
        curl -fsS -A "$user_agent" -X DELETE -H "Authorization: Bearer $tp_token" \
            "$tokens_endpoint" >/dev/null || echo "warning: failed to revoke token" >&2
        tp_token=
    fi
}

# Trusted Publishing tokens expire after 30 minutes, but waiting out rate
# limits can take hours, so a fresh token is exchanged for each publish.
refresh_token() {
    [ -n "$use_trusted_publishing" ] || return 0
    revoke_token
    jwt=$(curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
        "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=crates.io" |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["value"])')
    tp_token=$(printf '{"jwt":"%s"}' "$jwt" |
        curl -fsS -A "$user_agent" -X POST -H 'Content-Type: application/json' --data-binary @- \
            "$tokens_endpoint" |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["token"])')
    echo "::add-mask::$tp_token"
    CARGO_REGISTRY_TOKEN=$tp_token
    export CARGO_REGISTRY_TOKEN
}

use_trusted_publishing=
if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
    if [ -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ]; then
        echo "error: set CARGO_REGISTRY_TOKEN or run in GitHub Actions with id-token: write" >&2
        exit 1
    fi
    use_trusted_publishing=1
fi

log=$(mktemp)
trap 'revoke_token; rm -f "$log"' EXIT

while read -r name version; do
    if published "$name" "$version"; then
        echo "skip: $name $version is already on crates.io"
        continue
    fi
    # crates.io rate-limits publishing (new crates especially) and answers 429;
    # wait and retry instead of failing half-way through the release.
    attempt=1
    refresh_token
    until cargo publish -p "$name" --allow-dirty >"$log" 2>&1; do
        cat "$log"
        if grep -qE '429|Too Many Requests' "$log" && [ "$attempt" -lt 12 ]; then
            echo "rate-limited publishing $name; retrying in 10 minutes (attempt $attempt)"
            attempt=$((attempt + 1))
            sleep 600
            refresh_token
        else
            echo "error: failed to publish $name $version" >&2
            exit 1
        fi
    done
    cat "$log"
done <<EOF
$crates
EOF
