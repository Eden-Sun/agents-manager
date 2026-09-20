"""把 release daemon 起成 ppid=1、nice 0 的獨立行程。

由 `launchctl submit` 執行（launchd 一律 nice 0），本體 fork + setsid 之後父行程立刻結束，
job 就算結束、`launchctl remove` 不會殺到 daemon。直接在 pane 裡 setsid 起會繼承 pane 的
nice（忙的時候是 5），而非 root 降不回 0——2026-09-20 那顆 daemon 就是這樣變成 nice 5 的。

    python3 daemon-start.py <repo dir> <daemon log>
"""

import os
import sys

# pane 專用、不該被 daemon 繼承的環境變數（AM_DATA_DIR 會讓它開到別的資料目錄）。
DROP = (
    "AM_DATA_DIR", "AM_RUN_ID", "AM_EFFORT", "AM_HOOK_TOKEN", "AM_KIND", "AM_MODEL",
    "AM_PORT", "AM_BOT_ID", "AM_AGENT_NAME", "AM_OUTBOX", "AM_CONFIG_PATH",
    "AM_DAEMON_EXE", "AM_REAL_HERDR", "AM_PROJECT_ID", "AM_WORKSPACE_ID",
)


def main() -> int:
    repo, log = sys.argv[1], sys.argv[2]
    if os.fork() != 0:
        return 0
    os.setsid()
    os.chdir(repo)
    for key in DROP:
        os.environ.pop(key, None)
    out = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
    os.dup2(out, 1)
    os.dup2(out, 2)
    os.execv("./target/release/agents-managerd", ["./target/release/agents-managerd", "serve"])


if __name__ == "__main__":
    sys.exit(main())
