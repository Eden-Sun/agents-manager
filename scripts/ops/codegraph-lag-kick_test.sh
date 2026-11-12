#!/bin/bash
# codegraph-lag.py／codegraph-lag-kick.sh 的隔離測試：暫存 git repo＋假 codegraph.db＋假 agm，不碰真的索引。
#
#   bash scripts/ops/codegraph-lag-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if ! grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（不該出現 '$2'）"; FAIL=$((FAIL + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
REPO="$ROOT/repo"; BIN="$ROOT/bin"; FIX="$ROOT/fix"
mkdir -p "$REPO" "$BIN" "$FIX"
cp "$HERE/codegraph-lag.py" "$HERE/kick-common.sh" "$HERE/codegraph-lag-kick.sh" "$BIN/"
cat > "$BIN/agm" <<STUB
#!/bin/bash
echo "agm \$*" >> "$FIX/agm.log"
exit 0
STUB
chmod +x "$BIN/agm"

git -C "$REPO" init -q -b main
G="git -C $REPO -c user.name=t -c user.email=t@t"
echo 'fn a() {}' > "$REPO/a.rs"; echo 'fn b() {}' > "$REPO/b.rs"
$G add . && $G commit -q -m c1
OLD_TS=$(( $(date +%s) - 3600 ))
echo 'fn a2() {}' > "$REPO/a.rs"; GIT_COMMITTER_DATE="@$OLD_TS" $G commit -q -am c2
echo 'fn a3() {}' > "$REPO/a.rs"; GIT_COMMITTER_DATE="@$OLD_TS" $G commit -q -am c3

# 假索引：a.rs 停在 c1 的內容、b.rs 與 HEAD 一致。
mkdir -p "$REPO/.codegraph"
python3 - "$REPO" <<'PY'
import hashlib, sqlite3, sys, os
repo = sys.argv[1]
con = sqlite3.connect(os.path.join(repo, '.codegraph', 'codegraph.db'))
con.execute('create table files (path text primary key, content_hash text, indexed_at integer)')
con.execute('create table unresolved_refs (x integer)')
h = lambda s: hashlib.sha256(s.encode()).hexdigest()
con.execute('insert into files values (?,?,?)', ('a.rs', h('fn a() {}\n'), 1000))
con.execute('insert into files values (?,?,?)', ('b.rs', h('fn b() {}\n'), 1000))
con.commit()
PY
# canary-gap: 只用 python3 標準庫、PATH 原樣繼承；寫入只在 mktemp 的 $ROOT 內，不碰真的索引。
PY_BIN="python3 -B $BIN/codegraph-lag.py"

OUT=$($PY_BIN --repo "$REPO" --max-commits 20 --max-minutes 30 --json)
equals "過期檔判斷：a.rs 過期、b.rs 不算" "$(echo "$OUT" | python3 -c 'import json,sys;print(json.load(sys.stdin)["stale"])')" 1
equals "落後 commit 數：索引停在 c1，之後碰過 a.rs 的 c2、c3" "$(echo "$OUT" | python3 -c 'import json,sys;print(json.load(sys.stdin)["lag_commits"])')" 2
$PY_BIN --repo "$REPO" --max-commits 20 --max-minutes 600 >/dev/null; equals "兩個門檻都沒超過回 exit 0" "$?" 0
$PY_BIN --repo "$REPO" --max-commits 1 --max-minutes 30 >/dev/null; equals "超過 commit 門檻回 exit 1" "$?" 1
$PY_BIN --repo "$REPO" --max-commits 20 --max-minutes 1 >/dev/null; equals "超過分鐘門檻回 exit 1" "$?" 1

# 主樹的未提交改動：索引跟著主樹走，就不算過期。
echo 'fn a3() {}' > "$REPO/a.rs"
echo 'fn a2() {}' > "$REPO/a.rs"
python3 - "$REPO" <<'PY'
import hashlib, sqlite3, sys, os
con = sqlite3.connect(os.path.join(sys.argv[1], '.codegraph', 'codegraph.db'))
con.execute("update files set content_hash=? where path='a.rs'", (hashlib.sha256(b'fn a2() {}\n').hexdigest(),))
con.commit()
PY
OUT=$($PY_BIN --repo "$REPO" --max-commits 1 --max-minutes 1 --json)
equals "主樹與索引一致（未提交）不算落後" "$(echo "$OUT" | python3 -c 'import json,sys;print(json.load(sys.stdin)["stale"])')" 0
$PY_BIN --repo "$REPO" --max-commits 1 --max-minutes 1 >/dev/null; equals "一致時 exit 0" "$?" 0
git -C "$REPO" checkout -q -- a.rs

# kick：落後就喊 codegraph_index_lag；沒落後不喊；沒有索引喊 unreadable。
: > "$FIX/agm.log"
CG_LAG_MAX_COMMITS=1 CG_LAG_MAX_MINUTES=1 AGM_REPO="$REPO" AGM_BIN="$BIN/agm" CG_LAG_PY="$BIN/codegraph-lag.py" \
  CG_LAG_LOG="$ROOT/lag.log" bash "$BIN/codegraph-lag-kick.sh"; equals "kick 落後時 exit 0（失敗由 ops-alert 表達）" "$?" 0
check "落後時喊 codegraph_index_lag" "ops-alert .*--reason codegraph_index_lag" "$FIX/agm.log"

: > "$FIX/agm.log"
CG_LAG_MAX_COMMITS=20 CG_LAG_MAX_MINUTES=600 AGM_REPO="$REPO" AGM_BIN="$BIN/agm" CG_LAG_PY="$BIN/codegraph-lag.py" \
  CG_LAG_LOG="$ROOT/lag.log" bash "$BIN/codegraph-lag-kick.sh"
check_no "門檻內不喊" "ops-alert" "$FIX/agm.log"

NOIDX="$ROOT/noidx"; git -C "$ROOT" init -q "$NOIDX" 2>/dev/null; mkdir -p "$NOIDX"; git -C "$NOIDX" init -q -b main
git -C "$NOIDX" -c user.name=t -c user.email=t@t commit -q --allow-empty -m x
: > "$FIX/agm.log"
AGM_REPO="$NOIDX" AGM_BIN="$BIN/agm" CG_LAG_PY="$BIN/codegraph-lag.py" CG_LAG_LOG="$ROOT/lag.log" bash "$BIN/codegraph-lag-kick.sh"
check "沒有索引喊 codegraph_index_unreadable" "--reason codegraph_index_unreadable" "$FIX/agm.log"

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
