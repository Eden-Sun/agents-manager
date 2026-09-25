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
SERVICE_TOKEN_DIR="$AM_DATA/service-tokens"
SERVICE_TOKEN_FILE="$AM_DATA/service-tokens/daemon-swap.token"
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
WINDOW_TRIES="${SWAP_WINDOW_TRIES:-12}"   # 拿不到窗口時重試幾次（12 × 15 秒＝3 分鐘；一次 409 就 DEFER 會
                                          # 讓「某顆 bot 剛好翻回 working 那一瞬」變成整趟白跑，2026-09-21 實際發生過）
WINDOW_WAIT="${SWAP_WINDOW_WAIT_SECS:-15}"
SETTLE_TRIES="${SWAP_PROBE_SETTLE_TRIES:-12}"      # 自測回合收尾最多等幾次（12 × 5 秒＝1 分鐘）
SETTLE_WAIT="${SWAP_PROBE_SETTLE_WAIT_SECS:-5}"

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
service_capability() {
    "$PYTHON" - "$PORT" <<'PY'
import json, sys, urllib.error, urllib.request
base = f"http://127.0.0.1:{sys.argv[1]}"
try:
    with urllib.request.urlopen(base + "/api/session", timeout=3) as response:
        token = json.load(response).get("token", "")
    if not token:
        raise RuntimeError("session response has no UI token")
    request = urllib.request.Request(base + "/api/capabilities", headers={"X-AM-Token": token})
    with urllib.request.urlopen(request, timeout=3) as response:
        capabilities = json.load(response).get("capabilities", [])
    print("service" if "service_principals" in capabilities else "bootstrap")
except urllib.error.HTTPError as error:
    print("bootstrap" if error.code == 404 else "unknown")
except Exception:
    print("unknown")
PY
}
if [ -L "$SERVICE_TOKEN_DIR" ] || { [ -e "$SERVICE_TOKEN_DIR" ] && [ ! -d "$SERVICE_TOKEN_DIR" ]; } \
    || [ -L "$SERVICE_TOKEN_FILE" ] || { [ -e "$SERVICE_TOKEN_FILE" ] && [ ! -f "$SERVICE_TOKEN_FILE" ]; }; then
    log "ABORT: daemon-swap service credential 不是一般檔案"
    exit 4
elif [ -f "$SERVICE_TOKEN_FILE" ]; then
    SERVICE_MODE=service
    log "maintenance API identity: daemon-swap service principal"
else
    # Only an old daemon without this capability may use the one-time User bootstrap. If a new
    # daemon lost its service token file, fail closed instead of silently restoring broad User power.
    SERVICE_MODE=$(service_capability)
    case "$SERVICE_MODE" in
        bootstrap)
            # Older daemon cannot accept scoped service tokens; this is the one-time transition.
            log "maintenance API identity: one-time User bootstrap; upgraded daemon will create the service principal" ;;
        service)
            log "ABORT: daemon supports service principals but daemon-swap token file is missing"
            exit 4 ;;
        *)
            log "ABORT: cannot determine daemon service-auth capability; refusing User fallback"
            exit 4 ;;
    esac
fi

# lease_token 不進 argv（issue #477）：argv 對同一個 uid 的行程是公開的（`ps`），而這顆 token 是
# 「只在 acquire 回應出現一次、任何 API 都查不到」的一次性憑證——抄走就能收掉別人正在換 binary 的窗口。
# 寫進 0600 的檔，`agm` 用 --lease-token-file 讀；離開時不管成敗都刪掉。
TOKEN_FILE=""
cleanup_token() { [ -n "$TOKEN_FILE" ] && rm -f "$TOKEN_FILE"; return 0; }
trap cleanup_token EXIT

save_token() { # $1=token
    # 路徑要不可預測、而且**不要落在全域可寫的 /tmp**：umask 077 只在「這個檔是我們建的」時有用，
    # 同 uid 的行程（正是這張票的威脅模型）先建好同名檔或擺一條 symlink，`>` 就會沿用它的 owner／mode。
    # mktemp 的 XXXXXX ＋ O_EXCL 建在 AGM 自己的私有目錄（daemon-update-kick.sh 的 token 檔也放這）。
    TOKEN_FILE=$(mktemp "$AGM_DIR/daemon-swap.lease-token.XXXXXX") || {
        log "ABORT: 建不出 lease token 檔，不能在沒有辦法交還窗口的情況下往下走"
        exit 4
    }
    chmod 600 "$TOKEN_FILE"
    ( umask 077 && printf '%s' "$1" > "$TOKEN_FILE" ) || {
        log "ABORT: 寫不進 lease token 檔，不能在沒有辦法交還窗口的情況下往下走"
        exit 4
    }
}

# 交還窗口。**rc 不吞**（issue #477）：以前這三處都是 `>/dev/null 2>&1` 然後無條件 log「窗口已交還」，
# release 真的失敗時（daemon 不在、fence 過期、token 對不上）窗口會一直握到 TTL 到期，而紀錄說已經還了，
# 下一個人照著 log 判斷就會判錯。回傳 release 自己的 rc，由呼叫端決定要不要因此換結束碼。
release_window() { # $1=為什麼要還（寫進 log）
    local out rc
    out=$(agm lease release restart --owner "$OWNER" --fence "$FENCE" --lease-token-file "$TOKEN_FILE" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ]; then
        log "restart 窗口已交還（${1}）"
    else
        log "交還 restart 窗口失敗 rc=${rc}（${1}）：$(printf '%s' "$out" | tr -d '\n' | head -c 200)"
        log "窗口仍被握著，要等 TTL 到期或請 AGM 用 --force 接管——不要當成已經還了"
    fi
    return "$rc"
}
agm() {
    if [ "$SERVICE_MODE" = service ]; then
        AM_SERVICE_ID=daemon-swap AM_SERVICE_TOKEN_FILE="$SERVICE_TOKEN_FILE" \
            AM_BOT_ID= AM_BOT_TOKEN= AM_HOOK_TOKEN= "$AGM_BIN" --compact "$@"
    else
        AM_SERVICE_ID= AM_SERVICE_TOKEN_FILE= AM_BOT_ID= AM_BOT_TOKEN= AM_HOOK_TOKEN= "$AGM_BIN" --compact "$@"
    fi
}
dpid() { "$PGREP" -f '^\./target/release/agents-managerd serve$' | head -1; }
api() { "$CURL" -sf -o /dev/null "http://127.0.0.1:$PORT$1"; }
agm_probe() { # agm_probe <bot id> → "<http code> <body>"
    # 測試用：注入一支假的送達器，才不用真的打 daemon。
    [ -n "${SWAP_PROBE_CMD:-}" ] && { "$SWAP_PROBE_CMD" "$1"; return; }
    "$PYTHON" - "$PORT" "$SERVICE_TOKEN_FILE" "$1" "$SERVICE_MODE" <<'PY'
import json, os, stat, sys, urllib.error, urllib.parse, urllib.request
port, token_path, bot, mode = sys.argv[1:]
base = f"http://127.0.0.1:{port}"
if mode == "service":
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    fd = os.open(token_path, flags)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_mode & 0o077 or (hasattr(os, "getuid") and st.st_uid != os.getuid()):
            raise RuntimeError("service credential file is not a private regular file owned by this user")
        raw = os.read(fd, 4097)
    finally:
        os.close(fd)
    if len(raw) > 4096:
        raise RuntimeError("service credential file is too large")
    tok = raw.decode("utf-8").strip()
    if not tok or "\n" in tok or "\r" in tok:
        raise RuntimeError("service credential file must contain one non-empty token line")
    headers = {"X-AM-Service-Id": "daemon-swap", "X-AM-Service-Token": tok}
    url = base + f"/api/services/daemon-swap/probe/{urllib.parse.quote(bot, safe='')}"
    data = b""
else:
    # Old daemon, one-time migration only: the existing local User token keeps this first swap possible.
    with urllib.request.urlopen(base + "/api/session") as response:
        tok = json.load(response)["token"]
    headers = {"X-AM-Token": tok, "Content-Type": "application/json"}
    url = base + f"/api/bots/{bot}/prompt"
    data = json.dumps({"text": "[build 自測，回 ok 即可，不要做任何事]"}).encode()
req = urllib.request.Request(url, data=data, headers=headers, method="POST")
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

# 等自測那個回合收尾再拿窗口：它還在飛的時候 acquire 一定吃 409 not_idle（working 名單還是空的），
# 等於腳本自己擋自己——2026-09-21 連兩趟第 1 次 acquire 都是這樣被拒。上限用完就照樣往下走，
# 交給下面的窗口重試處理，不在這裡 DEFER。
i=0; SETTLED=no
while [ "$i" -lt "$SETTLE_TRIES" ]; do
    i=$((i + 1))
    BUSY=$(agm lease safety --approval "$APPROVAL" --owner "$OWNER" --exclude-bot "$OWNER" 2>/dev/null \
        | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("unknown"); raise SystemExit
ids = {x.get("bot_id") for k in ("in_flight", "working", "delivering") for x in d.get(k) or [] if isinstance(x, dict)}
print("yes" if sys.argv[1] in ids else "no")' "$PROBE_BOT")
    case "$BUSY" in
        no) SETTLED=yes; break ;;
        unknown) log "3b 自測回合狀態讀不到，直接去拿窗口"; break ;;
    esac
    [ "$i" -eq 1 ] && log "等自測回合收尾（$PROBE_BOT 還在飛）"
    [ "$i" -lt "$SETTLE_TRIES" ] && sleep "$SETTLE_WAIT"
done
[ "$SETTLED" = yes ] && log "3b self probe turn settled (check $i/$SETTLE_TRIES)"
[ "$BUSY" = yes ] && log "自測回合等了 $SETTLE_TRIES 次還在飛，照樣去拿窗口（被拒會重試）"

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
    WHY=$(printf '%s' "$OUT" | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit
det = d.get("detail") or {}
bits = [det.get("reason") or det.get("error") or d.get("error") or ""]
w = [x.get("name") for x in (det.get("safety") or {}).get("working") or []]
if w:
    bits.append("working=" + ",".join(str(n) for n in w))
else:
    fl = [str(x.get("bot_id")) for x in (det.get("safety") or {}).get("in_flight") or []]
    bits.append("working=(空，可能是自測回合" + (" in_flight=" + ",".join(fl) if fl else "") + ")")
if det.get("escalates_at"): bits.append("escalates_at=" + str(det["escalates_at"]))
print(" ".join(b for b in bits if b))')
    log "no window yet (try $i/$WINDOW_TRIES) reason=${WHY:-unparsed}: $(printf '%s' "$OUT" | tr -d '\n' | head -c 200)"
    [ "$i" -lt "$WINDOW_TRIES" ] && sleep "$WINDOW_WAIT"
done
[ "$HELD" = True ] || { log "DEFER: 拿不到 restart 窗口（試了 $WINDOW_TRIES 次，最後 reason=${WHY:-unparsed}）"; exit 4; }
log "restart lease fence=$FENCE token=$([ "$TOKEN" != - ] && echo saved || echo MISSING)"
save_token "$TOKEN"

SAFE=$(agm lease safety --approval "$APPROVAL" --owner "$OWNER" --exclude-bot "$OWNER" | "$PYTHON" -c 'import json,sys
d = json.load(sys.stdin)
print(d.get("safe"), [w.get("name") for w in d.get("working") or []], d.get("delivering"))')
log "3a recheck: $SAFE"
case "$SAFE" in
    True*) ;;
    *) log "ABORT: 換 binary 前複查不安全"
       # 交還失敗不改結束碼：4 的意思（沒窗口／複查不安全）沒變，而「有沒有還成」log 裡講得很清楚。
       release_window "3a 複查不安全" || true
       exit 4 ;;
esac

# ── 3. 備份 DB 與舊 binary ───────────────────────────────────────────────────
DBB="$DB.bak-$(date +%Y%m%d-%H%M)"
"$SQLITE" "$DB" ".backup $DBB" || { log "ABORT: DB 備份失敗"; exit 5; }
IC=$("$SQLITE" "$DBB" "pragma integrity_check" | head -1)
BUV=$("$SQLITE" "$DBB" "pragma user_version")
log "db backup $DBB integrity=$IC user_version=$BUV"
[ "$IC" = ok ] || { log "ABORT: 備份讀不回來（integrity_check=${IC}），不換版"
                    release_window "DB 備份讀不回來" || true
                    exit 5; }

OFF=$(wc -c < "$DLOG" 2>/dev/null || echo 0)
# 名單用「id<TAB>名字」：比對看 id（同名的新 bot 蓋不掉舊的那顆），log 印名字。
# 沒有 id 的列退回用名字比（`name:` 開頭，也就不可能被當成刻意刪除）。
# SWAP_T0 是換版窗口的起點，格式跟 daemon 的 db::now() 一樣（RFC3339、毫秒、Z），才能在 SQL 裡直接比字串；
# 取整到秒只會讓窗口往前多算不到一秒。
bot_rows() { agm state | "$PYTHON" -c 'import json,sys
print("\n".join(sorted("%s\t%s" % (b.get("id") or "name:%s" % b.get("name"), b.get("name")) for b in json.load(sys.stdin).get("bots") or [])))'; }
SWAP_T0=$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')
BEFORE_ROWS=$(bot_rows)

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
    # The old daemon cannot load or accept service principals. Remove credentials it generated before
    # the rollback so the next invocation uses the documented one-time User bootstrap path again.
    rm -f "$AM_DATA/service-tokens/daemon-swap.token" "$AM_DATA/service-tokens/herdr-upgrade.token"
    rmdir "$AM_DATA/service-tokens" 2>/dev/null || true
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
if [ "$SERVICE_MODE" = bootstrap ]; then
    if [ -f "$SERVICE_TOKEN_FILE" ] && [ ! -L "$SERVICE_TOKEN_DIR" ] && [ ! -L "$SERVICE_TOKEN_FILE" ]; then
        SERVICE_MODE=service
    else
        cap=$(service_capability)
        case "$cap" in
            service) rollback "新版 daemon 支援 service principal，但 daemon-swap token file 不存在或不安全" ;;
            bootstrap) rollback "新 binary 未提供 service principal capability" ;;
            *) rollback "無法確認新版 daemon 的 service principal capability" ;;
        esac
    fi
fi
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
# 換版本身成功了，但窗口沒交還就是下一個人拿不到窗口：整輪以 8 結束，不要靜靜走完（issue #477）。
RELEASE_FAILED=0
[ "$HELD_AFTER" = True ] && { release_window "daemon 沒有自動放掉，手動交還" || RELEASE_FAILED=1; }

sleep "$SETTLE"
SUP=$(agm supervisor | "$PYTHON" -c 'import json,sys
d = json.load(sys.stdin); print(d.get("status") or (d.get("supervisor") or {}).get("status"))')
log "supervisor status: $SUP"
[ "$SUP" = stopped ] && rollback "supervisor 停了"

AFTER_ROWS=$(bot_rows)
# 窗口內**刻意刪掉**的 bot 不算重啟弄丟的（2026-09-24 22:53 ca9a0330：父 bot 在這 45 秒裡刪了 child i263，
# 整趟被誤判回滾）。「刻意」只認刪除 API 留下的 intent：DELETE /api/bots|projects 在定案前先寫一筆
# delete_bot／delete_project（payload 帶當時的子孫 id），done 之後保留 24 小時。光有 deleted_at 不夠——
# 重啟後 reconcile 找不到 pane 而退役 child、或投影軟刪，也會寫 deleted_at，那正是這一步要抓的遺失。
# 父 bot 用 `herdr pane close` 收 child（不呼叫 DELETE）時，daemon 退役那顆 child 會寫一筆 retire_child 紀錄（#554）；
# 只認 subject 就是它、而且 cause 是 pane_closed（herdr 報過關閉事件、當下 pane 也不在）或 promoted 的。
# agent_missing（pane 還在、新 daemon 認不出 agent）／unconfirmed（pane 不在但沒人看到它被關）／herdr_restarted
# 正是換版會弄丟 child 的樣子，照樣回滾。判準見 SPEC §6.5a。
# 讀 DB 一律 -readonly；讀不到、id 格式不對都當成「沒有刪除紀錄」，照樣回滾。
deleted_on_purpose() { # $1=bot id → 印 1 才算
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT count(*) FROM bots b WHERE b.id = '$1'
      AND b.deleted_at IS NOT NULL AND b.deleted_at >= '$SWAP_T0'
      AND (EXISTS (SELECT 1 FROM intents i WHERE i.kind IN ('delete_bot', 'delete_project')
        AND i.status != 'abandoned' AND i.created_at >= '$SWAP_T0'
        AND (i.subject_id = b.id OR i.subject_id = b.project_id OR instr(i.payload_json, '\"' || b.id || '\"') > 0))
      OR EXISTS (SELECT 1 FROM intents i WHERE i.kind = 'retire_child' AND i.status = 'done'
        AND i.created_at >= '$SWAP_T0' AND i.subject_id = b.id
        AND json_extract(i.payload_json, '\$.cause') IN ('pane_closed', 'promoted')))" 2>/dev/null
}
MISSING=""; DELETED=""
while IFS="$(printf '\t')" read -r id name; do
    [ -n "$id$name" ] || continue
    printf '%s\n' "$AFTER_ROWS" | cut -f1 | grep -qxF -- "$id" && continue
    if [ "$(deleted_on_purpose "$id")" = 1 ]; then DELETED="$DELETED$name "; else MISSING="$MISSING$name "; fi
done <<EOF
$BEFORE_ROWS
EOF
log "bots before=$(printf '%s\n' "$BEFORE_ROWS" | grep -c .) after=$(printf '%s\n' "$AFTER_ROWS" | grep -c .) missing=[$MISSING] deleted_in_window=[$DELETED]"
[ -n "$(echo "$MISSING" | tr -d ' ')" ] && rollback "有 bot 不見了（沒有刪除紀錄）：$MISSING"

ERRS=$(tail -c "+$((OFF + 1))" "$DLOG" 2>/dev/null | grep -cE '\bERROR\b|drift')
log "daemon.log since restart: ERROR/drift lines=$ERRS"

echo "$SHORT" > "$AGM_DIR/daemon-update.built"
log "DONE .built=$SHORT pid=$(dpid) db_backup=$DBB"
# 換版成功、但窗口沒交還：下一個人拿不到窗口，要看得出來（issue #477）。
[ "$RELEASE_FAILED" = 0 ] || { log "EXIT 8: 換版成功，但 restart 窗口沒有交還成功（見上面的 rc）"; exit 8; }
