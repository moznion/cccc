#!/bin/sh
# Run by tagpr (tagpr.postVersionCommand) after it has bumped
# [workspace.package].version in Cargo.toml. tagpr rewrites only the first
# version occurrence, so align the internal `cccc-*` requirements in
# [workspace.dependencies] and the workspace members in Cargo.lock with it.
set -eu

next="${TAGPR_NEXT_VERSION#v}"

perl -pi -e 's/^(cccc-[\w-]+ = \{ path = "crates\/[^"]+", version = ")[^"]+(")/${1}'"$next"'${2}/' Cargo.toml
cargo update --workspace
