#!/usr/bin/env python3
"""Conservative changed-mode ops selection. No arguments/default ops still runs everything.

Output (stdout): the suites to run, one per line, or `__all__`. When `__all__` is chosen, the
triggering path and the reason are printed to stderr so check.sh can show why the full suite ran.
"""
import fnmatch
import os
import sys
from pathlib import Path

# 路徑 → ops 測試的規則在 scripts/ci-rules.tsv 第 4 欄（跟 ci-changed-parts.sh、ci-daemon-filters.sh 同一份）：
#   <suite> 該路徑獨立的測試檔；__all__ 分派器／護欄／跨 crate 輸入，改了要全跑；@default 走下面的預設推導。
# Shared helpers, manifests, CI machinery, fixtures and new/unknown scripts deliberately have no entry: they require the full suite.
RULES_FILE = Path(os.environ.get("CI_RULES_FILE") or Path(__file__).resolve().parent / "ci-rules.tsv")


def load_rules() -> list[tuple[str, str]]:
    rules = []
    for line in RULES_FILE.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        pat, _parts, _daemon, ops = line.split("|", 3)
        rules.append((pat, ops))
    return rules


def rule_for(rules: list[tuple[str, str]], path: str) -> str:
    for pat, ops in rules:
        if fnmatch.fnmatchcase(path, pat):
            return ops
    return "@default"


ALWAYS = {"scripts/test_discovery_test.sh", "scripts/rust_test_mount_test.sh", "scripts/db_filename_test.sh"}


def sibling_suite(path: str):
    """A plain script or Python module in scripts/ or scripts/ops with its own `<stem>_test.sh` next to it."""
    p = Path(path)
    if p.parent not in {Path("scripts"), Path("scripts/ops")} or p.suffix not in {".sh", ".py", ".ts"}:
        return None
    if p.stem.endswith("_test"):
        return None
    for stem in {p.stem, p.stem.replace("-", "_"), p.stem.replace("_", "-")}:
        cand = p.parent / f"{stem}_test.sh"
        if cand.is_file():
            return str(cand)
    return None


def select(paths: list[str]) -> list[str]:
    rules = load_rules()
    selected = set(ALWAYS)
    saw_ops = False

    def full(path: str, why: str) -> list[str]:
        print(f"ci-ops-tests: __all__ 觸發檔案：{path}（{why}）", file=sys.stderr)
        return ["__all__"]

    for path in paths:
        rule = rule_for(rules, path)
        in_ops = path.startswith(("scripts/", "bin/", "ops/", ".github/", ".claude/")) or path == "AGENTS.md"
        if rule == "__all__":
            return full(path, "分派器／護欄，改了要全跑" if in_ops else "跨 crate 輸入")
        if not in_ops:
            continue  # Other changed parts (web/Rust/docs) have their own checks.
        saw_ops = True
        if rule != "@default":
            suite = rule
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
