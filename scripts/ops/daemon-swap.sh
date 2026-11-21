#!/bin/bash
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
SYSTEMD_RUN="${SYSTEMD_RUN_BIN:-systemd-run}"
PLATFORM="${AGM_OPS_PLATFORM:-$(uname -s | tr 'A-Z' 'a-z')}"
PGREP="${PGREP_BIN:-pgrep}"
PYTHON="${PYTHON_BIN:-python3}"
HERDR="${HERDR_BIN:-herdr}"
PROBE_BOT="${SWAP_PROBE_BOT:-}"
PROBE_FALLBACK=""
if [ -z "$PROBE_BOT" ]; then
    _probe_ids=$("$PYTHON" -c 'import json,sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    d = {}
ok = lambda v: isinstance(v, str) and v.strip() and len(v.split()) == 1
ids = []
for k in ("swap_probe_bot_id", "responder_bot_id", "manager_bot_id"):
    v = d.get(k) if isinstance(d, dict) else None
    if ok(v) and v.strip() not in ids:
        ids.append(v.strip())
    if k == "swap_probe_bot_id" and ids:
        break
print(ids[0] if ids else "")
print(ids[-1] if len(ids) > 1 else "")' "$AGM_DIR/runtime.json" 2>/dev/null) || _probe_ids=""
    PROBE_BOT=$(printf '%s\n' "$_probe_ids" | sed -n 1p)
    PROBE_FALLBACK=$(printf '%s\n' "$_probe_ids" | sed -n 2p)
fi
PROBE_SKIPS="$AGM_DIR/daemon-swap.probe-skips"
PROBE_TRIES="${SWAP_PROBE_TRIES:-12}"
SETTLE="${SWAP_SETTLE_SECS:-45}"
WINDOW_TRIES="${SWAP_WINDOW_TRIES:-12}"
WINDOW_WAIT="${SWAP_WINDOW_WAIT_SECS:-15}"
SETTLE_TRIES="${SWAP_PROBE_SETTLE_TRIES:-12}"
SETTLE_WAIT="${SWAP_PROBE_SETTLE_WAIT_SECS:-5}"

SHA=""; OLD=""; OLDHASH=""; APPROVAL=""; OWNER=""; CHECKOUT=""
WINDOW_APPROVAL=""
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
for v in SHA OLD OLDHASH OWNER CHECKOUT; do
    eval "x=\$$v"
    [ -n "$x" ] || { echo "missing --$(echo $v | tr 'A-Z_' 'a-z-')" >&2; exit 2; }
done

LOG="${SWAP_LOG:-$AGM_DIR/daemon-swap.log}"
log() { echo "$(date '+%F %T') $*" | tee -a "$LOG"; }
case "$SHA" in
    ''|*[!0123456789abcdef]*) log "ABORT: --sha 必須是完整 40 碼小寫 commit id（收到 '${SHA}'）"; exit 10 ;;
esac
[ "${#SHA}" -eq 40 ] || { log "ABORT: --sha 必須是完整 40 碼小寫 commit id（收到 '${SHA}'）"; exit 10; }
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
START_PY="$SCRIPT_DIR/daemon-start.py"
if [ ! -f "$START_PY" ] || [ ! -r "$START_PY" ]; then
    log "ABORT: 找不到可讀的 daemon-start.py（${START_PY}），不停止目前 daemon"
    exit 3
fi
hash16() { shasum -a 256 "$1" 2>/dev/null | cut -c1-16; }

install_new_binary() {
    local dest=target/release/agents-managerd tmp="target/release/agents-managerd.new.$$"
    if cp "$NEWBIN" "$tmp" && [ "$(hash16 "$tmp")" = "$(hash16 "$NEWBIN")" ] && mv -f "$tmp" "$dest"; then
        return 0
    fi
    rm -f "$tmp"
    return 1
}

restore_old_binary() {
    local dest=target/release/agents-managerd tmp="target/release/agents-managerd.restore.$$"
    if [ -f "$PREV" ] && [ "$(hash16 "$PREV")" = "$OLDHASH" ] && mv -f "$PREV" "$dest" && [ "$(hash16 "$dest")" = "$OLDHASH" ]; then
        return 0
    fi
    if cp -p "$BAK" "$tmp" && [ "$(hash16 "$tmp")" = "$OLDHASH" ] && mv -f "$tmp" "$dest"; then
        return 0
    fi
    rm -f "$tmp"
    return 1
}

prune_old_db_backups() {
    local backup
    for backup in "$DB".bak-*; do
        [ -f "$backup" ] || continue
        [ "$backup" = "$DBB" ] && continue
        if rm -f "$backup"; then
            log "removed old DB backup $backup"
        else
            log "WARN: could not remove old DB backup $backup"
        fi
    done
}
write_built_marker() {
    local sha="$1" marker="$AGM_DIR/daemon-update.built" tmp
    [ ! -L "$marker" ] || return 1
    if [ -e "$marker" ] && [ ! -f "$marker" ]; then return 1; fi
    tmp=$(mktemp "$AGM_DIR/daemon-update.built.tmp.XXXXXX") || return 1
    if ! printf '%s\n' "$sha" > "$tmp"; then rm -f "$tmp"; return 1; fi
    if ! mv -f "$tmp" "$marker"; then rm -f "$tmp"; return 1; fi
    return 0
}
write_rejected_marker() {
    local sha="$1" marker="$AGM_DIR/daemon-update.rejected" tmp trailing_newlines
    [ ! -L "$marker" ] || return 1
    if [ -e "$marker" ] && [ ! -f "$marker" ]; then return 1; fi
    tmp=$(mktemp "$AGM_DIR/daemon-update.rejected.tmp.XXXXXX") || return 1
    if [ -f "$marker" ] && ! cat "$marker" > "$tmp"; then rm -f "$tmp"; return 1; fi
    if ! grep -qxF -- "$sha" "$tmp"; then
        if [ -s "$tmp" ]; then
            trailing_newlines=$(tail -c 1 "$tmp" | wc -l | tr -d '[:space:]')
            if [ "$trailing_newlines" = 0 ]; then printf '\n' >> "$tmp" || { rm -f "$tmp"; return 1; }; fi
        fi
        printf '%s\n' "$sha" >> "$tmp" || { rm -f "$tmp"; return 1; }
    fi
    if ! mv -f "$tmp" "$marker"; then rm -f "$tmp"; return 1; fi
    return 0
}
service_capability() {
    [ -n "${SWAP_CAP_CMD:-}" ] && { "$SWAP_CAP_CMD"; return; }
    "$PYTHON" - "$PORT" <<'PY'
import json, sys, urllib.error, urllib.request
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, _req, _fp, _code, _msg, _headers, _newurl):
        return None
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
base = f"http://127.0.0.1:{sys.argv[1]}"
try:
    with opener.open(base + "/api/session", timeout=3) as response:
        token = json.load(response).get("token", "")
    if not token:
        raise RuntimeError("session response has no UI token")
    request = urllib.request.Request(base + "/api/capabilities", headers={"X-AM-Token": token})
    with opener.open(request, timeout=3) as response:
        capabilities = json.load(response).get("capabilities", [])
    if "service_principals" not in capabilities:
        print("bootstrap")
    else:
        print("service" if "swap_restart_window" in capabilities else "service_old")
except urllib.error.HTTPError as error:
    print("bootstrap" if error.code == 404 else "unknown")
except Exception:
    print("unknown")
PY
}
if [ -L "$SERVICE_TOKEN_DIR" ] || { [ -e "$SERVICE_TOKEN_DIR" ] && [ ! -d "$SERVICE_TOKEN_DIR" ]; } \
    || [ -L "$SERVICE_TOKEN_FILE" ] || { [ -e "$SERVICE_TOKEN_FILE" ] && [ ! -f "$SERVICE_TOKEN_FILE" ]; }; then
    log "ABORT: daemon-swap service credential 不是一般檔案"
    exit 3
fi
CAP=$(service_capability)
case "$CAP" in
    service)
        [ -f "$SERVICE_TOKEN_FILE" ] || { log "ABORT: daemon supports service principals but daemon-swap token file is missing"; exit 3; }
        SERVICE_MODE=service
        WINDOW_MODE=service
        log "maintenance API identity: daemon-swap service principal" ;;
    service_old)
        [ -f "$SERVICE_TOKEN_FILE" ] || { log "ABORT: daemon supports service principals but daemon-swap token file is missing"; exit 3; }
        [ -n "$APPROVAL" ] || {
            log "ABORT: 線上 daemon 沒有 restart-window 路由；舊式換版需要明確的 --approval（rc=9）"
            exit 9
        }
        SERVICE_MODE=user
        WINDOW_MODE=legacy
        log "maintenance API identity: 核准的舊 daemon bootstrap（User token）" ;;
    bootstrap)
        [ -n "$APPROVAL" ] || {
            log "ABORT: 線上 daemon 沒有 restart-window 路由；舊式換版需要明確的 --approval（rc=9）"
            exit 9
        }
        SERVICE_MODE=user
        WINDOW_MODE=legacy
        log "maintenance API identity: 核准的舊 daemon bootstrap（User token）" ;;
    *)
        log "ABORT: cannot determine daemon service-auth capability; refusing User fallback"
        exit 3 ;;
esac

TOKEN_FILE=""
cleanup_token() { [ -n "$TOKEN_FILE" ] && rm -f "$TOKEN_FILE"; return 0; }
trap cleanup_token EXIT

save_token() {
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

prune_old_binary_backups() {
    local f
    for f in target/release/agents-managerd.bak-* target/release/agents-managerd.prev-*; do
        [ -f "$f" ] && [ ! -L "$f" ] || continue
        [ "$f" = "$BAK" ] && continue
        [ "$f" = "$PREV" ] && continue
        if rm -f "$f"; then
            log "removed old binary backup $f"
        else
            log "WARN: could not remove old binary backup $f"
        fi
    done
}

release_window() {
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
        AM_SERVICE_ID= AM_SERVICE_TOKEN_FILE= AM_BOT_ID= AM_BOT_TOKEN= AM_HOOK_TOKEN= \
            "$AGM_BIN" --compact "$@"
    fi
}
lease_safety() {
    if [ "$WINDOW_MODE" = legacy ]; then
        agm lease safety --approval "$APPROVAL" --owner "$OWNER" --exclude-bot "$OWNER"
    elif [ -n "$WINDOW_APPROVAL" ]; then
        agm lease safety --approval "$WINDOW_APPROVAL" --owner "$OWNER"
    else
        agm lease safety --owner "$OWNER"
    fi
}
daemon_pids() { "$PGREP" -f '^\./target/release/agents-managerd serve$' 2>/dev/null || true; }
dpid() { daemon_pids | head -1; }
stop_daemon() {
    local pid="$1" j=0
    [ -n "$pid" ] || return 0
    kill -TERM "$pid" 2>/dev/null
    while [ $j -lt 30 ]; do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; j=$((j + 1)); done
    kill -KILL "$pid" 2>/dev/null; sleep 1
    j=0
    while [ $j -lt 5 ]; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 1; j=$((j + 1))
    done
    log "ERROR: daemon pid ${pid} is still alive after SIGKILL"
    return 1
}
stop_daemon_for_rollback() {
    local pids count
    pids=$(daemon_pids)
    count=$(printf '%s\n' "$pids" | awk 'NF { n++ } END { print n + 0 }')
    if [ "$count" -gt 1 ]; then
        log "ERROR: found ${count} matching daemon processes during rollback"
        return 1
    fi
    [ "$count" -eq 0 ] && return 0
    stop_daemon "$pids"
}
api() { "$CURL" -sf -o /dev/null "http://127.0.0.1:$PORT$1"; }
agm_probe() {
    [ -n "${SWAP_PROBE_CMD:-}" ] && { "$SWAP_PROBE_CMD" "$1"; return; }
    "$PYTHON" - "$PORT" "$SERVICE_TOKEN_FILE" "$1" "$SERVICE_MODE" <<'PY'
import json, os, stat, sys, urllib.error, urllib.parse, urllib.request
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, _req, _fp, _code, _msg, _headers, _newurl):
        return None
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
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
    # 舊 daemon 不認 service token：只在明確 --approval 的 bootstrap 使用 User 身份自測。
    with opener.open(base + "/api/session") as response:
        tok = json.load(response)["token"]
    headers = {"X-AM-Token": tok, "Content-Type": "application/json"}
    url = base + f"/api/bots/{urllib.parse.quote(bot, safe='')}/prompt"
    data = json.dumps({"text": "[build 自測，回 ok 即可，不要做任何事]"}).encode("utf-8")
req = urllib.request.Request(url, data=data, headers=headers, method="POST")
try:
    r = opener.open(req)
    print(r.status, r.read().decode("utf-8", "replace")[:200])
except urllib.error.HTTPError as e:
    print(e.code, e.read().decode("utf-8", "replace")[:200])
except Exception as e:
    print("000", e)
PY
}

restart_window() {
    if [ "$WINDOW_MODE" = legacy ]; then
        agm lease acquire restart --owner "$OWNER" --approval "$APPROVAL" --commit "$SHA" --ttl 900 --exclude-bot "$OWNER" 2>&1
        return
    fi
    [ -n "${SWAP_WINDOW_CMD:-}" ] && { "$SWAP_WINDOW_CMD" "$OWNER" "$SHA"; return; }
    "$PYTHON" - "$PORT" "$SERVICE_TOKEN_FILE" "$OWNER" "$SHA" <<'PY'
import json, os, stat, sys, urllib.error, urllib.request
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, _req, _fp, _code, _msg, _headers, _newurl):
        return None
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
port, token_path, owner, sha = sys.argv[1:]
try:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    fd = os.open(token_path, flags)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_mode & 0o077 or (hasattr(os, "getuid") and st.st_uid != os.getuid()):
            raise RuntimeError("service credential file is not a private regular file owned by this user")
        tok = os.read(fd, 4097).decode("utf-8").strip()
    finally:
        os.close(fd)
    headers = {"X-AM-Service-Id": "daemon-swap", "X-AM-Service-Token": tok, "Content-Type": "application/json"}
    body = json.dumps({"owner": owner, "commit": sha, "ttl_secs": 900}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/services/daemon-swap/restart-window", data=body, headers=headers, method="POST")
    try:
        print(opener.open(req, timeout=30).read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        print(e.read().decode("utf-8", "replace"))
except Exception as e:
    print(json.dumps({"error": str(e)}))
PY
}

NEWBIN="$CHECKOUT/target/release/agents-managerd"
SHORT=$(echo "$SHA" | cut -c1-8)

HEAD_SHA=$(git -C "$CHECKOUT" rev-parse HEAD 2>/dev/null)
[ "$HEAD_SHA" = "$SHA" ] || { log "ABORT: checkout HEAD=$HEAD_SHA 不是 $SHA"; exit 3; }
[ -x "$NEWBIN" ] || { log "ABORT: 找不到新 binary $NEWBIN"; exit 3; }
BIN_VERSION_LINE=$("$NEWBIN" --version 2>/dev/null | head -n 1)
BIN_NAME=""; BIN_PKG=""; BIN_SHA=""; BIN_EXTRA=""
read -r BIN_NAME BIN_PKG BIN_SHA BIN_EXTRA <<EOF_VERSION
$BIN_VERSION_LINE
EOF_VERSION
BIN_SHA_CLEAN=${BIN_SHA%-dirty}
case "$BIN_SHA" in
    '') log "ABORT: 新 binary 的 --version 沒有內嵌 sha（'${BIN_VERSION_LINE}'）：說不出它是哪個 commit 建的，不換"; exit 10 ;;
esac
if [ -n "$BIN_EXTRA" ]; then
    log "ABORT: 新 binary 的 --version 格式看不懂（'${BIN_VERSION_LINE}'），不換"; exit 10
fi
if [ "$BIN_SHA_CLEAN" != "$BIN_SHA" ]; then
    log "ABORT: 新 binary 是髒樹建出來的（binary 內嵌 ${BIN_SHA}）：它不是 ${SHA} 這個 commit，不換"; exit 10
fi
[ "$BIN_SHA" = "$SHA" ] && log "binary sha ok: 內嵌 ${BIN_SHA} 符合核准的 ${SHA}" \
    || { log "ABORT: 新 binary 內嵌的 sha 是 ${BIN_SHA}，不是核准的 ${SHA}（checkout HEAD 對、但這顆 binary 不是從它建的？）；不拿窗口、不換"; exit 10; }
if ! git -C "$CHECKOUT" cat-file -e "${OLD}^{commit}" 2>/dev/null; then
    log "ABORT: 線上版本 $OLD 不在 checkout 裡，無法確認不是降版"
    exit 3
fi
if ! git -C "$CHECKOUT" merge-base --is-ancestor "$OLD" "$SHA" 2>/dev/null; then
    log "ABORT: 線上 $OLD 不是要換上的 $SHA 的祖先或同一顆，無法確認不是降版"
    exit 3
fi

cd "$AGM_REPO" || { log "ABORT: 進不去 $AGM_REPO"; exit 3; }
BAK="target/release/agents-managerd.bak-$OLD"
if [ ! -e "$BAK" ]; then
    BAKTMP="$BAK.tmp.$$"
    if ! cp -p target/release/agents-managerd "$BAKTMP" || [ "$(hash16 "$BAKTMP")" != "$OLDHASH" ] || ! mv "$BAKTMP" "$BAK"; then
        rm -f "$BAKTMP"
        log "ABORT: 無法建立回滾用的備份 ${BAK}（cp 失敗或內容不是 ${OLDHASH}）；什麼都沒動"
        exit 3
    fi
fi
GOT=$(hash16 "$BAK")
[ "$GOT" = "$OLDHASH" ] || { log "ABORT: 回滾用的 $BAK sha256=${GOT}，不是 $OLDHASH"; exit 3; }
log "rollback binary $BAK verified ($OLDHASH)"

EXP_UV=$("$PYTHON" - "$CHECKOUT/crates/am-base/src/db.rs" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r"SCHEMA_HISTORY[^=]*=\s*&?\s*\[(.*?)^\];", src, re.S | re.M)  # 結尾只認單獨成行的 `];`（#1459）
print(max(int(n) for n in re.findall(r"\(\s*(\d+)\s*,", m.group(1))) if m else "")
PY
)
case "$EXP_UV" in
    ''|*[!0-9]*) log "ABORT: 讀不到 checkout 的 SCHEMA_VERSION（拿到 '$EXP_UV'）——不要用上一輪的數字猜"; exit 3 ;;
esac
PRE_UV=$("$SQLITE" "$DB" "pragma user_version")
case "$PRE_UV" in
    ''|*[!0-9]*) log "ABORT: 讀不到 DB 的 user_version（拿到 '$PRE_UV'）——不換版"; exit 3 ;;
esac
if [ "$PRE_UV" -gt "$EXP_UV" ]; then
    log "ABORT: DB 的 schema 版本 $PRE_UV 比要換上的 binary 認得的 $EXP_UV 還新（要換上的 $SHA 比線上舊，或線上已經往前修過）——這顆 binary 會被版本閘擋下，不換"
    exit 3
fi
BUMPED=no; [ "$EXP_UV" != "$PRE_UV" ] && BUMPED=yes
log "schema: checkout SCHEMA_VERSION=$EXP_UV, db user_version=$PRE_UV, bumped=$BUMPED"

PANE="${HERDR_PANE_ID:-}"
if [ -z "$PANE" ]; then
    PANE=$("$HERDR" pane current 2>/dev/null | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit
print(((d.get("result") or {}).get("pane") or {}).get("pane_id") or ((d.get("result") or {}).get("pane") or {}).get("id") or "")')
fi
if [ -z "$PANE" ]; then
    LIST=$("$HERDR" pane list 2>&1); LRC=$?
    case "$LRC:$LIST" in
        0:*protocol_mismatch*|[!0]*)
            log "ABORT: herdr pane list rc=${LRC}：$(printf '%s' "$LIST" | tr -d '\n' | head -c 160)"; exit 3 ;;
    esac
    log "3b herdr pane list ok（排程執行，沒有自己的 pane）"
else
    READ=$("$HERDR" pane read "$PANE" --source recent_unwrapped --format ansi --lines 2 2>&1); RRC=$?
    case "$RRC:$READ" in
        0:*pane_not_found*|0:*protocol_mismatch*|[!0]*)
            log "ABORT: herdr pane read $PANE rc=${RRC}：$(printf '%s' "$READ" | tr -d '\n' | head -c 160)"; exit 3 ;;
    esac
    log "3b herdr pane read $PANE ok"
fi

PROBE_SKIPPED=no; PROBE_SKIP_WHY=""; PROBE_BLOCKED_REPLY=""
i=0
if [ -z "$PROBE_BOT" ]; then
    PROBE="no active run (沒有設定自測對象)"; PROBE_SKIPPED=yes; PROBE_SKIP_WHY="no active run"; i="$PROBE_TRIES"; PROBE_BOT="（未設定）"
fi
while [ "$i" -lt "$PROBE_TRIES" ]; do
    i=$((i + 1))
    PROBE=$(agm_probe "$PROBE_BOT")
    case "$PROBE" in
        200*) break ;;
        *"a turn is already in flight"*) sleep 10 ;;
        *"no active run"*)
            if [ -n "$PROBE_FALLBACK" ] && [ "$PROBE_FALLBACK" != "$PROBE_BOT" ]; then
                log "3b 自測對象 $PROBE_BOT 沒有在跑，改送 $PROBE_FALLBACK"
                PROBE_BOT="$PROBE_FALLBACK"; PROBE_FALLBACK=""; i=$((i - 1))
                continue
            fi
            PROBE_SKIPPED=yes; PROBE_SKIP_WHY="no active run"; break ;;
        409*'"draft'*)
            if [ -n "$PROBE_FALLBACK" ] && [ "$PROBE_FALLBACK" != "$PROBE_BOT" ]; then
                log "3b 自測對象 $PROBE_BOT 輸入框有未送出的字，改送 fallback $PROBE_FALLBACK"
                PROBE_BOT="$PROBE_FALLBACK"; PROBE_FALLBACK=""; i=$((i - 1))
                continue
            fi
            PROBE_SKIPPED=yes; PROBE_SKIP_WHY="輸入框有字"; break ;;
        409*"agent is blocked"*|409*"agent is busy"*|409*"agent_busy"*|409*"needs permission"*|409*"needs_permission"*|409*"run is not running"*)
            if [ -n "$PROBE_FALLBACK" ] && [ "$PROBE_FALLBACK" != "$PROBE_BOT" ]; then
                log "3b 自測對象 $PROBE_BOT 暫時不能收 prompt（$(printf '%s' "$PROBE" | head -c 120)），改送 fallback $PROBE_FALLBACK"
                PROBE_BOT="$PROBE_FALLBACK"; PROBE_FALLBACK=""; i=$((i - 1))
                continue
            fi
            PROBE_SKIPPED=yes; PROBE_SKIP_WHY="目標不能收 prompt"; PROBE_BLOCKED_REPLY=$(printf '%s' "$PROBE" | head -c 120); break ;;
        *) log "ABORT: 自測 prompt 回 $(printf '%s' "$PROBE" | head -c 160)"; exit 3 ;;
    esac
done
case "$PROBE" in
    200*) log "3b self probe ok: $(printf '%s' "$PROBE" | head -c 120)"; rm -f "$PROBE_SKIPS" "$PROBE_SKIPS.why" ;;
    *) if [ "$PROBE_SKIPPED" = yes ]; then
           if [ "$PROBE_SKIP_WHY" = "輸入框有字" ]; then
               log "3b 自測略過：$PROBE_BOT 輸入框有未送出的字（draft），不自動清別人的輸入框"
           elif [ "$PROBE_SKIP_WHY" = "目標不能收 prompt" ]; then
               log "3b 自測略過：$PROBE_BOT 暫時不能收 prompt（blocked／busy／needs permission：${PROBE_BLOCKED_REPLY:-?}），沒有可退回的對象"
           else
               log "3b 自測略過：$PROBE_BOT 沒有在跑（no active run），沒有東西可以送"
           fi
           printf '%s\n' "$PROBE_SKIP_WHY" > "$PROBE_SKIPS.why"
           _skips=$(cat "$PROBE_SKIPS" 2>/dev/null)
           case "$_skips" in ''|*[!0-9]*) _skips=0 ;; esac
           echo $((_skips + 1)) > "$PROBE_SKIPS.tmp" && mv -f "$PROBE_SKIPS.tmp" "$PROBE_SKIPS"
       else
           log "ABORT: 自測對象一直在跑回合，送不進去"; exit 3
       fi ;;
esac

i=0; SETTLED=no; BUSY=no
[ "$PROBE_SKIPPED" = yes ] && SETTLE_TRIES=0
while [ "$i" -lt "$SETTLE_TRIES" ]; do
    i=$((i + 1))
    BUSY=$(lease_safety 2>/dev/null \
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

TOKEN=""; FENCE=""
i=0
while [ "$i" -lt "$WINDOW_TRIES" ]; do
    i=$((i + 1))
    OUT=$(restart_window)
    PARSED=$(printf '%s' "$OUT" | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("False - -"); raise SystemExit
l = d.get("lease") or {}
a = d.get("approval") or {}
aid = a.get("id") if isinstance(a, dict) else None
s = d.get("safety") or {}
s = s if isinstance(s, dict) else {}
eap = s.get("escalation_approval_id")
tok = lambda v: v if isinstance(v, str) and v and " " not in v else "-"
print(l.get("held"), l.get("fence"), d.get("lease_token") or l.get("lease_token") or "-", tok(aid),
      "yes" if s.get("escalated") is True else "no", tok(eap))')
    HELD=$(echo "$PARSED" | cut -d' ' -f1); FENCE=$(echo "$PARSED" | cut -d' ' -f2); TOKEN=$(echo "$PARSED" | cut -d' ' -f3)
    WAP=$(echo "$PARSED" | cut -d' ' -f4); ACQ_ESC=$(echo "$PARSED" | cut -d' ' -f5); ACQ_ESC_AP=$(echo "$PARSED" | cut -d' ' -f6)
    [ "$HELD" = True ] && break
    WHY=$(printf '%s' "$OUT" | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    print(""); raise SystemExit
det = d.get("detail") or d
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
[ "$WINDOW_MODE" = service ] && [ "${WAP:--}" != - ] && WINDOW_APPROVAL="$WAP"

SAFE=$(lease_safety | ACQ_ESC="$ACQ_ESC" ACQ_ESC_AP="${ACQ_ESC_AP:--}" ACQ_AP="${WAP:--}" "$PYTHON" -c 'import json,os,sys
d = json.load(sys.stdin)
safe = d.get("safe") is True
esc = d.get("escalated")
carried = ""
if not safe and os.environ.get("ACQ_ESC") == "yes" and os.environ.get("ACQ_AP", "-") != "-" \
        and os.environ.get("ACQ_ESC_AP") == os.environ.get("ACQ_AP"):
    fields = ("delivering", "unreadable", "held_leases")
    if all(isinstance(d.get(k), list) for k in fields):
        others = [l for l in d["held_leases"] if not (isinstance(l, dict) and l.get("own") is True)]
        if not d["delivering"] and not d["unreadable"] and not others:
            safe, carried = True, " (沿用 acquire 的放寬)"
print(safe, [w.get("name") for w in d.get("working") or []], d.get("delivering"), "escalated=%s%s" % (esc, carried))')
log "3a recheck: $SAFE"
case "$SAFE" in
    True*) ;;
    *) log "ABORT: 換 binary 前複查不安全"
       release_window "3a 複查不安全" || true
       exit 4 ;;
esac

LIVEHASH=$(shasum -a 256 target/release/agents-managerd 2>/dev/null | cut -c1-16)
if [ -z "$LIVEHASH" ] || [ "$LIVEHASH" != "$OLDHASH" ]; then
    log "ABORT: 取得 restart 窗口後線上 binary 已改變（原版本 ${OLD}、目前 sha256=${LIVEHASH:-unknown}），重新讀取 live 版本後再試"
    release_window "線上 binary 已改變" || true
    exit 3
fi

DBB="$DB.bak-${SWAP_BACKUP_STAMP:-$(date +%Y%m%d-%H%M%S)}"
if [ -e "$DBB" ]; then
    log "ABORT: DB 備份 $DBB 已經存在，不覆蓋它（可能是上一趟換版前唯一的好備份）"
    release_window "DB 備份檔已存在" || true
    exit 5
fi
( umask 077 && "$SQLITE" "$DB" ".backup $DBB" ) || {
    log "ABORT: DB 備份失敗"
    release_window "DB 備份失敗" || true
    exit 5
}
IC=$("$SQLITE" "$DBB" "pragma integrity_check" | head -1)
BUV=$("$SQLITE" "$DBB" "pragma user_version")
log "db backup $DBB integrity=$IC user_version=$BUV"
[ "$IC" = ok ] || { log "ABORT: 備份讀不回來（integrity_check=${IC}），不換版"
                    release_window "DB 備份讀不回來" || true
                    exit 5; }

OFF=$(wc -c < "$DLOG" 2>/dev/null || echo 0)
bot_rows() { set -o pipefail; agm state | "$PYTHON" -c 'import json,sys
print("\n".join(sorted("%s\t%s" % (b.get("id") or "name:%s" % b.get("name"), b.get("name")) for b in json.load(sys.stdin).get("bots") or [])))'; }
SWAP_T0=$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')
if ! BEFORE_ROWS=$(bot_rows) || [ -z "$BEFORE_ROWS" ]; then
    log "ABORT: 換版前 agm state 讀取失敗或 bot 名單為空；不換 binary"
    release_window "換版前 bot 名單不可用" || true
    exit 3
fi

START_N=0
start() {
    L="am-daemon-swap-$$"
    if [ "$PLATFORM" = linux ]; then
        START_N=$((START_N + 1))
        [ -n "${XDG_RUNTIME_DIR:-}" ] || export XDG_RUNTIME_DIR="/run/user/$(id -u)"
        "$SYSTEMD_RUN" --user --collect --unit="$L-$START_N" -p Type=forking -p KillMode=process \
            -- "$(command -v "$PYTHON" || echo "$PYTHON")" "$START_PY" "$AGM_REPO" "$DLOG" >> "$LOG" 2>&1 || log "WARN: systemd-run rc=${?}（下面的 /api/session 驗證會接手判斷）"
        return
    fi
    "$LAUNCHCTL" remove "$L" 2>/dev/null
    "$LAUNCHCTL" submit -l "$L" -- "$PYTHON" "$START_PY" "$AGM_REPO" "$DLOG"
    sleep 2
    "$LAUNCHCTL" remove "$L" 2>/dev/null
}

rollback_new_children() {
    set -o pipefail
    rows=$("$SQLITE" -readonly "$DB" "SELECT id || char(9) || COALESCE(name, '') FROM bots
      WHERE managed_by = 'child' AND deleted_at IS NULL AND created_at >= '$SWAP_T0' ORDER BY created_at, id") || return 1
    printf '%s\n' "$rows" | "$PYTHON" -c 'import sys
before = {line.split("\t", 1)[0] for line in sys.argv[1].splitlines() if line}
for line in sys.stdin:
    line = line.rstrip("\n")
    if not line:
        continue
    ident, sep, name = line.partition("\t")
    if sep and ident not in before:
        print("%s (%s)" % (name, ident))' "$BEFORE_ROWS"
}

wait_session_after_rollback() {
    local j=0
    while [ $j -lt 30 ]; do api /api/session && return 0; sleep 1; j=$((j + 1)); done
    return 1
}

rollback() {
    log "ROLLBACK requested: $*"
    ROLLBACK_CHILDREN_KNOWN=yes
    ROLLBACK_NEW_CHILDREN=""
    if [ "$BUMPED" = yes ] && ! ROLLBACK_NEW_CHILDREN=$(rollback_new_children); then
        ROLLBACK_CHILDREN_KNOWN=no
        ROLLBACK_NEW_CHILDREN=""
    fi
    if [ "$BUMPED" = yes ]; then
        log "schema bumped $PRE_UV -> ${EXP_UV}：先往前修（舊 binary 開不了這個 DB）"
        if ! stop_daemon_for_rollback; then
            log "ERROR: cannot forward-fix while daemon is still alive; binary and DB left untouched"
            exit 11
        fi
        if install_new_binary; then
            start; sleep 3
            j=0; while [ $j -lt 30 ]; do api /api/session && break; sleep 1; j=$((j + 1)); done
            if api /api/session; then
                log "forward-fix ok：新 binary 服務中 pid $(dpid)，DB 留在 $EXP_UV 沒有還原"
                exit 6
            fi
        else
            log "forward-fix：新 binary 複製失敗，不起 daemon"
        fi
        log "forward-fix 失敗：新 binary 起不來，改還原 binary 與 DB"
    else
        if write_rejected_marker "$SHA"; then
            log "候選 sha 已在 rollback 前寫入 rejected 清單"
        else
            log "WARN: rollback 前 rejected 清單寫入失敗；kick 結束後會再試並告警"
        fi
        if ! stop_daemon_for_rollback; then
            log "ERROR: cannot roll back while daemon is still alive; binary and DB left untouched"
            exit 11
        fi
        if ! restore_old_binary; then
            log "ERROR: 舊 binary 還原失敗（${PREV}／${BAK} 都放不回或驗不過 ${OLDHASH}）；不起 daemon，DB 與備份原樣保留，要人工處理"
            exit 11
        fi
        rm -f "$AM_DATA/service-tokens/daemon-swap.token" "$AM_DATA/service-tokens/herdr-upgrade.token"
        rmdir "$AM_DATA/service-tokens" 2>/dev/null || true
        log "db left as is（schema 未升，舊 binary 可直接開；備份 ${DBB} 保留）"
        start; sleep 5
        if ! wait_session_after_rollback; then
            log "ERROR: 回滾後 daemon 沒有在服務（舊 binary 已換回，但 /api/session 30 秒內沒起來），不算已回滾，要人工處理"
            exit 12
        fi
        log "rolled back pid $(dpid)"
        exit 7
    fi
    if write_rejected_marker "$SHA"; then
        log "候選 sha 已在 rollback 前寫入 rejected 清單"
    else
        log "WARN: rollback 前 rejected 清單寫入失敗；kick 結束後會再試並告警"
    fi
    if ! stop_daemon_for_rollback; then
        log "ERROR: cannot roll back while daemon is still alive; binary and DB left untouched"
        exit 11
    fi
    if ! restore_old_binary; then
        log "ERROR: 舊 binary 還原失敗（${PREV}／${BAK} 都放不回或驗不過 ${OLDHASH}）；不起 daemon，DB 與備份原樣保留，要人工處理"
        exit 11
    fi
    rm -f "$AM_DATA/service-tokens/daemon-swap.token" "$AM_DATA/service-tokens/herdr-upgrade.token"
    rmdir "$AM_DATA/service-tokens" 2>/dev/null || true
    RESTORE_TMP="$DB.restore.$$"
    WAL_HOLD="$RESTORE_TMP-wal"
    SHM_HOLD="$RESTORE_TMP-shm"
    RESTORE_READY=yes
    RESTORE_ERROR="備份複製或同步失敗"
    if ( umask 077 && cp "$DBB" "$RESTORE_TMP" ) && chmod 600 "$RESTORE_TMP" \
        && "$PYTHON" -c 'import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
try:
    os.fsync(fd)
finally:
    os.close(fd)' "$RESTORE_TMP"; then
        if [ -e "$DB-wal" ] || [ -L "$DB-wal" ]; then
            if mv -f "$DB-wal" "$WAL_HOLD"; then :
            else RESTORE_READY=no; RESTORE_ERROR="無法暫存 WAL"; fi
        fi
        if [ "$RESTORE_READY" = yes ] && { [ -e "$DB-shm" ] || [ -L "$DB-shm" ]; }; then
            if mv -f "$DB-shm" "$SHM_HOLD"; then :
            else RESTORE_READY=no; RESTORE_ERROR="無法暫存 SHM"; fi
        fi
        if [ "$RESTORE_READY" = yes ]; then
            if mv -f "$RESTORE_TMP" "$DB"; then
                rm -f "$WAL_HOLD" "$SHM_HOLD"
                RESTORE_READY=done
            else
                RESTORE_READY=no
                RESTORE_ERROR="DB rename 失敗"
            fi
        fi
    else
        RESTORE_READY=no
    fi
    if [ "$RESTORE_READY" = done ]; then
        RU=$("$SQLITE" "$DB" "pragma user_version"); RI=$("$SQLITE" "$DB" "pragma integrity_check" | head -1)
        log "db restored from $DBB: user_version=$RU integrity=$RI"
        [ "$RI" = ok ] || log "WARN: 還原後的 DB integrity_check=$RI"
    else
        rm -f "$RESTORE_TMP"
        RESTORE_SIDECARS_OK=yes
        if [ -e "$WAL_HOLD" ] || [ -L "$WAL_HOLD" ]; then
            mv -f "$WAL_HOLD" "$DB-wal" || { RESTORE_SIDECARS_OK=no; log "ERROR: WAL 復原失敗，資料保留在 $WAL_HOLD"; }
        fi
        if [ -e "$SHM_HOLD" ] || [ -L "$SHM_HOLD" ]; then
            mv -f "$SHM_HOLD" "$DB-shm" || { RESTORE_SIDECARS_OK=no; log "ERROR: SHM 復原失敗，資料保留在 $SHM_HOLD"; }
        fi
        if [ "$RESTORE_SIDECARS_OK" = yes ]; then
            log "ERROR: DB 還原失敗（${RESTORE_ERROR}；備份 ${DBB}），原 DB 與它的 -wal／-shm 都保留；請手動還原後再啟動"
        else
            log "ERROR: DB 還原失敗（${RESTORE_ERROR}；備份 ${DBB}），sidecar 復原不完整，保留在上述路徑；請手動處理後再啟動"
        fi
    fi
    if [ "$ROLLBACK_CHILDREN_KNOWN" != yes ]; then
        log "WARN: rollback restored the DB but could not determine whether newly adopted children still have live panes"
    elif [ -n "$ROLLBACK_NEW_CHILDREN" ]; then
        log "WARN: rollback restored the DB but these newly adopted children may still have live panes: $ROLLBACK_NEW_CHILDREN"
    fi
    if [ "$RESTORE_READY" != done ]; then
        log "不起任何 daemon：舊 binary 開不了已升 schema 的 DB，要先人工還原 DB 再啟動"
        exit 11
    fi
    start; sleep 5
    if ! wait_session_after_rollback; then
        log "ERROR: 回滾後 daemon 沒有在服務（舊 binary 已換回，但 /api/session 30 秒內沒起來），不算已回滾，要人工處理"
        exit 12
    fi
    log "rolled back pid $(dpid)"
    exit 7
}

read_supervisor_status() {
    set -o pipefail
    agm supervisor | "$PYTHON" -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
status = d.get("status")
if not isinstance(status, str):
    sup = d.get("supervisor")
    status = sup.get("status") if isinstance(sup, dict) else None
if not isinstance(status, str) or not status.strip():
    raise SystemExit(1)
print(status.strip())'
}
supervisor_quota_gated() {
    set -o pipefail
    agm supervisor | "$PYTHON" -c 'import json,sys
from datetime import datetime,timezone
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
sup = d if isinstance(d.get("status"), str) else (d.get("supervisor") if isinstance(d.get("supervisor"), dict) else {})
at = sup.get("quota_reset_at")
if sup.get("status") != "waiting_quota" or not isinstance(at, str) or not at.strip():
    raise SystemExit(1)
try:
    t = datetime.fromisoformat(at.strip().replace("Z", "+00:00"))
except ValueError:
    raise SystemExit(1)
if t.tzinfo is None:
    t = t.replace(tzinfo=timezone.utc)
if t <= datetime.now(timezone.utc):
    raise SystemExit(1)
print(at.strip())'
}
PRE_SUP=$(read_supervisor_status 2>/dev/null) || PRE_SUP=""
log "supervisor status before swap: ${PRE_SUP:-讀不到}"

OLDPIDS=$(daemon_pids)
OLDPID_COUNT=$(printf '%s\n' "$OLDPIDS" | awk 'NF { n++ } END { print n + 0 }')
if [ "$OLDPID_COUNT" -eq 0 ]; then
    log "ABORT: could not identify the running daemon pid; refusing to replace binary"
    release_window "找不到線上 daemon 的 pid" || true
    exit 11
fi
if [ "$OLDPID_COUNT" -ne 1 ]; then
    log "ABORT: found ${OLDPID_COUNT} matching daemon processes; refusing to stop only one"
    release_window "找到多顆線上 daemon" || true
    exit 11
fi
OLDPID=$(printf '%s\n' "$OLDPIDS" | head -1); log "old pid $OLDPID"
NEWKB=$(( ($(wc -c < "$NEWBIN") + 1023) / 1024 ))
AVAILKB=$(df -Pk target/release 2>/dev/null | awk 'NR == 2 { print $4 }')
case "$AVAILKB" in
    ''|*[!0-9]*) log "WARN: 讀不到 target/release 的可用空間（'${AVAILKB}'），略過空間檢查" ;;
    *)
        if [ "$AVAILKB" -lt $((NEWKB * 2)) ]; then
            log "ABORT: target/release 可用 ${AVAILKB}KB 不到新 binary 的兩倍（$((NEWKB * 2))KB）；不停 daemon、不換"
            release_window "磁碟空間不足，沒換版" || true
            exit 5
        fi ;;
esac

RENEW_OUT=$(agm lease renew restart --owner "$OWNER" --fence "$FENCE" --ttl 900 --lease-token-file "$TOKEN_FILE" 2>&1)
RENEW_RC=$?
if [ "$RENEW_RC" -ne 0 ]; then
    log "ABORT: 換版前 restart lease 續約失敗 rc=${RENEW_RC}：$(printf '%s' "$RENEW_OUT" | tr -d '\n' | head -c 200)"
    release_window "換版前 restart lease 續約失敗" || true
    exit 4
fi
log "restart lease renewed before stopping daemon"

if ! stop_daemon "$OLDPID"; then
    log "ABORT: daemon is still alive after SIGKILL; refusing to replace binary"
    release_window "線上 daemon 無法停止" || true
    exit 11
fi
if ! PREV=$(mktemp "target/release/agents-managerd.prev-$OLD-$(date +%Y%m%d-%H%M%S)-XXXXXX"); then
    log "ABORT: 建不出換下來的舊 binary 備份檔；把舊 daemon 起回來"
    start; sleep 3
    release_window "備份檔建立失敗，沒換版" || true
    exit 5
fi
mv -f target/release/agents-managerd "$PREV"
if ! install_new_binary; then
    log "ABORT: 新 binary 複製到 target/release 失敗（磁碟滿了？）；把舊 daemon 起回來"
    if ! mv -f "$PREV" target/release/agents-managerd; then
        log "ERROR: 舊 binary 搬不回 $PREV → target/release/agents-managerd；daemon 沒起來，要人工處理"
        release_window "新 binary 複製失敗且舊 binary 搬不回" || true
        exit 11
    fi
    start; sleep 3
    release_window "新 binary 複製失敗，沒換版" || true
    exit 5
fi
start
sleep 2
NEWPID=$(dpid)
log "new pid $NEWPID binary=$(date -r target/release/agents-managerd '+%F %T' 2>/dev/null)"

ok=""; j=0
while [ $j -lt 30 ]; do api /api/session && { ok=1; break; }; sleep 1; j=$((j + 1)); done
[ -n "$ok" ] || rollback "/api/session 30 秒內沒起來"
log "session ok"
agm health >/dev/null 2>&1 || rollback "health 失敗"
log "health ok"

SUP_JSON=$(agm supervisor 2>/dev/null) || rollback "新 daemon 的 agm supervisor 讀取失敗，無法複核 sha"
DEPLOYED=$(printf '%s' "$SUP_JSON" | "$PYTHON" -c 'import json,sys
try:
    d = (json.load(sys.stdin) or {}).get("last_deploy") or {}
except Exception:
    d = {}
print(d.get("sha_full") or "-", "dirty" if d.get("dirty") else "clean")')
read -r DEPLOYED_SHA DEPLOYED_DIRTY <<EOF_DEPLOYED
$DEPLOYED
EOF_DEPLOYED
case "${DEPLOYED_SHA:--}" in
    -) rollback "新 daemon 的 /api/supervisor 沒回報 last_deploy.sha_full，複核不了它是不是核准的 ${SHA}" ;;
esac
[ "$DEPLOYED_SHA" = "$SHA" ] || rollback "新 daemon 回報的 sha ${DEPLOYED_SHA} 不是核准的 ${SHA}"
[ "${DEPLOYED_DIRTY:-clean}" = clean ] || rollback "新 daemon 回報自己是髒樹建的（${DEPLOYED_SHA}）"
[ -z "${DEPLOYED_SHA:-}" ] || log "deployed sha ok: daemon 回報 ${DEPLOYED_SHA}"

UV=$("$SQLITE" "$DB" "pragma user_version")
log "user_version=$UV (expected $EXP_UV)"
[ "$UV" = "$EXP_UV" ] || rollback "user_version 是 ${UV}，預期 $EXP_UV"

HELD_AFTER=$(agm lease status | "$PYTHON" -c 'import json,sys
L = [l for l in json.load(sys.stdin).get("leases") or [] if l.get("resource") == "restart"]
print(L[0].get("held") if L else "?")')
log "restart lease held=$HELD_AFTER"
RELEASE_FAILED=0
[ "$HELD_AFTER" = True ] && { release_window "daemon 沒有自動放掉，手動交還" || RELEASE_FAILED=1; }

sleep "$SETTLE"
if ! SUP=$(read_supervisor_status); then rollback "agm supervisor 讀取失敗"; fi
log "supervisor status: $SUP"
case "$SUP" in
    starting|idle|busy) ;;
    waiting_quota)
        if [ "$PRE_SUP" = waiting_quota ]; then
            log "supervisor status waiting_quota：換版前就是 waiting_quota，額度等待與這顆 binary 無關，不回滾"
        elif RESET_AT=$(supervisor_quota_gated 2>/dev/null); then
            log "supervisor status waiting_quota：額度閘門擋著（重置 ${RESET_AT}），與這顆 binary 無關，不回滾"
        else
            rollback "supervisor status 不健康或未知（${SUP:-空}）"
        fi ;;
    *) rollback "supervisor status 不健康或未知（${SUP:-空}）" ;;
esac

if ! AFTER_ROWS=$(bot_rows) || [ -z "$AFTER_ROWS" ]; then
    rollback "新版後 agm state 讀取失敗或 bot 名單為空"
fi
deleted_on_purpose() {
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT count(*) FROM bots b WHERE b.id = '$1'
      AND b.deleted_at IS NOT NULL AND b.deleted_at >= '$SWAP_T0'
      AND (EXISTS (SELECT 1 FROM intents i WHERE i.kind IN ('delete_bot', 'delete_project')
        AND i.status != 'abandoned'
        AND (i.created_at >= '$SWAP_T0' OR i.updated_at >= '$SWAP_T0' OR i.status IN ('pending', 'running'))
        AND (i.subject_id = b.id OR i.subject_id = b.project_id OR instr(i.payload_json, '\"' || b.id || '\"') > 0))
      OR EXISTS (SELECT 1 FROM intents i WHERE i.kind = 'retire_child' AND i.status = 'done'
        AND i.created_at >= '$SWAP_T0' AND i.subject_id = b.id
        AND (json_extract(i.payload_json, '\$.cause') IN ('pane_closed', 'promoted')
          OR (b.managed_by = 'child'
            AND json_extract(i.payload_json, '\$.cause') IN ('unconfirmed', 'parent_replaced_child')
            AND EXISTS (SELECT 1 FROM bots p
              WHERE p.id = json_extract(i.payload_json, '\$.parent_bot_id') AND p.deleted_at IS NULL
                AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = p.id AND r.state IN ('starting', 'running', 'stopping'))
                AND EXISTS (SELECT 1 FROM bots s WHERE s.managed_by = 'child'
                  AND (s.deleted_at IS NULL OR EXISTS (SELECT 1 FROM intents si
                    WHERE si.kind = 'retire_child' AND si.status = 'done' AND si.subject_id = s.id
                      AND si.created_at >= '$SWAP_T0' AND json_extract(si.payload_json, '\$.cause') = 'pane_closed'))
                  AND s.parent_bot_id = p.id AND s.id != b.id AND s.created_at >= '$SWAP_T0')))))
      OR EXISTS (SELECT 1 FROM intents i WHERE i.kind = 'retire_child' AND i.status = 'done'
        AND i.created_at >= '$SWAP_T0' AND i.subject_id = b.id AND b.managed_by = 'child'
        AND json_extract(i.payload_json, '\$.cause') = 'parent_replaced_child'
        AND json_extract(i.payload_json, '\$.pane') = 'gone'
        AND json_extract(i.payload_json, '\$.last_run.state') = 'exited'
        AND json_extract(i.payload_json, '\$.last_run.exit_reason') IN ('pane exited', 'agent not found during reconcile')
        AND EXISTS (SELECT 1 FROM bots p
          WHERE p.id = json_extract(i.payload_json, '\$.parent_bot_id') AND p.deleted_at IS NULL
            AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = p.id AND r.state IN ('starting', 'running', 'stopping'))))
      OR EXISTS (SELECT 1 FROM intents i WHERE i.kind = 'retire_child' AND i.status = 'done'
        AND i.created_at >= '$SWAP_T0' AND i.subject_id = b.id AND b.managed_by = 'child'
        AND json_extract(i.payload_json, '\$.mode') = 'implicit'
        AND json_extract(i.payload_json, '\$.why') = 'reconcile_run_already_ended'
        AND json_extract(i.payload_json, '\$.last_run.ended_at') IS NOT NULL
        AND json_extract(i.payload_json, '\$.last_run.ended_at') < '$SWAP_T0'
        AND COALESCE(json_extract(i.payload_json, '\$.last_run.state'), '') NOT IN ('starting', 'running', 'stopping')
        AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.state IN ('starting', 'running', 'stopping'))))" 2>/dev/null
}
reconcile_retired_child_parent() {
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT p.id FROM bots b
      JOIN intents i ON i.kind = 'retire_child' AND i.status = 'done' AND i.subject_id = b.id
        AND i.created_at >= '$SWAP_T0'
      JOIN bots p ON p.id = b.parent_bot_id AND p.id = json_extract(i.payload_json, '\$.parent_bot_id')
      WHERE b.id = '$1' AND b.managed_by = 'child' AND b.deleted_at IS NOT NULL AND b.deleted_at >= '$SWAP_T0'
        AND json_extract(i.payload_json, '\$.mode') = 'implicit'
        AND json_extract(i.payload_json, '\$.why') IN ('reconcile_agent_gone', 'reconcile_run_already_ended')
        AND json_extract(i.payload_json, '\$.cause') = 'unconfirmed'
        AND json_extract(i.payload_json, '\$.pane') = 'gone'
        AND p.deleted_at IS NULL
        AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = p.id AND r.state IN ('starting', 'running', 'stopping'))
      LIMIT 1" 2>/dev/null
}
live_child_parent() {
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT p.id FROM bots b JOIN bots p ON p.id = b.parent_bot_id
      WHERE b.id = '$1' AND b.managed_by = 'child' AND p.deleted_at IS NULL LIMIT 1" 2>/dev/null
}
retire_record_visible() {
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT count(*) FROM intents i WHERE i.subject_id = '$1'
      AND i.created_at >= '$SWAP_T0' AND i.status != 'abandoned'
      AND i.kind IN ('retire_child', 'delete_bot', 'delete_project')" 2>/dev/null
}
after_has_bot() {
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    printf '%s\n' "$AFTER_ROWS" | cut -f1 | grep -qxF -- "$1"
}
MISSING=""; DELETED=""
classify_missing() {
    MISSING=""; DELETED=""; WAITING=""; WAITING_IDS=""
    while IFS="$(printf '\t')" read -r id name; do
        [ -n "$id$name" ] || continue
        after_has_bot "$id" && continue
        if [ "$(deleted_on_purpose "$id")" = 1 ]; then
            DELETED="$DELETED$name "
        else
            parent_id=$(reconcile_retired_child_parent "$id")
            if [ -n "$parent_id" ] && after_has_bot "$parent_id"; then
                DELETED="$DELETED$name "
            else
                parent_id=$(live_child_parent "$id")
                if [ -n "$parent_id" ] && after_has_bot "$parent_id" && [ "$(retire_record_visible "$id")" = 0 ]; then
                    WAITING="$WAITING$name "
                    WAITING_IDS="$WAITING_IDS$id "
                else
                    MISSING="$MISSING$name "
                fi
            fi
        fi
    done <<< "$BEFORE_ROWS"
}
classify_missing
rechecks=0
while [ -n "$(echo "$WAITING_IDS" | tr -d ' ')" ] && [ "$rechecks" -lt 5 ]; do
    rechecks=$((rechecks + 1))
    log "reconcile child retirement pending for [$WAITING]; rechecking state and intent (${rechecks}/5)"
    sleep 1
    if ! AFTER_ROWS=$(bot_rows) || [ -z "$AFTER_ROWS" ]; then
        rollback "重查時新版後 agm state 讀取失敗或 bot 名單為空"
    fi
    classify_missing
done
MISSING="$MISSING$WAITING"
log "bots before=$(printf '%s\n' "$BEFORE_ROWS" | grep -c .) after=$(printf '%s\n' "$AFTER_ROWS" | grep -c .) missing=[$MISSING] deleted_in_window=[$DELETED]"
[ -n "$(echo "$MISSING" | tr -d ' ')" ] && rollback "有 bot 不見了（沒有刪除紀錄）：$MISSING"

ERRS=$(tail -c "+$((OFF + 1))" "$DLOG" 2>/dev/null | grep -cE '\bERROR\b|drift')
log "daemon.log since restart: ERROR/drift lines=$ERRS"

BUILT_WRITE_FAILED=0
if write_built_marker "$SHORT"; then
    prune_old_db_backups
    prune_old_binary_backups
    log "DONE .built=$SHORT pid=$(dpid) db_backup=$DBB"
else
    BUILT_WRITE_FAILED=1
    log "ERROR: 部署成功但寫入 daemon-update.built 失敗；新 binary 已通過驗證並在線，DB 備份保留在 $DBB"
fi
[ "$RELEASE_FAILED" = 0 ] || { log "EXIT 8: 換版成功，但 restart 窗口沒有交還成功（見上面的 rc）"; exit 8; }
[ "$BUILT_WRITE_FAILED" = 0 ] || { log "EXIT 11: 新 binary 健康在線，但部署 sha 標記沒有更新"; exit 11; }
