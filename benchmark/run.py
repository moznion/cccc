#!/usr/bin/env python3
"""Benchmark cccc against other TS/JS complexity tools.

A tool is only timed after a *proof-of-work* check confirms it actually
processed the corpus (a signature string must appear in its output). This guards
against silently-broken runs (missing module, bad path, wrong cwd) producing
meaningless near-zero timings. Tools that are not installed, or that fail the
proof check, are reported as skipped rather than fabricating a number.

Usage:
    python3 benchmark/run.py [--runs N] [--corpus PATH]

Prereqs: run `benchmark/setup.sh` first and `cargo build --release`.
"""
from __future__ import annotations

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WORK = os.path.join(HERE, ".work")
DEFAULT_CORPUS = os.path.join(WORK, "corpus-zod", "packages", "zod", "src")


def find_cccc() -> str | None:
    for p in (
        os.path.join(REPO, "target", "release", "cccc"),
        os.path.join(REPO, "target", "debug", "cccc"),
    ):
        if os.path.isfile(p):
            return p
    return shutil.which("cccc")


def node() -> str | None:
    return shutil.which("node")


def build_tools(corpus: str) -> list[dict]:
    """Each tool: name, argv, proof-of-work signature (bytes), and cwd.

    Commands use the corpus path relative to their cwd so behaviour matches a
    real invocation. Only tools whose executable resolves are included.
    """
    tools: list[dict] = []

    cccc = find_cccc()
    if cccc:
        tools.append(dict(
            name="cccc", argv=[cccc, corpus], cwd=None, sig=b'"function_count"',
        ))

    scc = shutil.which("scc")
    if scc:
        tools.append(dict(
            name="scc", argv=[scc, corpus], cwd=None, sig=b"TypeScript",
        ))

    if subprocess.run([sys.executable, "-c", "import lizard"],
                      capture_output=True).returncode == 0:
        tools.append(dict(
            name="lizard",
            argv=[sys.executable, "-m", "lizard", "-l", "typescript", corpus],
            cwd=None, sig=b"Total nloc",
        ))

    nd = node()
    eslint = os.path.join(WORK, "node_modules", ".bin", "eslint")
    eslintrc = os.path.join(WORK, ".eslintrc.json")
    if nd and os.path.isfile(eslint) and os.path.isfile(eslintrc):
        # eslint expands the glob itself; run from WORK so it resolves the
        # locally-installed parser + plugin.
        rel = os.path.relpath(corpus, WORK)
        tools.append(dict(
            name="eslint",
            argv=[nd, eslint, "--no-eslintrc", "-c", eslintrc,
                  "--format", "json", f"{rel}/**/*.ts"],
            cwd=WORK, sig=b'"messages"',
        ))

    return tools


def verify(tools: list[dict]) -> list[dict]:
    print("### VERIFY (proof each tool processed the corpus)")
    ok = []
    for t in tools:
        p = subprocess.run(t["argv"], cwd=t["cwd"],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        blob = p.stdout + p.stderr
        found = t["sig"] in blob
        print(f"  {t['name']:8} rc={p.returncode} bytes={len(blob)} "
              f"proof={'YES' if found else 'NO'}")
        if found:
            ok.append(t)
        else:
            print(f"           -> skipping {t['name']} (proof check failed)")
    return ok


def timed(t: dict) -> float:
    t0 = time.perf_counter()
    subprocess.run(t["argv"], cwd=t["cwd"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return (time.perf_counter() - t0) * 1000.0


def peak_rss(t: dict) -> int:
    p = subprocess.Popen(t["argv"], cwd=t["cwd"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _, _, ru = os.wait4(p.pid, 0)
    return ru.ru_maxrss  # bytes on macOS, kilobytes on Linux


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--corpus", default=DEFAULT_CORPUS)
    args = ap.parse_args()

    if not os.path.isdir(args.corpus):
        print(f"corpus not found: {args.corpus}\nRun benchmark/setup.sh first.",
              file=sys.stderr)
        return 1
    if not find_cccc():
        print("cccc binary not found. Run: cargo build --release", file=sys.stderr)
        return 1

    tools = verify(build_tools(args.corpus))
    if not tools:
        print("no tools passed verification", file=sys.stderr)
        return 1

    print(f"\n### TIMING (ms, median of {args.runs} runs after warmup)")
    results = {}
    for t in tools:
        timed(t)  # warmup
        ts = [timed(t) for _ in range(args.runs)]
        results[t["name"]] = statistics.median(ts)
        print(f"  {t['name']:8} median={results[t['name']]:9.1f}  "
              "runs=" + " ".join(f"{x:.1f}" for x in ts))

    rss_unit = "KB" if sys.platform.startswith("linux") else "bytes"
    print(f"\n### PEAK RSS ({rss_unit})")
    for t in tools:
        print(f"  {t['name']:8} {peak_rss(t)}")

    if "cccc" in results:
        base = results["cccc"]
        print("\n### RELATIVE (median time vs cccc)")
        for name, med in sorted(results.items(), key=lambda kv: kv[1]):
            if name == "cccc":
                print(f"  {name:8} baseline")
            elif med < base:
                print(f"  {name:8} {base / med:.1f}x faster")
            else:
                print(f"  {name:8} {med / base:.0f}x slower")
    return 0


if __name__ == "__main__":
    sys.exit(main())
