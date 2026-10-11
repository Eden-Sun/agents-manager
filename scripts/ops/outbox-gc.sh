#!/bin/zsh
# 清掉 outbox（給使用者下載的輸出目錄）裡超過 MAX_AGE_MIN 分鐘的檔案；空的 bot 子目錄一起收。
# 使用者 2026-09-16 裁示：scratchpad 不做輸出用；給使用者的檔案放 outbox，時效 1 小時，由 AGM 定期清理。
# 由 launchd com.agm.outbox-gc 每 10 分鐘跑一次，純機械不花 LLM。
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$DIR/outbox-gc.log"
MAX_AGE_MIN=${OUTBOX_MAX_AGE_MIN:-60}
CONFIG_FILE="$HOME/.config/agents-manager/config.toml"
# 資料目錄與 daemon 的 data_dir()（daemon/src/startup.rs）同一條規則（沒有 --config，就是 daemon 正式的啟動方式）：
#   [server] data_dir（~ 展開；相對路徑依設定檔所在目錄）> AM_DATA_DIR（非空）> ~/.config/agents-manager
# 分享名單的 DB 與 outbox（`<資料目錄>/outbox/<bot>`，daemon 的 outbox.rs）都在這個目錄底下（#1430）。
config_data_dir() {
  [ -r "$CONFIG_FILE" ] || return 0
  awk '
    /^[[:space:]]*\[/ { sec = ($0 ~ /^[[:space:]]*\[server\][[:space:]]*(#.*)?$/) ? 1 : 0; next }
    sec && /^[[:space:]]*data_dir[[:space:]]*=/ {
      s = $0; sub(/^[^=]*=[[:space:]]*/, "", s)
      if (s ~ /^"/) { sub(/^"/, "", s); sub(/".*$/, "", s) }
      else if (s ~ /^\047/) { sub(/^\047/, "", s); sub(/\047.*$/, "", s) }
      else { sub(/[[:space:]]*#.*$/, "", s); sub(/[[:space:]]+$/, "", s) }
      print s; exit
    }' "$CONFIG_FILE" 2>/dev/null
}
DATA_DIR=$(config_data_dir)
if [ -n "$DATA_DIR" ]; then
  case "$DATA_DIR" in "~") DATA_DIR=$HOME ;; "~/"*) DATA_DIR="$HOME/${DATA_DIR#\~/}" ;; esac
  case "$DATA_DIR" in /*) ;; *) DATA_DIR="$HOME/.config/agents-manager/$DATA_DIR" ;; esac
elif [ -n "${AM_DATA_DIR:-}" ]; then
  DATA_DIR=$AM_DATA_DIR
  case "$DATA_DIR" in /*) ;; *) DATA_DIR="$PWD/$DATA_DIR" ;; esac
else
  DATA_DIR="$HOME/.config/agents-manager"
fi
while [ "$DATA_DIR" != "/" ] && [ "${DATA_DIR%/}" != "$DATA_DIR" ]; do DATA_DIR=${DATA_DIR%/}; done
OUTBOX="${AM_OUTBOX_ROOT:-$DATA_DIR/outbox}"
[ -d "$OUTBOX" ] || exit 0
reject_outbox() { echo "$(date '+%F %T') 拒絕：$1 $OUTBOX" >> "$LOG"; exit 1; }
case "$OUTBOX" in *..*) reject_outbox "OUTBOX 帶 .." ;; esac
while [ "$OUTBOX" != "/" ] && [ "${OUTBOX%/}" != "$OUTBOX" ]; do OUTBOX=${OUTBOX%/}; done
OUTBOX_BASE="$DATA_DIR/outbox"
case "$OUTBOX" in "$OUTBOX_BASE"|"$OUTBOX_BASE/"*) ;; *) reject_outbox "OUTBOX 不在預期路徑" ;; esac
# The lexical guard is not enough: a symlinked parent makes the trusted-looking path resolve
# somewhere else. Check each component, including optional subdirectories in AM_OUTBOX_ROOT.
for part in "$HOME/.config" "$HOME/.config/agents-manager" "$DATA_DIR" "$OUTBOX_BASE"; do
  [ ! -L "$part" ] || reject_outbox "OUTBOX 路徑含 symlink（${part}）"
done
remaining=${OUTBOX#"$OUTBOX_BASE"}
current="$OUTBOX_BASE"
while [ -n "$remaining" ]; do
  remaining=${remaining#/}
  component=${remaining%%/*}
  [ -n "$component" ] || break
  current="$current/$component"
  [ ! -L "$current" ] || reject_outbox "OUTBOX 路徑含 symlink（${current}）"
  case "$remaining" in */*) remaining=${remaining#*/} ;; *) remaining= ;; esac
done
# Anchor subsequent relative find paths to the opened directory. A parent swapped after `cd`
# cannot redirect cleanup through the original absolute pathname.
DATA_REAL=$(cd "$DATA_DIR" 2>/dev/null && pwd -P) || reject_outbox "無法解析資料目錄"
EXPECTED_REAL="$DATA_REAL/outbox${OUTBOX#"$OUTBOX_BASE"}"
cd "$OUTBOX" 2>/dev/null || reject_outbox "無法進入 OUTBOX"
OUTBOX_REAL=$(pwd -P) || reject_outbox "無法解析 OUTBOX"
[ "$OUTBOX_REAL" = "$EXPECTED_REAL" ] || reject_outbox "OUTBOX 實際位置不符預期"
OUTBOX=.
# 保留期從「檔案進到 outbox」起算，不是檔案內容的 mtime：`mv`／`cp -p` 進來的舊檔 mtime 還是很久以前，只看 mtime 會在下一輪就清掉
# bot 剛交出去的檔。ctime 是搬入（或最後一次改 metadata）的時間，mtime 與 ctime 都超過才刪（跟 daemon 列表的 expires_at 同一個規則）。
# `OUTBOX_GC_NOW`（epoch 秒）是測試用的時鐘接縫：測試沒辦法把 ctime 往回改，只能把「現在」往後撥。
case "$MAX_AGE_MIN" in ''|*[!0-9]*) MAX_AGE_MIN=60 ;; esac
NOW=${OUTBOX_GC_NOW:-$(date +%s)}
CUTOFF=$((NOW - MAX_AGE_MIN * 60))
# 分享保留政策（#850，派工者決定）：有 `.am-share-keep` 標記的目錄保留 SHARE_KEEP_DAYS 天、總量不超過 SHARE_CAP_BYTES／SHARE_CAP_FILES。
# 數字與 daemon 的 `outbox::SHARE_KEEP_DAYS`／`SHARE_OUTBOX_MAX_BYTES`／`SHARE_OUTBOX_MAX_FILES` 同一組；環境變數是測試接縫。
SHARE_KEEP_DAYS=${OUTBOX_SHARE_KEEP_DAYS:-14}
SHARE_CAP_BYTES=${OUTBOX_SHARE_CAP_BYTES:-524288000}
SHARE_CAP_FILES=${OUTBOX_SHARE_CAP_FILES:-1000}
case "$SHARE_KEEP_DAYS" in ''|*[!0-9]*) SHARE_KEEP_DAYS=14 ;; esac
case "$SHARE_CAP_BYTES" in ''|*[!0-9]*) SHARE_CAP_BYTES=524288000 ;; esac
case "$SHARE_CAP_FILES" in ''|*[!0-9]*) SHARE_CAP_FILES=1000 ;; esac
SHARE_CUTOFF=$((NOW - SHARE_KEEP_DAYS * 86400))
REF=$(mktemp "${TMPDIR:-/tmp}/outbox-gc-ref.XXXXXX") || exit 1
SHARE_REF=$(mktemp "${TMPDIR:-/tmp}/outbox-gc-share-ref.XXXXXX") || { rm -f "$REF"; exit 1; }
trap 'rm -f "$REF" "$SHARE_REF"' EXIT
touch -t "$(date -r "$CUTOFF" +%Y%m%d%H%M.%S 2>/dev/null || date -d "@$CUTOFF" +%Y%m%d%H%M.%S)" "$REF" || exit 1
touch -t "$(date -r "$SHARE_CUTOFF" +%Y%m%d%H%M.%S 2>/dev/null || date -d "@$SHARE_CUTOFF" +%Y%m%d%H%M.%S)" "$SHARE_REF" || exit 1
# 分享用 bot（SPEC §20）的 outbox 走**分享保留政策**（使用者 2026-10-04 原本裁示整個不清，#850 因為一個外部連結就能把資料碟寫滿而改）：
# 它的 end user 是外部的人（例如長輩），隔天、隔幾天才回來拿 bot 給的檔是常態，所以不是 1 小時，而是保留 SHARE_KEEP_DAYS 天
# （同樣 mtime 與 ctime 都超過才刪），再加每顆總量上限（檔數、位元組，從最舊的 mtime 開始刪）。「是不是分享用 bot」由上面的
# 分享名單決定（#1291），不看 outbox 裡的 `.am-share-keep`：那個檔 bot 也寫得到，只當成舊版 daemon 的相容痕跡，這裡不再據以保留。
# 標記檔本身不刪、不算量（daemon 仍會放，給舊版腳本回退用；一個版本後拿掉）。bot 刪掉或分享停了，就不在名單內，回到 1 小時。
# 逐個 bot 目錄走：`-delete` 隱含 `-depth`，同一條 find 裡 `-prune` 不生效。`*(N/)` 只認真的目錄，不跟符號連結（跟原本的 find 一樣）；
# 上限那一步只認 `(.)` 一般檔案，符號連結不算、不刪。
SHARE_MARK=.am-share-keep
zmodload -F zsh/stat b:zstat 2>/dev/null
# 印出這個分享目錄刪了幾個檔。
share_prune() {
  local d=$1 n=0 f count=0 total=0
  local -a st files
  n=$(find "$d" -type f ! -name "$SHARE_MARK" ! -newer "$SHARE_REF" ! -newercm "$SHARE_REF" -print -delete 2>/dev/null | wc -l | tr -d ' ')
  n=${n:-0}
  files=("$d"/**/*(.DNom))   # 新的排前面（mtime）
  for f in $files; do
    [[ ${f:t} == $SHARE_MARK ]] && continue
    zstat -L -A st +size -- "$f" 2>/dev/null || continue
    count=$((count + 1))
    total=$((total + st[1]))
    if (( count > SHARE_CAP_FILES || total > SHARE_CAP_BYTES )); then
      rm -f -- "$f" && n=$((n + 1))
    fi
  done
  print -r -- "$n"
}
# 分享名單（#1291）：哪些 bot 現在真的在分享，只看 daemon 的資料庫（`shared_bots`，且 bot 與專案都沒刪；與 daemon 的 is_active_share 同一條查詢），
# 不看 outbox 裡的標記檔。標記檔放在 bot 自己寫得到的 outbox 裡，bot 建一個就能拿到分享的保留期與上限；DB 只有 daemon 會寫。
# 名單讀不到（沒有 DB、sqlite3 失敗、被鎖住）就整輪不刪、記錯誤退出：寧可多留，不可把分享中的檔清掉。
# 檔名要跟 daemon 寫的帳本檔一致：`agents-manager.sqlite3`（daemon 的 startup.rs、release_triage::default_db_path）。#1291 誤寫成 db.sqlite3，
# 讀不到名單就整輪拒絕，outbox 停擺（#1391）；測試的 fixture 用同一個真實檔名，寫錯測試會紅。
SHARE_DB="$DATA_DIR/agents-manager.sqlite3"
SHARE_SQL="SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL;"
[ -f "$SHARE_DB" ] || { echo "$(date '+%F %T') 拒絕整輪清理：讀不到分享名單（$SHARE_DB 不存在），什麼都不刪" >> "$LOG"; exit 1; }
SHARE_IDS=$(sqlite3 -cmd '.timeout 5000' -batch -readonly "$SHARE_DB" "$SHARE_SQL" 2>> "$LOG") \
  || { echo "$(date '+%F %T') 拒絕整輪清理：讀分享名單失敗（sqlite3），什麼都不刪" >> "$LOG"; exit 1; }
SHARE_LIST=("${(@f)SHARE_IDS}")
removed=0
removed_share=0
for d in "$OUTBOX"/*(N/); do
  name=${d:t}
  if (( ${SHARE_LIST[(Ie)$name]} )); then
    n=$(share_prune "$d")
    removed_share=$((removed_share + ${n:-0}))
    continue
  fi
  n=$(find "$d" -type f ! -newer "$REF" ! -newercm "$REF" -print -delete 2>/dev/null | wc -l | tr -d ' ')
  removed=$((removed + n))
done
find "$OUTBOX" -mindepth 1 -type d -empty -delete 2>/dev/null
[ "$removed" != "0" ] && echo "$(date '+%F %T') 清掉 $removed 個超過 ${MAX_AGE_MIN} 分鐘的檔案" >> "$LOG"
[ "$removed_share" != "0" ] && echo "$(date '+%F %T') 分享保留政策清掉 $removed_share 個檔案（${SHARE_KEEP_DAYS} 天／${SHARE_CAP_FILES} 檔／${SHARE_CAP_BYTES} 位元組）" >> "$LOG"
exit 0
