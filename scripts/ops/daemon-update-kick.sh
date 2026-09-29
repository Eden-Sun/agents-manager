#!/bin/bash
# 例行自動部署（使用者 2026-09-29 簡化）：腳本自己直接做，不經 LLM、不要核准單、不派建置 child。
# 由 systemd timer／launchd com.agm.daemon-update 每 5 分鐘跑一次。
#
#   1. 從 origin/main 沿 first-parent 往回找最新一顆 `ubuntu-ci` commit status＝success 的 sha
#      （ubuntu-ci 只跑最新 HEAD、會跳過中間的 sha，所以往回找有限幾顆）。
#   2. 跟 `daemon-update.built`（上次上線的 short sha）比：沒有會進 binary 的差異就結束
#      （路徑清單同 daemon 的 `agm build-inputs`，SPEC §18.11）。
#   3. 在**專用**的乾淨 checkout（預設 ~/.cache/agents-manager/deploy-checkout，不碰主樹與別人的 worktree）
#      checkout 那顆 sha，`bun run build` 再 `cargo build --release -p agents-managerd`。
#   4. 換版交給 `scripts/ops/daemon-swap.sh`（備份 binary 與 DB、沒人 working／送達中才換、換前再查一次、
#      重啟驗證、失敗回滾或升過 schema 往前修、寫 `.built`）。窗口由 daemon 的
#      `POST /api/services/daemon-swap/restart-window` 自己開，不需要核准單。
#   5. 「立即部署」（`POST /api/deploy/now` 寫 `daemon-update.now.json`，再叫起這個 job）：讀到請求檔就部署
#      那顆 sha，不等 ubuntu-ci（使用者按下就是裁示）；做完（或那顆 sha 不能部署）才刪檔，
#      等不到安全窗口就留著，下一輪再來。
#   6. 任何失敗照舊推 ops_alert（同 source+reason 由 daemon 節流），並寫 `daemon-update.log`。
#
# 邊界（老實說清楚）：建置與整樹測試不在這裡重跑——推 main 前 `scripts/check.sh changed` 已過、ubuntu-ci 在背景跑
# 整樹；這裡只認 ubuntu-ci 的綠燈。窗口（沒人在忙）是協調，不是 OS 層的鎖：這台機器上任何 shell 仍可直接
# kill daemon。安裝方式與排程 unit 見同目錄 README.md；這支腳本本身不安裝自己。
set -u
set -o pipefail

DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-$HOME/project/agents-manager}"      # 正式 daemon 跑的那份（daemon-swap 換它的 target/release）
DEPLOY="${AGM_DEPLOY_CHECKOUT:-$HOME/.cache/agents-manager/deploy-checkout}"   # 專用乾淨 checkout，只有這支腳本動它
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
CARGO="${CARGO_BIN:-$HOME/.cargo/bin/cargo}"
SWAP="${AGM_SWAP_SCRIPT:-$DEPLOY/scripts/ops/daemon-swap.sh}"   # 從要換上的那顆 checkout 跑（不裝到 AGM 目錄）
NICE="${NICE_BIN:-nice}"

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

# A second runner must not build or swap at the same time. 鎖裡寫 pid 與時間：被強制關機、SIGKILL 的那一輪
# EXIT trap 沒跑，鎖會留在磁碟上——只記一行「已有執行者」就 exit 0 的話，自動換版會永久、靜默地停住。
LOCK="$DIR/daemon-update.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-7200}     # 執行者還活著但卡了這麼久：喊人（冷建置要十幾分鐘，給寬）
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
cleanup() { settle; rm -rf "$LOCK" 2>/dev/null || true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
mkdir -p "$DIR" 2>/dev/null
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  # 還活著的執行者（pid 在，而且真的是這支腳本）：正常重疊就安靜跳過；卡太久才喊人。
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'daemon-update-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，自動部署停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "更新檢查已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
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

# ── 專用 checkout：不存在就 clone（來源＝主樹的 origin URL），之後每輪只 fetch ──
if [ ! -d "$DEPLOY/.git" ]; then
  ORIGIN_URL=$("$GIT" -C "$REPO" remote get-url origin 2>/dev/null) || { note_fail "讀不到 ${REPO} 的 origin URL，建不出專用 checkout"; exit 0; }
  mkdir -p "$(dirname "$DEPLOY")" 2>/dev/null
  "$GIT" clone -q --no-checkout "$ORIGIN_URL" "$DEPLOY" >> "$LOG" 2>&1 || { note_fail "clone 專用 checkout ${DEPLOY} 失敗"; exit 0; }
  log "建好專用 checkout ${DEPLOY}"
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
  rm -f "$BUILD_MARK"
  "$GIT" -C "$DEPLOY" checkout -q --force --detach "$TARGET" >> "$LOG" 2>&1 || { note_fail "checkout ${SHORT} 失敗"; exit 0; }
  "$GIT" -C "$DEPLOY" clean -fdq >> "$LOG" 2>&1 || true   # 沒有 -x：target／node_modules（被 ignore）留著，增量建置才快
  log "建置 ${SHORT}：web"
  ( cd "$DEPLOY/web" && "$BUN" install --frozen-lockfile && "$BUN" run build ) >> "$LOG" 2>&1 || { note_fail "web 建置失敗（${SHORT}）"; exit 0; }
  log "建置 ${SHORT}：daemon（cargo build --release）"
  ( cd "$DEPLOY" && AM_REAL_CARGO="$CARGO" PATH="$(dirname "$CARGO"):$PATH" "$NICE" -n 10 "$CARGO" build --release -p agents-managerd ) >> "$LOG" 2>&1 \
    || { note_fail "cargo build 失敗（${SHORT}）"; exit 0; }
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
  9)
    alert swap_daemon_too_old "線上 daemon 太舊，沒有 restart-window 路由，自動換版做不了。要先手動換過一次含這條路由的 binary（scripts/ops/README.md）"
    ROUND_FAIL="${ROUND_FAIL:-線上 daemon 太舊，沒有 restart-window 路由}"
    ;;
  *)
    note_fail "daemon-swap 失敗 rc=${SWAP_RC}（${SHORT}），細節見 ${DIR}/daemon-swap.log"
    ;;
esac
exit 0
