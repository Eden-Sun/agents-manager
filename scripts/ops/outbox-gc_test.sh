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

setup() {
  ROOT=$(mktemp -d)
  export HOME="$ROOT/home"
  mkdir -p "$ROOT/agm/bin" "$HOME/.config/agents-manager/outbox" "$HOME/.ssh"
  cp "$HERE/outbox-gc.sh" "$ROOT/agm/bin/outbox-gc.sh"
  OB="$HOME/.config/agents-manager/outbox"; LOG="$ROOT/agm/outbox-gc.log"; : > "$LOG"
  unset AM_OUTBOX_ROOT OUTBOX_MAX_AGE_MIN
}
teardown() { rm -rf "$ROOT"; }
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

# 2c. 分享用 bot 的 outbox（daemon 放了 `.am-share-keep`）整個不清：end user 隔天才回來拿是常態（使用者 2026-10-04）。
setup
mkdir -p "$OB/share/sub" "$OB/plain"
: > "$OB/share/.am-share-keep"; old "$OB/share/.am-share-keep"
echo x > "$OB/share/report.pdf"; old "$OB/share/report.pdf"
echo x > "$OB/share/sub/deep.pdf"; old "$OB/share/sub/deep.pdf"
echo x > "$OB/plain/old.txt"; old "$OB/plain/old.txt"
equals "正常跑 exit 0" "$(run_at 100000)" "0"
exists "分享用 bot 的檔幾天後還在" "$OB/share/report.pdf"
exists "分享用 bot 子目錄的檔也在" "$OB/share/sub/deep.pdf"
exists "標記檔本身不清" "$OB/share/.am-share-keep"
gone   "一般 bot 照常清" "$OB/plain/old.txt"
check  "log 只算一般 bot 那 1 個" "清掉 1 個超過 60 分鐘" "$LOG"
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

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
