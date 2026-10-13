#!/usr/bin/env python3
"""Print the fuzz workflow's job matrix as JSON: one {crate, target} entry per
cargo-fuzz target found under crates/*/fuzz.

Usage: fuzz-matrix.py [CHANGED_LIST]

With no argument every target is listed (scheduled and manual runs). Given a
file listing the paths a pull request changed, one per line, only targets
whose crate — or a workspace crate it depends on, directly or not — has a
changed file under it are listed. A change outside crates/ that can affect
every build (Cargo.lock, the toolchain, this workflow) selects them all.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# Paths outside the crates that can change any target's build or run.
GLOBAL = (
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    ".github/workflows/fuzz.yml",
    ".github/scripts/fuzz-matrix.py",
)


def toml_section(text, name):
    """The body of the `[name]` table in a Cargo manifest."""
    m = re.search(rf"^\[{re.escape(name)}\]\n(.*?)(?=^\[|\Z)", text, re.M | re.S)
    return m.group(1) if m else ""


def workspace_deps(crate):
    """The `cccc-*` crates `crate`'s manifest depends on."""
    text = (ROOT / "crates" / crate / "Cargo.toml").read_text()
    deps = toml_section(text, "dependencies")
    return set(re.findall(r"^(cccc-[a-z0-9-]+)\s*=", deps, re.M))


def closure(crate):
    """`crate` plus every workspace crate it transitively depends on."""
    seen, todo = set(), [crate]
    while todo:
        c = todo.pop()
        if c not in seen and (ROOT / "crates" / c).is_dir():
            seen.add(c)
            todo.extend(workspace_deps(c))
    return seen


def targets():
    for manifest in sorted(ROOT.glob("crates/*/fuzz/Cargo.toml")):
        crate = manifest.parent.parent.name
        for name in re.findall(
            r'^\[\[bin\]\]\nname = "([^"]+)"', manifest.read_text(), re.M
        ):
            yield crate, name


def selected(changed):
    if not changed or any(f in GLOBAL for f in changed):
        return list(targets())
    touched = {f.split("/")[1] for f in changed if f.startswith("crates/")}
    return [(c, t) for c, t in targets() if closure(c) & touched]


def main():
    changed = []
    if len(sys.argv) > 1:
        changed = Path(sys.argv[1]).read_text().splitlines()
    include = [{"crate": c, "target": t} for c, t in selected(changed)]
    print(json.dumps({"include": include}))


main()
