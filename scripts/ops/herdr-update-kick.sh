#!/bin/bash
# herdr 有新版時，整理出對 agents-manager 有沒有用、會不會壞，派給 AGM 排程處理（issue #66）。
# launchd `com.agm.herdr-update` 每天跑一次；唯讀，只派工，**不升級、不重啟 herdr server**。
#
# 版本比較與 CHANGELOG 段落擷取全部交給 `agents-managerd herdr-update-check`
# （`daemon/src/herdr_update.rs`）：這支腳本只負責三件事——問本機 herdr 版本、問 GitHub 最新穩定版、
# 抓 CHANGELOG 全文——然後照它印出的 JSON 決定要不要派工。不在這裡重做版本比較（`457dd14` 的
# `0.9.0` 判成比 `0.10.0` 新那個坑，字串比較一定會再踩一次）。
#
#   AGM_DIR、AGM_REPO、AM_BINARY、HERDR_REPO、HERDR_CHANGELOG_URL、AGM_HERDR_UPDATE_BOT 可覆寫（測試用）。
set -u
DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-$HOME/project/agents-manager}"
HERDR_REPO="${HERDR_REPO:-herdrdev/herdr}"
AGM="$DIR/bin/agm"
BIN="${AM_BINARY:-$REPO/target/release/agents-managerd}"
CHANGELOG_URL="${HERDR_CHANGELOG_URL:-https://raw.githubusercontent.com/${HERDR_REPO}/master/CHANGELOG.md}"
LOG="$DIR/herdr-update.log"
STATE="$DIR/herdr-update.last"       # 已經派過工的版本（should_notify 的去重就是靠比對這個檔）

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || exit 0
[ -x "$BIN" ] || { log "找不到 ${BIN}，跳過（需要先建好 release binary）"; exit 0; }
command -v herdr >/dev/null 2>&1 || { log "找不到 herdr，跳過"; exit 0; }
command -v gh >/dev/null 2>&1 || { log "找不到 gh，跳過"; exit 0; }

# 兩個執行者同時派會送出兩筆一樣的交辦。殘留的鎖交 AGM 檢查，寧可不派（同 claude-release-kick.sh）。
LOCK="$DIR/herdr-update.lock"
mkdir "$LOCK" 2>/dev/null || { log "已有執行者或殘留鎖，跳過"; exit 0; }
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

# 本機版本：`herdr --version` 印 "herdr 0.8.2"。這裡只剝掉固定的程式名前綴（純文字操作，
# 不是版本解析），數字怎麼比、位數不同要不要緊，一律留給 Rust CLI 的 version_string。
INSTALLED_LINE=$(herdr --version 2>/dev/null | head -1)
INSTALLED=$(printf '%s' "$INSTALLED_LINE" | sed -E 's/^[Hh]erdr[[:space:]]*//')
[ -n "$INSTALLED" ] || { log "讀不到本機 herdr 版本（herdr --version 沒輸出）"; exit 0; }

# 最新穩定版：GitHub release，排除 pre-release。Homebrew（brew info --json=v2 herdr）同樣查得到，
# 但這台機器上 `herdr` 實際跑的二進位是 bot 目錄裡的私有拷貝、不是 brew 連結那份（PATH shadow），
# 兩邊本來就可能不同步；release 清單是兩邊最後都會對齊的那個真相來源，改用它就不用管哪邊快哪邊慢。
LATEST_JSON=$(gh release list -R "$HERDR_REPO" --exclude-pre-releases -L 1 --json tagName 2>>"$LOG")
LATEST_TAG=$(printf '%s' "$LATEST_JSON" | python3 -c '
import json,sys
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(1)
if not isinstance(rows, list) or not rows or not isinstance(rows[0], dict):
    sys.exit(1)
tag = rows[0].get("tagName")
if not isinstance(tag, str) or not tag:
    sys.exit(1)
print(tag)
' 2>/dev/null) || LATEST_TAG=""
[ -n "$LATEST_TAG" ] || { log "查不到 ${HERDR_REPO} 的最新穩定版（gh release list）"; exit 0; }

CHANGELOG=$(mktemp -t herdr-changelog)
trap 'rm -f "$CHANGELOG"; rmdir "$LOCK" 2>/dev/null || true' EXIT
if ! curl -fsSL --max-time 20 "$CHANGELOG_URL" -o "$CHANGELOG" || [ ! -s "$CHANGELOG" ]; then
  log "抓不到 CHANGELOG（${CHANGELOG_URL}）"; exit 0
fi

LAST_ARGS=()
if [ -f "$STATE" ]; then
  LAST=$(tr -d '[:space:]' < "$STATE")
  [ -n "$LAST" ] && LAST_ARGS=(--last-notified "$LAST")
fi

# `${LAST_ARGS[@]+"${LAST_ARGS[@]}"}`：launchd 用系統 `/bin/bash`（macOS 還是 3.2）跑這支腳本，
# 舊 bash 的 `set -u` 對**空陣列**的 `"${arr[@]}"` 會直接 unbound variable 死掉（daemon-update-kick.sh
# 的 `${REVIEW[@]+"${REVIEW[@]}"}` 就是為了繞這個坑；這裡第一次跑、`$STATE` 還不存在時 `LAST_ARGS`
# 就是空的，一定會踩到）。
REPORT=$("$BIN" herdr-update-check --installed "$INSTALLED" --latest "$LATEST_TAG" \
  --changelog-file "$CHANGELOG" ${LAST_ARGS[@]+"${LAST_ARGS[@]}"} 2>>"$LOG") || {
  log "版本比較失敗（installed='${INSTALLED}' latest='${LATEST_TAG}'）"; exit 0
}

# 沒有更新、或這一版已經派過：安靜退出，不留 log（每天一次，「沒事」不值得留一行）。
SHOULD=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; print("1" if json.load(sys.stdin).get("should_notify") else "0")' 2>/dev/null) || SHOULD=0
[ "$SHOULD" = "1" ] || exit 0

LATEST_VERSION=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["latest_version"])')
BRIEF=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin)["brief"])')
[ -n "$LATEST_VERSION" ] && [ -n "$BRIEF" ] || { log "報告缺 latest_version 或 brief，跳過（${REPORT}）"; exit 0; }

# 派給誰：**不能是巡檢自己**（daemon 擋「總管對自己下交辦」，同 claude-release-kick.sh 那個坑）。
# 順序：明指的 AGM_HERDR_UPDATE_BOT ＞ runtime.json 的 herdr_update_bot_id（專用 child）＞
# release_bot_id（跟 claude 換版通知同一顆分析型 child 也合理）＞ 協調者（兜底，一定有人看得到）。
BOT="${AGM_HERDR_UPDATE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("herdr_update_bot_id") or d.get("release_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
fi
[ -n "$BOT" ] || { log "找不到要派給誰（AGM_HERDR_UPDATE_BOT／runtime.json 的 herdr_update_bot_id、release_bot_id 或 responder_bot_id），跳過"; exit 0; }

BODY=$(mktemp -t agm-herdr-update)
{
  printf '%s' "$BRIEF"
  printf '\n---\n本機 `herdr --version`：%s ｜ 最新穩定版（gh release list -R %s）：%s\nCHANGELOG：%s\n' \
    "$INSTALLED_LINE" "$HERDR_REPO" "$LATEST_TAG" "$CHANGELOG_URL"
} > "$BODY"

# 旗標是 `--request-id`（不是 --client-request-id）：拼錯 argparse 直接 exit 2
# （2026-09-16 claude-release-kick 出過這個事故，這裡照抄教訓）。
if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
     --request-id "agm-herdr-update-$LATEST_VERSION" >> "$LOG" 2>&1; then
  echo "$LATEST_VERSION" > "$STATE"
  log "herdr ${INSTALLED} → ${LATEST_VERSION}：已派 AGM 解析"
else
  log "派工失敗（${INSTALLED} → ${LATEST_VERSION}），下一輪再試"
fi
rm -f "$BODY"
