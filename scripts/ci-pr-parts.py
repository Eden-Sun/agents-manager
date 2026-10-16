#!/usr/bin/env python3
"""Select PR jobs using the same path classifier as ubuntu-ci; uncertainty runs everything."""
from __future__ import annotations

import json
import os
import re
import sys
import subprocess
from pathlib import Path

PARTS = ("ob", "ops", "web", "daemon", "lint")


def choose(event: str, base: str, head: str, baseline: dict, scope: dict | None = None) -> set[str]:
    if event != "pull_request":
        return set(PARTS)
    # Only inherit a completed full main run, never a failed/pending/docs-only status.
    statuses = baseline.get("statuses", [])
    latest = next((s for s in statuses if s.get("context") == "ubuntu-ci"), {})
    if latest.get("state") != "success" or latest.get("description") != "ob+ops+web+daemon 全綠":
        return set(PARTS)
    if not all(len(s) == 40 and all(c in "0123456789abcdef" for c in s) for s in (base, head, baseline.get("sha", ""))):
        return set(PARTS)
    full = baseline["sha"]
    # Verified ancestor must be in main's history; PR head need not contain main. The merged result can only
    # differ from `full` at paths changed on the main side (full..base, including unverified main commits) or the
    # PR side (full..head), so their union is a safe superset of the merge-tree diff.
    subprocess.run(["git", "merge-base", "--is-ancestor", full, base], check=True, capture_output=True)
    names: set[str] = set()
    for tip in (base, head):
        out = subprocess.check_output(["git", "-c", "core.quotePath=false", "diff", "--no-renames", "--name-only", full, tip], text=True)
        names.update(line for line in out.split("\n") if line)
    files = "".join(f"{name}\n" for name in sorted(names))
    result = subprocess.run(["bash", "scripts/ci-changed-parts.sh"], input=files, text=True, capture_output=True, check=True)
    selected = set(result.stdout.split())
    if not selected <= set(PARTS) or "full" in selected:
        return set(PARTS)
    if scope is not None and "ops" in selected:
        scope["ops_base"] = full
    if "daemon" in selected:
        selected.add("lint")
    return selected


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--baseline-ref":
        try:
            status = json.loads(Path(sys.argv[2]).read_text())
            latest = next((s for s in status.get("statuses", []) if s.get("context") == "ubuntu-ci"), {})
            desc = latest.get("description", "")
            if latest.get("state") == "success":
                if desc == "ob+ops+web+daemon 全綠":
                    print(status["sha"])
                # docs-only and scoped successes are not full evidence: resolve to the full-success ancestor they cite.
                elif match := re.fullmatch(r"docs-only skip（沿用完整成功基準 ([0-9a-f]{8})）", desc):
                    print(match[1])
                elif match := re.fullmatch(r"scoped (?:none|(?:ob|ops|web|daemon)(?:\+(?:ob|ops|web|daemon))*)（沿用完整成功基準 ([0-9a-f]{8})）", desc):
                    print(match[1])
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            pass
        sys.exit(0)
    scope = {}
    try:
        status = json.loads(Path(os.environ["CI_BASE_STATUS"]).read_text())
        parts = choose(os.environ["CI_EVENT"], os.environ.get("CI_BASE", ""), os.environ.get("CI_HEAD", ""), status, scope)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.SubprocessError):
        parts = set(PARTS)
    print("\n".join(f"{part}={'true' if part in parts else 'false'}" for part in PARTS))
    print(f"ops_base={scope.get('ops_base', '')}")
