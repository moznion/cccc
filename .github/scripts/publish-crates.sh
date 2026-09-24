#!/bin/sh
# Publish every publishable workspace crate to crates.io, dependencies first.
# Run by the `publish` job of .github/workflows/tagpr.yml after tagpr has tagged
# a release (and re-runnable by hand: versions already on crates.io are skipped).
#
# Usage: publish-crates.sh [--dry-run]
#   CARGO_REGISTRY_TOKEN must be set unless --dry-run is given.
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

log=$(mktemp)
trap 'rm -f "$log"' EXIT

while read -r name version; do
    if published "$name" "$version"; then
        echo "skip: $name $version is already on crates.io"
        continue
    fi
    # crates.io rate-limits publishing (new crates especially) and answers 429;
    # wait and retry instead of failing half-way through the release.
    attempt=1
    until cargo publish -p "$name" --allow-dirty >"$log" 2>&1; do
        cat "$log"
        if grep -qE '429|Too Many Requests' "$log" && [ "$attempt" -lt 12 ]; then
            echo "rate-limited publishing $name; retrying in 10 minutes (attempt $attempt)"
            attempt=$((attempt + 1))
            sleep 600
        else
            echo "error: failed to publish $name $version" >&2
            exit 1
        fi
    done
    cat "$log"
done <<EOF
$crates
EOF
