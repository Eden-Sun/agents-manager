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
case "$OUTBOX" in "$HOME/.config/agents-manager/outbox"|"$HOME/.config/agents-manager/outbox/"*) ;; *) echo "$(date '+%F %T') 拒絕：OUTBOX 不在預期路徑 $OUTBOX" >> "$LOG"; exit 1 ;; esac
removed=$(find "$OUTBOX" -mindepth 2 -type f -mmin +"$MAX_AGE_MIN" -print -delete 2>/dev/null | wc -l | tr -d ' ')
find "$OUTBOX" -mindepth 1 -type d -empty -delete 2>/dev/null
[ "$removed" != "0" ] && echo "$(date '+%F %T') 清掉 $removed 個超過 ${MAX_AGE_MIN} 分鐘的檔案" >> "$LOG"
exit 0
