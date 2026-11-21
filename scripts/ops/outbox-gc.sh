#!/bin/zsh
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$DIR/outbox-gc.log"
MAX_AGE_MIN=${OUTBOX_MAX_AGE_MIN:-60}
CONFIG_FILE="$HOME/.config/agents-manager/config.toml"
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
DATA_REAL=$(cd "$DATA_DIR" 2>/dev/null && pwd -P) || reject_outbox "無法解析資料目錄"
EXPECTED_REAL="$DATA_REAL/outbox${OUTBOX#"$OUTBOX_BASE"}"
cd "$OUTBOX" 2>/dev/null || reject_outbox "無法進入 OUTBOX"
OUTBOX_REAL=$(pwd -P) || reject_outbox "無法解析 OUTBOX"
[ "$OUTBOX_REAL" = "$EXPECTED_REAL" ] || reject_outbox "OUTBOX 實際位置不符預期"
OUTBOX=.
case "$MAX_AGE_MIN" in ''|*[!0-9]*) MAX_AGE_MIN=60 ;; esac
NOW=${OUTBOX_GC_NOW:-$(date +%s)}
CUTOFF=$((NOW - MAX_AGE_MIN * 60))
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
SHARE_MARK=.am-share-keep
zmodload -F zsh/stat b:zstat 2>/dev/null
share_prune() {
  local d=$1 n=0 f count=0 total=0
  local -a st files
  n=$(find "$d" -type f ! -name "$SHARE_MARK" ! -newer "$SHARE_REF" ! -newercm "$SHARE_REF" -print -delete 2>/dev/null | wc -l | tr -d ' ')
  n=${n:-0}
  files=("$d"/**/*(.DNom))
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
