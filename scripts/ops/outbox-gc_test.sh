#!/bin/bash
# outbox-gc.sh 的隔離測試（issue #318）：HOME 指到暫存目錄，腳本原樣複製進去跑，不碰真的 outbox。
# 測的是破壞性那一條：只刪 outbox 底下（含 bot 子目錄）超過保留期的檔，其他一律不動。
#
#   bash scripts/ops/outbox-gc_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

# 分享名單的資料庫檔名以 daemon 的原始碼為準（#1419）：startup.rs 開帳本的那一行、release_triage::default_db_path 兩處必須同名，
# 腳本的 SHARE_DB 也要跟它一樣。測試不再把檔名抄成字面值（那樣 daemon 改名時 fixture 與腳本會一起綠）。
# `OUTBOX_GC_TEST_REPO` 指到別的 repo 根目錄：刻意改 daemon 的常數、看測試會不會紅時用（不改真的 .rs）。
REPO="${OUTBOX_GC_TEST_REPO:-$HERE/../..}"
STARTUP_RS="$REPO/daemon/src/startup.rs"
TRIAGE_RS="$REPO/crates/am-base/src/release_triage/mod.rs"
SHARE_DB_NAME=$(grep 'open_with(&prepared.dir.join(' "$STARTUP_RS" 2>/dev/null | grep -o 'join("[^"]*\.sqlite3")' | head -1 | sed 's/^join("//; s/")$//')
TRIAGE_DB_NAME=$(grep -A3 'pub fn default_db_path' "$TRIAGE_RS" 2>/dev/null | grep -o 'join("[^"]*\.sqlite3")' | head -1 | sed 's/^join("//; s/")$//')
if [ -n "$SHARE_DB_NAME" ] && [ "$SHARE_DB_NAME" = "$TRIAGE_DB_NAME" ]; then
  echo "ok   - daemon 兩處開帳本的檔名一致（${SHARE_DB_NAME}）"; PASS=$((PASS + 1))
else
  echo "FAIL - daemon 開帳本的檔名對不上：startup.rs='${SHARE_DB_NAME}' release_triage='${TRIAGE_DB_NAME}'"; FAIL=$((FAIL + 1))
fi
if [ -n "$SHARE_DB_NAME" ]; then
  check "outbox-gc.sh 的 SHARE_DB 與 daemon 同名" "^SHARE_DB=.*/${SHARE_DB_NAME}\"" "$HERE/outbox-gc.sh"
fi

setup() {
  ROOT=$(mktemp -d)
  export HOME="$ROOT/home"
  mkdir -p "$ROOT/agm/bin" "$HOME/.config/agents-manager/outbox" "$HOME/.ssh"
  cp "$HERE/outbox-gc.sh" "$ROOT/agm/bin/outbox-gc.sh"
  OB="$HOME/.config/agents-manager/outbox"; LOG="$ROOT/agm/outbox-gc.log"; : > "$LOG"
  unset AM_OUTBOX_ROOT OUTBOX_MAX_AGE_MIN OUTBOX_SHARE_KEEP_DAYS OUTBOX_SHARE_CAP_BYTES OUTBOX_SHARE_CAP_FILES
  # 分享名單的資料庫（#1291）：檔名是 daemon 的（${SHARE_DB_NAME}，見上面從 startup.rs 取得的值），放在資料目錄底下。
  # 不從腳本拿，腳本寫錯檔名測試才抓得到（#1391）。結構只取 gc 會查的欄位。
  sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "CREATE TABLE projects (id TEXT PRIMARY KEY, deleted_at TEXT); CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, deleted_at TEXT); CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY);"
}
teardown() { rm -rf "$ROOT"; }
# 把 bot 登記成正在分享（shared_bots 有列、bot 與專案都沒刪）／把 bot 標成刪掉。
share_bot() { sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "INSERT OR IGNORE INTO projects (id) VALUES ('p-$1'); INSERT OR IGNORE INTO bots (id, project_id) VALUES ('$1', 'p-$1'); INSERT OR IGNORE INTO shared_bots (bot_id) VALUES ('$1');"; }
del_bot() { sqlite3 "$HOME/.config/agents-manager/${SHARE_DB_NAME}" "UPDATE bots SET deleted_at = 'x' WHERE id = '$1';"; }
old() { touch -t 202601010000 "$@"; }     # 遠超過任何保留期（只能往回改 mtime；ctime 一定是「現在」）
# 「搬進來的時間」是 ctime（mv／cp -p 會保留舊 mtime，ctime 卻是搬入那一刻），測試沒辦法把 ctime 往回改，
# 所以改成把腳本的時鐘往後撥：`run_at <分鐘>`＝假裝現在是 <分鐘> 分鐘之後（OUTBOX_GC_NOW 是腳本給測試的時鐘接縫）。
run_at() { OUTBOX_GC_NOW=$(( $(date +%s) + $1 * 60 )) zsh "$ROOT/agm/bin/outbox-gc.sh"; echo $?; }
run() { run_at 120; }                      # 兩小時後：剛寫好的檔（ctime＝現在）也已經過期
# 兩小時後（`run`）的時鐘看起來「剛改過」的檔：mtime 設在假時鐘的一分鐘前。
fresh() { touch -t "$(date -v+119M +%Y%m%d%H%M 2>/dev/null || date -d '119 minutes' +%Y%m%d%H%M)" "$@"; }
run_now() { zsh "$ROOT/agm/bin/outbox-gc.sh"; echo $?; }

# 1. 只刪超過保留期的檔；新檔、根目錄直屬檔、有檔的目錄都留下；空目錄收掉。
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

# 1b. bot 把產出放在子資料夾裡（`$AM_OUTBOX/報價_Q-1/*.pdf`，UI 現在也列得到）：每個檔各自照自己的時間清，
#     同一個子資料夾裡新的留、舊的刪、資料夾只在清空後才收；更深一層也一樣。
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

# 2. 保留期邊界與覆寫：30 分鐘的檔在 60 分鐘期限內留著；OUTBOX_MAX_AGE_MIN=5 就刪。
setup
mkdir -p "$OB/b"; echo x > "$OB/b/f.txt"; touch -t "$(date -v-30M +%Y%m%d%H%M 2>/dev/null || date -d '30 minutes ago' +%Y%m%d%H%M)" "$OB/b/f.txt"
run_now >/dev/null
exists "30 分鐘的檔在預設 60 分鐘內不刪" "$OB/b/f.txt"
check_no "沒刪東西就不寫 log" "清掉" "$LOG"
OUTBOX_MAX_AGE_MIN=5 run_at 30 >/dev/null
gone   "OUTBOX_MAX_AGE_MIN=5 時 30 分鐘的檔被刪" "$OB/b/f.txt"
check  "log 寫的是實際的保留期" "超過 5 分鐘" "$LOG"
teardown

# 2b. 以「搬進 outbox 的時間」（ctime）起算，不是檔案內容的 mtime：`mv`／`cp -p` 進來的舊檔 mtime 還是很久以前，
# 以前下一輪（最慢 10 分鐘）就被清掉，bot 剛交出去的檔使用者來不及拿。mtime 與 ctime 都過了才刪。
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

# 2c. 分享用 bot 的 outbox（daemon 放了 `.am-share-keep`）走分享保留政策（#850）：保留 14 天，不是 1 小時
# （end user 隔天才回來拿是常態，使用者 2026-10-04）；標記檔本身不清。
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

# 2d. 分享用 bot 的總量上限（#850）：超過檔數或位元組，從最舊的 mtime 開始刪，標記檔不算量也不刪。
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

# 2e. 分享保留以 daemon 的分享名單（DB）為準（#1291）：bot 自己在 outbox 建 `.am-share-keep` 不算數；
# 真的在分享的 bot 不用標記也保留；分享停了（bot 或專案刪掉）就回到 1 小時。
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

# 2f. 分享名單讀不到（沒有 DB、DB 壞掉）：整輪什麼都不刪、exit 1、記錄原因（#1291）。寧可多留，不可把分享中的檔清掉。
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

# 3. 不碰不該碰的：outbox 外面的私鑰、DB、scratchpad、別的 bot 資料，即使很舊。
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

# 4. symlink：指到 outbox 外面的連結不被跟進去刪目標。
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

# 4b. outbox 根目錄自己被換成 symlink 時，拒絕工作並明確記錄，不可靜默跳過清理。
setup
mv "$OB" "$ROOT/real-outbox"
mkdir -p "$ROOT/outside/botA"
echo x > "$ROOT/outside/botA/precious.txt"; old "$ROOT/outside/botA/precious.txt"
ln -s "$ROOT/outside" "$OB"
equals "symlinked outbox root is refused" "$(run)" "1"
check "symlinked outbox root refusal is logged" "OUTBOX 路徑含 symlink" "$LOG"
exists "symlinked outbox root target is untouched" "$ROOT/outside/botA/precious.txt"
teardown

# 4c. 任一父目錄 symlink 都會讓 lexical path guard 指向 outbox 外的真實目錄；拒絕後不能刪那裡的檔。
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

# 5. 護欄：AM_OUTBOX_ROOT 不在預期路徑就拒絕、exit 1、什麼都不刪、記 log。
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

# 6. outbox 不存在：靜默 exit 0，不建目錄、不寫 log。
setup
rmdir "$OB"
equals "outbox 不存在 exit 0" "$(run)" "0"
gone   "不會替它建目錄" "$OB"
check_no "不寫 log" "拒絕\|清掉" "$LOG"
teardown

# 護欄是「剛好等於該路徑或在它底下」，不是前綴比對（issue #372）：outbox-evil 這種同前綴的兄弟路徑要被拒，不能刪。
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

# 帶 .. 的路徑前綴上像 outbox，實際指到別處（#373）。
setup
mkdir -p "$HOME/.ssh/keys"; echo x > "$HOME/.ssh/keys/old"; old "$HOME/.ssh/keys/old"
equals "帶 .. 的路徑 exit 1" "$(AM_OUTBOX_ROOT="$OB/../../../.ssh" run)" "1"
exists "帶 .. 指到 outbox 之外的過期檔不刪" "$HOME/.ssh/keys/old"
teardown

# 4. 分享名單的資料庫檔名要跟 daemon 寫的一致（#1391、#1419）。#1291 把腳本的檔名寫成 db.sqlite3，daemon 實際寫的是
#    ${SHARE_DB_NAME}：讀不到名單就整輪拒絕清理，outbox 每 10 分鐘都停。這裡只放 daemon 原始碼裡的檔名（setup 用 ${SHARE_DB_NAME}），
#    分享中的 bot 的過期檔要留、一般過期檔照刪、整輪不能因為名單讀不到而拒絕。
setup
mkdir -p "$OB/shared" "$OB/plain"
echo x > "$OB/shared/old.txt"; touch -t "$(date -d '3 days ago' +%Y%m%d%H%M 2>/dev/null || date -v-3d +%Y%m%d%H%M)" "$OB/shared/old.txt"; share_bot shared
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "只有 daemon 的 ${SHARE_DB_NAME} 也照跑 exit 0" "$(run)" "0"
exists "分享中的 bot 的過期檔留下（名單讀得到）" "$OB/shared/old.txt"
gone   "一般的過期檔照刪" "$OB/plain/old.txt"
check_no "log 沒有拒絕整輪" "拒絕" "$LOG"
teardown

# 5. 資料目錄裡只有別的檔名的 DB（#1391 之前腳本讀的 db.sqlite3）：名單讀不到，整輪拒絕，一般過期檔一個都不刪。
setup
rm -f "$HOME/.config/agents-manager/${SHARE_DB_NAME}"
sqlite3 "$HOME/.config/agents-manager/db.sqlite3" "CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY);"
mkdir -p "$OB/plain"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "只有別的檔名的 DB：exit 1" "$(run)" "1"
exists "只有別的檔名的 DB：一般過期檔也不刪" "$OB/plain/old.txt"
check  "只有別的檔名的 DB：log 寫明拒絕整輪" "拒絕整輪清理" "$LOG"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
