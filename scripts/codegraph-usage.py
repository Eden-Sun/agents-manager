#!/usr/bin/env python3
"""CodeGraph 用了多少：本機 claude transcript 與 codex rollout 裡「找程式」的工具呼叫，codegraph 佔幾成。

  python3 scripts/codegraph-usage.py               # 最近 7 天
  python3 scripts/codegraph-usage.py --days 14
只讀檔、不改任何東西。找程式＝grep／rg、cat／sed -n／head／tail、Read，與 codegraph（MCP 或 CLI）。
"""
import argparse
import datetime as dt
import glob
import json
import os
import re
from collections import Counter

CLAUDE_ROOTS = ['~/.claude/projects', '~/.claude-*/projects']
CODEX_ROOTS = ['~/.codex/sessions', '~/.codex-*/sessions']
CG_CLI = re.compile(r'\bcodegraph (explore|callers|callees|impact|query|context|node|search)\b')
GREP = re.compile(r'(^|[|;&(\s])(rtk )?(grep|rg)\b')
READ = re.compile(r'(^|[|;&(\s])(sed -n|cat|nl -ba|head|tail)\b')


def classify(tool: str, text: str) -> str | None:
    """一次工具呼叫算哪一類；不是找程式回 None。`tool` 是工具名，`text` 是指令或參數。"""
    if 'codegraph' in tool:
        return 'codegraph'
    if tool == 'Read':
        return 'read'
    if tool in ('Grep', 'Glob'):
        return 'grep'
    if tool in ('Bash', 'exec', 'exec_command', 'shell', 'function_call'):
        if CG_CLI.search(text):
            return 'codegraph'
        if GREP.search(text):
            return 'grep'
        if READ.search(text):
            return 'read'
    return None


def _calls(path: str):
    """(時間, 工具名, 參數字串) — claude 的 tool_use 與 codex 的 function_call。"""
    for line in open(path, errors='ignore'):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        ts = r.get('timestamp')
        if not ts:
            continue
        p = r.get('payload') or {}
        if p.get('type') in ('function_call', 'custom_tool_call', 'mcp_tool_call'):
            yield ts, str(p.get('name') or ''), str(p.get('arguments') or p.get('input') or '')
            continue
        for c in (r.get('message') or {}).get('content') or []:
            if isinstance(c, dict) and c.get('type') == 'tool_use':
                inp = c.get('input') or {}
                yield ts, c.get('name', ''), str(inp.get('command') or inp)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument('--days', type=float, default=7)
    a = ap.parse_args()
    since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=a.days)
    files = []
    for root in CLAUDE_ROOTS + CODEX_ROOTS:
        files += glob.glob(os.path.expanduser(root) + '/**/*.jsonl', recursive=True)
    per_day: dict[str, Counter] = {}
    for f in files:
        if os.path.getmtime(f) < since.timestamp():
            continue
        src = 'codex' if '/sessions/' in f else 'claude'
        for ts, tool, text in _calls(f):
            t = dt.datetime.fromisoformat(ts.replace('Z', '+00:00'))
            kind = classify(tool, text) if t >= since else None
            if kind:
                per_day.setdefault(t.date().isoformat(), Counter())[f'{src}:{kind}'] += 1
    total = Counter()
    print('日期        找程式  codegraph  佔比   (claude/codex)')
    for day in sorted(per_day):
        c = per_day[day]
        total += c
        _row(day, c)
    _row('合計', total)


def _row(label: str, c: Counter) -> None:
    allc = sum(c.values())
    cg = c['claude:codegraph'] + c['codex:codegraph']
    print(f'{label:10} {allc:7} {cg:9} {cg / max(1, allc):6.1%}   ({c["claude:codegraph"]}/{c["codex:codegraph"]})')


if __name__ == '__main__':
    main()
