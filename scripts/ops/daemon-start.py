"""把 release daemon 起成 ppid=1、nice 0 的獨立行程。

由 `launchctl submit` 執行（launchd 一律 nice 0），本體 fork + setsid 之後父行程立刻結束，
job 就算結束、`launchctl remove` 不會殺到 daemon。直接在 pane 裡 setsid 起會繼承 pane 的
nice（忙的時候是 5），而非 root 降不回 0——2026-09-20 那顆 daemon 就是這樣變成 nice 5 的。

Linux 上改由 `systemd-run --user -p Type=forking -p KillMode=process` 執行（issue #677，見
daemon-swap.sh 的 start()）：systemd 看的是 cgroup 不是程序群，setsid 脫離不了，所以不靠
這裡的 fork 脫身，而是讓 systemd 把 fork 出來的 daemon 認成 main PID。

    python3 daemon-start.py <repo dir> <daemon log>
"""

import fcntl
import gzip
import os
import shutil
import sys

LOG_ROTATE_BYTES = 100 * 1024 * 1024
LOG_ROTATE_KEEP = 3

DROP = (
    "AM_DATA_DIR", "AM_RUN_ID", "AM_EFFORT", "AM_HOOK_TOKEN", "AM_KIND", "AM_MODEL",
    "AM_PORT", "AM_BOT_ID", "AM_BOT_TOKEN", "AM_AGENT_NAME", "AM_OUTBOX", "AM_CONFIG_PATH",
    "AM_DAEMON_EXE", "AM_REAL_HERDR", "AM_PROJECT_ID", "AM_WORKSPACE_ID",
    "AM_CHILD_OF", "AM_INSTANCE", "AM_INSTRUCTIONS_FILE",
    "CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME", "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
)

def daemon_path(home: str) -> str:
    return ":".join([f"{home}/.local/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"])

def rotate_log(log: str, limit: int = LOG_ROTATE_BYTES, keep: int = LOG_ROTATE_KEEP) -> bool:
    """daemon.log 超過 `limit` 就壓成 `<log>.1.gz`（舊的依序推成 `.2.gz`…，只留 `keep` 份）。

    只在新 daemon 還沒開檔、舊的已經結束時呼叫：先拿資料目錄的 `daemon.lock`（daemon 活著就拿不到，直接不輪替），
    所以搬走的檔沒有任何寫入者，不會丟行。搬走（rename）是瞬間的；壓縮的是搬走後的那一份，中途掛掉下次啟動接著做。
    回傳是否真的輪替了。
    """
    staging = f"{log}.rotating"
    lock_path = os.path.join(os.path.dirname(log) or ".", "daemon.lock")
    try:
        lock = open(lock_path, "a+")
    except OSError:
        return False
    try:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return False
        if os.path.exists(staging):
            pending = staging
        else:
            try:
                if os.path.getsize(log) <= limit:
                    return False
            except OSError:
                return False
            os.replace(log, staging)
            pending = staging
        oldest = f"{log}.{keep}.gz"
        if os.path.exists(oldest):
            os.remove(oldest)
        for i in range(keep - 1, 0, -1):
            src = f"{log}.{i}.gz"
            if os.path.exists(src):
                os.replace(src, f"{log}.{i + 1}.gz")
        tmp = f"{log}.1.gz.tmp"
        with open(pending, "rb") as src, gzip.open(tmp, "wb", compresslevel=6) as dst:
            shutil.copyfileobj(src, dst, 1 << 20)
        os.replace(tmp, f"{log}.1.gz")
        os.remove(pending)
        return True
    finally:
        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
        lock.close()


def main() -> int:
    repo, log = sys.argv[1], sys.argv[2]
    if os.fork() != 0:
        return 0
    os.setsid()
    os.chdir(repo)
    for key in DROP:
        os.environ.pop(key, None)
    os.environ["PATH"] = daemon_path(os.environ.get("HOME", ""))
    try:
        rotate_log(log)
    except OSError:
        pass
    out = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
    os.dup2(out, 1)
    os.dup2(out, 2)
    os.execv("./target/release/agents-managerd", ["./target/release/agents-managerd", "serve"])

if __name__ == "__main__":
    sys.exit(main())
