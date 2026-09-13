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

# A second runner must not create another approval or hand off the same lease. A killed runner
# leaves this directory behind: fail closed until AGM verifies no runner remains and removes it.
LOCK="$DIR/daemon-update.lock"
mkdir "$LOCK" 2>/dev/null || { log "更新檢查已有執行者或殘留鎖，交 AGM 檢查"; exit 0; }
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT
log "== check"
"$GIT" -C "$REPO" fetch -q origin main 2>>"$LOG" || log "fetch 失敗，用本地 origin/main"
HEAD_SHA=$("$GIT" -C "$REPO" rev-parse origin/main) || { log "無法讀取 origin/main，跳過"; exit 0; }

# 會影響 binary 的路徑。問 daemon 拿（它知道自己 include_str! 了什麼）；問不到再用保底清單。
PATHS=$("$AGM" --compact build-inputs 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
print(" ".join(d.get("paths") or []))
' 2>/dev/null) || PATHS=""
[ -n "$PATHS" ] || PATHS="daemon web Cargo.toml Cargo.lock docs/goals/agm-supervisor-persona.md scripts/agm.py"

BUILT_SHA=""
if [ -f "$BUILT" ]; then
  BUILT_SHA=$(tr -d '[:space:]' < "$BUILT")
  "$GIT" -C "$REPO" cat-file -e "${BUILT_SHA}^{commit}" 2>/dev/null || BUILT_SHA=""
fi
if [ -n "$BUILT_SHA" ]; then
  # shellcheck disable=SC2086  # PATHS 是刻意要拆成多個參數的
  if "$GIT" -C "$REPO" diff --quiet "$BUILT_SHA" origin/main -- $PATHS; then
    log "$BUILT_SHA 之後只動到不進 binary 的檔，跳過（origin/main ${HEAD_SHA}）"
    echo "$HEAD_SHA" > "$STATE"
    exit 0
  fi
fi
if [ -f "$STATE" ] && [ "$(cat "$STATE")" = "$HEAD_SHA" ]; then
  log "$HEAD_SHA 已經派過，跳過"; exit 0
fi

# 建置 child 還在嗎（不在就跳過這輪，不改派）
if ! "$AGM" --compact state | BOT="$BOT" python3 -c '
import json,os,sys
sys.exit(0 if any(b.get("id")==os.environ["BOT"] for b in json.load(sys.stdin).get("bots",[])) else 1)
' 2>/dev/null; then
  log "建置 child $BOT 不在，跳過"; exit 0
fi

# 上一筆更新派工還沒**結案**就不要再疊：回合跑完但沒人驗收的也算未結案。
PENDING=$("$AGM" --compact assignments --open 2>/dev/null | python3 -c '
import json,sys
rows = json.load(sys.stdin)["assignments"]
if not isinstance(rows, list): sys.exit(1)
mine = [r for r in rows if str(r.get("client_request_id","")).startswith("agm-daemon-update-")]
print(mine[0]["client_request_id"] if mine else "")
' 2>/dev/null) || { log "無法確認未結案派工，這輪不派"; exit 0; }
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
  log "無法從 runtime.json 取得 manager_bot_id，這輪不派"; exit 0;
}
EXCL=(--exclude-bot "$BOT" --exclude-bot "$MANAGER")

# safety 與 acquire 都傳同一份排除清單，由 daemon 判定；不再自行過濾快照。
# 回應須確認實際排除的 ID。舊 daemon / CLI 尚未支援或格式有誤就跳過，
# 保留正式熱修腳本直到 daemon 升級後再安裝本版。重啟仍另行核准。
SAFE=$("$AGM" --compact lease safety "${EXCL[@]}" 2>/dev/null | BUILD_BOT="$BOT" MANAGER_BOT="$MANAGER" python3 -c '
import json,sys,os
d=json.load(sys.stdin)
if not isinstance(d,dict) or not isinstance(d.get("safe"),bool): sys.exit(1)
for key in ("working","in_flight","unreadable"):
    if not isinstance(d.get(key),list) or any(not isinstance(x,dict) for x in d[key]): sys.exit(1)
ex={os.environ["BUILD_BOT"],os.environ["MANAGER_BOT"]}
applied=d.get("excluded_bot_ids")
if not isinstance(applied,list) or any(not isinstance(x,str) for x in applied) or set(applied) != ex: sys.exit(1)
working,in_flight,unreadable=d["working"],d["in_flight"],d["unreadable"]
ok=d["safe"] and not working and not in_flight and not unreadable
print("yes" if ok else ",".join(b.get("name","?") for b in working) or ("unreadable" if unreadable else "in_flight" if in_flight else "unknown"))
' 2>/dev/null) || SAFE="unknown"
if [ "$SAFE" != "yes" ]; then
  log "還有人在跑（${SAFE}），這輪不派"; exit 0
fi

# Reuse the same durable approval on later invocations. Creating a fresh pending request on
# every tick makes an asynchronous AGM decision impossible to consume. Store the full commit
# and requester so approval for one tree/owner can never authorize another.
APPROVAL=""
if [ -f "$APPROVAL_STATE" ]; then
  APPROVAL=$(python3 -c '
import json,sys
with open(sys.argv[1]) as f: d=json.load(f)
if d["commit"] == sys.argv[2] and d["owner"] == sys.argv[3]:
    if not isinstance(d["id"], str) or not d["id"]: sys.exit(1)
    print(d["id"])
' "$APPROVAL_STATE" "$HEAD_SHA" "$OWNER" 2>/dev/null) || {
    log "核准狀態檔損毀，交 AGM 檢查，這輪不派"; exit 0;
  }
fi
if [ -z "$APPROVAL" ]; then
  APPROVAL=$("$AGM" --compact approval request \
    --requester "$OWNER" --purpose rebuild \
    --scope "release rebuild（daemon/web/persona/agm.py）；restart 另行核准" \
    --commit "$HEAD_SHA" --expires-in 5400 2>>"$LOG" | python3 -c '
import json,sys
d=json.load(sys.stdin)
if not isinstance(d.get("id"),str) or not d["id"]: sys.exit(1)
print(d["id"])
' 2>/dev/null) || { log "申請核准失敗，這輪不派"; exit 0; }
  python3 -c '
import json,os,sys
path,commit,owner,aid=sys.argv[1:]
with open(path+".tmp","w") as f: json.dump(dict(commit=commit,owner=owner,id=aid),f)
os.replace(path+".tmp",path)
' "$APPROVAL_STATE" "$HEAD_SHA" "$OWNER" "$APPROVAL" || {
    log "保存核准 ID 失敗，這輪不派"; exit 0;
  }
  log "已申請核准 ${APPROVAL}（commit ${HEAD_SHA}），等 AGM 裁示"
fi

STATUS=$("$AGM" --compact approval list 2>/dev/null | APPROVAL="$APPROVAL" python3 -c '
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
' 2>/dev/null) || { log "無法確認核准 ${APPROVAL}，這輪不派"; exit 0; }
if [ "$STATUS" = "expired" ]; then
  rm -f "$APPROVAL_STATE"
  log "核准 ${APPROVAL} 已過期，下輪重新申請"; exit 0
fi
if [ "$STATUS" != "approved" ]; then
  log "核准狀態是 ${STATUS:-unknown}，這輪不派（下個整點再看）"; exit 0
fi

# 取得 rebuild 窗口：acquire 會在同一個鎖裡重驗一次 idle 再把窗口拿走。
LEASE=$("$AGM" --compact lease acquire rebuild --approval "$APPROVAL" --commit "$HEAD_SHA" \
  --owner "$OWNER" --ttl 3600 "${EXCL[@]}" 2>>"$LOG" | python3 -c '
import json,sys
print(json.load(sys.stdin).get("lease", {}).get("fence") or "")
' 2>/dev/null) || LEASE=""
if [ -z "$LEASE" ]; then
  log "拿不到 rebuild 窗口（可能有人正在做或核准對不上），這輪不派"; exit 0
fi
log "已取得 rebuild 窗口 fence=$LEASE"

TMP=$(mktemp)
cat "$DIR/daemon-update-task.md" > "$TMP" 2>/dev/null || true
{
  printf '\n---\n'
  printf 'origin/main %s。核准 %s，rebuild 租約 fence %s（owner %s）。\n' "$HEAD_SHA" "$APPROVAL" "$LEASE" "$OWNER"
  # shellcheck disable=SC2016  # 單引號是刻意的：反引號與 %s 都是要原樣印出去的文字
  printf '做完請回報，並用 `bin/agm lease release rebuild --owner %s --fence %s` 交還窗口；\n' "$OWNER" "$LEASE"
  printf '需要重啟正式 daemon 另外申請 restart 核准與租約，替換前請 AGM 重驗所有使用者與排程回合。\n'
} >> "$TMP"
if "$AGM" --compact assign --bot "$BOT" --text-file "$TMP" \
     --request-id "agm-daemon-update-$HEAD_SHA" \
     --owns daemon --owns web --owns Cargo.lock >> "$LOG" 2>&1; then
  echo "$HEAD_SHA" > "$STATE"
  log "已派工 agm-daemon-update-$HEAD_SHA"
else
  log "派工失敗，交還窗口"
  "$AGM" --compact lease release rebuild --owner "$OWNER" --fence "$LEASE" >> "$LOG" 2>&1 || true
fi
rm -f "$TMP"
