"""Read a bounded Codex rollout tail; invoked through the existing host SSH channel."""
import os
import re
from pathlib import Path
import sys

CAP = 8 * 1024 * 1024


def locate(home, session):
    """The canonical rollout file for `session` under `home`, or None. Never leaves `home/sessions`."""
    if not session or not all(c.isascii() and (c.isalnum() or c == "-") for c in session):
        return None
    # Expand against this host, never the daemon's home. Require an absolute home.
    home = home.strip() or "~/.codex"
    remote_home = os.path.expanduser("~")
    if home == "~" or home.startswith("~/"):
        home = remote_home + home[1:]
    home = home.replace("${HOME}", remote_home)
    home = re.sub(r"\$HOME(?![A-Za-z0-9_])", lambda _: remote_home, home)
    base = Path(home)
    if not base.is_absolute():
        return None
    base = base.resolve(strict=True)
    root = (base / "sessions").resolve(strict=True)
    if not root.is_relative_to(base) or not root.is_dir():
        return None
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
                    return None
                return real
    return None


def tail(home, session, limit):
    real = locate(home, session)
    if real is None:
        return b""
    with real.open("rb") as stream:
        stream.seek(0, os.SEEK_END)
        stream.seek(max(0, stream.tell() - min(int(limit), CAP)))
        return stream.read(min(int(limit), CAP))


def delta(home, session, limit, offset):
    """Whole lines after `offset`, prefixed by one `AM_ROLLOUT <next>` line; `next` is where the reader resumes.

    An `offset` past the end (the file was truncated or replaced) or more than the cap behind reads the
    last `min(limit, CAP)` bytes instead. Only complete lines are returned: an unterminated line stays unread.
    """
    if not offset.isascii() or not offset.isdigit():
        return b""
    real = locate(home, session)
    if real is None:
        return b""
    cap = min(int(limit), CAP)
    with real.open("rb") as stream:
        stream.seek(0, os.SEEK_END)
        size = stream.tell()
        start = int(offset)
        if start > size or size - start > cap:
            start = max(0, size - cap)
        # One read of the window; a mid-line start is detected from the byte before it, not by re-reading.
        previous = b"\n"
        if start > 0:
            stream.seek(start - 1)
            previous = stream.read(1)
        stream.seek(start)
        buf = stream.read(size - start)
    body_start = 0
    if previous != b"\n":
        # Mid-line starts (a rotated file, or the cap cut into a line) skip to the next line break.
        newline = buf.find(b"\n")
        if newline < 0:
            return b"AM_ROLLOUT %d\n" % start
        body_start = newline + 1
    end = max(buf.rfind(b"\n") + 1, body_start)
    text = buf[body_start:end] if end > body_start else b""
    return b"AM_ROLLOUT %d\n" % (start + end) + text


if __name__ == "__main__":
    try:
        if len(sys.argv) == 5:
            sys.stdout.buffer.write(delta(*sys.argv[1:5]))
        else:
            sys.stdout.buffer.write(tail(*sys.argv[1:]).decode("utf-8", errors="ignore").encode("utf-8"))
    except (OSError, ValueError):
        pass  # Missing/unreadable paths degrade to the screen's question count.
