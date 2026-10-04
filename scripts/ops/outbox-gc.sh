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
case "$OUTBOX" in *..*) echo "$(date '+%F %T') 拒絕：OUTBOX 帶 .. $OUTBOX" >> "$LOG"; exit 1 ;; "$HOME/.config/agents-manager/outbox"|"$HOME/.config/agents-manager/outbox/"*) ;; *) echo "$(date '+%F %T') 拒絕：OUTBOX 不在預期路徑 $OUTBOX" >> "$LOG"; exit 1 ;; esac
# 保留期從「檔案進到 outbox」起算，不是檔案內容的 mtime：`mv`／`cp -p` 進來的舊檔 mtime 還是很久以前，只看 mtime 會在下一輪就清掉
# bot 剛交出去的檔。ctime 是搬入（或最後一次改 metadata）的時間，mtime 與 ctime 都超過才刪（跟 daemon 列表的 expires_at 同一個規則）。
# `OUTBOX_GC_NOW`（epoch 秒）是測試用的時鐘接縫：測試沒辦法把 ctime 往回改，只能把「現在」往後撥。
case "$MAX_AGE_MIN" in ''|*[!0-9]*) MAX_AGE_MIN=60 ;; esac
NOW=${OUTBOX_GC_NOW:-$(date +%s)}
CUTOFF=$((NOW - MAX_AGE_MIN * 60))
REF=$(mktemp "${TMPDIR:-/tmp}/outbox-gc-ref.XXXXXX") || exit 1
trap 'rm -f "$REF"' EXIT
touch -t "$(date -r "$CUTOFF" +%Y%m%d%H%M.%S 2>/dev/null || date -d "@$CUTOFF" +%Y%m%d%H%M.%S)" "$REF" || exit 1
# 分享用 bot（SPEC §20）的 outbox 整個不清（使用者 2026-10-04）：它的 end user 是外部的人（例如長輩），隔天、隔幾天才回來
# 拿 bot 給的檔是常態，一小時就清等於檔案給了也拿不到。daemon 在那種 bot 的 outbox 放標記檔 `.am-share-keep`（啟動、建立、
# 開機時補），這裡看到就跳過整個目錄；檔案由擁有者自己整理，bot 刪掉時跟著收。逐個 bot 目錄走：`-delete` 隱含 `-depth`，
# 同一條 find 裡 `-prune` 不生效。`*(N/)` 只認真的目錄，不跟符號連結（跟原本的 find 一樣）。
SHARE_MARK=.am-share-keep
removed=0
for d in "$OUTBOX"/*(N/); do
  [ -e "$d/$SHARE_MARK" ] && continue
  n=$(find "$d" -type f ! -newer "$REF" ! -newercm "$REF" -print -delete 2>/dev/null | wc -l | tr -d ' ')
  removed=$((removed + n))
done
find "$OUTBOX" -mindepth 1 -type d -empty -delete 2>/dev/null
[ "$removed" != "0" ] && echo "$(date '+%F %T') 清掉 $removed 個超過 ${MAX_AGE_MIN} 分鐘的檔案" >> "$LOG"
exit 0
