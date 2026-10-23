#!/usr/bin/env python3
"""Conservative changed-mode ops selection. No arguments/default ops still runs everything."""
import sys
from pathlib import Path

# Independent entrypoints with a known isolated suite. Shared helpers, manifests, CI machinery,
# fixtures and new/unknown scripts deliberately do not appear here: they require the full suite.
ENTRYPOINTS = {
    "scripts/dev-sshd.sh": "scripts/dev-sshd_test.sh",
    **{f"scripts/ops/{name}.{ext}": f"scripts/ops/{name}_test.sh" for name, ext in [
        ("browser-gc-kick", "sh"), ("ci-watch-kick", "sh"), ("claude-release-kick", "sh"),
        ("daemon-boot", "sh"), ("daemon-update-kick", "sh"), ("dev-server-kick", "ts"),
        ("disk-gc", "sh"), ("herdr-full-restart", "sh"), ("herdr-lan-check", "sh"),
        ("herdr-update-kick", "sh"), ("outbox-gc", "sh"), ("pane-gc", "sh"),
        ("release-triage-kick", "sh"), ("ubuntu-ci", "sh"),
    ]},
    "scripts/ops/browser_gc_linux.py": "scripts/ops/browser-gc-linux_test.sh",
    "scripts/ops/claude_binary_diff.py": "scripts/ops/claude_binary_diff_test.sh",
}
CROSS_INPUTS = {
    "crates/am-base/src/db.rs", "crates/am-supervisor/src/supervisor/store.rs",
    "crates/am-supervisor/src/supervisor/roles.rs", "crates/am-supervisor/src/mission/store.rs",
}
ALWAYS = {"scripts/test_discovery_test.sh", "scripts/rust_test_mount_test.sh", "scripts/db_filename_test.sh"}


def select(paths: list[str]) -> list[str]:
    selected = set(ALWAYS)
    saw_ops = False
    for path in paths:
        if path in CROSS_INPUTS:
            return ["__all__"]
        if not path.startswith(("scripts/", "bin/", "ops/", ".github/", ".claude/")) and path != "AGENTS.md":
            continue  # Other changed parts (web/Rust/docs) have their own checks.
        saw_ops = True
        if path in ENTRYPOINTS:
            suite = ENTRYPOINTS[path]
        elif path.endswith("_test.sh") and Path(path).parent in {Path("scripts"), Path("scripts/ops")}:
            suite = path
        elif path == "scripts/agm_test.py":
            suite = path
        elif path.endswith("_test.py"):
            suite = path.removesuffix(".py") + ".sh"
            if path == "scripts/ops/browser_gc_linux_test.py":
                suite = "scripts/ops/browser-gc-linux_test.sh"
        else:
            return ["__all__"]
        if not Path(path).is_file() or not Path(suite).is_file():
            return ["__all__"]
        selected.add(suite)
    if not saw_ops:
        return ["__all__"]
    return sorted(selected)


if __name__ == "__main__":
    print("\n".join(select([line.rstrip("\n") for line in sys.stdin])))
