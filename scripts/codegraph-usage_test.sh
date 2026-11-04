#!/usr/bin/env bash
# scripts/codegraph-usage.py 的分類規則測試：只呼叫 classify()，不讀 ~/.claude*／~/.codex*。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# canary-gap: PATH 原樣繼承；只用 python 標準庫。
python3 -B - "$HERE/codegraph-usage.py" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location('cgu', sys.argv[1])
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
cases = [
    ('mcp__codegraph__codegraph_explore', '{}', 'codegraph'),
    ('Bash', 'codegraph callers foo', 'codegraph'),
    ('Bash', 'cd x && rtk grep -n foo a.rs', 'grep'),
    ('exec', 'rg foo', 'grep'),
    ('Bash', 'sed -n 1,20p a.rs', 'read'),
    ('Read', '{}', 'read'),
    ('Bash', 'cargo build', None),
    ('Bash', 'codegraph sync', None),  # 維護索引不算查詢
    ('Edit', '{}', None),
]
bad = [(t, x, want, m.classify(t, x)) for t, x, want in cases if m.classify(t, x) != want]
for b in bad:
    print('FAIL: classify%r want %r got %r' % (b[:2], b[2], b[3]))
sys.exit(1 if bad else 0)
PY
echo "ok   - codegraph-usage classify"
