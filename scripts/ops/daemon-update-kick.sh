#!/bin/bash
# 例行自動部署（使用者 2026-09-29 簡化）：腳本自己直接做，不經 LLM、不要核准單、不派建置 child。
# 由 systemd timer／launchd com.agm.daemon-update 每 5 分鐘跑一次。
#
#   1. 從 origin/main 沿 first-parent 往回找最新一顆 `ubuntu-ci` commit status＝success 的 sha
#      （ubuntu-ci 只跑最新 HEAD、會跳過中間的 sha，所以往回找有限幾顆）。
#   2. 跟 `daemon-update.built`（上次上線的 short sha）比：沒有會進 binary 的差異就結束
#      （路徑清單同 daemon 的 `agm build-inputs`，SPEC §18.11）。
#   3. 在**專用**的乾淨 checkout（預設 ~/.cache/agents-manager/deploy-checkout，不碰主樹與別人的 worktree）
#      checkout 那顆 sha，`bun run build` 再 `cargo build --release -p agents-managerd`。首次 clone 先放同目錄的暫存位置，
#      成功後才移到正式路徑；每輪使用前都核對它的 origin 仍等於正式 repo 的 origin。
#   4. 換版交給 `scripts/ops/daemon-swap.sh`（備份 binary 與 DB、沒人 working／送達中才換、換前再查一次、
#      重啟驗證、失敗回滾或升過 schema 往前修、寫 `.built`）。窗口由 daemon 的
#      `POST /api/services/daemon-swap/restart-window` 自己開，不需要核准單。
#   5. 「立即部署」（`POST /api/deploy/now` 寫 `daemon-update.now.json`，再叫起這個 job）：讀到請求檔就部署
#      那顆 sha，不等 ubuntu-ci（使用者按下就是裁示）；做完（或那顆 sha 不能部署）才刪檔，
#      等不到安全窗口就留著，下一輪再來。
#   6. 任何失敗照舊推 ops_alert（同 source+reason 由 daemon 節流），並寫 `daemon-update.log`。
#
#   7. （旗標，**預設關**）已安裝的 ops 腳本自動換新：旗標＝環境變數 `AGM_OPS_AUTO_INSTALL=1` 或檔案 `<AGM>/ops-auto-install.enabled`。
#      開著時，換版成功之後、以及「沒有會進 binary 的差異」那一輪（ops 腳本改了不會產生新 binary，只靠換版後那一步會永遠漏掉），
#      用該 sha 的 `scripts/ops/ops-install.sh` 把「已經裝了、跟 repo 不同」的 ops 腳本裝進 AGM bin：備份、原子替換、
#      自檢、壞了還原；只更新不新增，不碰排程 unit。後者要該 sha 的 ubuntu-ci 綠燈才裝。失敗推 ops_alert（`ops_install_failed`）。
#
# 邊界（老實說清楚）：建置與整樹測試不在這裡重跑——推 main 前 `scripts/check.sh changed` 已過、ubuntu-ci 在背景跑
# 整樹；這裡只認 ubuntu-ci 的綠燈。窗口（沒人在忙）是協調，不是 OS 層的鎖：這台機器上任何 shell 仍可直接
# kill daemon。安裝方式與排程 unit 見同目錄 README.md；這支腳本本身不安裝自己。
set -u
set -o pipefail

SELF="$(cd "$(dirname "$0")" && pwd)/${0##*/}"

DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-$HOME/project/agents-manager}"      # 正式 daemon 跑的那份（daemon-swap 換它的 target/release）
DEPLOY="${AGM_DEPLOY_CHECKOUT:-$HOME/.cache/agents-manager/deploy-checkout}"   # 專用乾淨 checkout，只有這支腳本動它
CLONE_STAGE=""
GH_REPO="${AGM_GH_REPO:-Eden-Sun/agents-manager}"
CI_CONTEXT="${AGM_CI_CONTEXT:-ubuntu-ci}"
CI_LOOKBACK="${AGM_CI_LOOKBACK:-30}"                  # 沿 first-parent 往回最多看幾顆
AGM="$DIR/bin/agm"
LOG="$DIR/daemon-update.log"
BUILT="$DIR/daemon-update.built"                      # 上次真的換上正式 binary 的 short sha（daemon-swap 寫）
REJECTED="$DIR/daemon-update.rejected"                # 換上去後被回滾的 sha：自動路徑不再挑它
NOW_FILE="$DIR/daemon-update.now.json"
OWNER="${AM_AGENT_NAME:-daemon-update-kick}"
GIT="${GIT_BIN:-/usr/bin/git}"
GH="${GH_BIN:-gh}"
BUN="${BUN_BIN:-bun}"
CARGO="cargo"                                           # 一律走 PATH 上的 build-slot shim
SWAP="${AGM_SWAP_SCRIPT:-$DEPLOY/scripts/ops/daemon-swap.sh}"   # 從要換上的那顆 checkout 跑（不裝到 AGM 目錄）
NICE="${NICE_BIN:-nice}"

cargo_shim_dir() {
  for _candidate in "$HOME"/.config/agents-manager/bots/*/bin/cargo; do
    [ -f "$_candidate" ] && [ -x "$_candidate" ] || continue
    if head -n 12 "$_candidate" 2>/dev/null | grep -q 'AM_SHIM_MARKER'; then
      printf '%s\n' "${_candidate%/cargo}"
      return 0
    fi
  done
  return 1
}

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

# 卡住了、自己解不開時喊人：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）。
# 只寫 log 的話，正式 daemon 從此不再自動換版而沒有任何人知道。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  if [ -x "$AGM" ]; then
    "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
      log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
  else
    log "agm CLI 不在 ${AGM}，ops-alert 只留在這份 log"
  fi
}

# ── ops 腳本自動換新（旗標，預設關；見檔頭 7）──
OPS_AUTO_FLAG="$DIR/ops-auto-install.enabled"
ops_auto_on() { [ "${AGM_OPS_AUTO_INSTALL:-0}" = 1 ] || [ -f "$OPS_AUTO_FLAG" ]; }
# ops_install_step <sha> <要不要 ubuntu-ci 綠燈 0|1>：永遠回 0（這一步壞了不能影響部署的結果）。
ops_install_step() {
  ops_auto_on || return 0
  _osha="$1"; _ogreen="$2"
  _oscript=$("$GIT" -C "$DEPLOY" show "${_osha}:scripts/ops/ops-install.sh" 2>/dev/null) || { log "ops 自動安裝：${_osha} 沒有 ops-install.sh，跳過"; return 0; }
  _oplan=$(printf '%s\n' "$_oscript" | bash -s -- --repo "$DEPLOY" --ref "$_osha" --dir "$DIR" --dry-run 2>>"$LOG")
  _on=$(printf '%s\n' "$_oplan" | sed -n 's/.*changes=\([0-9][0-9]*\) failed=.*/\1/p' | tail -1)
  case "$_on" in ''|*[!0-9]*) log "ops 自動安裝：看不懂 dry-run 的結果，這輪不裝"; return 0 ;; esac
  # 手改過的安裝檔不覆蓋（ops-install 報 drifted）：不算失敗，但要有人看，不然那支永遠停在舊版而沒人知道（同 source+reason daemon 每小時只收一則）。
  if printf '%s\n' "$_oplan" | grep -q '^drifted '; then
    alert ops_install_drift "已安裝的 ops 腳本被手改過（不是 repo 任何一版），自動換新不會覆蓋它：$(printf '%s\n' "$_oplan" | grep '^drifted ' | cut -d' ' -f2 | head -5 | tr '\n' ' ')。先看差在哪（agm ops-sync --check），要換就手動 ops-install.sh --force（舊檔會備份）"
  fi
  [ "$_on" -gt 0 ] || return 0
  if [ "$_ogreen" = 1 ]; then
    _ostate=$("$GH" api "repos/${GH_REPO}/commits/${_osha}/status" -q ".statuses[]|select(.context==\"${CI_CONTEXT}\")|.state" 2>>"$LOG" | head -1)
    if [ "$_ostate" != success ]; then
      log "ops 自動安裝：有 ${_on} 支待更新，但 $(printf '%s' "$_osha" | cut -c1-8) 的 ${CI_CONTEXT} 是「${_ostate:-沒有狀態}」，等綠燈"
      return 0
    fi
  fi
  _orc=0
  _oout=$(printf '%s\n' "$_oscript" | bash -s -- --repo "$DEPLOY" --ref "$_osha" --dir "$DIR" 2>>"$LOG") || _orc=$?
  printf '%s\n' "$_oout" | sed 's/^/ops-install: /' >> "$LOG"
  if [ "$_orc" -ne 0 ]; then
    alert ops_install_failed "自動換新已安裝的 ops 腳本有失敗（沒換成的舊版原封不動）：$(printf '%s\n' "$_oout" | grep '^failed ' | head -3 | tr '\n' ';')。細節見 ${LOG}，備份在 ${DIR}/ops-install-backups"
  else
    log "ops 自動安裝：已把 ${_on} 支換成 $(printf '%s' "$_osha" | cut -c1-8) 的版本"
  fi
  return 0
}

# A second runner must not build or swap at the same time. OS advisory lock 串行化建立／回收目錄鎖，
# 避免兩個同時醒來的 runner 各自刪掉對方剛重建的 lock。目錄裡仍寫 pid 與時間供 hung 診斷；SIGKILL 後 OS lock 自動釋放。
LOCK="$DIR/daemon-update.lock"
LOCK_GUARD="$DIR/daemon-update.lock.guard"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-7200}     # 執行者還活著但卡了這麼久：喊人（冷建置要十幾分鐘，給寬）
acquire_guard() {
  [ ! -L "$LOCK_GUARD" ] || return 2
  exec 9>>"$LOCK_GUARD" || return 2
  python3 -c '
import errno, fcntl, sys
try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError as e:
    sys.exit(1 if e.errno in (errno.EACCES, errno.EAGAIN) else 2)
' >/dev/null 2>&1
}
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
is_self_runner() { # is_self_runner <pid>：只認真正以這支腳本為 bash/sh 入口的行程
  _cmd=$(ps -o command= -p "$1" 2>/dev/null) || return 1
  RUNNER_COMMAND="$_cmd" RUNNER_SCRIPT="$SELF" python3 -c '
import os, shlex, sys
try:
    argv = shlex.split(os.environ["RUNNER_COMMAND"])
except ValueError:
    sys.exit(1)
if len(argv) < 2 or os.path.basename(argv[0]).lstrip("-") not in ("bash", "sh"):
    sys.exit(1)
sys.exit(0 if os.path.normpath(argv[1]) == os.environ["RUNNER_SCRIPT"] else 1)
' >/dev/null 2>&1
}
# 連續「沒能完成」不能永遠只有 local log：連續 FAIL_ALERT_AFTER 輪（預設 6＝約 30 分鐘）推 ops_alert，
# 完整跑完一輪清零。「還有人在跑、等安全窗口」「沒有新的綠燈 commit」是正常的等，不算失敗。
FAILS="$DIR/daemon-update.fails"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-6}
ROUND_FAIL=""
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
settle() {
  if [ -n "$ROUND_FAIL" ]; then
    _n=$(cat "$FAILS" 2>/dev/null)
    case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
    _n=$((_n + 1)); echo "$_n" > "$FAILS"
    [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "自動部署連續 ${_n} 輪沒能完成：${ROUND_FAIL}"
  else
    rm -f "$FAILS" 2>/dev/null
  fi
  return 0
}
cleanup() {
  settle
  [ -z "$CLONE_STAGE" ] || rm -rf "$CLONE_STAGE" 2>/dev/null || true
  rm -rf "$LOCK" 2>/dev/null || true
}
take_lock() {
  mkdir "$LOCK" 2>/dev/null || return 1
  if ! echo "$$ $(date +%s)" > "$LOCK/owner"; then rm -rf "$LOCK" 2>/dev/null; return 1; fi
  trap cleanup EXIT
  return 0
}
mkdir -p "$DIR" 2>/dev/null
_guard_rc=0
acquire_guard || _guard_rc=$?
if [ "$_guard_rc" -eq 1 ]; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ "$_age" -lt 0 ]; then
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
  elif [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，自動部署停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    fi
  elif [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
    alert runner_hung "自動部署 runner 持有 OS lock 已 ${_age} 秒，但鎖的 owner（${_pid:-未知}）無法驗證；部署停住。請確認後必要時結束行程並移除 ${LOCK}"
  fi
  exit 0
elif [ "$_guard_rc" -ne 0 ]; then
  alert lock_unavailable "無法建立或取得部署鎖（${LOCK_GUARD}），本輪跳過"
  exit 0
fi
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  # 還活著的執行者（pid 在，而且真的是這支腳本）：正常重疊就安靜跳過；卡太久才喊人。
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，自動部署停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    fi
    exit 0
  fi
  # 執行者不在了：等一小段時間再回收，避開「別人剛 mkdir、還沒寫 owner」的那幾毫秒。
  if [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），自動部署停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi
log "== check"

# 立即部署請求（SPEC §18.2）：daemon 的 `POST /api/deploy/now` 寫下要部署的 sha 再叫起這個 job。
NOW=0; NOW_SHA=""
drop_now() { rm -f "$NOW_FILE"; log "收掉立即部署請求（$1）"; }
if [ -f "$NOW_FILE" ]; then
  NOW_SHA=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
c=d.get("sha")
if not isinstance(c,str) or not c.strip() or " " in c.strip(): sys.exit(1)
print(c.strip())
' "$NOW_FILE" 2>/dev/null) || NOW_SHA=""
  if [ -n "$NOW_SHA" ]; then
    NOW=1
    log "立即部署請求：commit ${NOW_SHA}（使用者按下，不等 ubuntu-ci）"
  else
    alert now_request_corrupt "立即部署請求 ${NOW_FILE} 讀不出 sha，已收掉；請使用者重按一次"
    drop_now "檔案壞掉"
  fi
fi

# ── 專用 checkout：來源＝主樹的 origin URL；首次 clone 成功後才公開正式路徑 ──
ORIGIN_URL=$("$GIT" -C "$REPO" remote get-url origin 2>/dev/null) || { note_fail "讀不到 ${REPO} 的 origin URL，建不出專用 checkout"; exit 0; }
if [ ! -d "$DEPLOY/.git" ]; then
  mkdir -p "$(dirname "$DEPLOY")" 2>/dev/null || { note_fail "無法建立專用 checkout 的父目錄 $(dirname "$DEPLOY")"; exit 0; }
  if [ -e "$DEPLOY" ] || [ -L "$DEPLOY" ]; then
    alert deploy_checkout_invalid "專用 checkout 路徑 ${DEPLOY} 已存在但不是 git clone；保留原路徑，不自動清除或覆蓋"
    note_fail "專用 checkout 路徑 ${DEPLOY} 已存在但不是 git clone"
    exit 0
  fi
  CLONE_STAGE=$(mktemp -d "${DEPLOY}.clone.XXXXXX" 2>>"$LOG") || { note_fail "無法建立專用 checkout 的暫存目錄"; exit 0; }
  "$GIT" clone -q --no-checkout "$ORIGIN_URL" "$CLONE_STAGE/repo" >> "$LOG" 2>&1 || { note_fail "clone 專用 checkout ${DEPLOY} 失敗"; exit 0; }
  if [ -e "$DEPLOY" ] || [ -L "$DEPLOY" ]; then
    alert deploy_checkout_invalid "clone 完成時專用 checkout 路徑 ${DEPLOY} 已被建立；保留既有路徑並丟棄暫存 clone"
    note_fail "clone 完成時專用 checkout 路徑已被建立"
    exit 0
  fi
  mv "$CLONE_STAGE/repo" "$DEPLOY" >> "$LOG" 2>&1 || { note_fail "無法將暫存 clone 移到專用 checkout ${DEPLOY}"; exit 0; }
  log "建好專用 checkout ${DEPLOY}"
fi
CHECKOUT_ORIGIN_URL=$("$GIT" -C "$DEPLOY" remote get-url origin 2>/dev/null) || CHECKOUT_ORIGIN_URL=""
if [ -z "$CHECKOUT_ORIGIN_URL" ] || [ "$CHECKOUT_ORIGIN_URL" != "$ORIGIN_URL" ]; then
  alert deploy_checkout_origin_mismatch "專用 checkout ${DEPLOY} 的 origin 與正式 repo 不同；預期「${ORIGIN_URL}」、實際「${CHECKOUT_ORIGIN_URL:-讀取失敗}」，停止 fetch、建置與部署"
  note_fail "專用 checkout origin 不符合正式 repo，停止部署"
  exit 0
fi
"$GIT" -C "$DEPLOY" fetch -q origin main >> "$LOG" 2>&1 || { note_fail "fetch origin/main 失敗，這輪不動"; exit 0; }
HEAD_SHA=$("$GIT" -C "$DEPLOY" rev-parse origin/main) || { note_fail "無法讀取 origin/main，跳過"; exit 0; }

# 會影響 binary 的路徑。問 daemon 拿（它知道自己 include_str! 了什麼）；問不到再用保底清單。
PATHS=""
[ -x "$AGM" ] && PATHS=$("$AGM" --compact build-inputs 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
print(" ".join(d.get("paths") or []))
' 2>/dev/null)
[ -n "$PATHS" ] || PATHS="daemon web Cargo.toml Cargo.lock docs/goals/agm-supervisor-persona.md docs/goals/agm-responder-persona.md scripts/agm.py"
# shellcheck disable=SC2086  # PATHS 是刻意要拆成多個參數的
binary_same() { "$GIT" -C "$DEPLOY" diff --quiet "$1" "$2" -- $PATHS; }   # exit 1＝有差；128＝讀不到（當成有差）

BUILT_SHA=""
[ -f "$BUILT" ] && BUILT_SHA=$(tr -d '[:space:]' < "$BUILT")
if [ -z "$BUILT_SHA" ] || ! "$GIT" -C "$DEPLOY" cat-file -e "${BUILT_SHA}^{commit}" 2>/dev/null; then
  alert built_unknown "讀不到線上是哪一版（${BUILT} 是「${BUILT_SHA}」），不知道從哪裡往上換，自動部署停住。請確認線上 binary 的 short sha 後寫進那個檔"
  exit 0
fi
BUILT_FULL=$("$GIT" -C "$DEPLOY" rev-parse "${BUILT_SHA}^{commit}")

# ── 挑要部署的 sha ──
TARGET=""
if [ "$NOW" = 1 ]; then
  TARGET=$("$GIT" -C "$DEPLOY" rev-parse --verify --quiet "${NOW_SHA}^{commit}") || TARGET=""
  if [ -z "$TARGET" ] || ! "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$TARGET" origin/main 2>/dev/null; then
    alert now_target_invalid "立即部署的 commit ${NOW_SHA} 不在 origin/main 上（或讀不到），這趟不做"
    drop_now "commit 不在 origin/main"; exit 0
  fi
  if ! "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$BUILT_FULL" "$TARGET" 2>/dev/null; then
    alert now_target_older "線上版本 ${BUILT_SHA} 不是立即部署目標 ${NOW_SHA} 的祖先或同一顆，這趟拒絕降版"
    drop_now "target older than live"; exit 0
  fi
  if binary_same "$BUILT_FULL" "$TARGET"; then
    drop_now "${BUILT_SHA} 到 ${NOW_SHA} 只動到不進 binary 的檔，已經是最新"; exit 0
  fi
else
  # 線上那顆到 origin/main 之間沒有會進 binary 的差異就結束，不去問 GitHub。
  if binary_same "$BUILT_FULL" "$HEAD_SHA"; then
    log "${BUILT_SHA} 之後沒有會進 binary 的差異，跳過（origin/main ${HEAD_SHA}）"
    ops_install_step "$HEAD_SHA" 1
    exit 0
  fi
  # 沿 first-parent 往回找最新一顆 ubuntu-ci 綠燈；走到線上那顆（或它的祖先）為止——再往回都是舊的。
  SEEN=0
  for CAND in $("$GIT" -C "$DEPLOY" rev-list --first-parent -n "$CI_LOOKBACK" origin/main); do
    SEEN=$((SEEN + 1))
    [ "$CAND" = "$BUILT_FULL" ] && break
    "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$CAND" "$BUILT_FULL" 2>/dev/null && break
    binary_same "$BUILT_FULL" "$CAND" && break            # 這顆以下都沒有新的 binary 內容
    if [ -f "$REJECTED" ] && grep -qx "$CAND" "$REJECTED"; then
      log "${CAND} 之前換上去被回滾過，不再挑它"; continue
    fi
    GH_RC=0
    GH_OUT=$("$GH" api "repos/${GH_REPO}/commits/${CAND}/status" -q ".statuses[]|select(.context==\"${CI_CONTEXT}\")|.state" 2>>"$LOG") || GH_RC=$?
    STATE=$(printf '%s\n' "$GH_OUT" | head -1)
    if [ "$GH_RC" -ne 0 ]; then
      note_fail "問不到 ${CAND} 的 ${CI_CONTEXT} 狀態（gh rc=${GH_RC}），這輪不動"; exit 0
    fi
    if [ "$STATE" = success ]; then TARGET=$CAND; break; fi
    log "${CAND} 的 ${CI_CONTEXT} 是「${STATE:-沒有狀態}」，往前一顆找"
  done
  if [ -z "$TARGET" ]; then
    log "往回 ${SEEN} 顆沒有比線上 ${BUILT_SHA} 新、又有會進 binary 的差異、${CI_CONTEXT} 綠燈的 commit，等下一輪"
    exit 0
  fi
  log "挑到 ${TARGET}（${CI_CONTEXT} success；origin/main ${HEAD_SHA}，線上 ${BUILT_SHA}）"
fi
SHORT=$(printf '%s' "$TARGET" | cut -c1-8)

# ── 建置（同一顆 sha 已經建好就不重建：等安全窗口那幾輪不要每 5 分鐘編一次）──
BUILD_MARK="$DEPLOY/target/release/.built-for"
if [ "$(cat "$BUILD_MARK" 2>/dev/null)" = "$TARGET" ] && [ -x "$DEPLOY/target/release/agents-managerd" ]; then
  log "${SHORT} 已經建好，直接換版"
else
  CARGO_SHIM_DIR=$(cargo_shim_dir) || {
    alert cargo_shim_missing "排程環境找不到 build-slot cargo shim（~/.config/agents-manager/bots/*/bin/cargo），不退回直接 cargo；請確認 daemon 已寫入 shim"
    note_fail "cargo build-slot shim 不存在，${SHORT} 未建置"
    exit 0
  }
  rm -f "$BUILD_MARK"
  "$GIT" -C "$DEPLOY" checkout -q --force --detach "$TARGET" >> "$LOG" 2>&1 || { note_fail "checkout ${SHORT} 失敗"; exit 0; }
  "$GIT" -C "$DEPLOY" clean -fdq >> "$LOG" 2>&1 || true   # 沒有 -x：target／node_modules（被 ignore）留著，增量建置才快
  log "建置 ${SHORT}：web"
  ( cd "$DEPLOY/web" && "$BUN" install --frozen-lockfile && "$BUN" run build ) >> "$LOG" 2>&1 || { note_fail "web 建置失敗（${SHORT}）"; exit 0; }
  # rust_embed 在編譯時讀 web/dist；只跑 cargo build 會重用舊的 daemon crate，讓新版 UI 沒嵌進 binary。
  # 只清 daemon package，保留其他依賴快取；--locked 防止 Cargo.lock 漂移成與 commit 不同的 binary。
  log "建置 ${SHORT}：daemon（cargo clean 後 cargo build --locked --release）"
  ( cd "$DEPLOY" && PATH="$CARGO_SHIM_DIR:$PATH" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 "$NICE" -n 19 "$CARGO" clean -p agents-managerd --release ) >> "$LOG" 2>&1 \
    || { note_fail "cargo clean 失敗（${SHORT}）"; exit 0; }
  ( cd "$DEPLOY" && PATH="$CARGO_SHIM_DIR:$PATH" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 "$NICE" -n 19 "$CARGO" build --locked --release -p agents-managerd ) >> "$LOG" 2>&1 \
    || { note_fail "cargo build --locked 失敗（${SHORT}）"; exit 0; }
  [ -x "$DEPLOY/target/release/agents-managerd" ] || { note_fail "cargo build 沒產出 binary（${SHORT}）"; exit 0; }
  echo "$TARGET" > "$BUILD_MARK"
  log "建好 ${SHORT}"
fi

# ── 換版 ──
LIVE_BIN="$REPO/target/release/agents-managerd"
OLDHASH=$(shasum -a 256 "$LIVE_BIN" 2>/dev/null | cut -c1-16)
[ -n "$OLDHASH" ] || { note_fail "讀不到正式 binary ${LIVE_BIN}，不換版"; exit 0; }
[ -f "$SWAP" ] || { note_fail "找不到換版腳本 ${SWAP}"; exit 0; }
log "換版 ${BUILT_SHA} → ${SHORT}"
SWAP_RC=0
AGM_DIR="$DIR" AGM_REPO="$REPO" bash "$SWAP" --sha "$TARGET" --old "$BUILT_SHA" --old-hash "$OLDHASH" --owner "$OWNER" --checkout "$DEPLOY" >> "$LOG" 2>&1 || SWAP_RC=$?
case "$SWAP_RC" in
  0)
    log "已換上 ${SHORT}"
    [ "$NOW" = 1 ] && drop_now "已部署 ${SHORT}"
    ops_install_step "$TARGET" 0
    ;;
  4)
    # 有人在忙／窗口拿不到：正常的等。立即部署的請求留著，下一輪再試。
    log "還有人在忙（或複查不安全），這輪沒換版（daemon-swap rc=4）；下一輪再試"
    ;;
  6)
    # 新版起來了、只是升過 schema 所以往前修；daemon-swap 沒寫 .built，這裡補上，否則下一輪會對同一顆重做一遍。
    echo "$SHORT" > "$BUILT"
    alert swap_forward_fixed "換上 ${SHORT} 後驗證沒過，但升過 schema 不能放回舊 binary，已往前修：新 binary 服務中。請看 ${DIR}/daemon-swap.log 確認"
    [ "$NOW" = 1 ] && drop_now "往前修"
    ;;
  7)
    echo "$TARGET" >> "$REJECTED"
    alert swap_rolled_back "換上 ${SHORT} 失敗、已回滾到 ${BUILT_SHA}；之後不再自動挑這顆。請看 ${DIR}/daemon-swap.log"
    [ "$NOW" = 1 ] && drop_now "已回滾"
    ;;
  8)
    alert swap_lease_not_released "換上 ${SHORT} 成功，但 restart 窗口沒交還成功，下一次換版可能拿不到窗口。請看 ${DIR}/daemon-swap.log"
    [ "$NOW" = 1 ] && drop_now "已部署 ${SHORT}（窗口沒交還）"
    ;;
  10)
    # 換版之前就擋下、什麼都沒動：這次建出來的 binary 不是 ${SHORT} 這顆 commit 的（沒重建？髒樹？），不是 commit 本身有問題，所以不記進 rejected。
    # 失敗的 binary 仍有 .built-for；清掉標記讓同一 sha 下一輪從乾淨的 daemon crate 重建，而不是永遠重試同一個壞產物。
    rm -f "$BUILD_MARK"
    alert swap_binary_sha_mismatch "要換上 ${SHORT} 的 binary 內嵌的 sha 不是它（或髒樹建的、或舊 binary 沒內嵌 sha），換版中止、窗口沒拿、線上沒動。請看 ${DIR}/daemon-swap.log"
    ROUND_FAIL="${ROUND_FAIL:-新 binary 的內嵌 sha 對不上 ${SHORT}}"
    ;;
  9)
    alert swap_daemon_too_old "線上 daemon 太舊，沒有 restart-window 路由，自動換版做不了。要先手動換過一次含這條路由的 binary（scripts/ops/README.md）"
    ROUND_FAIL="${ROUND_FAIL:-線上 daemon 太舊，沒有 restart-window 路由}"
    ;;
  *)
    note_fail "daemon-swap 失敗 rc=${SWAP_RC}（${SHORT}），細節見 ${DIR}/daemon-swap.log"
    ;;
esac
exit 0
