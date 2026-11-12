#!/usr/bin/env python3
"""CodeGraph 用量：本機各 kind（claude／codex／grok／agy）的工具呼叫裡，「找程式」與 codegraph 各佔幾成。

  python3 scripts/codegraph-usage.py               # 最近 7 天，kind 表＋ bot 表
  python3 scripts/codegraph-usage.py --days 14 --top 20

只讀檔、不改任何東西。
- 找程式＝grep／rg、cat／sed -n／head／tail、Read／read_file，與 codegraph（MCP 或 CLI）。
- kind：claude（~/.claude*/projects）、codex（~/.codex*/sessions）、grok（~/.grok/sessions/*/*/updates.jsonl）。
  agy（antigravity）的對話是 protobuf、沒有工具呼叫紀錄，只列出這段時間的提問數，工具欄標 n/a。
- bot：transcript 沒有 bot id，用工作目錄推：`.claude/worktrees/<名>` → 名；主 checkout → main；其他取目錄名。
- 「explore 後仍 grep/Read 同檔」：codegraph 呼叫的參數裡有檔名（含 `/` 或副檔名）時，之後同 session 5 次找程式
  內又出現同一檔名的 grep／Read，就算一次。分母只算帶檔名的 codegraph 呼叫；沒有檔名的不計。
"""
import argparse
import datetime as dt
import glob
import json
import os
import re
import urllib.parse
from collections import Counter, defaultdict
from dataclasses import dataclass

HOME = os.path.expanduser('~')
CLAUDE_ROOTS = [f'{HOME}/.claude/projects', f'{HOME}/.claude-*/projects']
CODEX_ROOTS = [f'{HOME}/.codex/sessions', f'{HOME}/.codex-*/sessions']
GROK_GLOB = f'{HOME}/.grok/sessions/*/*/updates.jsonl'
AGY_HISTORY = f'{HOME}/.gemini/antigravity-cli/history.jsonl'
KINDS = ('claude', 'codex', 'grok', 'agy')

CG_CLI = re.compile(r'\bcodegraph (explore|callers|callees|impact|query|context|node|search)\b')
GREP = re.compile(r'(^|[|;&(\s])(rtk )?(grep|rg)\b')
READ = re.compile(r'(^|[|;&(\s])(sed -n|cat|nl -ba|head|tail)\b')
FILE_TOKEN = re.compile(r'[\w./-]*[\w-]+\.(?:rs|ts|tsx|js|mjs|py|sh|md|go|toml|json|yml|yaml|sql)\b')
FIND_KINDS = ('codegraph', 'grep', 'read')
FOLLOW_WINDOW = 5


@dataclass
class Call:
    kind: str       # claude／codex／grok
    bot: str
    session: str
    ts: dt.datetime
    tool: str
    text: str


def classify(tool: str, text: str) -> str | None:
    """一次工具呼叫算哪一類；不是找程式回 None。`tool` 是工具名，`text` 是指令或參數。"""
    if 'codegraph' in tool.lower():
        return 'codegraph'
    if tool == 'Read':
        return 'read'
    if tool in ('Grep', 'Glob'):
        return 'grep'
    if tool in ('Bash', 'exec', 'exec_command', 'shell', 'function_call', 'run_terminal_command'):
        if CG_CLI.search(text):
            return 'codegraph'
        if GREP.search(text):
            return 'grep'
        if READ.search(text):
            return 'read'
    return None


def bot_of(cwd: str) -> str:
    if not cwd:
        return '?'
    m = re.search(r'/\.claude/worktrees/([^/]+)', cwd)
    if m:
        return m.group(1)
    if cwd.rstrip('/').endswith('/project/agents-manager'):
        return 'main'
    return os.path.basename(cwd.rstrip('/')) or '?'


def _ts(s: str) -> dt.datetime | None:
    try:
        return dt.datetime.fromisoformat(s.replace('Z', '+00:00'))
    except (ValueError, AttributeError):
        return None


def _claude(path: str, since: dt.datetime):
    sess = os.path.basename(path)
    cwd = ''
    for line in open(path, errors='ignore'):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        cwd = r.get('cwd') or cwd
        ts = _ts(r.get('timestamp') or '')
        if not ts or ts < since:
            continue
        for c in (r.get('message') or {}).get('content') or []:
            if isinstance(c, dict) and c.get('type') == 'tool_use':
                inp = c.get('input') or {}
                text = str(inp.get('command') or inp.get('file_path') or inp.get('pattern') or inp)
                yield Call('claude', bot_of(cwd), sess, ts, c.get('name', ''), text)


def _codex(path: str, since: dt.datetime):
    sess = os.path.basename(path)
    cwd = ''
    for line in open(path, errors='ignore'):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        p = r.get('payload') or {}
        if isinstance(p, dict) and p.get('cwd'):
            cwd = p['cwd']
        ts = _ts(r.get('timestamp') or '')
        if not ts or ts < since or not isinstance(p, dict):
            continue
        if p.get('type') in ('function_call', 'custom_tool_call', 'mcp_tool_call'):
            text = str(p.get('arguments') or p.get('input') or '')
            yield Call('codex', bot_of(cwd), sess, ts, str(p.get('name') or ''), text)


def _grok(path: str, since: dt.datetime):
    sess = os.path.basename(os.path.dirname(path))
    cwd = urllib.parse.unquote(os.path.basename(os.path.dirname(os.path.dirname(path))))
    bot = bot_of(cwd)
    for line in open(path, errors='ignore'):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        u = ((r.get('params') or {}).get('update')) or {}
        if u.get('sessionUpdate') != 'tool_call':
            continue
        ts = dt.datetime.fromtimestamp(float(r.get('timestamp') or 0), dt.timezone.utc)
        if ts < since:
            continue
        title = str(u.get('title') or '')
        raw = u.get('rawInput') or {}
        tool = {'read_file': 'Read', 'run_terminal_command': 'Bash'}.get(title, title)
        text = str(raw.get('command') or raw) if tool == 'Bash' else json.dumps(raw, ensure_ascii=False)
        yield Call('grok', bot, sess, ts, tool, text)


def _classify_call(c: Call) -> str | None:
    if c.kind == 'grok' and c.tool in ('grep', 'list_dir'):
        return 'grep'
    return classify(c.tool, c.text)


def _files_in(c: Call) -> set[str]:
    return set(FILE_TOKEN.findall(c.text))


def collect(days: float):
    since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=days)
    calls: list[Call] = []
    for pat in CLAUDE_ROOTS:
        for f in glob.glob(pat + '/**/*.jsonl', recursive=True):
            if os.path.getmtime(f) >= since.timestamp():
                calls += list(_claude(f, since))
    for pat in CODEX_ROOTS:
        for f in glob.glob(pat + '/**/*.jsonl', recursive=True):
            if os.path.getmtime(f) >= since.timestamp():
                calls += list(_codex(f, since))
    for f in glob.glob(GROK_GLOB):
        if os.path.getmtime(f) >= since.timestamp():
            calls += list(_grok(f, since))
    agy = 0
    if os.path.exists(AGY_HISTORY):
        for line in open(AGY_HISTORY, errors='ignore'):
            try:
                ts = int(json.loads(line).get('timestamp') or 0) / 1000
            except ValueError:
                continue
            if ts and dt.datetime.fromtimestamp(ts, dt.timezone.utc) >= since:
                agy += 1
    calls.sort(key=lambda c: c.ts)
    return calls, agy


def followups(calls: list[Call]) -> dict[str, list[int]]:
    """kind → [帶檔名的 codegraph 呼叫數, 其後同檔又 grep／Read 的次數]。"""
    out: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    by_sess: dict[tuple, list[Call]] = defaultdict(list)
    for c in calls:
        by_sess[(c.kind, c.session)].append(c)
    for seq in by_sess.values():
        finds = [(i, c, _classify_call(c)) for i, c in enumerate(seq)]
        finds = [x for x in finds if x[2] in FIND_KINDS]
        for k, (i, c, cat) in enumerate(finds):
            if cat != 'codegraph':
                continue
            files = _files_in(c)
            if not files:
                continue
            out[c.kind][0] += 1
            later = [x for x in finds[k + 1:k + 1 + FOLLOW_WINDOW] if x[2] in ('grep', 'read')]
            if any(files & _files_in(x[1]) for x in later):
                out[c.kind][1] += 1
    return out


def _pct(a: int, b: int) -> str:
    return f'{a / b:6.1%}' if b else '   n/a'


def report(calls: list[Call], agy: int, days: float, top: int) -> None:
    per_kind: dict[str, Counter] = defaultdict(Counter)
    per_bot: dict[tuple, Counter] = defaultdict(Counter)
    for c in calls:
        cat = _classify_call(c)
        per_kind[c.kind]['all'] += 1
        per_bot[(c.bot, c.kind)]['all'] += 1
        if cat:
            per_kind[c.kind][cat] += 1
            per_kind[c.kind]['find'] += 1
            per_bot[(c.bot, c.kind)][cat] += 1
            per_bot[(c.bot, c.kind)]['find'] += 1
    fu = followups(calls)
    print(f'CodeGraph 用量（最近 {days:g} 天）\n')
    print('kind     工具呼叫  找程式  codegraph  佔工具呼叫  佔找程式  explore後仍grep/Read同檔')
    for k in KINDS:
        c = per_kind.get(k)
        if k == 'agy':
            print(f'{k:8} {"n/a":>8}  {"n/a":>6}  {"n/a":>9}  {"n/a":>10}  {"n/a":>8}  '
                  f'（{agy} 則提問，對話為 protobuf，無工具紀錄）')
            continue
        if not c:
            print(f'{k:8} {0:>8}  {0:>6}  {0:>9}  {"n/a":>10}  {"n/a":>8}  {"n/a":>10}')
            continue
        cg = c['codegraph']
        f2, hit = fu.get(k, [0, 0])
        print(f'{k:8} {c["all"]:8}  {c["find"]:6}  {cg:9}  {_pct(cg, c["all"]):>10}  {_pct(cg, c["find"]):>8}  '
              f'{hit}/{f2} {_pct(hit, f2)}')
    print('\n各 bot（依工具呼叫數排序，前 %d）' % top)
    print('bot                    kind    工具呼叫  codegraph  佔工具呼叫')
    rows = sorted(per_bot.items(), key=lambda kv: -kv[1]['all'])[:top]
    for (bot, kind), c in rows:
        print(f'{bot[:22]:22} {kind:7} {c["all"]:8}  {c["codegraph"]:9}  {_pct(c["codegraph"], c["all"]):>10}')


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument('--days', type=float, default=7)
    ap.add_argument('--top', type=int, default=15)
    a = ap.parse_args()
    calls, agy = collect(a.days)
    report(calls, agy, a.days, a.top)


if __name__ == '__main__':
    main()
