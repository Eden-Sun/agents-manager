#!/bin/bash
# herdr 有新版時，整理出對 agents-manager 有沒有用、會不會壞，派給 AGM 排程處理（issue #66）。
# launchd `com.agm.herdr-update` 每天跑一次；唯讀，只派工，**不升級、不重啟 herdr server**。
#
# 版本比較與 CHANGELOG 段落擷取全部交給 `agents-managerd herdr-update-check`
# （`daemon/src/herdr_update.rs`）：這支腳本只負責三件事——問本機 herdr 版本、問 GitHub 最新穩定版、
# 抓 CHANGELOG 全文——然後照它印出的 JSON 決定要不要派工。不在這裡重做版本比較（`457dd14` 的
# `0.9.0` 判成比 `0.10.0` 新那個坑，字串比較一定會再踩一次）。
#
#   AGM_DIR、AGM_REPO、AM_BINARY、HERDR_REPO、HERDR_CHANGELOG_URL、AGM_HERDR_UPDATE_BOT、AGM_EXTRA_PATH、
#   AGM_LOCK_STALE_SECS、AGM_LOCK_HUNG_SECS、AGM_FAIL_ALERT_AFTER 可覆寫（測試用）。
#
# launchd 預設 PATH 不含 Homebrew（#66 留言／#204 C）：開頭自補 PATH；缺 herdr／gh／python3／curl
# 不靜默 exit 0，log＋ops_alert，否則 job「已排程」但永遠不派。
#
# 另外兩個「24 小時內一定收到交辦」的洞（#66 review 留言）：
#   1. 鎖是純 `mkdir`：拿鎖後被 SIGKILL／斷電，目錄永久殘留，之後每天都跳過。改成 pid＋時間的鎖，
#      執行者不在就回收；還活著卻卡太久才喊人（同 release-triage-kick.sh）。
#   2. 每個「這輪沒能完成檢查」的出口都只寫 local log：連續失敗會永遠沒人知道。連續 AGM_FAIL_ALERT_AFTER 輪就推 ops_alert。
set -u
# launchd 用系統 /bin/bash（macOS 內建 3.2）跑這支：只用 3.2 有的語法（沒有 mapfile、關聯陣列、${x,,}）。
# 補在 PATH 前面；順序照使用者登入 shell 的 `which herdr`（~/.local/bin 在 /opt/homebrew/bin 前）。
# AGM_EXTRA_PATH 只給測試蓋掉（空字串＝不補，不會在 PATH 前面留一個空的「目前目錄」）。
_extra="${AGM_EXTRA_PATH-${HOME:-/nonexistent}/.local/bin:/opt/homebrew/bin:/usr/local/bin}"
[ -n "$_extra" ] && PATH="$_extra:$PATH"
export PATH

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
HERDR_REPO="${HERDR_REPO:-herdrdev/herdr}"
AGM="$DIR/bin/agm"
BIN="${AM_BINARY:-$REPO/target/release/agents-managerd}"
CHANGELOG_URL="${HERDR_CHANGELOG_URL:-https://raw.githubusercontent.com/${HERDR_REPO}/master/CHANGELOG.md}"
LOG="$DIR/herdr-update.log"
STATE="$DIR/herdr-update.last"       # 已經派過工的版本（should_notify 的去重就是靠比對這個檔）
FAILS="$DIR/herdr-update.fails"      # 連續幾輪沒能完成檢查（完成一次就清掉）
OWNER="${AM_AGENT_NAME:-herdr-update-kick}"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-2}

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

# 狀態檔一律先寫暫存檔再 rename（同一目錄＝同一檔案系統）：`echo > $STATE` 被中斷（斷電、SIGKILL）會留下空檔，
# 空的狀態檔在這裡等於「第一次執行」，會漏掉一次該派的工。寫不成功就保留舊檔並回非 0。
write_state() { # write_state <內容>
  _t="$STATE.tmp.$$"
  if printf '%s\n' "$1" > "$_t" 2>/dev/null && mv -f "$_t" "$STATE" 2>/dev/null; then return 0; fi
  rm -rf "$_t" 2>/dev/null
  return 1
}

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }

alert() { # alert <reason> <detail>：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}

# 這一輪沒能完成檢查。一次網路抖動不吵人，但連續 FAIL_ALERT_AFTER 輪（每天一輪＝隔天還是不行）就推 ops_alert，
# 不能永遠只有 local log。收尾（trap）照跑。
fail_run() { # fail_run <log 訊息>
  log "$1"
  _n=$(cat "$FAILS" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$((_n + 1))
  echo "$_n" > "$FAILS"
  [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "herdr 新版偵測連續 ${_n} 輪沒能完成：${1}"
  exit 0
}
ran_ok() { rm -f "$FAILS" 2>/dev/null; return 0; }

[ -x "$BIN" ] || fail_run "找不到 ${BIN}，跳過（需要先建好 release binary）"
command -v herdr >/dev/null 2>&1 || { alert missing_dependency "找不到 herdr（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v gh >/dev/null 2>&1 || { alert missing_dependency "找不到 gh（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v curl >/dev/null 2>&1 || { alert missing_dependency "找不到 curl（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }

# 鎖：兩個執行者同時派會送出兩筆一樣的交辦。鎖裡寫 pid 與時間（同 release-triage-kick.sh／daemon-update-kick.sh）：
# SIGKILL／斷電那一輪 EXIT trap 沒跑，鎖會留在磁碟上——執行者不在就回收接手；還活著但卡太久才喊人。
LOCK="$DIR/herdr-update.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時（含舊版腳本留下、沒有 owner 檔的鎖），超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}     # 執行者還活著但卡了這麼久：喊人
lock_age() { # lock_age → 鎖建立到現在幾秒（讀不到就當 0）
  _born=$(python3 -c '
import os,sys
try:
    print(int(os.path.getmtime(sys.argv[1])))
except OSError:
    print(0)
' "$LOCK" 2>/dev/null) || _born=0
  case "$_born" in ''|*[!0-9]*) _born=0 ;; esac
  [ "$_born" = 0 ] && { echo 0; return; }
  echo $(( $(date +%s) - _born ))
}
TMPS=()
cleanup() { rm -rf "$LOCK" 2>/dev/null || true; [ ${#TMPS[@]} -gt 0 ] && rm -f "${TMPS[@]}" 2>/dev/null; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'herdr-update-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，herdr 新版偵測停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
    fi
    exit 0
  fi
  if [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），herdr 新版偵測停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

# 本機版本：`herdr --version` 印 "herdr 0.8.2"。這裡只剝掉固定的程式名前綴（純文字操作，
# 不是版本解析），數字怎麼比、位數不同要不要緊，一律留給 Rust CLI 的 version_string。
INSTALLED_LINE=$(herdr --version 2>/dev/null | head -1)
INSTALLED=$(printf '%s' "$INSTALLED_LINE" | sed -E 's/^[Hh]erdr[[:space:]]*//')
[ -n "$INSTALLED" ] || fail_run "讀不到本機 herdr 版本（herdr --version 沒輸出）"

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
[ -n "$LATEST_TAG" ] || fail_run "查不到 ${HERDR_REPO} 的最新穩定版（gh release list）"

CHANGELOG=$(mktemp -t herdr-changelog); TMPS+=("$CHANGELOG")
if ! curl -fsSL --max-time 20 "$CHANGELOG_URL" -o "$CHANGELOG" || [ ! -s "$CHANGELOG" ]; then
  fail_run "抓不到 CHANGELOG（${CHANGELOG_URL}）"
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
  --changelog-file "$CHANGELOG" ${LAST_ARGS[@]+"${LAST_ARGS[@]}"} 2>>"$LOG") ||
  fail_run "版本比較失敗（installed='${INSTALLED}' latest='${LATEST_TAG}'）"

# 沒有更新、或這一版已經派過：安靜退出，不留 log（每天一次，「沒事」不值得留一行）。
# 報告看不懂（不是 JSON、沒有布林的 should_notify）是這輪沒能完成檢查，不是「沒有新版」（#226）：
# 以前 `|| SHOULD=0` 把它當成沒事，安靜退出還把連續失敗清零，CLI 換了輸出形狀就永遠沒人知道。
SHOULD=$(printf '%s' "$REPORT" | python3 -c '
import json,sys
v = json.load(sys.stdin).get("should_notify")
if not isinstance(v, bool):
    sys.exit(1)
print("1" if v else "0")
' 2>/dev/null) || fail_run "看不懂版本比較的報告（${REPORT}）"
[ "$SHOULD" = "1" ] || { ran_ok; exit 0; }

LATEST_VERSION=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["latest_version"])')
BRIEF=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin)["brief"])')
[ -n "$LATEST_VERSION" ] && [ -n "$BRIEF" ] || fail_run "報告缺 latest_version 或 brief，跳過（${REPORT}）"

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
[ -n "$BOT" ] || fail_run "找不到要派給誰（AGM_HERDR_UPDATE_BOT／runtime.json 的 herdr_update_bot_id、release_bot_id 或 responder_bot_id），跳過"

BODY=$(mktemp -t agm-herdr-update); TMPS+=("$BODY")
{
  printf '%s' "$BRIEF"
  printf '\n---\n本機 `herdr --version`：%s ｜ 最新穩定版（gh release list -R %s）：%s\nCHANGELOG：%s\n' \
    "$INSTALLED_LINE" "$HERDR_REPO" "$LATEST_TAG" "$CHANGELOG_URL"
} > "$BODY"

# 旗標是 `--request-id`（不是 --client-request-id）：拼錯 argparse 直接 exit 2
# （2026-09-16 claude-release-kick 出過這個事故，這裡照抄教訓）。
if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
     --request-id "agm-herdr-update-$LATEST_VERSION" >> "$LOG" 2>&1; then
  write_state "$LATEST_VERSION" || log "寫不了狀態檔 ${STATE}（已派成功；下一輪同 request-id 由 daemon 去重）"
  ran_ok
  log "herdr ${INSTALLED} → ${LATEST_VERSION}：已派 AGM 解析"
else
  fail_run "派工失敗（${INSTALLED} → ${LATEST_VERSION}），下一輪再試"
fi
