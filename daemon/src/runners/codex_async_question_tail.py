"""Read a bounded Codex rollout tail; invoked through the existing host SSH channel."""
import os
import re
from pathlib import Path
import sys


def tail(home, session, limit):
    if not session or not all(c.isascii() and (c.isalnum() or c == "-") for c in session):
        return b""
    # Expand against this host, never the daemon's home. Require an absolute home.
    home = home.strip() or "~/.codex"
    remote_home = os.path.expanduser("~")
    if home == "~" or home.startswith("~/"):
        home = remote_home + home[1:]
    home = home.replace("${HOME}", remote_home)
    home = re.sub(r"\$HOME(?![A-Za-z0-9_])", lambda _: remote_home, home)
    base = Path(home)
    if not base.is_absolute():
        return b""
    base = base.resolve(strict=True)
    root = (base / "sessions").resolve(strict=True)
    if not root.is_relative_to(base) or not root.is_dir():
        return b""
    suffix = "-" + session + ".jsonl"

    def directories(path):
        # Do not descend through symlinks, including intermediate date directories.
        return sorted((p for p in path.iterdir() if not p.is_symlink() and p.is_dir()), reverse=True)

    for year in directories(root):
        for month in directories(year):
            for day in directories(month):
                candidates = [p for p in day.iterdir() if p.name.startswith("rollout-") and p.name.endswith(suffix)]
                if not candidates:
                    continue
                found = max(candidates, key=lambda p: (p.stat().st_mtime_ns, p.name))
                real = found.resolve(strict=True)
                if not real.is_relative_to(root) or not real.is_file():
                    return b""
                with real.open("rb") as stream:
                    stream.seek(0, os.SEEK_END)
                    stream.seek(max(0, stream.tell() - min(int(limit), 8 * 1024 * 1024)))
                    return stream.read(min(int(limit), 8 * 1024 * 1024))
    return b""


if __name__ == "__main__":
    try:
        sys.stdout.buffer.write(tail(*sys.argv[1:]).decode("utf-8", errors="ignore").encode("utf-8"))
    except (OSError, ValueError):
        pass  # Missing/unreadable paths degrade to the screen's question count.
