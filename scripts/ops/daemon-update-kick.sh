#!/bin/bash
# 例行更新：正式 daemon 的 release binary 落後 origin/main 時，向 AGM 申請核准、取得 rebuild
# 租約，再把重建重啟任務派給建置 child。由 launchd com.agm.daemon-update 每小時整點跑一次。
#
# 這份是 repo 版本（scripts/ops/daemon-update-kick.sh），跟 2026-09-12 之前那份放在
# ~/.config/agents-manager/supervisor/AGM/bin/ 的差別是三件事：
#
#   1. **未結案判斷吃新的生命週期**。回合跑完只到 awaiting_review，不是 completed；只看
#      completed/failed 會在上一筆還沒人驗收時就疊下一筆（SPEC §18.8）。
#   2. **建置輸入不再只看 daemon/web/Cargo**。persona（docs/goals/agm-supervisor-persona.md）
#      與 CLI（scripts/agm.py）都是 include_str! 進 binary 的，docs-only 的判斷會漏掉它們，
#      所以路徑清單跟 daemon 說的一致（`agm build-inputs`，SPEC §18.11）。
#   3. **空閒判斷改成租約**。原本是一次快照：讀完「沒人在跑」之後到真的替換 binary 之間還有
#      好幾分鐘，期間隨時可能有人開始工作，而兩個申請都可能同時被允許「等空檔執行」。現在是
#      先 `lease safety` 等窗口、再 `lease acquire` 在同一個鎖裡重驗並拿走窗口；拿著 restart
#      租約期間 supervisor assignment 派送暫停；其他 prompt 路徑仍需 AGM 協調。
#
# 邊界（老實說清楚）：租約只約束走 API 與這些腳本的路徑。這台機器上任何一個 shell 還是可以
# 直接 kill daemon 或自己跑 cargo build，那不是這裡能強制的；租約是協調，不是 OS 層的鎖。
#
# 安裝方式與 launchd plist 見同目錄 README.md。這支腳本本身不安裝自己。
set -u
set -o pipefail

DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-$HOME/project/agents-manager}"
BOT="${AGM_BUILD_BOT:-}"            # 建置 child 的 bot id；沒設就跳過（絕不改派給使用者的 bot）
AGM="$DIR/bin/agm"
LOG="$DIR/daemon-update.log"
STATE="$DIR/daemon-update.last"     # 已派工的 origin/main sha
APPROVAL_STATE="$DIR/daemon-update.approval.json"
BUILT="$DIR/daemon-update.built"    # 上次真的建進正式 binary 的 short sha
OWNER="${AM_AGENT_NAME:-daemon-update-kick}"
GIT=/usr/bin/git

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }
[ -n "$BOT" ] || { log "沒設 AGM_BUILD_BOT，跳過（不改派給別的 bot）"; exit 0; }

# 卡住了、自己解不開時喊人：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）。
# 只寫 log 的話，正式 daemon 從此不再自動換版而沒有任何人知道（review 2026-09-16 c1 M1）。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}

# A second runner must not create another approval or hand off the same lease. 鎖裡寫 pid 與時間：
# 被強制關機、斷電、SIGKILL 的那一輪 EXIT trap 沒跑，鎖會留在磁碟上，重開機也還在——以前每 5 分鐘
# 只記一行「已有執行者或殘留鎖」就 exit 0，換版流程永久、靜默地停住（review 2026-09-16 c1 M1）。
LOCK="$DIR/daemon-update.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
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
# 連續「沒能完成」不能永遠只有 local log（換版流程靜默停住＝正式 daemon 從此不再自動更新，且沒人知道）：
# 過了觸發條件、真的往下檢查的那一輪，只要出過「讀不到／申請不了／拿不到窗口／派不出去」就算失敗，
# 連續 FAIL_ALERT_AFTER 輪（預設 3；整點才會過閘＝約 3 小時）推 ops_alert；完整跑完一輪清零。
# 沒過觸發條件的輪次（每 5 分鐘那些）不動計數。「還有人在跑」「等 AGM 裁示」是正常的等，不算失敗。
FAILS="$DIR/daemon-update.fails"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-3}
ROUND_FAIL=""; CHECKED=0
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
settle() {
  if [ -n "$ROUND_FAIL" ]; then
    _n=$(cat "$FAILS" 2>/dev/null)
    case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
    _n=$((_n + 1)); echo "$_n" > "$FAILS"
    [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "例行更新連續 ${_n} 輪沒能完成：${ROUND_FAIL}"
  elif [ "$CHECKED" = 1 ]; then
    rm -f "$FAILS" 2>/dev/null
  fi
  return 0
}
cleanup() { settle; rm -rf "$LOCK" 2>/dev/null || true; }
# 交還 rebuild 窗口。**rc 不吞**（issue #477，i407 審核）：以前兩處都是 `… || true`，
# release 真的失敗時（daemon 不在、fence 過期、token 對不上）窗口會一直握到 TTL，而 log 上一行
# 已經寫了「交還窗口」，下一個人照 log 判斷就會判錯。$1 是為什麼要還，其餘參數是 token 的帶法。
release_rebuild() {
    local why="$1"; shift
    local rc=0
    "$AGM" --compact lease release rebuild --owner "$OWNER" --fence "$LEASE" "$@" >> "$LOG" 2>&1 || rc=$?
    if [ "$rc" -eq 0 ]; then
        log "rebuild 窗口已交還（${why}）"
    else
        log "交還 rebuild 窗口失敗 rc=${rc}（${why}）：窗口仍被握著，要等 TTL 到期或請 AGM 用 --force 接管——不要當成已經還了"
    fi
    return "$rc"
}
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  # 還活著的執行者（pid 在，而且真的是這支腳本）：正常重疊就安靜跳過；卡太久才喊人。
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'daemon-update-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，例行更新停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "更新檢查已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
    fi
    exit 0
  fi
  # 執行者不在了：pid 查不到、或根本沒有 owner 檔（舊格式的鎖）。等一小段時間再回收，避開
  # 「別人剛 mkdir、還沒寫 owner」的那幾毫秒。
  if [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），例行更新停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi
log "== check"

# 立即部署（使用者 2026-09-25，SPEC §18.2）：使用者在 UI 左上角按「立即部署」時，daemon 以使用者的名義核准一筆
# rebuild（requester＝這支腳本的 OWNER、commit＝確認框上那顆），寫下 daemon-update.now.json，再 launchctl kickstart
# 這個 job。這一輪因此略過三道「排程」的閘：觸發條件（整點／門檻／等太久）、「同 commit 已派過」、等 AGM 裁示。
# **安全條件一條都不略過**：建置 child 要在、上一筆更新要結案、沒人 working 才拿得到 rebuild 窗口（等太久的放寬照舊）、
# 派工正文的固定條件（乾淨 HEAD worktree、整樹測試、.bak、驗證失敗回滾）照抄。等不到窗口就留著請求，下一輪（5 分鐘）再試；
# 請求只在派工成功、或那筆核准已經不能用（過期／撤銷／用掉）時才收掉，所以最長活到核准的 6 小時有效期。
NOW_FILE="$DIR/daemon-update.now.json"
NOW=0; NOW_APPROVAL=""; NOW_SHA=""
drop_now() { rm -f "$NOW_FILE"; log "收掉立即部署請求（$1）"; }
if [ -f "$NOW_FILE" ]; then
  NOW_LINE=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
a,c=d.get("approval_id"),d.get("sha")
if not isinstance(a,str) or not a.strip() or not isinstance(c,str) or not c.strip() or " " in a+c: sys.exit(1)
print(a.strip(), c.strip())
' "$NOW_FILE" 2>/dev/null) || NOW_LINE=""
  if [ -n "$NOW_LINE" ]; then
    NOW=1; NOW_APPROVAL=${NOW_LINE%% *}; NOW_SHA=${NOW_LINE#* }
    log "立即部署請求：核准 ${NOW_APPROVAL}，commit ${NOW_SHA}"
  else
    alert now_request_corrupt "立即部署請求 ${NOW_FILE} 讀不出 approval_id／sha，已收掉；請使用者重按一次"
    drop_now "檔案壞掉"
  fi
fi

# 觸發條件有三個：整點的例行檢查、**累積夠多重建申請**（使用者 2026-09-14），或**最早一筆申請已經等太久**
# （使用者 2026-09-15：不能一直卡著等湊滿）。launchd 每 5 分鐘跑一次，所以「整點」＝分鐘 < 5；
# 門檻 `AGM_REBUILD_THRESHOLD`（預設 3；使用者 2026-09-16 從 5 降下來）、等待上限 `AGM_REBUILD_MAX_WAIT_MIN`（預設 30 分鐘）。
# 請求＝上次真的上線（`daemon-update.built` 的 mtime）之後建立、還沒被否決、**還沒過期**的 rebuild 核准申請，
# 同一個 requester 對同一個 commit 只算一筆。**這支腳本自己的申請不算**（review2 2026-09-16）：算進去的話
# 自己先申請、30 分鐘後自己觸發「等太久」，每 5 分鐘跑一輪、main 一動就再申請一筆，協調者每 5 分鐘被叫一次。
# 自己有還在等的申請（pending／approved、沒過期）時另外照常每輪往下跑：在等 AGM 裁示或安全窗口，不必等整點。
# 數不出來＝未知，**不是 0**（#336）：這輪照常往下檢查並記成失敗（連續幾輪喊人），不退回純整點。
THRESHOLD=${AGM_REBUILD_THRESHOLD:-3}
MAX_WAIT_MIN=${AGM_REBUILD_MAX_WAIT_MIN:-30}
MINUTE=$(( 10#${AGM_TEST_MINUTE:-$(date +%M)} ))   # AGM_TEST_MINUTE 只給隔離測試用
REQUESTS=$("$AGM" --compact approval list 2>/dev/null | BUILT_FILE="$BUILT" OWNER="$OWNER" python3 -c '
import json, os, sys
from datetime import datetime, timezone
try:
    rows = json.load(sys.stdin)["approvals"]
except Exception:
    sys.exit(1)
if not isinstance(rows, list):
    sys.exit(1)
since = 0.0
try:
    since = os.path.getmtime(os.environ["BUILT_FILE"])
except OSError:
    pass
def created(row):
    s = str(row.get("created_at") or "").replace("Z", "+00:00")
    try:
        d = datetime.fromisoformat(s)
    except ValueError:
        return None
    if d.tzinfo is None:
        d = d.replace(tzinfo=timezone.utc)
    return d.timestamp()
now = datetime.now(timezone.utc).timestamp()
seen = set()
oldest = None
mine = 0
for r in rows:
    if not isinstance(r, dict) or r.get("purpose") != "rebuild":
        continue
    if r.get("status") not in ("pending", "approved"):
        continue
    exp = created({"created_at": r.get("expires_at")}) if r.get("expires_at") else None
    if exp is not None and exp <= now:
        continue
    if str(r.get("requester") or "") == os.environ["OWNER"]:
        mine = 1
        continue
    at = created(r)
    if at is None or at <= since:
        continue
    seen.add((str(r.get("requester") or ""), str(r.get("target_commit") or "")))
    oldest = at if oldest is None else min(oldest, at)
# 第二欄＝最早那筆等了幾分鐘（沒有申請、或時間在未來就是 0）；第三欄＝自己有沒有還在等的申請。
waited = 0 if oldest is None else max(0, int((now - oldest) // 60))
print(len(seen), waited, mine)
' 2>/dev/null) || REQUESTS=""
MINE=${REQUESTS##* }
WAITED=${REQUESTS#* }
WAITED=${WAITED%% *}
REQUESTS=${REQUESTS%% *}
# 讀不到＝未知，不是 0（#336）：當 0 會讓非整點那一輪落到下面「這輪不檢查」而靜默跳過，門檻／等太久／自己有核准在等
# 三個「不等整點」的觸發全部失效。未知時這輪照常往下檢查（多檢查一次無害），並記成失敗，連續幾輪推 ops_alert。
UNKNOWN=0
case "$REQUESTS" in ''|*[!0-9]*) note_fail "讀不到重建申請數（approval list 壞了或格式不符）：這輪照常往下檢查，不當成沒人申請"; UNKNOWN=1; REQUESTS=0 ;; esac
case "$WAITED" in ''|*[!0-9]*) WAITED=0 ;; esac
case "$MINE" in 1) ;; *) MINE=0 ;; esac
if [ "$NOW" = 1 ]; then
  log "使用者按了立即部署，不等整點"
elif [ "$REQUESTS" -ge "$THRESHOLD" ]; then
  log "重建申請 ${REQUESTS}/${THRESHOLD}，不等整點"
elif [ "$REQUESTS" -gt 0 ] && [ "$WAITED" -ge "$MAX_WAIT_MIN" ]; then
  log "最早一筆重建申請已等 ${WAITED} 分鐘（上限 ${MAX_WAIT_MIN}），不等整點（申請 ${REQUESTS}/${THRESHOLD}）"
elif [ "$MINE" = 1 ]; then
  log "自己的重建核准還在等（裁示或安全窗口），不等整點（別人的申請 ${REQUESTS}/${THRESHOLD}）"
elif [ "$UNKNOWN" = 1 ]; then
  log "申請數未知，不等整點：這輪照常檢查"
elif [ "$MINUTE" -ge 5 ]; then
  log "非整點且重建申請只有 ${REQUESTS}/${THRESHOLD}（最早一筆等了 ${WAITED} 分鐘），這輪不檢查"; exit 0
fi
CHECKED=1   # 過了觸發條件：從這裡起這一輪算「真的檢查」，失敗才會累計、完整跑完才會清零
"$GIT" -C "$REPO" fetch -q origin main 2>>"$LOG" || log "fetch 失敗，用本地 origin/main"
HEAD_SHA=$("$GIT" -C "$REPO" rev-parse origin/main) || { note_fail "無法讀取 origin/main，跳過"; exit 0; }
# 立即模式建的是確認框上那顆（daemon 驗過它在 origin/main 上），不是這一刻的 HEAD。
DIFF_TO=origin/main
if [ "$NOW" = 1 ]; then
  if ! "$GIT" -C "$REPO" merge-base --is-ancestor "$NOW_SHA" origin/main 2>/dev/null; then
    alert now_target_invalid "立即部署的 commit ${NOW_SHA} 不在 origin/main 上（或讀不到），這趟不做"
    drop_now "commit 不在 origin/main"; exit 0
  fi
  DIFF_TO=$NOW_SHA
fi

# 會影響 binary 的路徑。問 daemon 拿（它知道自己 include_str! 了什麼）；問不到再用保底清單。
PATHS=$("$AGM" --compact build-inputs 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
print(" ".join(d.get("paths") or []))
' 2>/dev/null) || PATHS=""
[ -n "$PATHS" ] || PATHS="daemon web Cargo.toml Cargo.lock docs/goals/agm-supervisor-persona.md docs/goals/agm-responder-persona.md scripts/agm.py"
# <commit> 到 origin/main 之間有沒有動到會進 binary 的檔、動到的 commit 有幾個。例行與立即兩條路都用這一組，
# 路徑清單只有上面那份 PATHS——立即路徑以前不判斷、直接寫「只動到不進 binary 的檔」，連三趟都是錯的。
# diff 出錯（exit 128）算有動到：寧可說「留下一趟」，不要宣稱沒差。
# shellcheck disable=SC2086  # PATHS 是刻意要拆成多個參數的
binary_changed_since() { ! "$GIT" -C "$REPO" diff --quiet "$1" origin/main -- $PATHS; }
# shellcheck disable=SC2086
binary_commits_since() { "$GIT" -C "$REPO" rev-list --count "$1..origin/main" -- $PATHS 2>/dev/null || echo '?'; }

BUILT_SHA=""
if [ -f "$BUILT" ]; then
  BUILT_SHA=$(tr -d '[:space:]' < "$BUILT")
  "$GIT" -C "$REPO" cat-file -e "${BUILT_SHA}^{commit}" 2>/dev/null || BUILT_SHA=""
fi
if [ -n "$BUILT_SHA" ]; then
  # shellcheck disable=SC2086  # PATHS 是刻意要拆成多個參數的
  if "$GIT" -C "$REPO" diff --quiet "$BUILT_SHA" "$DIFF_TO" -- $PATHS; then
    if [ "$NOW" = 1 ]; then
      drop_now "${BUILT_SHA} 到 ${NOW_SHA} 只動到不進 binary 的檔，已經是最新"; exit 0
    fi
    log "$BUILT_SHA 之後只動到不進 binary 的檔，跳過（origin/main ${HEAD_SHA}）"
    echo "$HEAD_SHA" > "$STATE"
    exit 0
  fi
fi
# 立即模式不看「已派過」：上一趟派過同一顆但沒上線（回滾、阻塞）時，使用者重按就是要再來一次。
if [ "$NOW" != 1 ] && [ -f "$STATE" ] && [ "$(cat "$STATE")" = "$HEAD_SHA" ]; then
  log "$HEAD_SHA 已經派過，跳過"; exit 0
fi

# 任務說明檔不在就不往下走：以前 `cat 任務檔 > $TMP || true` 吞掉失敗，會在拿到 rebuild 租約之後派出一則只有
# 尾巴、沒有任何做法說明的交辦，建置 child 不知道要幹嘛，窗口卻被占住。要在申請核准、拿租約之前擋下。
[ -f "$DIR/daemon-update-task.md" ] || { note_fail "找不到 ${DIR}/daemon-update-task.md，這輪不派（尚未申請核准或拿租約）"; exit 0; }

# 建置 child 還在嗎（不在就跳過這輪，不改派）
if ! "$AGM" --compact state | BOT="$BOT" python3 -c '
import json,os,sys
sys.exit(0 if any(b.get("id")==os.environ["BOT"] for b in json.load(sys.stdin).get("bots",[])) else 1)
' 2>/dev/null; then
  note_fail "建置 child $BOT 不在，跳過"; exit 0
fi

# 上一筆更新派工還沒**結案**就不要再疊：回合跑完但沒人驗收的也算未結案。
PENDING=$("$AGM" --compact assignments --open 2>/dev/null | python3 -c '
import json,sys
rows = json.load(sys.stdin)["assignments"]
if not isinstance(rows, list): sys.exit(1)
mine = [r for r in rows if str(r.get("client_request_id","")).startswith("agm-daemon-update-")]
print(mine[0]["client_request_id"] if mine else "")
' 2>/dev/null) || { note_fail "無法確認未結案派工，這輪不派"; exit 0; }
if [ -n "$PENDING" ]; then
  log "上一筆更新還沒結案（${PENDING}），跳過"; exit 0
fi

# SPEC §18.2 條件 3：建置前等待其他 bot 空閒，排除建置 child 與 AGM 自己。
# 不把 runtime 路徑插進 Python 原始碼，路徑含空白／引號也能正常讀取。
MANAGER=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
manager=d.get("manager_bot_id")
if not isinstance(manager,str) or not manager.strip(): sys.exit(1)
print(manager.strip())
' "$DIR/runtime.json" 2>/dev/null) || {
  note_fail "無法從 runtime.json 取得 manager_bot_id，這輪不派"; exit 0;
}
# AGM 雙角色（SPEC §18.15）：協調者也是 AGM，一樣排除。沒建立（或舊 CLI 沒有這個子命令）就只有巡檢。
RESPONDER=$("$AGM" --compact responder show 2>/dev/null | python3 -c '
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    sys.exit(0)
b=d.get("bot_id") if isinstance(d,dict) and d.get("configured") else None
if isinstance(b,str) and b.strip(): print(b.strip())
' 2>/dev/null) || RESPONDER=""
# 這支腳本以哪個角色派工：runtime.json 的 role（巡檢目錄＝patrol）。舊部署沒寫 role，CLI 也還不認
# `--review-by`，就不帶，行為照舊。
ROLE=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
r=d.get("role")
if r in ("patrol","responder"): print(r)
' "$DIR/runtime.json" 2>/dev/null) || ROLE=""
EXCL=(--exclude-bot "$BOT" --exclude-bot "$MANAGER")
[ -n "$RESPONDER" ] && [ "$RESPONDER" != "$MANAGER" ] && EXCL+=(--exclude-bot "$RESPONDER")
REVIEW=()
[ -n "$ROLE" ] && REVIEW=(--review-by "$ROLE")

# safety 與 acquire 都傳同一份排除清單，由 daemon 判定；不再自行過濾快照。
# 回應須確認實際排除的 ID。舊 daemon / CLI 尚未支援或格式有誤就跳過，
# 保留正式熱修腳本直到 daemon 升級後再安裝本版。重啟仍另行核准。
# Reuse the same durable approval on later invocations. Creating a fresh pending request on
# every tick makes an asynchronous AGM decision impossible to consume. Store the full commit
# and requester so approval for one tree/owner can never authorize another.
APPROVAL=""
APPR_COMMIT="$HEAD_SHA"   # 這筆核准是針對哪個 commit（acquire 要對得上）
DEFERRED=0                # 1＝照核准的舊 commit 建，HEAD 多出來、會進 binary 的改動留到下一輪
SUPERSEDE=""
# approval_status <id> → pending／approved／denied／…（過期的 pending／approved 讀成 expired）；查不到回非 0。
# 先用 `--id` 查（清單只回最新 100 筆，舊的那筆會被擠出去）；舊的 CLI 不認得就退回整份清單。
approval_status() {
  local _q _s
  for _q in "--id $1" ""; do
    # shellcheck disable=SC2086  # _q 是刻意要拆成兩個參數的（沒有時是空字串）
    _s=$("$AGM" --compact approval list $_q 2>/dev/null | APPROVAL="$1" python3 -c '
import datetime,json,os,sys
rows=json.load(sys.stdin)["approvals"]
if not isinstance(rows,list): sys.exit(1)
a=next((a for a in rows if a["id"]==os.environ["APPROVAL"]),None)
if a is None: sys.exit(1)
status=a["status"]
if a.get("expires_at") and status in ("pending","approved"):
    expiry=datetime.datetime.fromisoformat(a["expires_at"].replace("Z","+00:00"))
    if expiry <= datetime.datetime.now(datetime.timezone.utc): status="expired"
print(status)
' 2>/dev/null) && [ -n "$_s" ] && { echo "$_s"; return 0; }
  done
  return 1
}
if [ "$NOW" = 1 ]; then
  # 核准是 daemon 在使用者按下時開的：只認 rebuild、申請者是自己、commit 對得上、還是 approved 沒過期的那筆。
  # 查不到＝這輪不知道（留著請求下一輪再問）；查得到但不能用＝收掉請求，不要一直拿它去撞 acquire。
  NOW_STATUS=$("$AGM" --compact approval list --id "$NOW_APPROVAL" 2>/dev/null | APPROVAL="$NOW_APPROVAL" OWNER="$OWNER" SHA="$NOW_SHA" python3 -c '
import datetime,json,os,sys
rows=json.load(sys.stdin)["approvals"]
a=next((a for a in rows if a.get("id")==os.environ["APPROVAL"]),None)
if a is None: sys.exit(1)
if a.get("purpose")!="rebuild" or a.get("requester")!=os.environ["OWNER"] or a.get("target_commit")!=os.environ["SHA"]:
    print("mismatch"); sys.exit(0)
status=a.get("status") or ""
if a.get("expires_at") and status=="approved":
    expiry=datetime.datetime.fromisoformat(a["expires_at"].replace("Z","+00:00"))
    if expiry <= datetime.datetime.now(datetime.timezone.utc): status="expired"
print(status)
' 2>/dev/null) || NOW_STATUS=""
  case "$NOW_STATUS" in
    approved) ;;
    "") note_fail "查不到立即部署的核准 ${NOW_APPROVAL}，這輪不派（請求留著，下一輪再問）"; exit 0 ;;
    *) alert now_approval_unusable "立即部署的核准 ${NOW_APPROVAL} 不能用（${NOW_STATUS}），這趟不做；要部署請使用者重按"
       drop_now "核准 ${NOW_STATUS}"; exit 0 ;;
  esac
  APPROVAL=$NOW_APPROVAL
  APPR_COMMIT=$NOW_SHA
  # 使用者按下之後 main 又動到要建的東西：跟例行路徑的 DEFERRED 同一件事——只建按下的那顆，
  # 「已派過」記它（不是 HEAD），下一輪例行路徑才會為 HEAD 另外申請。
  if [ "$NOW_SHA" != "$HEAD_SHA" ] && binary_changed_since "$NOW_SHA"; then
    DEFERRED=1
    log "立即部署 ${NOW_SHA} 之後 origin/main ${HEAD_SHA} 又動到會進 binary 的檔，留下一趟"
  fi
  # 狀態檔改指這一筆：這趟上線後例行路徑看到的是它（consumed）→ 需要時為 HEAD 重新申請，而不是繼續等一筆舊的。
  python3 -c '
import json,os,sys
path,commit,owner,aid=sys.argv[1:]
with open(path+".tmp","w") as f: json.dump(dict(commit=commit,owner=owner,id=aid),f)
os.replace(path+".tmp",path)
' "$APPROVAL_STATE" "$NOW_SHA" "$OWNER" "$APPROVAL" || { note_fail "保存核准 ID 失敗，這輪不派"; exit 0; }
  rm -f "$DIR/daemon-update.undecided"
else
# ── 例行路徑：申請／沿用核准、等 AGM 裁示（縮排刻意不動，對照歷史比較好讀）──
if [ -f "$APPROVAL_STATE" ]; then
  STATE_LINE=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
if d["owner"] == sys.argv[2]:
    if not isinstance(d["id"], str) or not d["id"] or not isinstance(d["commit"], str) or not d["commit"]: sys.exit(1)
    print(d["id"], d["commit"])
' "$APPROVAL_STATE" "$OWNER" 2>/dev/null) || {
    alert state_corrupt "核准狀態檔 ${APPROVAL_STATE} 讀不出 id／commit，例行更新停住。請人工看過內容後修好或刪掉它"
    exit 0;
  }
  if [ -n "$STATE_LINE" ]; then
    OLD_ID=${STATE_LINE%% *}
    OLD_COMMIT=${STATE_LINE#* }
    if [ "$OLD_COMMIT" = "$HEAD_SHA" ]; then
      APPROVAL=$OLD_ID
    elif "$GIT" -C "$REPO" cat-file -e "${OLD_COMMIT}^{commit}" 2>/dev/null \
         && ! binary_changed_since "$OLD_COMMIT"; then
      # main 動了，但只動到不進 binary 的檔：建出來的東西一樣，沿用原本的核准（不為 docs-only 的 commit 再叫醒協調者）。
      APPROVAL=$OLD_ID
      APPR_COMMIT=$OLD_COMMIT
      log "origin/main ${OLD_COMMIT} → ${HEAD_SHA} 只動到不進 binary 的檔，沿用核准 ${OLD_ID}"
    elif [ "$(approval_status "$OLD_ID")" = "approved" ] && [ "$(cat "$STATE" 2>/dev/null)" != "$OLD_COMMIT" ]; then
      # 已經核准、那顆 commit 還沒派過（在等安全窗口）：照核准的那顆建，HEAD 留到下一輪（issue #439）。
      # 以前這裡一律 supersede：main 約每 5 分鐘一個 push、kick 每 5 分鐘一輪，協調者 1 分鐘內核准的那張
      # 下一輪就被取代，核准永遠派不出去（2026-09-24 11:04～11:25 連開四張）。
      APPROVAL=$OLD_ID
      APPR_COMMIT=$OLD_COMMIT
      DEFERRED=1
      log "核准 ${OLD_ID} 已核准、還沒派工：照它的 commit ${OLD_COMMIT} 建，origin/main ${HEAD_SHA} 留到下一輪"
    else
      # 真的換了要建的東西、而舊的還沒核准（pending）：新申請取代舊的，等待時間接過去（SPEC §18.10 supersedes）。
      # pending 照舊換成新 commit：還沒人裁示，讓協調者審的就是現在要建的東西；daemon 會收掉舊的那則還沒送出的
      # approval_requested，不多叫醒一次。不換的話核准後建的是舊的，下一輪又得為 HEAD 再申請一次＝多一次裁示、多一次重建。
      # 舊的已經 denied／expired／consumed／superseded（或這顆 commit 已派過）：一樣開新的，daemon 對不能用的舊申請不接等待。
      SUPERSEDE=$OLD_ID
    fi
  fi
fi
# 舊的 `bin/agm` 不認得 --supersedes（argparse 會整筆拒絕）：先問 CLI 支不支援，不支援就照舊開一筆新的。
SUP_ARGS=()
if [ -n "$SUPERSEDE" ] && "$AGM" approval --help 2>/dev/null | grep -q -- '--supersedes'; then
  SUP_ARGS=(--supersedes "$SUPERSEDE")
fi

# 最多兩輪：舊的那筆已經被用掉／取代時（例如上一個窗口過期沒交還，daemon 當場消耗了它），這一輪就重新申請，
# 不要白等到下一個整點。
for ROUND in 1 2; do
if [ -z "$APPROVAL" ]; then
  # issue #421：有效期 6 小時（21600），不是 90 分鐘。90 分鐘會在協調者沒裁示的那個晚上自己過期，
  # 下個整點只能重新申請，部署就一小時一次地原地打轉（2026-09-23 停了 9 小時）。6 小時足以跨過
  # 「協調者要人去 /login」這種需要人介入的等待；重申請由 daemon 自己 supersede，不會累積 pending。
  APPROVAL=$("$AGM" --compact approval request \
    --requester "$OWNER" --purpose rebuild \
    --scope "release rebuild（daemon/web/persona/agm.py）；restart 另行核准" \
    --commit "$HEAD_SHA" --expires-in 21600 ${SUP_ARGS[@]+"${SUP_ARGS[@]}"} 2>>"$LOG" | python3 -c '
import json,sys
d=json.load(sys.stdin)
if not isinstance(d.get("id"),str) or not d["id"]: sys.exit(1)
print(d["id"])
' 2>/dev/null) || { note_fail "申請核准失敗，這輪不派"; exit 0; }
  APPR_COMMIT="$HEAD_SHA"
  python3 -c '
import json,os,sys
path,commit,owner,aid=sys.argv[1:]
with open(path+".tmp","w") as f: json.dump(dict(commit=commit,owner=owner,id=aid),f)
os.replace(path+".tmp",path)
' "$APPROVAL_STATE" "$HEAD_SHA" "$OWNER" "$APPROVAL" || {
    note_fail "保存核准 ID 失敗，這輪不派"; exit 0;
  }
  if [ ${#SUP_ARGS[@]} -gt 0 ]; then
    log "已申請核准 ${APPROVAL}（commit ${HEAD_SHA}，取代 ${SUPERSEDE}），等 AGM 裁示"
  else
    log "已申請核准 ${APPROVAL}（commit ${HEAD_SHA}），等 AGM 裁示"
  fi
  SUP_ARGS=()
fi

STATUS=$(approval_status "$APPROVAL") || STATUS=""
if [ -z "$STATUS" ]; then
  alert approval_missing "查不到核准 ${APPROVAL}（狀態檔 ${APPROVAL_STATE} 指著它），例行更新停住。請確認那筆核准還在不在，不在就刪掉狀態檔讓它重新申請"
  exit 0
fi
case "$STATUS" in
  expired|consumed|superseded)
    rm -f "$APPROVAL_STATE"
    if [ "$ROUND" = 1 ]; then
      log "核准 ${APPROVAL} 已經不能用（${STATUS}），重新申請"
      APPROVAL=""
      continue
    fi
    log "核准 ${APPROVAL} 剛申請就不能用（${STATUS}），下輪再試"; exit 0 ;;
esac
break
done
# 協調者多久沒裁示（issue #420）：自己的申請從第一次看到 pending 起算，到期重申請（expired → 新的一筆）
# 也接著算，直到看到別的狀態才清掉。超過 AGM_UNDECIDED_ALERT_SECS（預設 5400＝90 分鐘）還沒裁示就推
# ops_alert——2026-09-23 協調者沒登入，申請每 90 分鐘過期、每小時重申請，停了 9 小時，log 只有
# 「核准狀態是 pending」，看不出是協調者掛了。
#
# 這個數字**不再等於 `--expires-in`**（issue #421 把有效期改成 6 小時，理由見下面那段）：它是腳本這一側的
# 備援通道。daemon 那一側更快也更準——開 5 分鐘改派給巡檢、開 30 分鐘開 `approval_stalled` incident
# （SPEC §18.10）。兩條都留著：daemon 那條要 daemon 活著且 inbox 送得出去，這條只要 launchd 還在跑。
UNDECIDED="$DIR/daemon-update.undecided"
UNDECIDED_ALERT_SECS=${AGM_UNDECIDED_ALERT_SECS:-5400}
if [ "$STATUS" = "pending" ]; then
  _now=$(date +%s)
  _since=$(cat "$UNDECIDED" 2>/dev/null)
  case "$_since" in ''|*[!0-9]*) _since=$_now; echo "$_since" > "$UNDECIDED" ;; esac
  _waited=$((_now - _since))
  if [ "$_waited" -ge "$UNDECIDED_ALERT_SECS" ]; then
    alert approval_undecided "協調者 $((_waited / 3600)) 小時 $((_waited % 3600 / 60)) 分沒裁示例行重建的核准（目前是 ${APPROVAL}，到期只會重新申請）。請看協調者在不在、有沒有登入：bin/agm responder show、bin/agm health"
  fi
else
  rm -f "$UNDECIDED"
fi
if [ "$STATUS" != "approved" ]; then
  log "核准狀態是 ${STATUS:-unknown}，這輪不派（下個整點再看）"; exit 0
fi
fi   # 例行路徑的核准段到這裡

# 取得 rebuild 窗口：acquire 會在同一個鎖裡重驗一次 idle 再把窗口拿走。

# 順序重要：**先拿到自己的核准，再問 safety**。「等太久就縮小封鎖面」是綁在**當下這筆核准**
# 等了多久（SPEC §18.10），不帶 --approval 問出來的是「最早那筆還活著的核准」——用別人的時鐘
# 決定自己要不要繼續，升級邏輯在這條路上等於死碼（review 2026-09-16）。
# `safe` 由 daemon 判（SPEC §18.10）：核准等超過門檻時它會自己縮小封鎖面，這裡不能再 AND 一次
# 自己的條件，否則「思考中不擋」永遠生效不了。格式不對仍然一律當不安全。
# `--owner`：跟等一下 acquire 用同一個身分問，自己手上的租約（例如上一輪還沒交還的 rebuild）不算擋
# ——否則這裡判不安全，而 acquire 其實拿得到（SPEC §18.10「自己的租約不擋自己」）。
SAFE_RAW=$("$AGM" --compact lease safety --approval "$APPROVAL" "${EXCL[@]}" --owner "$OWNER" 2>/dev/null | BUILD_BOT="$BOT" MANAGER_BOT="$MANAGER" RESPONDER_BOT="$RESPONDER" python3 -c '
import json,sys,os
d=json.load(sys.stdin)
if not isinstance(d,dict) or not isinstance(d.get("safe"),bool): sys.exit(1)
for key in ("working","in_flight","unreadable"):
    if not isinstance(d.get(key),list) or any(not isinstance(x,dict) for x in d[key]): sys.exit(1)
ex={os.environ["BUILD_BOT"],os.environ["MANAGER_BOT"]} | ({os.environ["RESPONDER_BOT"]} if os.environ.get("RESPONDER_BOT") else set())
applied=d.get("excluded_bot_ids")
if not isinstance(applied,list) or any(not isinstance(x,str) for x in applied) or set(applied) != ex: sys.exit(1)
def rows(key):
    v=d.get(key)
    return v if isinstance(v,list) and all(isinstance(x,dict) for x in v) else []
def names(v): return ",".join(str(r.get("name") or r.get("bot_id") or "?") for r in v)
working,in_flight,unreadable=d["working"],d["in_flight"],d["unreadable"]
delivering=rows("delivering")
# 自己握的租約不算擋，理由也不該把它列出來（daemon 標 own:true）。
held=[l for l in rows("held_leases") if l.get("own") is not True]
if d["safe"]:
    state="yes"
elif delivering: state="送達中:"+names(delivering)
elif held: state="租約:"+",".join(str(l.get("resource") or "?") for l in held)
elif unreadable: state="unreadable"
elif working: state=names(working)
elif in_flight: state="in_flight"
else: state="unknown"
waited=d.get("waited_secs")
print("%s|%d|%d" % (state, 1 if d.get("escalated") is True else 0, waited if isinstance(waited,int) else 0))
' 2>/dev/null) || SAFE_RAW="unknown|0|0"
SAFE=${SAFE_RAW%%|*}
ESC_REST=${SAFE_RAW#*|}
ESCALATED=${ESC_REST%%|*}
WAITED_SECS=${ESC_REST##*|}
case "$WAITED_SECS" in ''|*[!0-9]*) WAITED_SECS=0 ;; esac
case "$SAFE" in unknown|unreadable) note_fail "讀不到／看不懂安全窗口判定（${SAFE}），這輪不派" ;; esac
if [ "$SAFE" != "yes" ]; then
  if [ "$NOW" = 1 ]; then
    log "還有人在跑（${SAFE}），立即部署等安全窗口：請求留著，下一輪再試"
  else
    log "還有人在跑（${SAFE}），這輪不派"
  fi
  exit 0
fi
# 這次不是等到全靜止才換的：log 與派工正文都要寫明（SPEC §18.10）。
ESC_NOTE=""
if [ "$ESCALATED" = "1" ]; then
  ESC_MINS=$((WAITED_SECS / 60))
  ESC_NOTE="這次是升級後才換：核准後已等 ${ESC_MINS} 分鐘，daemon 縮小封鎖面（思考中不擋，只擋送達臨界區／租約／讀不到畫面）。"
  log "安全窗口是升級後才成立的：核准後已等 ${ESC_MINS} 分鐘（縮小封鎖面）"
fi

# acquire 的回應同時帶 fence 與**一次性的 lease_token**：交還窗口要出示它（owner／fence 是公開欄位，
# 光憑它們誰都能把別人正在換 binary 的窗口收掉）。token 只在這一次回應裡出現，之後查不到。
ACQUIRED=$("$AGM" --compact lease acquire rebuild --approval "$APPROVAL" --commit "$APPR_COMMIT" \
  --owner "$OWNER" --ttl 3600 "${EXCL[@]}" 2>>"$LOG" | python3 -c '
import json,sys
d = json.load(sys.stdin)
print("%s|%s" % (d.get("lease", {}).get("fence") or "", d.get("lease_token") or ""))
' 2>/dev/null) || ACQUIRED=""
LEASE=${ACQUIRED%%|*}
LEASE_TOKEN=${ACQUIRED#*|}
if [ -z "$LEASE" ]; then
  note_fail "拿不到 rebuild 窗口（可能有人正在做或核准對不上），這輪不派"; exit 0
fi
# 舊 daemon 還沒有 token（升級前的那一版）：照舊不帶，release 那邊會放行並留 warn。
# token **不進派工正文，也不進 argv**（review2 sup #5、issue #477）：正文會出現在
# `GET /api/supervisor/assignments`、建置 child 的對話紀錄，而 assign 的輸出還會寫進這份 log；
# argv 則是同一個 uid 的行程用 `ps` 就看得到。寫進只有本人讀得到的檔案，自己交還與派工正文
# 都用 `--lease-token-file <路徑>`，token 本身從頭到尾只出現在那個 600 的檔裡。
# 固定路徑＋`rm -f` 再 `>` 是可預測的：同一個 uid 的行程（正是這張票的威脅模型）可以先把那個名字
# 佔住、或擺一條 symlink，`>` 就會沿用既有檔的 owner／mode——umask 077 只在「這個檔是我們建的」時
# 有用。mktemp 是 O_EXCL ＋ 隨機名，佔不住也猜不到（i92b 審核）。
# 上一輪留下的先清掉（含舊版的固定檔名）：能拿到新的 rebuild 窗口就代表舊窗口已經不在，那些 token 早就沒用。
rm -f "$DIR/daemon-update.lease-token" "$DIR"/daemon-update.lease-token.*
TOKEN_FILE=""
TOKEN_ARG=""
TOKEN_TEXT=""
if [ -n "$LEASE_TOKEN" ]; then
  if TOKEN_FILE=$(mktemp "$DIR/daemon-update.lease-token.XXXXXX") \
       && chmod 600 "$TOKEN_FILE" \
       && ( umask 077 && printf '%s' "$LEASE_TOKEN" > "$TOKEN_FILE" ); then
    TOKEN_ARG=" --lease-token-file $TOKEN_FILE"
    TOKEN_TEXT=" --lease-token-file $TOKEN_FILE"
  else
    note_fail "寫不進 lease token 檔，嘗試交還窗口，這輪不派"
    [ -n "$TOKEN_FILE" ] && rm -f "$TOKEN_FILE"
    # 檔寫不出來時走 stdin，仍然不讓 token 進 argv。
    printf '%s' "$LEASE_TOKEN" | release_rebuild "token 檔寫不出來" --lease-token - || true
    exit 0
  fi
fi
log "已取得 rebuild 窗口 fence=$LEASE"

# 立即模式：restart 也由使用者這一下授權。以建置 child 的名義申請（§3c：restart 的 requester／owner 要是它自己的
# bot id，否則 acquire 會 409 exclude_not_requester），request id 固定成 `deploy-now-restart-<rebuild 核准>`：
# **daemon 在建立當下**核准（對得上請求檔、使用者核准的 rebuild、同一個 commit 才核准）。這支腳本不打 decide——
# #447 之後 approve 要驗過的 AGM 角色，launchd 沒有。回來不是 approved 就不帶，建置 child 照例行流程申請（AGM 裁示）。
RESTART_APPROVAL=""
if [ "$NOW" = 1 ]; then
  RESTART_APPROVAL=$("$AGM" --compact approval request --requester "$BOT" --purpose restart \
    --scope "daemon 重啟（立即部署 ${NOW_SHA}；使用者已在 UI 核准，rebuild 核准 ${APPROVAL}）" \
    --commit "$NOW_SHA" --request-id "deploy-now-restart-${APPROVAL}" --expires-in 21600 2>>"$LOG" | python3 -c '
import json,sys
d=json.load(sys.stdin)
if not isinstance(d.get("id"),str) or not d["id"] or d.get("status")!="approved": sys.exit(1)
print(d["id"])
' 2>/dev/null) || RESTART_APPROVAL=""
  if [ -n "$RESTART_APPROVAL" ]; then
    log "立即部署的 restart 核准 ${RESTART_APPROVAL} 已由 daemon 核准（建置 child ${BOT}）"
  else
    log "立即部署的 restart 核准沒有當場核准：建置 child 照例行流程申請"
  fi
fi

TMP=$(mktemp)
cat "$DIR/daemon-update-task.md" > "$TMP" 2>/dev/null || true
{
  printf '\n---\n'
  # 派工目標是**核准的那個 commit**，不是派工當下的 HEAD：兩者不同時（沿用核准的 docs-only 情形）建 HEAD 等於
  # 建一個沒審過的版本（2026-09-22：核准 513f2320、建了 69010d72）。已核准之後 HEAD 又動到要建的東西（DEFERRED）也一樣，
  # 新的留到下一輪另外申請（issue #439）。
  printf '要建、要重啟的 commit：%s（核准 %s 針對的就是它）。rebuild 租約 fence %s（owner %s）。\n' "$APPR_COMMIT" "$APPROVAL" "$LEASE" "$OWNER"
  if [ "$DEFERRED" = 1 ]; then
    printf 'origin/main 現在是 %s，比核准的多出會進 binary 的改動：之後還有 %s 個 commit 動到 binary，留下一趟；這次仍然只 checkout %s 來建，restart 核准也申請 %s，不要拿 HEAD——新的留到下一輪另外申請核准。\n' "$HEAD_SHA" "$(binary_commits_since "$APPR_COMMIT")" "$APPR_COMMIT" "$APPR_COMMIT"
  elif [ "$APPR_COMMIT" != "$HEAD_SHA" ]; then
    printf 'origin/main 現在是 %s，多出來的 commit 只動到不進 binary 的檔；仍然 checkout %s 來建，restart 核准也申請 %s，不要拿 HEAD。\n' "$HEAD_SHA" "$APPR_COMMIT" "$APPR_COMMIT"
  fi
  [ -n "$ESC_NOTE" ] && printf '%s\n' "$ESC_NOTE"
  if [ "$NOW" = 1 ]; then
    printf '這趟是使用者在 UI 左上角按「立即部署」觸發的：rebuild 核准 %s 由使用者核准，不等排程、不等 AGM 裁示。固定條件一條都不省（乾淨 HEAD worktree、整樹測試、沒人 working 才換、.bak、驗證失敗回滾）。\n' "$APPROVAL"
    if [ -n "$RESTART_APPROVAL" ]; then
      printf 'restart 核准 %s（requester＝你自己 %s、commit %s）也已由使用者一併核准：跳過 3c 第 2 步的申請與等待，直接用它走第 3 步與 `scripts/ops/daemon-swap.sh --approval %s`。\n' "$RESTART_APPROVAL" "$BOT" "$APPR_COMMIT" "$RESTART_APPROVAL"
    else
      printf 'restart 核准沒能預先開好：照 3c 第 2 步自己申請。\n'
    fi
  fi
  # shellcheck disable=SC2016  # 單引號是刻意的：反引號與 %s 都是要原樣印出去的文字
  printf '做完請回報，並用 `bin/agm lease release rebuild --owner %s --fence %s%s` 交還窗口；\n' "$OWNER" "$LEASE" "$TOKEN_TEXT"
  printf '（lease-token 只在 acquire 那一次出現、只寫在上面那個檔案裡：用 --lease-token-file 讓 agm 自己去讀，不要 cat 出來、不要印出來、不要貼進回報——argv 同 uid 的行程看得到。真的拿不到就請 AGM 用 --force 並附理由接管。）\n'
  printf '需要重啟正式 daemon 另外申請 restart 核准與租約，替換前請 AGM 重驗所有使用者與排程回合。\n'
} >> "$TMP"
# 立即模式的 request id 多帶核准 id：同一顆 commit 先前派過（沒上線）時，同一個 id 會被當成重送、拿回舊的那筆。
CRID="agm-daemon-update-$APPR_COMMIT"
[ "$NOW" = 1 ] && CRID="agm-daemon-update-${APPR_COMMIT}-now-${APPROVAL}"
if "$AGM" --compact assign --bot "$BOT" --text-file "$TMP" \
     --request-id "$CRID" ${REVIEW[@]+"${REVIEW[@]}"} \
     --owns daemon --owns web --owns Cargo.lock >> "$LOG" 2>&1; then
  # 「已派過」記的是這次實際建出來的東西：DEFERRED 時記核准的 commit，下一輪才會看到 HEAD 還沒建（記 HEAD 會讓它被當成已派過）。
  if [ "$DEFERRED" = 1 ]; then echo "$APPR_COMMIT" > "$STATE"; else echo "$HEAD_SHA" > "$STATE"; fi
  log "已派工 ${CRID}（origin/main ${HEAD_SHA}）"
  [ "$NOW" = 1 ] && drop_now "已派工 ${CRID}"
else
  note_fail "派工失敗，嘗試交還窗口"
  # shellcheck disable=SC2086  # TOKEN_ARG 是刻意要拆成兩個參數的（沒有 token 時是空字串）
  release_rebuild "派工失敗" $TOKEN_ARG || true
  [ -n "$TOKEN_FILE" ] && rm -f "$TOKEN_FILE"
fi
rm -f "$TMP"
