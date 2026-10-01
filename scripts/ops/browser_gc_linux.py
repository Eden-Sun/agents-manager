#!/usr/bin/env python3
"""Linux-only, non-GUI portion of browser-gc (headless Chrome and stale profiles)."""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable


@dataclass(frozen=True)
class ChromeProcess:
    pid: int
    ppid: int
    uid: int
    age_seconds: int
    start_ticks: int
    argv: tuple[str, ...]
    profile: str | None
    debug_port: int | None


@dataclass
class GCResult:
    killed_pids: list[int] = field(default_factory=list)
    kept_pids: list[int] = field(default_factory=list)
    removed_profiles: list[Path] = field(default_factory=list)


def _proc_stat(path: Path) -> tuple[int, int] | None:
    """Return ppid and start ticks, parsing comm from its final ')' (it can contain spaces)."""
    try:
        raw = (path / "stat").read_text(encoding="ascii")
        close = raw.rfind(")")
        fields = raw[close + 2 :].split()
        if close < 0 or len(fields) <= 19:
            return None
        return int(fields[1]), int(fields[19])
    except (OSError, ValueError):
        return None


def _proc_uid(path: Path) -> int | None:
    try:
        for line in (path / "status").read_text(encoding="ascii").splitlines():
            if line.startswith("Uid:"):
                return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return None


def _profile_arg(argv: tuple[str, ...]) -> str | None:
    for index, arg in enumerate(argv):
        if arg.startswith("--user-data-dir="):
            return arg.split("=", 1)[1]
        if arg == "--user-data-dir" and index + 1 < len(argv):
            return argv[index + 1]
    return None


def _debug_port_arg(argv: tuple[str, ...]) -> int | None:
    for index, arg in enumerate(argv):
        value = None
        if arg.startswith("--remote-debugging-port="):
            value = arg.split("=", 1)[1]
        elif arg == "--remote-debugging-port" and index + 1 < len(argv):
            value = argv[index + 1]
        if value is not None:
            try:
                port = int(value)
                return port if 1 <= port <= 65535 else None
            except ValueError:
                return None
    return None


def _is_orphan_parent(ppid: int, proc_root: Path, uid: int) -> bool:
    """孤兒的父程序：pid 1，或這個使用者自己的 `systemd --user`（Linux user session 的 subreaper；孤兒掛在它底下、不是 pid 1）。"""
    if ppid == 1:
        return True
    parent = proc_root / str(ppid)
    if _proc_uid(parent) != uid:
        return False
    try:
        argv = tuple(os.fsdecode(arg) for arg in (parent / "cmdline").read_bytes().split(b"\0") if arg)
    except OSError:
        return False
    return bool(argv) and Path(argv[0]).name == "systemd" and "--user" in argv[1:]


def _chrome_process(path: Path, uptime: float, ticks_per_second: int, uid: int) -> ChromeProcess | None:
    try:
        raw = (path / "cmdline").read_bytes()
        argv = tuple(os.fsdecode(arg) for arg in raw.split(b"\0") if arg)
    except OSError:
        return None
    if not argv:
        return None
    binary = Path(argv[0]).name.lower()
    if not any(name in binary for name in ("chrome", "chromium")):
        return None
    if not any(arg == "--headless" or arg.startswith("--headless=") for arg in argv):
        return None
    if any(arg.startswith("--type=") for arg in argv):
        return None

    proc_uid = _proc_uid(path)
    stat = _proc_stat(path)
    if proc_uid != uid or stat is None:
        return None
    ppid, start_ticks = stat
    age = max(0, int(uptime - start_ticks / ticks_per_second))
    return ChromeProcess(
        pid=int(path.name),
        ppid=ppid,
        uid=proc_uid,
        age_seconds=age,
        start_ticks=start_ticks,
        argv=argv,
        profile=_profile_arg(argv),
        debug_port=_debug_port_arg(argv),
    )


def _read_processes(proc_root: Path, uptime_path: Path, uid: int) -> list[ChromeProcess]:
    try:
        uptime = float(uptime_path.read_text(encoding="ascii").split()[0])
        ticks = int(os.sysconf("SC_CLK_TCK"))
    except (OSError, ValueError, IndexError):
        return []
    found: list[ChromeProcess] = []
    try:
        proc_entries = list(proc_root.iterdir())
    except OSError:
        return []
    for path in proc_entries:
        if not path.name.isdecimal():
            continue
        process = _chrome_process(path, uptime, ticks, uid)
        if process is not None:
            found.append(process)
    return found


def _active_chrome_profiles(proc_root: Path, tmp_root: Path, uid: int) -> set[Path] | None:
    """Include non-headless Chrome too, so a marked profile is not removed while any Chrome uses it."""
    try:
        proc_entries = list(proc_root.iterdir())
    except OSError:
        return None
    profiles: set[Path] = set()
    for path in proc_entries:
        if not path.name.isdecimal() or _proc_uid(path) != uid:
            continue
        try:
            argv = tuple(os.fsdecode(arg) for arg in (path / "cmdline").read_bytes().split(b"\0") if arg)
        except OSError:
            continue
        if not argv or not any(name in Path(argv[0]).name.lower() for name in ("chrome", "chromium")):
            continue
        profile = _safe_profile(_profile_arg(argv), tmp_root)
        if profile is not None:
            profiles.add(profile)
    return profiles


def has_established_cdp_connection(port: int) -> bool | None:
    """True/False if ss answered; None means unknown and callers must preserve the process."""
    try:
        result = subprocess.run(
            ["ss", "-Htn", "state", "established", "sport", "=", f":{port}"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return bool(result.stdout.strip())


def _safe_profile(raw: str | None, tmp_root: Path) -> Path | None:
    if not raw:
        return None
    candidate = Path(raw)
    root = tmp_root.resolve()
    if not candidate.is_absolute() or ".." in candidate.parts or not candidate.name.startswith("am-"):
        return None
    try:
        if candidate.is_symlink() or candidate.resolve(strict=False).parent != root:
            return None
        if not candidate.is_dir() or candidate.stat().st_uid != os.getuid():
            return None
    except (OSError, RuntimeError):
        return None
    return candidate


def _has_profile_marker(path: Path) -> bool:
    return (path / "Local State").exists() or (path / "Default").is_dir() or (path / "DevToolsActivePort").exists()


def _remove_profile(path: Path, tmp_root: Path, log: Callable[[str], None]) -> bool:
    safe = _safe_profile(str(path), tmp_root)
    if safe is None or not _has_profile_marker(safe):
        return False
    try:
        size_kb = sum(p.stat().st_size for p in safe.rglob("*") if p.is_file()) // 1024
        shutil.rmtree(safe)
        log(f"  移除 headless Chrome profile {safe}（約 {size_kb}KB）")
        return True
    except OSError as exc:
        log(f"  保留 profile {safe}（移除失敗：{exc}）")
        return False


def _pid_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def _terminate(process: ChromeProcess, proc_root: Path, uid: int, send_signal, pid_exists, sleep_fn) -> bool:
    pid = process.pid
    if not _same_process(process, proc_root, uid):
        return True
    try:
        send_signal(pid, signal.SIGTERM)
    except ProcessLookupError:
        return True
    except OSError:
        return False
    sleep_fn(3)
    if not pid_exists(pid):
        return True
    # The kernel may reuse a PID after TERM. Never send KILL to a new process.
    if not _same_process(process, proc_root, uid):
        return True
    try:
        send_signal(pid, signal.SIGKILL)
    except OSError:
        return False
    sleep_fn(0.1)
    return not _same_process(process, proc_root, uid)


def _same_process(snapshot: ChromeProcess, proc_root: Path, uid: int) -> bool:
    current = _chrome_process(proc_root / str(snapshot.pid), uptime=0, ticks_per_second=1, uid=uid)
    if current is None:
        return False
    return (
        current.start_ticks == snapshot.start_ticks
        and current.ppid == snapshot.ppid
        and current.argv == snapshot.argv
    )


def run_once(
    *,
    proc_root: Path = Path("/proc"),
    uptime_path: Path = Path("/proc/uptime"),
    tmp_root: Path = Path("/tmp"),
    connection_probe=has_established_cdp_connection,
    send_signal=os.kill,
    pid_exists=_pid_exists,
    sleep_fn=time.sleep,
    log: Callable[[str], None] = print,
    pane_gc: Callable[[], int | None] | None = None,
) -> GCResult:
    """Reap only stale same-user orphan browser roots, then clean safe marked profiles."""
    uid = os.getuid()
    processes = _read_processes(proc_root, uptime_path, uid)
    result = GCResult()
    reaped_profiles: set[Path] = set()
    live_profiles: set[Path] = set()

    for process in processes:
        reason = None
        if not _is_orphan_parent(process.ppid, proc_root, uid):
            reason = "父程序仍存在"
        elif process.age_seconds < 120:
            reason = "啟動未滿 2 分鐘"
        elif process.debug_port is None:
            reason = "沒有可核對的 CDP port"
        else:
            connected = connection_probe(process.debug_port)
            if connected is not False:
                reason = "CDP 連線存在或狀態無法判定"

        if reason is None and not _same_process(process, proc_root, uid):
            reason = "掃描後行程身分已改變"

        if reason is None and _terminate(process, proc_root, uid, send_signal, pid_exists, sleep_fn):
            result.killed_pids.append(process.pid)
            profile = _safe_profile(process.profile, tmp_root)
            if profile is not None:
                reaped_profiles.add(profile)
            log(f"  reap headless Chrome pid {process.pid}（孤兒、無 CDP 連線、活了 {process.age_seconds}s）")
        else:
            result.kept_pids.append(process.pid)
            reason = reason or "結束程序失敗，profile 保留"
            log(f"  keep headless Chrome pid {process.pid}（{reason}）")
            profile = _safe_profile(process.profile, tmp_root)
            if profile is not None:
                live_profiles.add(profile)

    active_profiles = _active_chrome_profiles(proc_root, tmp_root, uid)
    if active_profiles is None:
        log("profile 清理略過（無法讀取 /proc 行程清單）")
    else:
        live_profiles.update(active_profiles)

    for profile in sorted(reaped_profiles - live_profiles) if active_profiles is not None else []:
        if _remove_profile(profile, tmp_root, log):
            result.removed_profiles.append(profile)

    # Reap old, marked profiles left by an earlier interrupted run. Never follow a symlink,
    # touch a profile still used by a Chrome process, or remove a directory younger than an hour.
    cutoff = time.time() - 3600
    try:
        candidates = sorted(tmp_root.glob("am-*")) if active_profiles is not None else []
    except OSError:
        candidates = []
    for path in candidates:
        safe = _safe_profile(str(path), tmp_root)
        if safe is None or safe in live_profiles or safe in result.removed_profiles or not _has_profile_marker(safe):
            continue
        try:
            if safe.stat().st_mtime >= cutoff:
                continue
        except OSError:
            continue
        if _remove_profile(safe, tmp_root, log):
            result.removed_profiles.append(safe)

    log(f"headless Chrome：收掉 {len(result.killed_pids)}／保留 {len(result.kept_pids)}")
    log(f"profile 目錄：刪掉 {len(result.removed_profiles)} 個")
    if pane_gc is not None:
        try:
            rc = pane_gc()
            if isinstance(rc, int) and rc != 0:
                log(f"pane-gc 失敗（rc={rc}）")
        except Exception as exc:  # keep local browser cleanup independent from pane tooling
            log(f"pane-gc 失敗（{exc}）")
    return result


def main() -> int:
    agm_dir = Path(__file__).resolve().parent.parent
    log_path = agm_dir / "browser-gc.log"

    def append_log(message: str) -> None:
        line = f"{time.strftime('%Y-%m-%d %H:%M:%S')} {message}\n"
        try:
            with log_path.open("a", encoding="utf-8") as stream:
                stream.write(line)
        except OSError as exc:
            print(f"browser-gc: cannot write {log_path}: {exc}", file=sys.stderr)

    def run_pane_gc() -> int:
        script = agm_dir / "bin" / "pane-gc.sh"
        if not script.is_file():
            append_log(f"pane-gc 略過（找不到 {script}）")
            return 0
        env = os.environ.copy()
        env["PATH"] = f"{Path.home() / '.local/bin'}:/usr/local/bin:/usr/bin:/bin:{env.get('PATH', '')}"
        return subprocess.run(["bash", str(script)], env=env, check=False).returncode

    append_log("== browser-gc Linux kick")
    run_once(log=append_log, pane_gc=run_pane_gc)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
