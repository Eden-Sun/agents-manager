#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

REPO="${OUTBOX_GC_TEST_REPO:-$HERE/../..}"
STARTUP_RS="$REPO/daemon/src/startup.rs"
TRIAGE_RS="$REPO/crates/am-base/src/release_triage/mod.rs"
extract_startup_name() { grep 'open_with(&prepared.dir.join(' | grep -o 'join("[^"]*\.sqlite3")' | head -1 | sed 's/^join("//; s/")$//'; }
extract_triage_name() { grep -A3 'pub fn default_db_path' | grep -o 'join("[^"]*\.sqlite3")' | head -1 | sed 's/^join("//; s/")$//'; }
db_name_from() {
  local name
  name=$(cat "$1" 2>/dev/null | "$3")
  [ -n "$name" ] || { sleep 1; name=$(cat "$1" 2>/dev/null | "$3"); }
  [ -n "$name" ] || name=$(git -C "$REPO" show "HEAD:$2" 2>/dev/null | "$3")
  printf '%s' "$name"
}
SHARE_DB_NAME=$(db_name_from "$STARTUP_RS" daemon/src/startup.rs extract_startup_name)
TRIAGE_DB_NAME=$(db_name_from "$TRIAGE_RS" crates/am-base/src/release_triage/mod.rs extract_triage_name)
SKIP_SUITE=""
if [ -z "$SHARE_DB_NAME" ]; then
  echo "FAIL - could not read the DB file name from startup.rs（${STARTUP_RS}；工作目錄與 git HEAD 都讀不到），跳過依賴分享名單資料庫的測試"; FAIL=$((FAIL + 1))
  SKIP_SUITE=1
elif [ "$SHARE_DB_NAME" = "$TRIAGE_DB_NAME" ]; then
  echo "ok   - daemon 兩處開帳本的檔名一致（${SHARE_DB_NAME}）"; PASS=$((PASS + 1))
else
  echo "FAIL - daemon 開帳本的檔名對不上：startup.rs='${SHARE_DB_NAME}' release_triage='${TRIAGE_DB_NAME}'"; FAIL=$((FAIL + 1))
fi
[ -n "${OUTBOX_GC_TEST_NAMES_ONLY:-}" ] && SKIP_SUITE=1
if [ -n "$SHARE_DB_NAME" ]; then
  check "outbox-gc.sh 的 SHARE_DB 與 daemon 同名" "^SHARE_DB=.*/${SHARE_DB_NAME}\"" "$HERE/outbox-gc.sh"
fi

if [ -z "${OUTBOX_GC_TEST_INNER:-}" ]; then
  SELF="$HERE/outbox-gc_test.sh"
  SELF_FAKE=$(mktemp -d)
  mkdir -p "$SELF_FAKE/daemon/src" "$SELF_FAKE/crates/am-base/src/release_triage"
  printf '// 這個假的 startup.rs 沒有開帳本的那一行\n' > "$SELF_FAKE/daemon/src/startup.rs"
  cp "$TRIAGE_RS" "$SELF_FAKE/crates/am-base/src/release_triage/mod.rs"
  out=$(OUTBOX_GC_TEST_REPO="$SELF_FAKE" OUTBOX_GC_TEST_INNER=1 bash "$SELF" 2>&1); rc=$?
  nfail=$(printf '%s\n' "$out" | grep -c '^FAIL' || true)
  if [ "$rc" -ne 0 ] && [ "$nfail" -eq 1 ] && printf '%s\n' "$out" | grep -q '^FAIL - could not read the DB file name from startup.rs'; then
    echo "ok   - 自我測試：缺行的假 repo 只出一個 FAIL、不連鎖"; PASS=$((PASS + 1))
  else
    echo "FAIL - 自我測試：缺行的假 repo 應只出一個 FAIL（rc=${rc}，FAIL=${nfail}）"; printf '%s\n' "$out" | sed 's/^/      /'; FAIL=$((FAIL + 1))
  fi
  out=$(OUTBOX_GC_TEST_NAMES_ONLY=1 OUTBOX_GC_TEST_INNER=1 bash "$SELF" 2>&1); rc=$?
  if [ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -q '^ok   - daemon 兩處開帳本的檔名一致'; then
    echo "ok   - 自我測試：正常 repo 讀得到檔名並通過"; PASS=$((PASS + 1))
  else
    echo "FAIL - 自我測試：正常 repo 應通過（rc=${rc}）"; printf '%s\n' "$out" | sed 's/^/      /'; FAIL=$((FAIL + 1))
  fi
  G=$(mktemp -d)
  mkdir -p "$G/daemon/src" "$G/crates/am-base/src/release_triage"
  cp "$STARTUP_RS" "$G/daemon/src/startup.rs"
  cp "$TRIAGE_RS" "$G/crates/am-base/src/release_triage/mod.rs"
  git -C "$G" init -q && git -C "$G" add -A && git -C "$G" -c user.name=selftest -c user.email=selftest@localhost commit -qm init
  printf '// 工作目錄暫時沒有那一行\n' > "$G/daemon/src/startup.rs"
  out=$(OUTBOX_GC_TEST_REPO="$G" OUTBOX_GC_TEST_NAMES_ONLY=1 OUTBOX_GC_TEST_INNER=1 bash "$SELF" 2>&1); rc=$?
  if [ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -q '^ok   - daemon 兩處開帳本的檔名一致'; then
    echo "ok   - 自我測試：工作目錄讀不到時退回 git HEAD 並通過"; PASS=$((PASS + 1))
  else
    echo "FAIL - 自我測試：工作目錄讀不到時應退回 git HEAD（rc=${rc}）"; printf '%s\n' "$out" | sed 's/^/      /'; FAIL=$((FAIL + 1))
  fi
  rm -rf "$SELF_FAKE" "$G"
fi

if [ -z "$SKIP_SUITE" ]; then

setup() {
  ROOT=$(mktemp -d)
  export HOME="$ROOT/home"
  mkdir -p "$ROOT/agm/bin" "$HOME/.config/agents-manager/outbox" "$HOME/.ssh"
  cp "$HERE/outbox-gc.sh" "$ROOT/agm/bin/outbox-gc.sh"
  OB="$HOME/.config/agents-manager/outbox"; LOG="$ROOT/agm/outbox-gc.log"; : > "$LOG"
  unset AM_OUTBOX_ROOT AM_DATA_DIR OUTBOX_MAX_AGE_MIN OUTBOX_SHARE_KEEP_DAYS OUTBOX_SHARE_CAP_BYTES OUTBOX_SHARE_CAP_FILES
  sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "CREATE TABLE projects (id TEXT PRIMARY KEY, deleted_at TEXT); CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, deleted_at TEXT); CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY);"
}
teardown() { rm -rf "$ROOT"; }
share_bot() { sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "INSERT OR IGNORE INTO projects (id) VALUES ('p-$1'); INSERT OR IGNORE INTO bots (id, project_id) VALUES ('$1', 'p-$1'); INSERT OR IGNORE INTO shared_bots (bot_id) VALUES ('$1');"; }
del_bot() { sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "UPDATE bots SET deleted_at = 'x' WHERE id = '$1';"; }
old() { touch -t 202601010000 "$@"; }
run_at() { OUTBOX_GC_NOW=$(( $(date +%s) + $1 * 60 )) zsh "$ROOT/agm/bin/outbox-gc.sh"; echo $?; }
run() { run_at 120; }
fresh() { touch -t "$(date -v+119M +%Y%m%d%H%M 2>/dev/null || date -d '119 minutes' +%Y%m%d%H%M)" "$@"; }
run_now() { zsh "$ROOT/agm/bin/outbox-gc.sh"; echo $?; }

setup
mkdir -p "$OB/botA/sub" "$OB/botB" "$OB/botC" "$OB/botD"
echo x > "$OB/botA/old.txt"; old "$OB/botA/old.txt"
echo x > "$OB/botA/sub/deep-old.txt"; old "$OB/botA/sub/deep-old.txt"
echo x > "$OB/botB/new.txt"; fresh "$OB/botB/new.txt"
echo x > "$OB/botC/old.txt"; old "$OB/botC/old.txt"
echo x > "$OB/botC/new.txt"; fresh "$OB/botC/new.txt"
echo x > "$OB/rootfile.txt"; old "$OB/rootfile.txt"
equals "正常跑 exit 0" "$(run)" "0"
gone   "bot 目錄裡過期的檔被刪" "$OB/botA/old.txt"
gone   "更深一層過期的檔被刪" "$OB/botA/sub/deep-old.txt"
gone   "刪光後的空目錄（含子目錄）被收掉" "$OB/botA"
gone   "本來就是空的 bot 目錄被收掉" "$OB/botD"
exists "新檔不刪" "$OB/botB/new.txt"
exists "同目錄的新檔不刪" "$OB/botC/new.txt"
gone   "同目錄的過期檔刪" "$OB/botC/old.txt"
exists "outbox 根目錄直屬的檔不碰（mindepth 2）" "$OB/rootfile.txt"
exists "outbox 根目錄本身還在" "$OB"
check  "有刪就記 log（3 個）" "清掉 3 個超過 60 分鐘" "$LOG"
teardown

setup
mkdir -p "$OB/botE/報價_Q-1/附件" "$OB/botE/空的/更空"
echo x > "$OB/botE/報價_Q-1/old.pdf"; old "$OB/botE/報價_Q-1/old.pdf"
echo x > "$OB/botE/報價_Q-1/new.pdf"; fresh "$OB/botE/報價_Q-1/new.pdf"
echo x > "$OB/botE/報價_Q-1/附件/old.csv"; old "$OB/botE/報價_Q-1/附件/old.csv"
echo x > "$OB/botE/報價_Q-1/附件/new.csv"; fresh "$OB/botE/報價_Q-1/附件/new.csv"
equals "子資料夾的清理 exit 0" "$(run)" "0"
gone   "子資料夾裡過期的檔被刪" "$OB/botE/報價_Q-1/old.pdf"
exists "子資料夾裡新的檔留著" "$OB/botE/報價_Q-1/new.pdf"
gone   "更深一層過期的檔被刪" "$OB/botE/報價_Q-1/附件/old.csv"
exists "更深一層新的檔留著" "$OB/botE/報價_Q-1/附件/new.csv"
exists "還有檔的子資料夾留著" "$OB/botE/報價_Q-1"
gone   "本來就空的巢狀子資料夾（含更深一層）被收掉" "$OB/botE/空的"
check  "log 記刪了 2 個" "清掉 2 個超過 60 分鐘" "$LOG"
teardown

setup
mkdir -p "$OB/b"; echo x > "$OB/b/f.txt"; touch -t "$(date -v-30M +%Y%m%d%H%M 2>/dev/null || date -d '30 minutes ago' +%Y%m%d%H%M)" "$OB/b/f.txt"
run_now >/dev/null
exists "30 分鐘的檔在預設 60 分鐘內不刪" "$OB/b/f.txt"
check_no "沒刪東西就不寫 log" "清掉" "$LOG"
OUTBOX_MAX_AGE_MIN=5 run_at 30 >/dev/null
gone   "OUTBOX_MAX_AGE_MIN=5 時 30 分鐘的檔被刪" "$OB/b/f.txt"
check  "log 寫的是實際的保留期" "超過 5 分鐘" "$LOG"
teardown

setup
mkdir -p "$OB/b"; echo x > "$OB/b/moved-in.pdf"; old "$OB/b/moved-in.pdf"
run_now >/dev/null
exists "mtime 很舊、剛搬進來（ctime 是現在）的檔不刪" "$OB/b/moved-in.pdf"
check_no "沒刪就不寫 log" "清掉" "$LOG"
run_at 30 >/dev/null
exists "搬進來 30 分鐘後仍在保留期內" "$OB/b/moved-in.pdf"
run_at 61 >/dev/null
gone   "搬進來超過 60 分鐘才刪" "$OB/b/moved-in.pdf"
teardown

setup
mkdir -p "$OB/share/sub" "$OB/plain"
share_bot share
: > "$OB/share/.am-share-keep"; old "$OB/share/.am-share-keep"
echo x > "$OB/share/report.pdf"; old "$OB/share/report.pdf"
echo x > "$OB/share/sub/deep.pdf"; old "$OB/share/sub/deep.pdf"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "正常跑 exit 0" "$(run_at $((60 * 24 * 7)))" "0"
exists "分享用 bot 的檔 7 天後還在" "$OB/share/report.pdf"
exists "分享用 bot 子目錄的檔也在" "$OB/share/sub/deep.pdf"
exists "標記檔本身不清" "$OB/share/.am-share-keep"
gone   "一般 bot 照常清" "$OB/plain/old.txt"
check  "log 只算一般 bot 那 1 個" "清掉 1 個超過 60 分鐘" "$LOG"
check_no "7 天內分享保留政策沒刪東西" "分享保留政策" "$LOG"
equals "正常跑 exit 0" "$(run_at $((60 * 24 * 15)))" "0"
gone   "超過 14 天的分享檔被刪" "$OB/share/report.pdf"
gone   "超過 14 天的子目錄檔也刪" "$OB/share/sub/deep.pdf"
exists "刪光之後標記檔還在（分享目錄不被收掉）" "$OB/share/.am-share-keep"
check  "log 記分享保留政策刪了 2 個" "分享保留政策清掉 2 個檔案" "$LOG"
teardown

setup
mkdir -p "$OB/share"
share_bot share
: > "$OB/share/.am-share-keep"
for i in 1 2 3 4 5; do echo x > "$OB/share/f$i.txt"; touch -t "20260101000$i" "$OB/share/f$i.txt"; done
OUTBOX_SHARE_CAP_FILES=3 run_at 120 >/dev/null
gone   "超過檔數上限：最舊的 f1 被刪" "$OB/share/f1.txt"
gone   "超過檔數上限：次舊的 f2 被刪" "$OB/share/f2.txt"
exists "留下最新的 3 個（f3）" "$OB/share/f3.txt"
exists "留下最新的 3 個（f4）" "$OB/share/f4.txt"
exists "留下最新的 3 個（f5）" "$OB/share/f5.txt"
exists "標記檔不算量、不刪" "$OB/share/.am-share-keep"
check  "log 記刪了 2 個" "分享保留政策清掉 2 個檔案" "$LOG"
teardown
setup
mkdir -p "$OB/share/sub"
share_bot share
: > "$OB/share/.am-share-keep"
printf '0123456789' > "$OB/share/a.bin"; touch -t 202601010001 "$OB/share/a.bin"
printf '0123456789' > "$OB/share/sub/b.bin"; touch -t 202601010002 "$OB/share/sub/b.bin"
printf '0123456789' > "$OB/share/c.bin"; touch -t 202601010003 "$OB/share/c.bin"
OUTBOX_SHARE_CAP_BYTES=25 run_at 120 >/dev/null
gone   "超過位元組上限：最舊的 a.bin 被刪" "$OB/share/a.bin"
exists "20 位元組在 25 的上限內（含子目錄的 b.bin）" "$OB/share/sub/b.bin"
exists "最新的 c.bin 留下" "$OB/share/c.bin"
teardown
setup
mkdir -p "$OB/share" "$ROOT/outside"
share_bot share
: > "$OB/share/.am-share-keep"
echo secret > "$ROOT/outside/precious.txt"; old "$ROOT/outside/precious.txt"
ln -s "$ROOT/outside/precious.txt" "$OB/share/link.txt"
echo x > "$OB/share/f.txt"
OUTBOX_SHARE_CAP_FILES=1 run_at $((60 * 24 * 15)) >/dev/null
exists "分享目錄裡的符號連結目標不被刪" "$ROOT/outside/precious.txt"
teardown

setup
mkdir -p "$OB/fake" "$OB/live" "$OB/gone" "$OB/proj"
: > "$OB/fake/.am-share-keep"
echo x > "$OB/fake/report.pdf"
share_bot live
echo x > "$OB/live/report.pdf"
: > "$OB/gone/.am-share-keep"; share_bot gone; del_bot gone
echo x > "$OB/gone/report.pdf"
share_bot proj
sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "UPDATE projects SET deleted_at = 'x' WHERE id = 'p-proj';"
echo x > "$OB/proj/report.pdf"
equals "正常跑 exit 0" "$(run)" "0"
gone   "bot 自建的標記不保留：過期檔照一小時清" "$OB/fake/report.pdf"
exists "真的在分享中的 bot 不用標記也保留" "$OB/live/report.pdf"
gone   "分享停了（bot 刪掉）的檔照一小時清" "$OB/gone/report.pdf"
gone   "分享停了（專案刪掉）的檔照一小時清" "$OB/proj/report.pdf"
teardown

setup
mkdir -p "$OB/plain"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
rm -f "$HOME/.config/agents-manager/${SHARE_DB_NAME}"
equals "沒有 DB：exit 1" "$(run)" "1"
exists "沒有 DB：一般檔也不刪" "$OB/plain/old.txt"
check  "沒有 DB：log 寫明拒絕整輪" "拒絕整輪清理" "$LOG"
teardown
setup
mkdir -p "$OB/plain"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
printf 'not a database' > "$HOME/.config/agents-manager/${SHARE_DB_NAME}"
equals "DB 壞掉：exit 1" "$(run)" "1"
exists "DB 壞掉：一般檔也不刪" "$OB/plain/old.txt"
check  "DB 壞掉：log 寫明拒絕整輪" "拒絕整輪清理" "$LOG"
teardown

setup
mkdir -p "$OB/b" "$HOME/.config/agents-manager/bots/x" "$ROOT/scratch"
echo x > "$OB/b/old.txt"; old "$OB/b/old.txt"
echo KEY > "$HOME/.ssh/id_rsa"; old "$HOME/.ssh/id_rsa"
echo DB > "$HOME/.config/agents-manager/agents-manager.db"; old "$HOME/.config/agents-manager/agents-manager.db"
echo T > "$HOME/.config/agents-manager/ui-token"; old "$HOME/.config/agents-manager/ui-token"
echo c > "$HOME/.config/agents-manager/config.toml"; old "$HOME/.config/agents-manager/config.toml"
echo s > "$HOME/.config/agents-manager/bots/x/claude-settings.json"; old "$HOME/.config/agents-manager/bots/x/claude-settings.json"
echo p > "$ROOT/scratch/note.md"; old "$ROOT/scratch/note.md"
run >/dev/null
gone   "outbox 裡過期的檔照刪" "$OB/b/old.txt"
exists "私鑰不碰" "$HOME/.ssh/id_rsa"
exists "SQLite DB 不碰" "$HOME/.config/agents-manager/agents-manager.db"
exists "ui-token 不碰" "$HOME/.config/agents-manager/ui-token"
exists "config.toml 不碰" "$HOME/.config/agents-manager/config.toml"
exists "bots/ 底下的檔不碰" "$HOME/.config/agents-manager/bots/x/claude-settings.json"
exists "scratchpad 不碰" "$ROOT/scratch/note.md"
teardown

setup
mkdir -p "$OB/b" "$ROOT/outside"
echo x > "$ROOT/outside/precious.txt"; old "$ROOT/outside/precious.txt"
ln -s "$ROOT/outside" "$OB/linkdir"
ln -s "$ROOT/outside/precious.txt" "$OB/b/linkfile"
ln "$ROOT/outside/precious.txt" "$OB/b/shared"
run >/dev/null
exists "目錄 symlink 的目標檔不被刪" "$ROOT/outside/precious.txt"
gone   "outbox 的 hardlink 項目只解除自己那個連結" "$OB/b/shared"
exists "外部 hardlink 仍指向原檔" "$ROOT/outside/precious.txt"
teardown

setup
mv "$OB" "$ROOT/real-outbox"
mkdir -p "$ROOT/outside/botA"
echo x > "$ROOT/outside/botA/precious.txt"; old "$ROOT/outside/botA/precious.txt"
ln -s "$ROOT/outside" "$OB"
equals "symlinked outbox root is refused" "$(run)" "1"
check "symlinked outbox root refusal is logged" "OUTBOX 路徑含 symlink" "$LOG"
exists "symlinked outbox root target is untouched" "$ROOT/outside/botA/precious.txt"
teardown

setup
REAL_CONFIG="$ROOT/real-config"
mv "$HOME/.config/agents-manager" "$REAL_CONFIG"
mkdir -p "$REAL_CONFIG/outbox/botA"
echo x > "$REAL_CONFIG/outbox/botA/precious.txt"; old "$REAL_CONFIG/outbox/botA/precious.txt"
ln -s "$REAL_CONFIG" "$HOME/.config/agents-manager"
equals "symlinked outbox parent is refused" "$(run)" "1"
check "symlinked outbox parent refusal is logged" "OUTBOX 路徑含 symlink" "$LOG"
exists "symlinked outbox parent target is untouched" "$REAL_CONFIG/outbox/botA/precious.txt"
teardown

setup
mkdir -p "$ROOT/elsewhere/b"; echo x > "$ROOT/elsewhere/b/old.txt"; old "$ROOT/elsewhere/b/old.txt"
AM_OUTBOX_ROOT="$ROOT/elsewhere" ; export AM_OUTBOX_ROOT
equals "outbox 外的路徑 exit 1" "$(run)" "1"
exists "外面的檔沒被刪" "$ROOT/elsewhere/b/old.txt"
check  "拒絕有記 log" "拒絕：OUTBOX 不在預期路徑" "$LOG"
AM_OUTBOX_ROOT="$HOME/.config/agents-manager"; export AM_OUTBOX_ROOT
mkdir -p "$HOME/.config/agents-manager/bots/y"; echo x > "$HOME/.config/agents-manager/bots/y/old.txt"; old "$HOME/.config/agents-manager/bots/y/old.txt"
equals "指到上一層（agents-manager 本身）也拒絕" "$(run)" "1"
exists "上一層底下的 bot 檔沒被刪" "$HOME/.config/agents-manager/bots/y/old.txt"
exists "DB 所在目錄的檔沒被刪" "$OB"
unset AM_OUTBOX_ROOT
teardown

setup
rmdir "$OB"
equals "outbox 不存在 exit 0" "$(run)" "0"
gone   "不會替它建目錄" "$OB"
check_no "不寫 log" "拒絕\|清掉" "$LOG"
teardown

setup
EVIL="$HOME/.config/agents-manager/outbox-evil"
mkdir -p "$EVIL/botA"; echo x > "$EVIL/botA/old.txt"; old "$EVIL/botA/old.txt"
equals "同前綴的假路徑（outbox-evil）exit 1" "$(AM_OUTBOX_ROOT="$EVIL" run)" "1"
exists "假路徑裡的過期檔不刪" "$EVIL/botA/old.txt"
check  "拒絕寫進 log" "拒絕：OUTBOX 不在預期路徑" "$LOG"
teardown
setup
mkdir -p "$OB/botA"; echo x > "$OB/botA/old.txt"; old "$OB/botA/old.txt"
equals "尾端帶斜線的正確路徑照跑 exit 0" "$(AM_OUTBOX_ROOT="$OB/" run)" "0"
gone   "尾端帶斜線的正確路徑照刪" "$OB/botA/old.txt"
teardown

setup
mkdir -p "$HOME/.ssh/keys"; echo x > "$HOME/.ssh/keys/old"; old "$HOME/.ssh/keys/old"
equals "帶 .. 的路徑 exit 1" "$(AM_OUTBOX_ROOT="$OB/../../../.ssh" run)" "1"
exists "帶 .. 指到 outbox 之外的過期檔不刪" "$HOME/.ssh/keys/old"
teardown

setup
mkdir -p "$OB/shared" "$OB/plain"
echo x > "$OB/shared/old.txt"; touch -t "$(date -d '3 days ago' +%Y%m%d%H%M 2>/dev/null || date -v-3d +%Y%m%d%H%M)" "$OB/shared/old.txt"; share_bot shared
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "只有 daemon 的 ${SHARE_DB_NAME} 也照跑 exit 0" "$(run)" "0"
exists "分享中的 bot 的過期檔留下（名單讀得到）" "$OB/shared/old.txt"
gone   "一般的過期檔照刪" "$OB/plain/old.txt"
check_no "log 沒有拒絕整輪" "拒絕" "$LOG"
teardown

setup
rm -f "$HOME/.config/agents-manager/${SHARE_DB_NAME}"
sqlite3 "$HOME/.config/agents-manager/db.sqlite3" "CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY);"
mkdir -p "$OB/plain"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "只有別的檔名的 DB：exit 1" "$(run)" "1"
exists "只有別的檔名的 DB：一般過期檔也不刪" "$OB/plain/old.txt"
check  "只有別的檔名的 DB：log 寫明拒絕整輪" "拒絕整輪清理" "$LOG"
teardown

dd_make() {
  local dd=$1
  mkdir -p "$dd/outbox/shared" "$dd/outbox/plain"
  sqlite3 "$dd/${SHARE_DB_NAME}" "CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY, deleted_at TEXT); CREATE TABLE IF NOT EXISTS bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, deleted_at TEXT); CREATE TABLE IF NOT EXISTS shared_bots (bot_id TEXT PRIMARY KEY); INSERT OR IGNORE INTO projects (id) VALUES ('p-shared'); INSERT OR IGNORE INTO bots (id, project_id) VALUES ('shared', 'p-shared'); INSERT OR IGNORE INTO shared_bots (bot_id) VALUES ('shared');"
  echo x > "$dd/outbox/shared/old.txt"; old "$dd/outbox/shared/old.txt"
  echo x > "$dd/outbox/plain/old.txt"; old "$dd/outbox/plain/old.txt"
}
setup
ROOT_DD="$ROOT/data"
dd_make "$ROOT_DD"
mkdir -p "$OB/plain"; echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "AM_DATA_DIR 指到別處：exit 0" "$(AM_DATA_DIR="$ROOT_DD" run)" "0"
exists "AM_DATA_DIR 的分享中 bot 的過期檔留下（名單從那個 DB 讀）" "$ROOT_DD/outbox/shared/old.txt"
gone   "AM_DATA_DIR 的一般過期檔照刪（outbox 也在那個資料目錄）" "$ROOT_DD/outbox/plain/old.txt"
exists "預設位置的 outbox 不碰（不是這次的資料目錄）" "$OB/plain/old.txt"
teardown

setup
ROOT_DD="$ROOT/cfgdata"
dd_make "$ROOT_DD"
printf '[server]\ndata_dir = "%s"\n' "$ROOT_DD" > "$HOME/.config/agents-manager/config.toml"
equals "設定檔 [server] data_dir（絕對路徑）：exit 0" "$(run)" "0"
exists "設定檔指到的資料目錄：分享中的過期檔留下" "$ROOT_DD/outbox/shared/old.txt"
gone   "設定檔指到的資料目錄：一般過期檔照刪" "$ROOT_DD/outbox/plain/old.txt"
teardown

setup
dd_make "$HOME/.config/agents-manager/rel"
printf '[server]\ndata_dir = "rel"\n' > "$HOME/.config/agents-manager/config.toml"
equals "設定檔 data_dir 是相對路徑：依設定檔所在目錄解析" "$(run)" "0"
exists "相對路徑解析到設定檔旁邊：分享中的過期檔留下" "$HOME/.config/agents-manager/rel/outbox/shared/old.txt"
gone   "相對路徑解析到設定檔旁邊：一般過期檔照刪" "$HOME/.config/agents-manager/rel/outbox/plain/old.txt"
teardown

setup
dd_make "$HOME/tilde-data"
printf '[server]\ndata_dir = "~/tilde-data"\n' > "$HOME/.config/agents-manager/config.toml"
equals "設定檔 data_dir 的 ~ 展開成 HOME：exit 0" "$(run)" "0"
exists "~ 展開後的資料目錄：分享中的過期檔留下" "$HOME/tilde-data/outbox/shared/old.txt"
gone   "~ 展開後的資料目錄：一般過期檔照刪" "$HOME/tilde-data/outbox/plain/old.txt"
teardown

setup
dd_make "$ROOT/cfgdata"
dd_make "$ROOT/envdata"
rm -f "$ROOT/envdata/${SHARE_DB_NAME}"
printf '[server]\ndata_dir = "%s"\n' "$ROOT/cfgdata" > "$HOME/.config/agents-manager/config.toml"
equals "設定檔與 AM_DATA_DIR 同時有：設定檔優先（exit 0）" "$(AM_DATA_DIR="$ROOT/envdata" run)" "0"
exists "設定檔優先：分享中的過期檔留下（名單來自設定檔指到的 DB）" "$ROOT/cfgdata/outbox/shared/old.txt"
teardown

setup
dd_make "$HOME/.config/agents-manager"
mkdir -p "$ROOT/empty-data/outbox/plain"; echo x > "$ROOT/empty-data/outbox/plain/old.txt"; old "$ROOT/empty-data/outbox/plain/old.txt"
equals "資料目錄裡沒有 DB、預設位置有：exit 1，不回頭讀預設位置的 DB" "$(AM_DATA_DIR="$ROOT/empty-data" run)" "1"
exists "沒讀到名單：資料目錄的一般過期檔也不刪" "$ROOT/empty-data/outbox/plain/old.txt"
check  "沒讀到名單：log 寫明拒絕整輪" "拒絕整輪清理" "$LOG"
teardown

fi

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
