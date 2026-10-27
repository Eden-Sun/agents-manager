#!/usr/bin/env bash
# scripts/claude-cache-stats.py 的隔離測試：只用自造 jsonl（--root 指到暫存目錄），不讀 ~/.claude*。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
    echo "skip: 沒有 python3"
    exit 0
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
# canary-gap: PATH 原樣繼承；被測腳本與這段造資料只用 python 標準庫。
python3 -B - "$TMP" <<'PY'
import datetime, json, os, sys
root = sys.argv[1]
now = datetime.datetime.now(datetime.timezone.utc)
def rec(mid, rid, minutes_ago, i, w, r):
    ts = (now - datetime.timedelta(minutes=minutes_ago)).isoformat().replace("+00:00", "Z")
    return json.dumps({"timestamp": ts, "requestId": rid,
                       "message": {"id": mid, "usage": {"input_tokens": i, "cache_creation_input_tokens": w, "cache_read_input_tokens": r}}})
for proj, rows in {
    "-x-supervisor-AGM": [rec("a", "1", 300, 1, 1000, 0), rec("a", "1", 300, 1, 1000, 0),  # 重複行只算一次
                          rec("b", "2", 290, 1, 0, 1000), rec("c", "3", 100, 1, 5000, 1000)],  # 190 分間隔＝冷喚醒
    "-x-supervisor-AGM-responder": [rec("d", "4", 10, 1, 0, 2000)],
}.items():
    os.makedirs(f"{root}/{proj}")
    open(f"{root}/{proj}/aaaaaaaa-0000.jsonl", "w").write("\n".join(rows) + "\n")
PY
# canary-gap: PATH 原樣繼承；被測腳本只用 python 標準庫、只讀 --root 指到的暫存目錄，不 spawn 其他行程。
S="python3 -B $HERE/claude-cache-stats.py"
out="$($S summary --root "$TMP" --project 'supervisor-AGM$' 2>/dev/null)"
grep -q "supervisor-AGM " <<<"$out" || { echo "FAIL: summary 沒列出 supervisor-AGM"; echo "$out"; exit 1; }
grep -q "responder" <<<"$out" && { echo "FAIL: \$ 結尾比對把 responder 也算進去"; exit 1; }
# 3 筆去重後請求：write 6000、read 2000 → hit = 2000/(3+6000+2000)=25.0%
grep -q "TOTAL .* 3 " <<<"$out" && grep -q "25.0" <<<"$out" || { echo "FAIL: 去重或命中率不對"; echo "$out"; exit 1; }
out="$($S sessions --root "$TMP" --project 'supervisor-AGM$' --min-turns 1 2>/dev/null)"
grep -Eq "aaaaaaaa +3 +.* 1 +0\.0" <<<"$out" || { echo "FAIL: sessions 輪數／冷喚醒次數不對"; echo "$out"; exit 1; }
echo "ok: claude-cache-stats"
