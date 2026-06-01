#!/usr/bin/env bash
# Prepare the benchmark workspace: clone the corpus and install the comparison
# tools. Idempotent — safe to re-run. Everything lands under benchmark/.work/,
# which is git-ignored.
#
# Optional comparison tools (skipped with a warning if their package manager is
# absent): scc (brew/go), lizard (pip). The Node tools are installed locally.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$HERE/.work"
CORPUS_REPO="https://github.com/colinhacks/zod.git"
CORPUS_DIR="$WORK/corpus-zod"

mkdir -p "$WORK"

# --- corpus -----------------------------------------------------------------
if [ ! -d "$CORPUS_DIR" ]; then
  echo "==> cloning corpus (zod, shallow)"
  git clone --depth 1 "$CORPUS_REPO" "$CORPUS_DIR"
else
  echo "==> corpus already present: $CORPUS_DIR"
fi
# remove declaration files so every tool sees identical input
find "$CORPUS_DIR/packages/zod/src" -name '*.d.ts' -delete 2>/dev/null || true

# --- Node tools (eslint + ts parser + sonarjs) ------------------------------
echo "==> installing Node tools (eslint, @typescript-eslint/parser, eslint-plugin-sonarjs)"
( cd "$WORK"
  [ -f package.json ] || npm init -y >/dev/null 2>&1
  npm install --no-save eslint@8 @typescript-eslint/parser eslint-plugin-sonarjs
)
cp "$HERE/eslintrc.json" "$WORK/.eslintrc.json"

# --- scc (optional native baseline) -----------------------------------------
if ! command -v scc >/dev/null 2>&1; then
  if command -v brew >/dev/null 2>&1; then
    echo "==> installing scc via Homebrew"
    brew install scc || echo "WARN: scc install failed; it will be skipped"
  else
    echo "WARN: scc not found and no brew; scc will be skipped (try: go install github.com/boyter/scc/v3@latest)"
  fi
fi

# --- lizard (optional, cyclomatic-only) -------------------------------------
if ! python3 -c 'import lizard' >/dev/null 2>&1; then
  echo "==> installing lizard (pip --user)"
  python3 -m pip install --user lizard \
    || echo "WARN: lizard install failed; it will be skipped"
fi

echo
echo "Setup complete. Build cccc in release mode, then run the benchmark:"
echo "    cargo build --release"
echo "    python3 benchmark/run.py"
