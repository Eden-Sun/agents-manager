#!/bin/bash
# 換掉正式 daemon 的 release binary 並重啟（AGM 建置 child 的第 3～7 步）。
#
# 為什麼進 repo：2026-09-20 那趟 33 秒停機的根因是「每趟臨時寫一份腳本」——上一輪把
# `user_version` 寫死成 10，這輪 schema 升到 11，腳本就誤判成失敗、回滾、舊 binary 被版本閘
# 擋下、daemon 起不來。腳本有版本控制與測試才擋得住這種事。
#
#   scripts/ops/daemon-swap.sh --sha <full sha> --old <short sha> --old-hash <sha256 前 16 碼> \
#       --approval <restart 核准 id> --owner <自己的 bot id> --checkout <乾淨 worktree>
#
# 前提（呼叫端負責）：checkout 已經建好 release binary、整樹測試已經過、rebuild 租約已交還、
# restart 核准已核准。這支只做「拿窗口 → 備份 → 換 binary → 重啟 → 驗證」，任何一步不對就照
# §18.13 的方向處理（升過 schema 預設往前修，不把舊 binary 放回去）。
set -u

AGM_DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
AGM_REPO="${AGM_REPO:-$HOME/project/agents-manager}"
AM_DATA="${AM_DATA:-$HOME/.config/agents-manager}"
DB="${DAEMON_DB:-$AM_DATA/agents-manager.sqlite3}"
DLOG="${DAEMON_LOG:-$AM_DATA/daemon.log}"
PORT="${AM_PORT_SWAP:-7788}"
AGM_BIN="${AGM_BIN:-$AGM_DIR/bin/agm}"
SQLITE="${SQLITE_BIN:-sqlite3}"
CURL="${CURL_BIN:-curl}"
LAUNCHCTL="${LAUNCHCTL_BIN:-launchctl}"
PGREP="${PGREP_BIN:-pgrep}"
PYTHON="${PYTHON_BIN:-python3}"
HERDR="${HERDR_BIN:-herdr}"
PROBE_BOT="${SWAP_PROBE_BOT:-01M248GA4H1TAHJCZRKVR73S3C}"   # AGM 的 browser-gc child（3b 自測對象）
PROBE_TRIES="${SWAP_PROBE_TRIES:-12}"   # 對方正在跑回合時，等它結束重送的次數上限
SETTLE="${SWAP_SETTLE_SECS:-45}"          # 重啟後等多久再看 supervisor／名單（測試會調小）
WINDOW_TRIES="${SWAP_WINDOW_TRIES:-1}"    # 拿不到窗口時重試幾次（呼叫端通常自己輪詢）

SHA=""; OLD=""; OLDHASH=""; APPROVAL=""; OWNER=""; CHECKOUT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --sha) SHA="$2"; shift 2 ;;
        --old) OLD="$2"; shift 2 ;;
        --old-hash) OLDHASH="$2"; shift 2 ;;
        --approval) APPROVAL="$2"; shift 2 ;;
        --owner) OWNER="$2"; shift 2 ;;
        --checkout) CHECKOUT="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
for v in SHA OLD OLDHASH APPROVAL OWNER CHECKOUT; do
    eval "x=\$$v"
    [ -n "$x" ] || { echo "missing --$(echo $v | tr 'A-Z_' 'a-z-')" >&2; exit 2; }
done

LOG="${SWAP_LOG:-$AGM_DIR/daemon-swap.log}"
log() { echo "$(date '+%F %T') $*" | tee -a "$LOG"; }
agm() { "$AGM_BIN" --compact "$@"; }
dpid() { "$PGREP" -f '^\./target/release/agents-managerd serve$' | head -1; }
api() { "$CURL" -sf -o /dev/null "http://127.0.0.1:$PORT$1"; }
agm_probe() { # agm_probe <bot id> → "<http code> <body>"
    # 測試用：注入一支假的送達器，才不用真的打 daemon。
    [ -n "${SWAP_PROBE_CMD:-}" ] && { "$SWAP_PROBE_CMD" "$1"; return; }
    "$PYTHON" - "$1" "$PORT" "$OWNER" <<'PY'
import json, sys, urllib.request
bot, port, owner = sys.argv[1], sys.argv[2], sys.argv[3]
base = f"http://127.0.0.1:{port}"
tok = json.load(urllib.request.urlopen(base + "/api/session"))["token"]
body = {"text": "[build 自測，回 ok 即可，不要做任何事]", "relay_from": owner}
req = urllib.request.Request(base + f"/api/bots/{bot}/prompt", data=json.dumps(body).encode(),
                             headers={"X-AM-Token": tok, "Content-Type": "application/json"}, method="POST")
try:
    r = urllib.request.urlopen(req)
    print(r.status, r.read().decode("utf-8", "replace")[:200])
except urllib.error.HTTPError as e:
    print(e.code, e.read().decode("utf-8", "replace")[:200])
except Exception as e:
    print("000", e)
PY
}

NEWBIN="$CHECKOUT/target/release/agents-managerd"
SHORT=$(echo "$SHA" | cut -c1-8)

# ── 1. 前置核對 ──────────────────────────────────────────────────────────────
HEAD_SHA=$(git -C "$CHECKOUT" rev-parse HEAD 2>/dev/null)
case "$HEAD_SHA" in
    "$SHA"*) ;;
    *) log "ABORT: checkout HEAD=$HEAD_SHA 不是 $SHA"; exit 3 ;;
esac
[ -x "$NEWBIN" ] || { log "ABORT: 找不到新 binary $NEWBIN"; exit 3; }

# CD 信任閘門（SPEC §18.2d）：例行更新在申請核准前問過一次；這裡是最後一道——不經例行更新、由 bot 直接部署的
# 那條路也得過。線上那一版（--old）→ 要換上去的 --sha 之間有外人的東西就不換。查不出來一樣不換。
# 閘門優先用 install 在 AGM 目錄的那份：這支腳本是從「要部署的 checkout」跑的，同一個 checkout 裡的閘門不能算數。
GATE="$AGM_DIR/bin/cd-trust-gate.py"
[ -f "$GATE" ] || GATE="$(cd "$(dirname "$0")" && pwd)/cd-trust-gate.py"
GH_BIN="${AGM_GH_BIN:-$(command -v gh 2>/dev/null || echo /opt/homebrew/bin/gh)}"
GATE_FROM=$(git -C "$CHECKOUT" rev-parse "${OLD}^{commit}" 2>/dev/null) || { log "ABORT: 線上那一版 $OLD 不在 checkout 的歷史裡，信任閘門無從比對"; exit 3; }
GATE_OUT=$("$PYTHON" "$GATE" check --repo "$CHECKOUT" --from "$GATE_FROM" --to "$HEAD_SHA" --state-dir "$AGM_DIR" --gh "$GH_BIN" 2>&1) || {
    log "ABORT: CD 信任閘門沒過（${OLD}..${SHORT}）：$(printf '%s' "$GATE_OUT" | tr '\n' ';' | cut -c1-600)"
    exit 3
}
log "cd trust gate passed (${OLD}..${SHORT})"

cd "$AGM_REPO" || { log "ABORT: 進不去 $AGM_REPO"; exit 3; }
BAK="target/release/agents-managerd.bak-$OLD"
[ -e "$BAK" ] || cp -p target/release/agents-managerd "$BAK"
GOT=$(shasum -a 256 "$BAK" | cut -c1-16)
[ "$GOT" = "$OLDHASH" ] || { log "ABORT: 回滾用的 $BAK sha256=${GOT}，不是 $OLDHASH"; exit 3; }
log "rollback binary $BAK verified ($OLDHASH)"

# 預期的 schema 版本**從 checkout 讀**，不寫死：SCHEMA_HISTORY 的最後一項就是這顆 binary 認得的版本。
EXP_UV=$("$PYTHON" - "$CHECKOUT/daemon/src/db.rs" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r"SCHEMA_HISTORY[^=]*=\s*&?\s*\[(.*?)\];", src, re.S)
print(max(int(n) for n in re.findall(r"\(\s*(\d+)\s*,", m.group(1))) if m else "")
PY
)
case "$EXP_UV" in
    ''|*[!0-9]*) log "ABORT: 讀不到 checkout 的 SCHEMA_VERSION（拿到 '$EXP_UV'）——不要用上一輪的數字猜"; exit 3 ;;
esac
PRE_UV=$("$SQLITE" "$DB" "pragma user_version")
BUMPED=no; [ "$EXP_UV" != "$PRE_UV" ] && BUMPED=yes
log "schema: checkout SCHEMA_VERSION=$EXP_UV, db user_version=$PRE_UV, bumped=$BUMPED"

# ── 1b. 3b：對真 herdr 驗（換 binary 之前，腳本自己做，不由呼叫端帶值）──────────
# pane id 一律當場取：${HERDR_PANE_ID}（pane 裡本來就有），沒有才 `herdr pane current`。
# 2026-09-16 與 2026-09-20 兩次都是呼叫端把自己的 pane id 寫死，pane 換了以後讀到
# pane_not_found；第二次還照樣換了 binary。所以這一段不接受外部傳進來的 pane id。
PANE="${HERDR_PANE_ID:-}"
if [ -z "$PANE" ]; then
    PANE=$("$HERDR" pane current 2>/dev/null | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit
print(((d.get("result") or {}).get("pane") or {}).get("pane_id") or ((d.get("result") or {}).get("pane") or {}).get("id") or "")')
fi
[ -n "$PANE" ] || { log "ABORT: 取不到自己的 pane id（HERDR_PANE_ID 沒設，herdr pane current 也讀不到）"; exit 3; }
READ=$("$HERDR" pane read "$PANE" --source recent_unwrapped --format ansi --lines 2 2>&1); RRC=$?
case "$RRC:$READ" in
    0:*pane_not_found*|0:*protocol_mismatch*|[!0]*)
        log "ABORT: herdr pane read $PANE rc=${RRC}：$(printf '%s' "$READ" | tr -d '\n' | head -c 160)"; exit 3 ;;
esac
log "3b herdr pane read $PANE ok"

# 自測 prompt：對方正在跑回合（a turn is already in flight）是結構性的，等它結束重送；
# 其他非 200 一律當成送達線有問題，不換 binary。
i=0
while [ "$i" -lt "$PROBE_TRIES" ]; do
    i=$((i + 1))
    PROBE=$(agm_probe "$PROBE_BOT")
    case "$PROBE" in
        200*) break ;;
        *"a turn is already in flight"*) sleep 10 ;;
        *) log "ABORT: 自測 prompt 回 $(printf '%s' "$PROBE" | head -c 160)"; exit 3 ;;
    esac
done
case "$PROBE" in
    200*) log "3b self probe ok: $(printf '%s' "$PROBE" | head -c 120)" ;;
    *) log "ABORT: 自測對象一直在跑回合，送不進去"; exit 3 ;;
esac

# ── 2. 拿 restart 窗口，換 binary 前一刻再查一次（§3a）────────────────────────
TOKEN=""; FENCE=""
i=0
while [ "$i" -lt "$WINDOW_TRIES" ]; do
    i=$((i + 1))
    OUT=$(agm lease acquire restart --owner "$OWNER" --approval "$APPROVAL" --commit "$SHA" --ttl 900 --exclude-bot "$OWNER" 2>&1)
    PARSED=$(printf '%s' "$OUT" | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("False - -"); raise SystemExit
l = d.get("lease") or {}
print(l.get("held"), l.get("fence"), d.get("lease_token") or l.get("lease_token") or "-")')
    HELD=$(echo "$PARSED" | cut -d' ' -f1); FENCE=$(echo "$PARSED" | cut -d' ' -f2); TOKEN=$(echo "$PARSED" | cut -d' ' -f3)
    [ "$HELD" = True ] && break
    log "no window yet: $(printf '%s' "$OUT" | tr -d '\n' | head -c 200)"
    [ "$i" -lt "$WINDOW_TRIES" ] && sleep 15
done
[ "$HELD" = True ] || { log "DEFER: 拿不到 restart 窗口"; exit 4; }
log "restart lease fence=$FENCE token=$([ "$TOKEN" != - ] && echo saved || echo MISSING)"

SAFE=$(agm lease safety --approval "$APPROVAL" --owner "$OWNER" --exclude-bot "$OWNER" | "$PYTHON" -c 'import json,sys
d = json.load(sys.stdin)
print(d.get("safe"), [w.get("name") for w in d.get("working") or []], d.get("delivering"))')
log "3a recheck: $SAFE"
case "$SAFE" in
    True*) ;;
    *) agm lease release restart --owner "$OWNER" --fence "$FENCE" --lease-token "$TOKEN" >/dev/null 2>&1
       log "ABORT: 換 binary 前複查不安全，窗口已交還"; exit 4 ;;
esac

# ── 3. 備份 DB 與舊 binary ───────────────────────────────────────────────────
DBB="$DB.bak-$(date +%Y%m%d-%H%M)"
"$SQLITE" "$DB" ".backup $DBB" || { log "ABORT: DB 備份失敗"; exit 5; }
IC=$("$SQLITE" "$DBB" "pragma integrity_check" | head -1)
BUV=$("$SQLITE" "$DBB" "pragma user_version")
log "db backup $DBB integrity=$IC user_version=$BUV"
[ "$IC" = ok ] || { agm lease release restart --owner "$OWNER" --fence "$FENCE" --lease-token "$TOKEN" >/dev/null 2>&1
                    log "ABORT: 備份讀不回來（integrity_check=${IC}），不換版"; exit 5; }

OFF=$(wc -c < "$DLOG" 2>/dev/null || echo 0)
BEFORE_NAMES=$(agm state | "$PYTHON" -c 'import json,sys
print("\n".join(sorted(b["name"] for b in json.load(sys.stdin).get("bots") or [])))')

# ── 4. 換 binary 並重啟 ──────────────────────────────────────────────────────
# 啟動一律經過 launchd：pane 忙的時候會被 renice 到 5，子行程繼承後降不回去
# （非 root 不能降 nice）。launchd 跑的啟動器是 nice 0，啟動器 fork+setsid 後結束，
# daemon 就是 ppid=1、nice 0 的獨立行程；job 自己結束後 remove 不會殺到它。
start() {
    L="am-daemon-swap-$$"
    "$LAUNCHCTL" remove "$L" 2>/dev/null
    "$LAUNCHCTL" submit -l "$L" -- "$PYTHON" "$(dirname "$0")/daemon-start.py" "$AGM_REPO" "$DLOG"
    sleep 2
    "$LAUNCHCTL" remove "$L" 2>/dev/null
}

rollback() {
    log "ROLLBACK requested: $*"
    # 升過 schema 就不能把舊 binary 放回去：v(N+1) 開過的 DB，v(N) 的 binary 會被版本閘擋下、
    # 起不來（2026-09-20 實際發生過）。預設往前修，只有新 binary 真的起不來才動 DB。
    if [ "$BUMPED" = yes ]; then
        log "schema bumped $PRE_UV -> ${EXP_UV}：先往前修（舊 binary 開不了這個 DB）"
        kill -TERM "$(dpid)" 2>/dev/null; sleep 5
        cp "$NEWBIN" target/release/agents-managerd; start; sleep 3
        j=0; while [ $j -lt 30 ]; do api /api/session && break; sleep 1; j=$((j + 1)); done
        if api /api/session; then
            log "forward-fix ok：新 binary 服務中 pid $(dpid)，DB 留在 $EXP_UV 沒有還原"
            exit 6
        fi
        log "forward-fix 失敗：新 binary 起不來，改還原 binary 與 DB"
    fi
    kill -TERM "$(dpid)" 2>/dev/null; sleep 5
    cp -p "$BAK" target/release/agents-managerd
    # DB 還原：daemon 已停；主檔與 -wal／-shm 要一起處理——新版留下的 WAL 配舊主檔會變成半新半舊。
    rm -f "$DB-wal" "$DB-shm"
    cp -p "$DBB" "$DB"
    RU=$("$SQLITE" "$DB" "pragma user_version"); RI=$("$SQLITE" "$DB" "pragma integrity_check" | head -1)
    log "db restored from $DBB: user_version=$RU integrity=$RI"
    [ "$RI" = ok ] || log "WARN: 還原後的 DB integrity_check=$RI"
    start; sleep 5
    log "rolled back pid $(dpid)"
    exit 7
}

OLDPID=$(dpid); log "old pid $OLDPID"
kill -TERM "$OLDPID" 2>/dev/null
j=0; while [ $j -lt 30 ]; do kill -0 "$OLDPID" 2>/dev/null || break; sleep 1; j=$((j + 1)); done
kill -0 "$OLDPID" 2>/dev/null && { kill -KILL "$OLDPID"; sleep 1; }
PREV="target/release/agents-managerd.prev-$OLD-$(date +%Y%m%d-%H%M%S)"
[ -e "$PREV" ] && { log "ABORT: $PREV 已存在"; exit 5; }
mv target/release/agents-managerd "$PREV"
cp "$NEWBIN" target/release/agents-managerd
start
sleep 2
NEWPID=$(dpid)
log "new pid $NEWPID binary=$(date -r target/release/agents-managerd '+%F %T' 2>/dev/null)"

# ── 5. 驗證 ─────────────────────────────────────────────────────────────────
ok=""; j=0
while [ $j -lt 30 ]; do api /api/session && { ok=1; break; }; sleep 1; j=$((j + 1)); done
[ -n "$ok" ] || rollback "/api/session 30 秒內沒起來"
log "session ok"
agm health >/dev/null 2>&1 || rollback "health 失敗"
log "health ok"

UV=$("$SQLITE" "$DB" "pragma user_version")
log "user_version=$UV (expected $EXP_UV)"
[ "$UV" = "$EXP_UV" ] || rollback "user_version 是 ${UV}，預期 $EXP_UV"

HELD_AFTER=$(agm lease status | "$PYTHON" -c 'import json,sys
L = [l for l in json.load(sys.stdin).get("leases") or [] if l.get("resource") == "restart"]
print(L[0].get("held") if L else "?")')
log "restart lease held=$HELD_AFTER"
[ "$HELD_AFTER" = True ] && { agm lease release restart --owner "$OWNER" --fence "$FENCE" --lease-token "$TOKEN" >/dev/null 2>&1
                              log "restart lease 手動交還（daemon 沒有自動放掉）"; }

sleep "$SETTLE"
SUP=$(agm supervisor | "$PYTHON" -c 'import json,sys
d = json.load(sys.stdin); print(d.get("status") or (d.get("supervisor") or {}).get("status"))')
log "supervisor status: $SUP"
[ "$SUP" = stopped ] && rollback "supervisor 停了"

AFTER_NAMES=$(agm state | "$PYTHON" -c 'import json,sys
print("\n".join(sorted(b["name"] for b in json.load(sys.stdin).get("bots") or [])))')
MISSING=$(comm -23 <(printf '%s\n' "$BEFORE_NAMES") <(printf '%s\n' "$AFTER_NAMES") | tr '\n' ' ')
log "bots before=$(printf '%s\n' "$BEFORE_NAMES" | grep -c .) after=$(printf '%s\n' "$AFTER_NAMES" | grep -c .) missing=[$MISSING]"
[ -n "$(echo "$MISSING" | tr -d ' ')" ] && rollback "有 bot 不見了：$MISSING"

ERRS=$(tail -c "+$((OFF + 1))" "$DLOG" 2>/dev/null | grep -cE '\bERROR\b|drift')
log "daemon.log since restart: ERROR/drift lines=$ERRS"

echo "$SHORT" > "$AGM_DIR/daemon-update.built"
log "DONE .built=$SHORT pid=$(dpid) db_backup=$DBB"
