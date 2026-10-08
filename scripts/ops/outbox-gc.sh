#!/bin/zsh
# 清掉 outbox（給使用者下載的輸出目錄）裡超過 MAX_AGE_MIN 分鐘的檔案；空的 bot 子目錄一起收。
# 使用者 2026-09-16 裁示：scratchpad 不做輸出用；給使用者的檔案放 outbox，時效 1 小時，由 AGM 定期清理。
# 由 launchd com.agm.outbox-gc 每 10 分鐘跑一次，純機械不花 LLM。
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$DIR/outbox-gc.log"
OUTBOX="${AM_OUTBOX_ROOT:-$HOME/.config/agents-manager/outbox}"
MAX_AGE_MIN=${OUTBOX_MAX_AGE_MIN:-60}
[ -d "$OUTBOX" ] || exit 0
reject_outbox() { echo "$(date '+%F %T') 拒絕：$1 $OUTBOX" >> "$LOG"; exit 1; }
case "$OUTBOX" in *..*) reject_outbox "OUTBOX 帶 .." ;; esac
while [ "$OUTBOX" != "/" ] && [ "${OUTBOX%/}" != "$OUTBOX" ]; do OUTBOX=${OUTBOX%/}; done
OUTBOX_BASE="$HOME/.config/agents-manager/outbox"
case "$OUTBOX" in "$OUTBOX_BASE"|"$OUTBOX_BASE/"*) ;; *) reject_outbox "OUTBOX 不在預期路徑" ;; esac
# The lexical guard is not enough: a symlinked parent makes the trusted-looking path resolve
# somewhere else. Check each component, including optional subdirectories in AM_OUTBOX_ROOT.
for part in "$HOME/.config" "$HOME/.config/agents-manager" "$OUTBOX_BASE"; do
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
HOME_REAL=$(cd "$HOME" 2>/dev/null && pwd -P) || reject_outbox "無法解析 HOME"
EXPECTED_REAL="$HOME_REAL/.config/agents-manager/outbox${OUTBOX#"$OUTBOX_BASE"}"
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
# （同樣 mtime 與 ctime 都超過才刪），再加每顆總量上限（檔數、位元組，從最舊的 mtime 開始刪）。daemon 在那種 bot 的 outbox 放標記檔
# `.am-share-keep`（啟動、建立、開機時補），這裡看到就走這套；標記檔本身不刪、不算量。bot 刪掉時標記拿掉，回到 1 小時。
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
removed=0
removed_share=0
for d in "$OUTBOX"/*(N/); do
  if [ -e "$d/$SHARE_MARK" ]; then
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
