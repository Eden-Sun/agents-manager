#!/usr/bin/env python3
"""Deterministic Claude binary diff (issue #1919 #4).

用法：claude_binary_diff.py OLD NEW [--timeout 秒] [--max-bytes N] [--max-lines N]

把 claude-release-diff-task.md 固定的三個比較做成不靠模型的純文字報告：
  1. `OLD --help` ↔ `NEW --help`，以及兩邊共有或新增子命令的 `--help`；
  2. `.describe("…")` 欄位差集；
  3. `CLAUDE_*`／`CLAUDE_CODE_*` 環境變數名差集。

不在 daemon 行程裡讀 binary：本腳本是獨立行程，--help 以子行程執行（逾時、輸出上限、
空環境與拋棄式 HOME），binary 以 8 MiB 區塊串流掃描，記憶體不隨檔案大小成長。
只讀：不寫任何狀態、不改設定。退出碼 0＝報告已產出（個別區塊失敗會寫在報告裡，
不影響其他區塊）；2＝參數或檔案不對。
"""
from __future__ import annotations

import argparse
import difflib
import os
import re
import resource
import signal
import subprocess
import sys
import tempfile
import time

CHUNK = 8 * 1024 * 1024
OVERLAP = 1024  # 大於單一 describe 比對的最長長度（300 字元＋引號與括號）
DESCRIBE_RE = re.compile(rb'\.describe\("((?:[^"\\]|\\.){20,300})"\)')
ENV_RE = re.compile(rb"CLAUDE(?:_CODE)?_[A-Z0-9_]{3,40}")
HELP_CAP = 1024 * 1024
MAX_SUBCOMMANDS = 40
FIELD_WIDTH = 240


class Deadline:
    def __init__(self, seconds: float):
        self.end = time.monotonic() + seconds

    def left(self) -> float:
        return self.end - time.monotonic()


def run_help(binary: str, args: list[str], deadline: Deadline, home: str) -> tuple[str | None, str]:
    """回傳 (輸出, 錯誤說明)；輸出為 None 代表沒跑成。"""
    budget = min(15.0, deadline.left())
    if budget <= 0:
        return None, "已超過總逾時"
    env = {"PATH": "/usr/bin:/bin", "HOME": home, "CLAUDE_CONFIG_DIR": os.path.join(home, "claude"),
           "DISABLE_AUTOUPDATER": "1", "NO_COLOR": "1", "TERM": "dumb", "CI": "1"}

    def limits() -> None:
        os.setsid()
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_CPU, (int(budget) + 5, int(budget) + 5))

    try:
        proc = subprocess.Popen([binary, *args, "--help"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, env=env, cwd=home, preexec_fn=limits)
    except OSError as err:
        return None, f"無法執行：{err}"
    try:
        out, _ = proc.communicate(timeout=budget)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            proc.kill()
        proc.communicate()
        return None, f"逾時（{budget:.0f} 秒）"
    text = out[:HELP_CAP].decode("utf-8", "replace")
    if proc.returncode != 0 and not text.strip():
        return None, f"exit {proc.returncode} 且沒有輸出"
    return text, ""


def subcommands(help_text: str) -> list[str]:
    names: list[str] = []
    in_commands = False
    for line in help_text.splitlines():
        if line.startswith("Commands:"):
            in_commands = True
            continue
        if not in_commands:
            continue
        if line and not line.startswith(" "):
            break  # 下一個區段
        m = re.match(r"^  ([a-z][a-z0-9-]*)(?:\|[a-z0-9|-]+)?(?:\s|$)", line)
        if m and m.group(1) != "help":
            names.append(m.group(1))
    return names


def unified(old: str, new: str, label: str) -> list[str]:
    return list(difflib.unified_diff(old.splitlines(), new.splitlines(), f"OLD {label}", f"NEW {label}",
                                     lineterm="", n=0))


def help_section(old: str, new: str, deadline: Deadline, home: str) -> list[str]:
    out = ["== HELP DIFF =="]
    old_help, old_err = run_help(old, [], deadline, home)
    new_help, new_err = run_help(new, [], deadline, home)
    if old_help is None or new_help is None:
        out.append(f"ERROR: OLD --help：{old_err or 'ok'}；NEW --help：{new_err or 'ok'}")
        return out
    diff = unified(old_help, new_help, "--help")
    out += diff if diff else ["(頂層 --help 無差異)"]
    old_subs, new_subs = subcommands(old_help), subcommands(new_help)
    added = [s for s in new_subs if s not in old_subs]
    removed = [s for s in old_subs if s not in new_subs]
    out.append(f"NEW SUBCOMMANDS: {', '.join(added) if added else '(無)'}")
    out.append(f"REMOVED SUBCOMMANDS: {', '.join(removed) if removed else '(無)'}")
    for sub in added[:MAX_SUBCOMMANDS]:
        text, err = run_help(new, [sub], deadline, home)
        out.append(f"-- NEW `{sub} --help` --")
        out += text.splitlines() if text is not None else [f"ERROR: {err}"]
    common = [s for s in new_subs if s in old_subs][:MAX_SUBCOMMANDS]
    for sub in common:
        a, ea = run_help(old, [sub], deadline, home)
        b, eb = run_help(new, [sub], deadline, home)
        if a is None or b is None:
            out.append(f"-- `{sub} --help` -- ERROR: OLD：{ea or 'ok'}；NEW：{eb or 'ok'}")
            continue
        diff = unified(a, b, f"{sub} --help")
        if diff:
            out.append(f"-- CHANGED `{sub} --help` --")
            out += diff
    return out


def scan(path: str, deadline: Deadline) -> tuple[set[str], set[str]]:
    fields: set[str] = set()
    envs: set[str] = set()
    with open(path, "rb") as fh:
        tail = b""
        while True:
            if deadline.left() <= 0:
                raise TimeoutError("掃描 binary 超過總逾時")
            block = fh.read(CHUNK)
            if not block:
                break
            data = tail + block
            for m in DESCRIBE_RE.finditer(data):
                fields.add(m.group(1).decode("utf-8", "replace"))
            envs.update(x.decode("ascii") for x in ENV_RE.findall(data))
            tail = data[-OVERLAP:]
    return fields, envs


def clip(text: str) -> str:
    return text if len(text) <= FIELD_WIDTH else text[:FIELD_WIDTH] + "…"


def strings_section(old: str, new: str, deadline: Deadline) -> list[str]:
    try:
        old_fields, old_envs = scan(old, deadline)
        new_fields, new_envs = scan(new, deadline)
    except (OSError, TimeoutError) as err:
        return ["== DESCRIBE FIELDS / ENV ==", f"ERROR: {err}"]
    out = ["== NEW FIELDS (describe) =="]
    out += [f" - {clip(s)}" for s in sorted(new_fields - old_fields)] or ["(無)"]
    out.append("== REMOVED FIELDS (describe) ==")
    out += [f" - {clip(s)}" for s in sorted(old_fields - new_fields)] or ["(無)"]
    out.append("== NEW ENV ==")
    out.append(", ".join(sorted(new_envs - old_envs)) or "(無)")
    out.append("== REMOVED ENV ==")
    out.append(", ".join(sorted(old_envs - new_envs)) or "(無)")
    return out


def cap(lines: list[str], max_lines: int, max_bytes: int) -> list[str]:
    result: list[str] = []
    size = 0
    for i, line in enumerate(lines):
        size += len(line.encode("utf-8")) + 1
        if len(result) >= max_lines or size > max_bytes:
            result.append(f"[截斷：還有 {len(lines) - i} 行未列出，上限 {max_lines} 行／{max_bytes} bytes]")
            break
        result.append(line)
    return result


def build_report(old: str, new: str, timeout: float, max_lines: int, max_bytes: int) -> str:
    with tempfile.TemporaryDirectory(prefix="claude-binary-diff-") as home:
        lines = ["Claude binary diff（決定性比較；唯讀，未實測任何新功能）", f"OLD={old}", f"NEW={new}"]
        # 兩個區塊各自一份逾時：--help 卡住不該害 binary 字串掃描沒機會跑。
        lines += help_section(old, new, Deadline(timeout), home)
        lines += strings_section(old, new, Deadline(timeout))
    return "\n".join(cap(lines, max_lines, max_bytes)) + "\n"


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("old")
    ap.add_argument("new")
    ap.add_argument("--timeout", type=float, default=120.0, help="每個區塊（--help／字串掃描）的逾時秒數（預設 120）")
    ap.add_argument("--max-lines", type=int, default=600)
    ap.add_argument("--max-bytes", type=int, default=60000)
    args = ap.parse_args(argv)
    for label, path in (("OLD", args.old), ("NEW", args.new)):
        if not os.path.isfile(path) or not os.access(path, os.X_OK):
            print(f"{label} 不是可執行檔：{path}", file=sys.stderr)
            return 2
    sys.stdout.write(build_report(args.old, args.new, args.timeout, args.max_lines, args.max_bytes))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
