#!/bin/zsh
# 收掉 herdr 的殭屍 pane。由 browser-gc-kick.sh 每 6 小時呼叫，也可手動跑。
# 規則（純機械，不花 LLM）：
#   1. 前景程式是卡住的互動式登入（claude auth login / gcloud auth login / codex login）
#      且活超過 MAX_AGE 秒（預設 24h）→ 關 pane。
#   2. pane list 有、pane get 卻 pane_not_found 的幽靈紀錄 → 只記錄（herdr 內部殘留，關不掉）。
#   3. 只有 -zsh 沒前景程式的閒置 shell、以及 bot 的 claude/codex 行程 → 不動。
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$DIR/browser-gc.log"
MAX_AGE=${PANE_GC_MAX_AGE:-86400}
# launchd 的 PATH 只有 /usr/bin:/bin；herdr 裝在 homebrew 或 ~/.local/bin。
export PATH="/opt/homebrew/bin:/Users/m4p/.local/bin:$PATH"
command -v herdr >/dev/null || { echo "pane-gc: herdr 不在 PATH" >> "$LOG"; exit 0; }

etime_secs() {
  ps -o etime= -p "$1" 2>/dev/null | tr -d ' ' | awk -F'[-:]' '{
    if (NF==4) print (($1*24+$2)*60+$3)*60+$4;
    else if (NF==3) print (($1*60)+$2)*60+$3;
    else if (NF==2) print $1*60+$2; }'
}

closed=0; ghost=0
for pid_pane in $(herdr pane list 2>/dev/null | python3 -c '
import json,sys
for p in json.load(sys.stdin)["result"]["panes"]: print(p["pane_id"])'); do
  info=$(herdr pane process-info --pane "$pid_pane" 2>/dev/null)
  if [ -z "$info" ] || ! echo "$info" | grep -q '"foreground_processes"'; then
    if herdr pane get "$pid_pane" 2>&1 | grep -q pane_not_found; then
      echo "  ghost pane $pid_pane（list 有、get 找不到）" >> "$LOG"; ghost=$((ghost+1))
    fi
    continue
  fi
  # herdr 偶爾吐出不合法 JSON（cmdline 內含引號），解析失敗就跳過這個 pane。
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
      && { echo "  close pane $pid_pane（$cmd 卡了 ${secs}s）" >> "$LOG"; closed=$((closed+1)); }
  fi
done
echo "pane：關掉 $closed／幽靈 $ghost" >> "$LOG"
