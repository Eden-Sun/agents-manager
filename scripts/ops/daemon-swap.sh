#!/bin/bash
# 換掉正式 daemon 的 release binary 並重啟（例行自動部署 daemon-update-kick.sh 的換版那一段）。
#
# 為什麼進 repo：2026-09-20 那趟 33 秒停機的根因是「每趟臨時寫一份腳本」——上一輪把
# `user_version` 寫死成 10，這輪 schema 升到 11，腳本就誤判成失敗、回滾、舊 binary 被版本閘
# 擋下、daemon 起不來。腳本有版本控制與測試才擋得住這種事。
#
#   scripts/ops/daemon-swap.sh --sha <full sha> --old <short sha> --old-hash <sha256 前 16 碼> \
#       --owner <窗口持有者名稱> --checkout <乾淨 checkout> [--approval <舊流程核准 id>]
#
# 前提（呼叫端負責）：checkout 已經建好 release binary，而且那顆 commit 已經過 ubuntu-ci（或使用者按了立即部署）。
# 不需要核准單（使用者 2026-09-29）：窗口由 daemon 的 `POST /api/services/daemon-swap/restart-window` 開——
# daemon-swap 服務身分自己開一筆立即核准的 restart 單，再走同一個 acquire：沒有人 working／送達中、沒有別人的
# 租約才拿得到，拿到時 assignment 派送暫停。這支只做「拿窗口 → 備份 → 換 binary → 重啟 → 驗證」，任何一步
# 不對就照 §18.13 的方向處理（升過 schema 預設往前修，不把舊 binary 放回去）。
#
# 結束碼：0 換版完成；3 前置核對不過；4 沒拿到窗口（有人在忙，下一輪再來）；5 備份失敗；
#         10 新 binary 內嵌的 sha 不是 --sha（換版之前就中止，什麼都沒動）；
#         6 新版起來了但升過 schema 所以往前修；7 已回滾；8 換好但窗口沒交還；9 daemon 太舊，沒有 restart-window 路由。
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
# darwin／linux：決定重啟走 launchd 還是 systemd（issue #677）。AGM_OPS_PLATFORM 只給測試蓋掉。
PLATFORM="${AGM_OPS_PLATFORM:-$(uname -s | tr 'A-Z' 'a-z')}"
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
WINDOW_APPROVAL=""   # restart-window 模式下 daemon 開窗口用的核准 id（從窗口回應讀；舊 task 傳的 --approval 不算）
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
# --sha is a trust boundary: abbreviated ids must not make the checkout, built
# binary, or post-start daemon appear to be approved.
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
service_capability() {
    # 測試用：注入假的能力探測，才不用真的打 daemon。
    [ -n "${SWAP_CAP_CMD:-}" ] && { "$SWAP_CAP_CMD"; return; }
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
# 新 daemon 只認 daemon-swap service principal。尚未提供 restart-window 的舊 daemon，僅接受明確帶來的舊核准 id，
# 並用本機 User token 走原本的 lease acquire；一般自動部署不帶核准，仍安全中止，需按文件先做一次核准 bootstrap。
if [ -L "$SERVICE_TOKEN_DIR" ] || { [ -e "$SERVICE_TOKEN_DIR" ] && [ ! -d "$SERVICE_TOKEN_DIR" ]; } \
    || [ -L "$SERVICE_TOKEN_FILE" ] || { [ -e "$SERVICE_TOKEN_FILE" ] && [ ! -f "$SERVICE_TOKEN_FILE" ]; }; then
    log "ABORT: daemon-swap service credential 不是一般檔案"
    exit 4
fi
CAP=$(service_capability)
case "$CAP" in
    service)
        [ -f "$SERVICE_TOKEN_FILE" ] || { log "ABORT: daemon supports service principals but daemon-swap token file is missing"; exit 4; }
        SERVICE_MODE=service
        WINDOW_MODE=service
        log "maintenance API identity: daemon-swap service principal" ;;
    service_old)
        [ -f "$SERVICE_TOKEN_FILE" ] || { log "ABORT: daemon supports service principals but daemon-swap token file is missing"; exit 4; }
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
        exit 4 ;;
esac

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
        AM_SERVICE_ID= AM_SERVICE_TOKEN_FILE= AM_BOT_ID= AM_BOT_TOKEN= AM_HOOK_TOKEN= \
            "$AGM_BIN" --compact "$@"
    fi
}
lease_safety() {
    if [ "$WINDOW_MODE" = legacy ]; then
        agm lease safety --approval "$APPROVAL" --owner "$OWNER" --exclude-bot "$OWNER"
    elif [ -n "$WINDOW_APPROVAL" ]; then
        # 拿到窗口之後的 §3a 複查：綁 restart-window 開窗口用的**那一張**核准，跟 acquire 同一套判斷（issue #840）。
        # 不帶的話放寬要靠「最早那筆活著的核准」與使用者的「現在換版」碰巧對上，複查會把 acquire 剛給的放寬推翻。
        agm lease safety --approval "$WINDOW_APPROVAL" --owner "$OWNER"
    else
        agm lease safety --owner "$OWNER"
    fi
}
dpid() { "$PGREP" -f '^\./target/release/agents-managerd serve$' | head -1; }
# 停 daemon：TERM、最多等 30 秒、還在就 KILL。換版主線與 rollback 共用——rollback 以前只 TERM 後固定 sleep 5，
# 新 daemon 還沒退就覆蓋 binary、刪 -wal／-shm、蓋掉 DB，等於在活著的 daemon 底下動它的資料。
stop_daemon() { # stop_daemon <pid>（空的就什麼都不做）
    local pid="$1" j=0
    [ -n "$pid" ] || return 0
    kill -TERM "$pid" 2>/dev/null
    while [ $j -lt 30 ]; do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; j=$((j + 1)); done
    kill -KILL "$pid" 2>/dev/null; sleep 1
    return 0
}
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
    # 舊 daemon 不認 service token：只在明確 --approval 的 bootstrap 使用 User 身分自測。
    with urllib.request.urlopen(base + "/api/session") as response:
        tok = json.load(response)["token"]
    headers = {"X-AM-Token": tok, "Content-Type": "application/json"}
    url = base + f"/api/bots/{urllib.parse.quote(bot, safe='')}/prompt"
    data = json.dumps({"text": "[build 自測，回 ok 即可，不要做任何事]"}).encode("utf-8")
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

# 開 restart 窗口：daemon-swap 服務身分打 restart-window，daemon 自己開一筆立即核准的單再走 acquire
# （安全檢查、暫停派送、fence／lease_token 都照舊）。印出回應本文（成功與被拒都是 JSON，下面同一份解析）。
restart_window() {
    if [ "$WINDOW_MODE" = legacy ]; then
        agm lease acquire restart --owner "$OWNER" --approval "$APPROVAL" --commit "$SHA" --ttl 900 --exclude-bot "$OWNER" 2>&1
        return
    fi
    # 測試用：注入假的窗口回應（同樣印出回應本文）。
    [ -n "${SWAP_WINDOW_CMD:-}" ] && { "$SWAP_WINDOW_CMD" "$OWNER" "$SHA"; return; }
    "$PYTHON" - "$PORT" "$SERVICE_TOKEN_FILE" "$OWNER" "$SHA" <<'PY'
import json, os, stat, sys, urllib.error, urllib.request
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
        print(urllib.request.urlopen(req, timeout=30).read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        print(e.read().decode("utf-8", "replace"))
except Exception as e:
    print(json.dumps({"error": str(e)}))
PY
}

NEWBIN="$CHECKOUT/target/release/agents-managerd"
SHORT=$(echo "$SHA" | cut -c1-8)

# ── 1. 前置核對 ──────────────────────────────────────────────────────────────
HEAD_SHA=$(git -C "$CHECKOUT" rev-parse HEAD 2>/dev/null)
[ "$HEAD_SHA" = "$SHA" ] || { log "ABORT: checkout HEAD=$HEAD_SHA 不是 $SHA"; exit 3; }
[ -x "$NEWBIN" ] || { log "ABORT: 找不到新 binary $NEWBIN"; exit 3; }
# 要換上去的這顆 binary 真的是核准的那個 commit 建出來的嗎？daemon 自己驗不了（換上去的檔案只有這支腳本摸得到），
# 所以換之前先問 binary 自己：`--version` 印 `<name> <版本> <完整 sha>[-dirty]`（clap 在進 main 之前就結束，不啟動服務）。
# 對不上／髒樹建的／舊 binary 沒內嵌 sha（讀不出來）一律中止（rc=10）：窗口都還沒拿、舊 daemon 沒停、binary 沒動。
# 上面的 HEAD 檢查只證明 checkout 在那一顆；binary 是不是從那個 checkout 建的，是這一步才證明。
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
# 線上 (--old) 必須是要換上的 commit 的祖先或同一顆。無法證明不是降版就拒絕（#638）。
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
    # cp may create a short/partial file before it reports ENOSPC or another error. Never publish
    # that as the stable rollback path: a later retry would see it and permanently refuse to replace it.
    BAK_TMP=$(mktemp "${BAK}.tmp.XXXXXX") || { log "ABORT: 建不出回滾 binary 暫存備份"; exit 3; }
    if ! cp -p target/release/agents-managerd "$BAK_TMP"; then
        rm -f "$BAK_TMP"
        log "ABORT: 回滾 binary 備份複製失敗"
        exit 3
    fi
    TMP_GOT=$(shasum -a 256 "$BAK_TMP" | cut -c1-16)
    if [ "$TMP_GOT" != "$OLDHASH" ]; then
        rm -f "$BAK_TMP"
        log "ABORT: 回滾 binary 暫存備份 sha256=${TMP_GOT}，不是 $OLDHASH"
        exit 3
    fi
    # A concurrent invocation may have published the same OLD backup after our existence check.
    # ln is atomic and will not overwrite an existing path; the common verification below checks either copy.
    if ! ln "$BAK_TMP" "$BAK" && [ ! -e "$BAK" ]; then
        rm -f "$BAK_TMP"
        log "ABORT: 發佈回滾 binary 備份失敗"
        exit 3
    fi
    rm -f "$BAK_TMP"
fi
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
# 讀不到（sqlite3 壞了、DB 開不了）不能當成「升過 schema」往下跑；DB 比要換上的 binary 還新的話，
# 換上去的 binary 一定被版本閘擋下（db.rs `apply_migrations`），停機之後才發現就晚了——停 daemon 之前就拒絕。
case "$PRE_UV" in
    ''|*[!0-9]*) log "ABORT: 讀不到 DB 的 user_version（拿到 '$PRE_UV'）——不換版"; exit 3 ;;
esac
if [ "$PRE_UV" -gt "$EXP_UV" ]; then
    log "ABORT: DB 的 schema 版本 $PRE_UV 比要換上的 binary 認得的 $EXP_UV 還新（要換上的 $SHA 比線上舊，或線上已經往前修過）——這顆 binary 會被版本閘擋下，不換"
    exit 3
fi
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
if [ -z "$PANE" ]; then
    # 排程（systemd timer／launchd）跑的沒有自己的 pane：退而確認 herdr socket 通、協定對得上。
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

# 自測 prompt：對方正在跑回合（a turn is already in flight）是結構性的，等它結束重送；
# 其他非 200 一律當成送達線有問題，不換 binary。
# 例外：自測對象根本沒有在跑（`no active run`，例如 Linux 主機上沒有桌面所以 offline 的 browser-gc child）——
# 沒有東西可以送，不是送達線壞了；這一步略過（上面的 herdr 讀取仍然做了），不能讓每趟自動換版都卡在這。
PROBE_SKIPPED=no
i=0
while [ "$i" -lt "$PROBE_TRIES" ]; do
    i=$((i + 1))
    PROBE=$(agm_probe "$PROBE_BOT")
    case "$PROBE" in
        200*) break ;;
        *"a turn is already in flight"*) sleep 10 ;;
        *"no active run"*) PROBE_SKIPPED=yes; break ;;
        *) log "ABORT: 自測 prompt 回 $(printf '%s' "$PROBE" | head -c 160)"; exit 3 ;;
    esac
done
case "$PROBE" in
    200*) log "3b self probe ok: $(printf '%s' "$PROBE" | head -c 120)" ;;
    *) if [ "$PROBE_SKIPPED" = yes ]; then
           log "3b 自測略過：$PROBE_BOT 沒有在跑（no active run），沒有東西可以送"
       else
           log "ABORT: 自測對象一直在跑回合，送不進去"; exit 3
       fi ;;
esac

# 等自測那個回合收尾再拿窗口：它還在飛的時候 acquire 一定吃 409 not_idle（working 名單還是空的），
# 等於腳本自己擋自己——2026-09-21 連兩趟第 1 次 acquire 都是這樣被拒。上限用完就照樣往下走，
# 交給下面的窗口重試處理，不在這裡 DEFER。
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

# ── 2. 拿 restart 窗口，換 binary 前一刻再查一次（§3a）────────────────────────
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
print(l.get("held"), l.get("fence"), d.get("lease_token") or l.get("lease_token") or "-", aid if isinstance(aid, str) and aid and " " not in aid else "-")')
    HELD=$(echo "$PARSED" | cut -d' ' -f1); FENCE=$(echo "$PARSED" | cut -d' ' -f2); TOKEN=$(echo "$PARSED" | cut -d' ' -f3)
    WAP=$(echo "$PARSED" | cut -d' ' -f4)
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
# 舊 daemon 的窗口回應沒有 approval：照舊不帶，複查退回「最早那筆活著的核准」。
[ "$WINDOW_MODE" = service ] && [ "${WAP:--}" != - ] && WINDOW_APPROVAL="$WAP"

# 跟 acquire 同一套判斷：放寬生效（同一張核准等滿門檻，或使用者按了「現在換版」）時 working 不擋，
# 送達臨界區、別人的租約、讀不到狀態照擋——這些都由 daemon 的 safety 決定，這裡不自己重算。
SAFE=$(lease_safety | "$PYTHON" -c 'import json,sys
d = json.load(sys.stdin)
print(d.get("safe"), [w.get("name") for w in d.get("working") or []], d.get("delivering"), "escalated=%s" % d.get("escalated"))')
log "3a recheck: $SAFE"
case "$SAFE" in
    True*) ;;
    *) log "ABORT: 換 binary 前複查不安全"
       # 交還失敗不改結束碼：4 的意思（沒窗口／複查不安全）沒變，而「有沒有還成」log 裡講得很清楚。
       release_window "3a 複查不安全" || true
       exit 4 ;;
esac

# 前置的祖先檢查與備份發生在拿窗口之前。拿到 restart 窗口後，確認正式 binary 沒在等待期間被另一趟換掉。
LIVEHASH=$(shasum -a 256 target/release/agents-managerd 2>/dev/null | cut -c1-16)
if [ -z "$LIVEHASH" ] || [ "$LIVEHASH" != "$OLDHASH" ]; then
    log "ABORT: 取得 restart 窗口後線上 binary 已改變（原版本 ${OLD}、目前 sha256=${LIVEHASH:-unknown}），重新讀取 live 版本後再試"
    release_window "線上 binary 已改變" || true
    exit 3
fi

# ── 3. 備份 DB 與舊 binary ───────────────────────────────────────────────────
# 檔名到秒，而且已經存在就拒絕：`.backup` 對既有檔是整個覆蓋，同一分鐘的第二趟會把上一趟換版前唯一的好備份蓋成「已經 migrate 過的」。
# SWAP_BACKUP_STAMP 只給測試固定檔名。
DBB="$DB.bak-${SWAP_BACKUP_STAMP:-$(date +%Y%m%d-%H%M%S)}"
if [ -e "$DBB" ]; then
    log "ABORT: DB 備份 $DBB 已經存在，不覆蓋它（可能是上一趟換版前唯一的好備份）"
    release_window "DB 備份檔已存在" || true
    exit 5
fi
# DB 裡有 bot 的 hook token 等憑證：備份只給自己讀（600），不看呼叫端的 umask。
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
# 名單用「id<TAB>名字」：比對看 id（同名的新 bot 蓋不掉舊的那顆），log 印名字。
# 沒有 id 的列退回用名字比（`name:` 開頭，也就不可能被當成刻意刪除）。
# SWAP_T0 是換版窗口的起點，格式跟 daemon 的 db::now() 一樣（RFC3339、毫秒、Z），才能在 SQL 裡直接比字串；
# 取整到秒只會讓窗口往前多算不到一秒。
bot_rows() { set -o pipefail; agm state | "$PYTHON" -c 'import json,sys
print("\n".join(sorted("%s\t%s" % (b.get("id") or "name:%s" % b.get("name"), b.get("name")) for b in json.load(sys.stdin).get("bots") or [])))'; }
SWAP_T0=$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')
if ! BEFORE_ROWS=$(bot_rows) || [ -z "$BEFORE_ROWS" ]; then
    log "ABORT: 換版前 agm state 讀取失敗或 bot 名單為空；不換 binary"
    release_window "換版前 bot 名單不可用" || true
    exit 3
fi

# ── 4. 換 binary 並重啟 ──────────────────────────────────────────────────────
# 啟動一律經過 launchd：pane 忙的時候會被 renice 到 5，子行程繼承後降不回去
# （非 root 不能降 nice）。launchd 跑的啟動器是 nice 0，啟動器 fork+setsid 後結束，
# daemon 就是 ppid=1、nice 0 的獨立行程；job 自己結束後 remove 不會殺到它。
#
# Linux（issue #677）：同一件事交給 systemd user manager（nice 0），`systemd-run --user` 就是
# `launchctl submit` 的對應——一次性的 transient unit，不必另外安裝 unit 檔，AGM_REPO／DAEMON_LOG
# 照樣從這裡傳。兩個屬性缺一不可：
#   - `Type=forking`：啟動器 fork 之後父行程就結束，systemd 把留下來的 daemon 認成 main PID；
#     預設的 simple 會在啟動器一結束就判定 unit 結束。
#   - `KillMode=process`：systemd 收 unit 時看的是整個 cgroup，不是程序群，setsid 脫離不了；
#     預設 control-group 會在 daemon 停下時連它起的子行程一起殺。只殺 main PID 才跟 macOS 一樣。
# unit 名每次不同（`-$START_N`）：往前修／回滾時上一顆的 unit 可能還沒被 `--collect` 收掉。
START_N=0
start() {
    L="am-daemon-swap-$$"
    if [ "$PLATFORM" = linux ]; then
        START_N=$((START_N + 1))
        # 不是從登入 session 叫起來（沒有 pam_systemd）就沒有 XDG_RUNTIME_DIR，systemd-run --user 連不到 user bus。
        [ -n "${XDG_RUNTIME_DIR:-}" ] || export XDG_RUNTIME_DIR="/run/user/$(id -u)"
        # 直譯器寫成絕對路徑：transient unit 的 PATH 是 user manager 的，不是這個 pane 的。
        "$SYSTEMD_RUN" --user --collect --unit="$L-$START_N" -p Type=forking -p KillMode=process \
            -- "$(command -v "$PYTHON" || echo "$PYTHON")" "$START_PY" "$AGM_REPO" "$DLOG" >> "$LOG" 2>&1 || log "WARN: systemd-run rc=${?}（下面的 /api/session 驗證會接手判斷）"
        return
    fi
    "$LAUNCHCTL" remove "$L" 2>/dev/null
    "$LAUNCHCTL" submit -l "$L" -- "$PYTHON" "$START_PY" "$AGM_REPO" "$DLOG"
    sleep 2
    "$LAUNCHCTL" remove "$L" 2>/dev/null
}

# `rollback()` runs before the later top-level child inventory, so define its query helper before any rollback can execute.
rollback_new_children() { # 印出備份後新收編、還活著且不在換版前清單的 child；讀取錯誤交給呼叫端警示
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

rollback() {
    log "ROLLBACK requested: $*"
    # Capture children adopted by the new daemon before restoring the backup. Their panes can outlive
    # the DB restore, so at minimum report the exact rows that will become untracked.
    ROLLBACK_CHILDREN_KNOWN=yes
    if ! ROLLBACK_NEW_CHILDREN=$(rollback_new_children); then
        ROLLBACK_CHILDREN_KNOWN=no
        ROLLBACK_NEW_CHILDREN=""
    fi
    # 升過 schema 就不能把舊 binary 放回去：v(N+1) 開過的 DB，v(N) 的 binary 會被版本閘擋下、
    # 起不來（2026-09-20 實際發生過）。預設往前修，只有新 binary 真的起不來才動 DB。
    if [ "$BUMPED" = yes ]; then
        log "schema bumped $PRE_UV -> ${EXP_UV}：先往前修（舊 binary 開不了這個 DB）"
        stop_daemon "$(dpid)"
        cp "$NEWBIN" target/release/agents-managerd; start; sleep 3
        j=0; while [ $j -lt 30 ]; do api /api/session && break; sleep 1; j=$((j + 1)); done
        if api /api/session; then
            log "forward-fix ok：新 binary 服務中 pid $(dpid)，DB 留在 $EXP_UV 沒有還原"
            exit 6
        fi
        log "forward-fix 失敗：新 binary 起不來，改還原 binary 與 DB"
    fi
    stop_daemon "$(dpid)"
    cp -p "$BAK" target/release/agents-managerd
    # The old daemon cannot load or accept service principals. Remove credentials it generated before
    # the rollback so a stale token file is not mistaken for a working credential.
    rm -f "$AM_DATA/service-tokens/daemon-swap.token" "$AM_DATA/service-tokens/herdr-upgrade.token"
    rmdir "$AM_DATA/service-tokens" 2>/dev/null || true
    # DB 還原：daemon 已停；主檔與 -wal／-shm 要一起處理——新版留下的 WAL 配舊主檔會變成半新半舊。
    # **原子**：先寫同目錄的暫存檔（600）、fsync，再暫存 -wal／-shm，最後 rename 覆蓋主檔；同一檔案系統的 rename 是原子的。
    # 複製或 rename 失敗時原 DB 與 sidecar 都要留住，備份也還在，可以手動還原。
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
    start; sleep 5
    log "rolled back pid $(dpid)"
    exit 7
}

# 換版前先讀一次 supervisor status（issue #771）：額度等待（waiting_quota）是換版前就存在的狀態，跟新 binary 無關。
# 換版後只拿來判斷「這一刻起沒有變壞」：換版前讀不到或不是 waiting_quota，就沒有這個前提，不放寬。
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
PRE_SUP=$(read_supervisor_status 2>/dev/null) || PRE_SUP=""
log "supervisor status before swap: ${PRE_SUP:-讀不到}"

# Check the same-second recovery filename while the old daemon is still available. If this path
# already exists, abort and return the lease before any stop/move can leave the service offline.
PREV="target/release/agents-managerd.prev-$OLD-$(date +%Y%m%d-%H%M%S)"
if [ -e "$PREV" ]; then
    log "ABORT: $PREV 已存在"
    release_window "上一趟的 PREV binary 已存在" || true
    exit 5
fi

OLDPID=$(dpid); log "old pid $OLDPID"
stop_daemon "$OLDPID"
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

# 新 daemon 起來之後再用它自己的 API 複核一次（上面驗的是檔案，這裡驗的是跑起來的那個行程）：
# `agm supervisor` 的 last_deploy.sha_full / dirty。binary 驗過了、起來的卻不是它（啟動器拉起別顆、舊行程沒停掉）也要回滾。
# agm 自己失敗（daemon 讀不到）不在這裡判：下面讀 supervisor status 時同一個失敗會回滾，原因講得更準。
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
# 換版本身成功了，但窗口沒交還就是下一個人拿不到窗口：整輪以 8 結束，不要靜靜走完（issue #477）。
RELEASE_FAILED=0
[ "$HELD_AFTER" = True ] && { release_window "daemon 沒有自動放掉，手動交還" || RELEASE_FAILED=1; }

sleep "$SETTLE"
if ! SUP=$(read_supervisor_status); then rollback "agm supervisor 讀取失敗"; fi
log "supervisor status: $SUP"
case "$SUP" in
    starting|idle|busy) ;;
    waiting_quota)
        # 換版前就是 waiting_quota、換版後仍是：額度等待不是新版造成的（2026-10-02 兩顆無辜 commit 因此被回滾並進了 .rejected）。
        # 換版前健康、換版後才 waiting_quota 仍然回滾。
        if [ "$PRE_SUP" = waiting_quota ]; then
            log "supervisor status waiting_quota：換版前就是 waiting_quota，額度等待與這顆 binary 無關，不回滾"
        else
            rollback "supervisor status 不健康或未知（${SUP:-空}）"
        fi ;;
    *) rollback "supervisor status 不健康或未知（${SUP:-空}）" ;;
esac

if ! AFTER_ROWS=$(bot_rows) || [ -z "$AFTER_ROWS" ]; then
    rollback "新版後 agm state 讀取失敗或 bot 名單為空"
fi
# 窗口內**刻意刪掉**的 bot 不算重啟弄丟的（2026-09-24 22:53 ca9a0330：父 bot 在這 45 秒裡刪了 child i263，
# 整趟被誤判回滾）。「刻意」只認刪除 API 留下的 intent：DELETE /api/bots|projects 在定案前先寫一筆
# delete_bot／delete_project（payload 帶當時的子孫 id），done 之後保留 24 小時。光有 deleted_at 不夠——
# 重啟後 reconcile 找不到 pane 而退役 child、或投影軟刪，也會寫 deleted_at，那正是這一步要抓的遺失。
# 父 bot 用 `herdr pane close` 收 child（不呼叫 DELETE）時，daemon 退役那顆 child 會寫一筆 retire_child 紀錄（#554）；
# 一般只認 subject 就是它、而且 cause 是 pane_closed（herdr 報過關閉事件、當下 pane 也不在）或 promoted 的。
# `unconfirmed`／`parent_replaced_child` 原本只有在父 bot 仍有 active run、且窗口內收編新 child 時才放過。
# 開機 reconcile 的另一種窄例外是：有 parent 的 child 確認 pane 已 gone、父 bot 仍有 active run 且仍在 after 名單；
# 最多重查 5 秒等同一輪 reconcile 的 intent 落地。母 bot 消失、pane 還在的 agent_missing、herdr_restarted 與其他 unconfirmed 仍回滾。
# 判準見 SPEC §6.5a。
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
        AND (json_extract(i.payload_json, '\$.cause') IN ('pane_closed', 'promoted')
          OR (b.managed_by = 'child'
            AND json_extract(i.payload_json, '\$.cause') IN ('unconfirmed', 'parent_replaced_child')
            AND EXISTS (SELECT 1 FROM bots p
              WHERE p.id = json_extract(i.payload_json, '\$.parent_bot_id') AND p.deleted_at IS NULL
                AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = p.id AND r.state IN ('starting', 'running', 'stopping'))
                AND EXISTS (SELECT 1 FROM bots s WHERE s.managed_by = 'child' AND s.deleted_at IS NULL
                  AND s.parent_bot_id = p.id AND s.id != b.id AND s.created_at >= '$SWAP_T0'))))))" 2>/dev/null
}
reconcile_retired_child_parent() { # $1=bot id → 印出經 reconcile 退役、pane 已 gone 且 parent 還 active 的 parent id
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
live_child_parent() { # $1=bot id → 印出 live parent id，供短暫重查 intent
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT p.id FROM bots b JOIN bots p ON p.id = b.parent_bot_id
      WHERE b.id = '$1' AND b.managed_by = 'child' AND p.deleted_at IS NULL LIMIT 1" 2>/dev/null
}
# 這顆 bot 自己的退役／刪除紀錄已經看得到（不管合不合格）。不合格就不必再空等 intent 落地。
retire_record_visible() { # $1=bot id → 印出筆數；讀失敗印空，呼叫端當成「已有紀錄」不再等
    case "$1" in ''|*[!A-Za-z0-9_-]*) return 1 ;; esac
    "$SQLITE" -readonly "$DB" "SELECT count(*) FROM intents i WHERE i.subject_id = '$1'
      AND i.created_at >= '$SWAP_T0' AND i.status != 'abandoned'
      AND i.kind IN ('retire_child', 'delete_bot', 'delete_project')" 2>/dev/null
}
after_has_bot() { # $1=bot id
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
                # 只等「紀錄還沒落地」。agent_missing 這類已經寫明的不合格原因立刻回滾。
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

echo "$SHORT" > "$AGM_DIR/daemon-update.built"
prune_old_db_backups
log "DONE .built=$SHORT pid=$(dpid) db_backup=$DBB"
# 換版成功、但窗口沒交還：下一個人拿不到窗口，要看得出來（issue #477）。
[ "$RELEASE_FAILED" = 0 ] || { log "EXIT 8: 換版成功，但 restart 窗口沒有交還成功（見上面的 rc）"; exit 8; }
