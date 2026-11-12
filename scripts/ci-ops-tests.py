#!/usr/bin/env python3
"""Conservative changed-mode ops selection. No arguments/default ops still runs everything.

Output (stdout): the suites to run, one per line, or `__all__`. When `__all__` is chosen, the
triggering path and the reason are printed to stderr so check.sh can show why the full suite ran.
"""
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
    # AGENTS.md：scripts/lint_scope_test.sh 釘住它的 clippy 指令（#1077）；只跑那支，不必全跑。
    "AGENTS.md": "scripts/lint_scope_test.sh",
    # agm.py 由 daemon include_str! 編進去，但它的契約測試就是 scripts/agm_test.py。
    "scripts/agm.py": "scripts/agm_test.py",
    # host-state-transfer 的測試是 .py，由 cutover-to-host_test.sh 呼叫（test_discovery_test.sh 也釘著這對應）。
    "scripts/ops/host-state-transfer.py": "scripts/ops/cutover-to-host_test.sh",
    "scripts/ops/host-state-transfer_test.py": "scripts/ops/cutover-to-host_test.sh",
    "scripts/ops/browser_gc_linux_test.py": "scripts/ops/browser-gc-linux_test.sh",
    # canary 基線棘輪由 destructive-canary_test.sh 讀。
    "scripts/ops/canary-baseline.tsv": "scripts/ops/destructive-canary_test.sh",
}
# 分派器與全域基礎設施：改了它們，整條 ops 路徑的選擇或護欄都可能變，一律全跑。
DISPATCHERS = {
    "scripts/check.sh", "scripts/ci-changed-parts.sh", "scripts/ci-ops-tests.py", "scripts/ci-daemon-filters.sh",
    "scripts/ci-am-base-filters.py", "scripts/ci-crate-only-tests.py", "scripts/ci-pr-parts.py",
    "scripts/ops/destructive-canary.sh",
}
CROSS_INPUTS = {
    "crates/am-base/src/db.rs", "crates/am-supervisor/src/supervisor/store.rs",
    "crates/am-supervisor/src/supervisor/roles.rs", "crates/am-supervisor/src/mission/store.rs",
}
ALWAYS = {"scripts/test_discovery_test.sh", "scripts/rust_test_mount_test.sh", "scripts/db_filename_test.sh"}


def sibling_suite(path: str):
    """A plain script or Python module in scripts/ or scripts/ops with its own `<stem>_test.sh` next to it."""
    p = Path(path)
    if p.parent not in {Path("scripts"), Path("scripts/ops")} or p.suffix not in {".sh", ".py", ".ts"}:
        return None
    if p.stem.endswith("_test") or path in DISPATCHERS:
        return None
    for stem in {p.stem, p.stem.replace("-", "_"), p.stem.replace("_", "-")}:
        cand = p.parent / f"{stem}_test.sh"
        if cand.is_file():
            return str(cand)
    return None


def select(paths: list[str]) -> list[str]:
    selected = set(ALWAYS)
    saw_ops = False

    def full(path: str, why: str) -> list[str]:
        print(f"ci-ops-tests: __all__ 觸發檔案：{path}（{why}）", file=sys.stderr)
        return ["__all__"]

    for path in paths:
        if path in CROSS_INPUTS:
            return full(path, "跨 crate 輸入")
        if not path.startswith(("scripts/", "bin/", "ops/", ".github/", ".claude/")) and path != "AGENTS.md":
            continue  # Other changed parts (web/Rust/docs) have their own checks.
        saw_ops = True
        if path in DISPATCHERS:
            return full(path, "分派器／護欄，改了要全跑")
        if path in ENTRYPOINTS:
            suite = ENTRYPOINTS[path]
        elif path.endswith("_test.sh") and Path(path).parent in {Path("scripts"), Path("scripts/ops")}:
            suite = path
        elif path == "scripts/agm_test.py":
            suite = path
        elif path.endswith("_test.py"):
            suite = path.removesuffix(".py") + ".sh"
        elif sibling_suite(path):
            suite = sibling_suite(path)
        else:
            return full(path, "沒有對應的獨立測試（共用輔助、設定、CI、task 文件或新檔）")
        if not Path(path).is_file() or not Path(suite).is_file():
            return full(path, f"對應的測試 {suite} 不存在")
        selected.add(suite)
    if not saw_ops:
        return full("（無 ops 輸入）", "沒有任何 ops 路徑")
    return sorted(selected)


if __name__ == "__main__":
    print("\n".join(select([line.rstrip("\n") for line in sys.stdin])))
