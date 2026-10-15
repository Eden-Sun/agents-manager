#!/usr/bin/env python3
"""Resolve am-base's composition filters from the daemon's actual #[path] mounts."""
import re
import sys
from pathlib import Path

BROAD = "runners::am_base_tests::"
PATH_MOD = re.compile(r'#\[path\s*=\s*"([^"]+)"\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;')
PATH = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')


def select(root: Path, changed: str) -> list[str]:
    relative = Path(changed).relative_to("crates/am-base/src")
    module = relative.parts[0].split(".")[0]
    if module in {"lib", "db", "race_point", "shared_host"}:
        return [BROAD]
    harness = root / "daemon/src/runners/am_base_tests.rs"
    source = harness.read_text()
    mounts = PATH_MOD.findall(source)
    if not mounts or len(mounts) != len(PATH.findall(source)):
        return [BROAD]
    mapping: dict[str, set[str]] = {}
    for wrapper_path, alias in mounts:
        wrapper = harness.parent / wrapper_path
        origins = PATH.findall(wrapper.read_text())
        if not origins:
            return [BROAD]
        for origin in origins:
            original = (wrapper.parent / origin).resolve().relative_to(root / "crates/am-base/src")
            owner = original.parts[0].split(".")[0]
            mapping.setdefault(owner, set()).add(f"{BROAD}{alias}::")
    return sorted(mapping.get(module, {BROAD}))


if __name__ == "__main__":
    try:
        filters = select(Path.cwd().resolve(), sys.argv[1])
    except (OSError, ValueError):
        filters = [BROAD]  # Missing/moved/unknown wiring must never narrow the suite.
    print("\n".join(filters))
