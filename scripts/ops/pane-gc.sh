#!/bin/zsh
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$DIR/browser-gc.log"
MAX_AGE=${PANE_GC_MAX_AGE:-86400}
case "$MAX_AGE" in
  *[!0-9]*) echo "pane-gc: PANE_GC_MAX_AGE 不是正整數，跳過 pane 回收" >> "$LOG"; exit 0 ;;
esac
while [ "${MAX_AGE#0}" != "$MAX_AGE" ]; do MAX_AGE=${MAX_AGE#0}; done
[ -n "$MAX_AGE" ] || MAX_AGE=0
if [ "$MAX_AGE" = 0 ]; then
  echo "pane-gc: PANE_GC_MAX_AGE 不是正整數，跳過 pane 回收" >> "$LOG"
  exit 0
fi
if [ "${#MAX_AGE}" -gt 9 ]; then
  echo "pane-gc: PANE_GC_MAX_AGE 超出範圍，跳過 pane 回收" >> "$LOG"
  exit 0
fi
export PATH="/opt/homebrew/bin:/Users/me/.local/bin:$PATH"
command -v herdr >/dev/null || { echo "pane-gc: herdr 不在 PATH" >> "$LOG"; exit 0; }

etime_secs() {
  ps -o etime= -p "$1" 2>/dev/null | tr -d ' ' | awk -F'[-:]' '{
    if (NF==4) print (($1*24+$2)*60+$3)*60+$4;
    else if (NF==3) print (($1*60)+$2)*60+$3;
    else if (NF==2) print $1*60+$2; }'
}

errf=$(mktemp "${TMPDIR:-/tmp}/pane-gc.XXXXXX" 2>/dev/null) || errf=""
if [ -n "$errf" ]; then errf_owned=1; else errf=/dev/null; errf_owned=0; fi
cleanup_errf() { if [ "$errf_owned" = 1 ]; then rm -f "$errf"; fi; }
list_out=$(herdr pane list 2>"$errf")
parsed=$(printf '%s\n' "$list_out" | python3 -c '
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    print("ERR 回的不是 JSON"); sys.exit(0)
try:
    ids=[p["pane_id"] for p in d["result"]["panes"]]
except Exception:
    e=d.get("error") if isinstance(d,dict) else None
    print("ERR", (e.get("code") if isinstance(e,dict) else None) or "沒有 result.panes"); sys.exit(0)
print("OK")
for i in ids: print(i)' 2>/dev/null)
case "$parsed" in
  OK|"OK
"*) pane_ids=${parsed#OK} ;;
  "ERR "*)
    echo "pane-gc: herdr pane list ${parsed#ERR }（排程環境要帶 HERDR_SESSION；HERDR_SESSION=${HERDR_SESSION:-未設定}）：$(printf '%s %s' "$list_out" "$(head -c 120 "$errf" 2>/dev/null)" | head -c 120)" >> "$LOG"
    cleanup_errf; exit 0 ;;
  *)
    echo "pane-gc: 解析 herdr pane list 失敗（python3 沒有輸出，不是 0 個 pane）" >> "$LOG"
    cleanup_errf; exit 0 ;;
esac
cleanup_errf
closed=0; ghost=0
for pid_pane in $(printf '%s\n' "$pane_ids"); do
  info=$(herdr pane process-info --pane "$pid_pane" 2>/dev/null)
  if [ -z "$info" ] || ! echo "$info" | grep -q '"foreground_processes"'; then
    if herdr pane get "$pid_pane" 2>&1 | grep -q pane_not_found; then
      echo "  ghost pane ${pid_pane}（list 有、get 找不到）" >> "$LOG"; ghost=$((ghost+1))
    fi
    continue
  fi
  read -r fpid cmd <<<"$(echo "$info" | python3 -c '
import json,sys
try:
    fp=json.load(sys.stdin)["result"]["process_info"]["foreground_processes"]
    print(fp[0]["pid"], fp[0]["cmdline"][:80]) if fp else print("", "")
except Exception:
    print("", "")' 2>/dev/null)"
  [ -z "$fpid" ] && continue
  case "$cmd" in
    "claude auth login"*|*"gcloud.py auth login"*|"gcloud auth login"*|"codex login"*) ;;
    *) continue ;;
  esac
  secs=$(etime_secs "$fpid")
  if [ -n "$secs" ] && [ "$secs" -ge "$MAX_AGE" ]; then
    herdr pane close "$pid_pane" >/dev/null 2>&1 \
      && { echo "  close pane ${pid_pane}（$cmd 卡了 ${secs}s）" >> "$LOG"; closed=$((closed+1)); }
  fi
done
echo "pane：關掉 ${closed}／幽靈 $ghost" >> "$LOG"
