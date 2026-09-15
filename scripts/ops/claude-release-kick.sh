#!/bin/bash
# Claude Code 換版就派 AGM 解析新版有什麼用得上的，結論當通知回給使用者（使用者 2026-09-16）。
# launchd `com.agm.claude-release` 每 30 分鐘跑一次；唯讀，只派工，不 build 不重啟。
#
#   AGM_DIR、CLAUDE_VERSIONS_DIR、AGM_RELEASE_BOT 可覆寫（測試用）。
set -u
DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
VERSIONS="${CLAUDE_VERSIONS_DIR:-$HOME/.local/share/claude/versions}"
AGM="$DIR/bin/agm"
LOG="$DIR/claude-release.log"
STATE="$DIR/claude-release.last"     # 已經解析過的版本
TASK="$DIR/claude-release-task.md"

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || exit 0
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -d "$VERSIONS" ] || { log "找不到版本目錄 ${VERSIONS}，跳過"; exit 0; }

# 兩個執行者同時派會送出兩筆一樣的交辦。殘留的鎖交 AGM 檢查，寧可不派。
LOCK="$DIR/claude-release.lock"
mkdir "$LOCK" 2>/dev/null || { log "已有執行者或殘留鎖，跳過"; exit 0; }
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

# 版本目錄名就是版本號；最新的那個是現在會跑的（claude 自己更新時寫進去）。
NEW=$(ls -t "$VERSIONS" 2>/dev/null | head -1)
[ -n "$NEW" ] || { log "版本目錄是空的，跳過"; exit 0; }
DONE_VER=""
[ -f "$STATE" ] && DONE_VER=$(tr -d '[:space:]' < "$STATE")
if [ "$NEW" = "$DONE_VER" ]; then
  exit 0      # 沒換版：安靜退出，不寫 log（每 30 分鐘一次，不值得留一行）
fi
# 第一次跑（還沒有 state）只記下現在的版本，不為「安裝當下已經在的版本」派一次工。
if [ -z "$DONE_VER" ]; then
  echo "$NEW" > "$STATE"
  log "第一次執行，記下目前版本 ${NEW}，不派工"
  exit 0
fi
OLD=$DONE_VER

# 派給 AGM 自己：使用者要的是「AGM 解析完跳通知」，AGM 的回覆就出現在使用者入口。
BOT="${AGM_RELEASE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("manager_bot_id") or "")' "$DIR/runtime.json" 2>/dev/null)
fi
[ -n "$BOT" ] || { log "找不到要派給誰（AGM_RELEASE_BOT／runtime.json manager_bot_id），跳過"; exit 0; }

BODY=$(mktemp -t agm-claude-release)
trap 'rm -f "$BODY"; rmdir "$LOCK" 2>/dev/null || true' EXIT
{
  cat "$TASK"
  printf '\n\n---\n本次：舊版 %s → 新版 %s\n' "$OLD" "$NEW"
  printf 'OLD=%s/%s\nNEW=%s/%s\n' "$VERSIONS" "$OLD" "$VERSIONS" "$NEW"
} > "$BODY"

if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
     --client-request-id "agm-claude-release-$NEW" >> "$LOG" 2>&1; then
  echo "$NEW" > "$STATE"
  log "Claude Code ${OLD} → ${NEW}：已派 AGM 解析"
else
  log "派工失敗（${OLD} → ${NEW}），下一輪再試"
fi
