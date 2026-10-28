"""Tie E35's built source snapshot and binary to the current workspace.

bounded-evidence.py <E34-source> <E35-built-source> <workspace> <HTTP-summary> <index> <new-output-dir>
"""
import difflib
import hashlib
import json
from pathlib import Path
import sys


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    if len(sys.argv) != 7:
        raise SystemExit(__doc__)
    baseline, candidate, workspace, summary, index, output = map(Path, sys.argv[1:])
    http = json.loads(summary.read_text())
    if not http["complete"] or sha(Path(http["exe"]).read_bytes()) != http["binary_sha256"]:
        raise RuntimeError("completed HTTP result and unchanged binary required")
    paths = {Path("Cargo.toml"), Path("Cargo.lock")}
    paths.update(p.relative_to(workspace) for p in (workspace / "crates").rglob("*.rs"))
    paths.update(p.relative_to(workspace) for p in (workspace / "crates").rglob("Cargo.toml"))
    paths.update(Path(p) for p in ["vendor/ripgrep/crates/searcher/src/searcher/core.rs",
                                  "vendor/ripgrep/crates/searcher/src/searcher/mod.rs"])
    checked, changed, patch = [], [], []
    for path in sorted(paths):
        current = (workspace / path).read_bytes()
        if (candidate / path).read_bytes() != current:
            raise RuntimeError(f"built source differs from workspace: {path}")
        checked.append({"path": str(path), "sha256": sha(current)})
        previous = (baseline / path).read_bytes() if (baseline / path).exists() else b""
        if previous != current:
            changed.append({"path": str(path), "before_sha256": sha(previous) if previous else None,
                            "after_sha256": sha(current)})
            patch.extend(difflib.unified_diff(previous.decode().splitlines(True), current.decode().splitlines(True),
                                             fromfile=f"a/{path}", tofile=f"b/{path}"))
    data = {"baseline": str(baseline), "candidate": str(candidate), "workspace": str(workspace),
            "binary_sha256": http["binary_sha256"], "index": str(index),
            "catalog_sha256": sha((index / "catalog.json").read_bytes()),
            "fixture_sha256": sha((index / "fixture.json").read_bytes()),
            "index_bytes": sum(p.stat().st_size for p in index.rglob("*") if p.is_file()),
            "changed_files": changed, "verified_compile_inputs": checked}
    output.mkdir(parents=True, exist_ok=False)
    (output / "source-evidence.json").write_text(json.dumps(data, indent=2) + "\n")
    (output / "implementation.patch").write_text("".join(patch))
    print(f"{len(checked)} compile inputs match; {len(changed)} changed since E34; {data['index_bytes']} index bytes")


if __name__ == "__main__":
    main()
