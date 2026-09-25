#!/bin/bash
# herdr 0.8.2 → 0.9.0 升級（2026-09-17 k8bw2f 準備；執行要 AGM 開窗口）。
#
# **必須在 herdr 外面跑**：server 一停，herdr pane 裡的行程全部結束，包括執行這支腳本的 pane。
#   launchctl submit -l dev.agents-manager.herdr-upgrade -- /bin/bash ~/.config/agents-manager/ops/herdr-upgrade/herdr-upgrade.sh --go
# 沒帶 --go 就是 dry run：只做前置檢查與快照，不改任何東西。
#
# 每一步寫 logs/upgrade-<時間>.log；結束（成功、失敗、中止）都把結果推給協調者。
#
# 這份是 repo 裡的原始檔；資料目錄那份（~/.config/agents-manager/ops/herdr-upgrade/）是下次維護窗口前
# 從這裡複製過去的副本，不要只改副本。
#
# daemon API 身分（issue #556）：launchd 跑的是獨立的 **service principal** `herdr-upgrade`，不是使用者、
# 也不冒充任何 bot。憑證是 daemon 建的 `<資料目錄>/service-tokens/herdr-upgrade.token`（目錄 0700、檔 0600），
# 只在 python 程式裡讀進 header，不進 argv（`curl -H` 會出現在 `ps`）、環境變數或 log。能打的路徑由 daemon
# 的 `service_auth::allows` 限定：唯讀的 capabilities／supervisor/state／panes／supervisor/health，
# 加上兩支專用路由——`/api/services/herdr-upgrade/resume/{id}`（固定 native resume）與
# `/api/services/herdr-upgrade/notify`（只送協調者、來源固定 daemon）。憑證缺、不安全或回 401／403 就停，
# 不向 daemon 要共用 UI token、也不退回它。
set -u
R="$HOME/.config/agents-manager/ops/herdr-upgrade"
TS=$(date +%Y%m%d-%H%M%S)
LOG="$R/logs/upgrade-$TS.log"
SNAP="$R/logs/snapshot-$TS"
mkdir -p "$R/logs" "$SNAP"
GO=0; [ "${1:-}" = "--go" ] && GO=1
export PATH="/opt/homebrew/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
unset HERDR_SOCKET_PATH HERDR_PANE_ID HERDR_SESSION

AM_DATA="${AM_DATA:-$HOME/.config/agents-manager}"
SERVICE_TOKEN_FILE="$AM_DATA/service-tokens/herdr-upgrade.token"
API_BASE="${HERDR_UPGRADE_API:-http://127.0.0.1:7788}"
OLD_HERDR="$R/rollback/herdr-0.8.2/bin/herdr"
OLD_SHA=3e0f0c2d5edc41f592963ef90f5d872db801cc7dbd0e01731023897ee428904a
SESSIONS="agents-manager am-attach-remote"
HCFG="$HOME/.config/herdr/config.toml"

log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$LOG"; }
# svc <METHOD> <path> [輸出檔] [JSON body 檔]：以 service principal 打 daemon。回應 body 寫到輸出檔（沒給就 stdout），
# stderr 最後一行是 HTTP 狀態碼（連不上是 000）；憑證檔缺或不安全時最後一行是 CRED、exit 3，什麼都不送。
svc() {
  python3 - "$API_BASE" "$SERVICE_TOKEN_FILE" "$@" <<'EOF'
import os, stat, sys, urllib.error, urllib.request
base, token_path, method, path = sys.argv[1:5]
out = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] else None
body_file = sys.argv[6] if len(sys.argv) > 6 else None
def refuse(why):
    print(f"service credential refused: {why}", file=sys.stderr)
    print("CRED", file=sys.stderr)
    sys.exit(3)
parent = os.path.dirname(os.path.abspath(token_path))
try:
    dst = os.lstat(parent)
except OSError as e:
    refuse(f"{parent}: {e}")
if not stat.S_ISDIR(dst.st_mode) or dst.st_uid != os.getuid() or dst.st_mode & 0o077:
    refuse(f"{parent} must be a 0700 directory owned by this user")
try:
    fd = os.open(token_path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
except OSError as e:
    refuse(f"{token_path}: {e}")
try:
    st = os.fstat(fd)
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o077:
        refuse(f"{token_path} must be a 0600 regular file owned by this user")
    raw = os.read(fd, 4097)
finally:
    os.close(fd)
tok = raw.decode("utf-8", "replace").strip() if len(raw) <= 4096 else ""
if not tok or "\n" in tok or "\r" in tok:
    refuse(f"{token_path} must hold one non-empty token line")
headers = {"X-AM-Service-Id": "herdr-upgrade", "X-AM-Service-Token": tok}
data = None
if body_file:
    data = open(body_file, "rb").read()
    headers["Content-Type"] = "application/json"
elif method == "POST":
    data = b""
req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
try:
    with urllib.request.urlopen(req, timeout=20) as r:
        code, payload = r.status, r.read()
except urllib.error.HTTPError as e:
    code, payload = e.code, e.read()
except Exception:
    code, payload = 0, b""
if out:
    open(out, "wb").write(payload)
else:
    sys.stdout.buffer.write(payload)
print(f"{code:03d}", file=sys.stderr)
EOF
}
# svc_code <METHOD> <path> [輸出檔] [body 檔]：只要狀態碼（body 丟掉或寫檔）。
svc_code() { svc "$1" "$2" "${3:-/dev/null}" "${4:-}" 2>&1 >/dev/null | tail -n 1; }
notify() {
  [ -n "${NO_NOTIFY:-}" ] && return 0
  python3 - "$1" > "$SNAP/notify.json" <<'EOF'
import json, sys
print(json.dumps({"text": sys.argv[1]}))
EOF
  svc_code POST /api/services/herdr-upgrade/notify "$SNAP/notify-resp.json" "$SNAP/notify.json" >/dev/null
}
finish() {
  local status=$1; shift
  log "== $status: $*"
  notify "[來自 herdr-upgrade.sh] ${status}：$*（log：${LOG}）"
  exit $([ "$status" = OK ] && echo 0 || echo 1)
}

log "== herdr upgrade $([ $GO = 1 ] && echo GO || echo DRY-RUN)"

# ---------------------------------------------------------------- 1. 前置檢查
[ -x "$OLD_HERDR" ] && [ "$(shasum -a 256 "$OLD_HERDR" | cut -d' ' -f1)" = "$OLD_SHA" ] || finish ABORT "回滾用的 0.8.2 備份不在或雜湊不符"
[ "$(herdr --version 2>/dev/null)" = "herdr 0.8.2" ] || finish ABORT "目前的 herdr 不是 0.8.2：$(herdr --version 2>&1)"
brew info --json=v2 herdr | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["formulae"][0]["versions"]["stable"]=="0.9.0" else 1)' \
  || finish ABORT "brew 的穩定版不是 0.9.0，這支腳本只針對 0.9.0 驗過"
code=$(svc_code GET /api/supervisor/health)
case "$code" in
  200) ;;
  401|403) NO_NOTIFY=1 finish ABORT "herdr-upgrade service credential 被拒（HTTP ${code}）：daemon 要支援 service_principals，且 $SERVICE_TOKEN_FILE 要是它建的那份" ;;
  CRED) NO_NOTIFY=1 finish ABORT "herdr-upgrade service credential 不存在或權限不安全：$SERVICE_TOKEN_FILE" ;;
  *) NO_NOTIFY=1 finish ABORT "daemon 沒回應（HTTP ${code}）" ;;
esac
# resume 需要 daemon 支援（見 RUNBOOK「阻塞」）：沒有就不准往下，免得所有 bot 失去對話。
svc GET /api/capabilities 2>/dev/null | grep -q '"resume_native_start"' \
  || { [ $GO = 1 ] && finish ABORT "daemon 還不支援 start+resume_native（RUNBOOK 阻塞 1），不能升級"; log "WARN daemon 還不支援 start+resume_native（dry run 繼續）"; }
log "preflight ok"

# ---------------------------------------------------------------- 2. 快照（要讓使用者事先知道會關掉什麼）
svc GET /api/supervisor/state "$SNAP/state.json" 2>/dev/null
svc GET /api/panes "$SNAP/panes.json" 2>/dev/null
sqlite3 -readonly "$HOME/.config/agents-manager/agents-manager.sqlite3" \
  "SELECT b.id, b.name, b.kind, b.managed_by, r.state, r.agent_status, r.pane_id, r.native_session_id
     FROM bots b JOIN runs r ON r.bot_id=b.id AND r.state IN ('starting','running','stopping')
    WHERE b.deleted_at IS NULL ORDER BY b.name" > "$SNAP/running.tsv"
python3 - "$SNAP/panes.json" > "$SNAP/non-agent-panes.txt" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1])); d = d.get("panes", d)
for p in d:
    print(f'{p.get("pane_id")}\t{p.get("kind")}\towned_by={p.get("owned_by")}\towner={p.get("owner_bot_id")}\tfg={p.get("foreground")}\tports={p.get("listen_ports")}\tcwd={p.get("cwd")}')
EOF
cp -p "$HCFG" "$SNAP/herdr-config.toml.bak"
log "snapshot: $(wc -l < "$SNAP/running.tsv") running bots, $(wc -l < "$SNAP/non-agent-panes.txt") non-agent panes → $SNAP"
# 還在回合中的 bot 不能停（同 daemon 重啟判準）
BUSY=$(awk -F'|' '$6=="working"{print $2}' "$SNAP/running.tsv" | tr '\n' ' ')
[ -z "$BUSY" ] || finish ABORT "還有 bot 在 working：$BUSY"

[ $GO = 1 ] || finish OK "dry run 完成，沒有改動"

# ---------------------------------------------------------------- 3. 關掉 herdr 自己的 agent 還原
# 實測：0.9.0 重啟後會自己在 pane 打 `claude --resume <id>`，不帶 daemon 的 --settings／--dangerously-skip-permissions／
# 帳號環境，會跟 daemon 的 resume 撞成兩份。交給 daemon 做。
python3 - "$HCFG" <<'EOF'
import re, sys
p = sys.argv[1]; s = open(p).read()
if re.search(r'(?m)^\s*resume_agents_on_restore\s*=', s):
    s = re.sub(r'(?m)^(\s*resume_agents_on_restore\s*=).*$', r'\1 false', s)
elif re.search(r'(?m)^\[session\]\s*$', s):
    s = re.sub(r'(?m)^\[session\]\s*$', '[session]\nresume_agents_on_restore = false', s, count=1)
else:
    s = s.rstrip("\n") + "\n\n[session]\nresume_agents_on_restore = false\n"
open(p, "w").write(s)
EOF
log "herdr config: resume_agents_on_restore = false"

# ---------------------------------------------------------------- 4. 換 binary（保留 0.8.2 keg 供回滾）
HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_AUTO_UPDATE=1 brew upgrade herdr >> "$LOG" 2>&1 || finish FAIL "brew upgrade 失敗（server 還沒動，bot 不受影響）"
[ "$(herdr --version)" = "herdr 0.9.0" ] || finish FAIL "brew 後版本不是 0.9.0"
log "binary: $(herdr --version)"

# ---------------------------------------------------------------- 5. 停 server，launchd（KeepAlive）用新 binary 拉起
# 0.9.0 client 對 0.8.2 server 回 protocol_mismatch，所以停舊 server 要用舊 client。
for S in $SESSIONS; do
  if "$OLD_HERDR" --session "$S" server stop >> "$LOG" 2>&1; then
    log "stopped $S via 0.8.2 client"
  else
    launchctl kickstart -k "gui/$(id -u)/dev.agents-manager.herdr-$S" >> "$LOG" 2>&1 && log "kickstart -k ${S}（socket 不在，直接重啟 launchd job）"
  fi
done
for i in $(seq 1 60); do
  herdr --session agents-manager status server 2>/dev/null | grep -q "version: 0.9.0" && break
  sleep 1
done
herdr --session agents-manager status server 2>/dev/null | grep -q "version: 0.9.0" || finish FAIL "0.9.0 server 60 秒內沒起來——執行 herdr-rollback.sh"
log "server: $(herdr --session agents-manager status server | tr '\n' ' ')"

# ---------------------------------------------------------------- 6. daemon 重新連上（不重啟 daemon，見 RUNBOOK）
for i in $(seq 1 90); do
  svc GET /api/supervisor/health 2>/dev/null | grep -Eq '"daemon": *\{"connected": *true' && break
  sleep 1
done
svc GET /api/supervisor/health 2>/dev/null | grep -Eq '"daemon": *\{"connected": *true' || finish FAIL "daemon 90 秒沒連回 herdr——看 daemon.log，必要時依 RUNBOOK 重啟 daemon"
log "daemon reconnected"

# ---------------------------------------------------------------- 7. 帶 resume 把 bot 接回來
while IFS='|' read -r id name kind managed state status pane sid; do
  [ -n "$id" ] || continue
  code=$(svc_code POST "/api/services/herdr-upgrade/resume/$id" "$SNAP/resume-$id.json")
  log "resume $name ($kind, $managed, sid=${sid:-none}) → HTTP $code $(head -c 160 "$SNAP/resume-$id.json")"
done < "$SNAP/running.tsv"

# ---------------------------------------------------------------- 8. 驗證
sleep 30
svc GET /api/supervisor/state "$SNAP/state-after.json" 2>/dev/null
sqlite3 -readonly "$HOME/.config/agents-manager/agents-manager.sqlite3" \
  "SELECT b.id, b.name, b.kind, b.managed_by, r.state, r.agent_status, r.pane_id, r.native_session_id
     FROM bots b JOIN runs r ON r.bot_id=b.id AND r.state IN ('starting','running','stopping')
    WHERE b.deleted_at IS NULL ORDER BY b.name" > "$SNAP/running-after.tsv"
MISSING=$(comm -23 <(cut -d'|' -f2 "$SNAP/running.tsv" | sort) <(cut -d'|' -f2 "$SNAP/running-after.tsv" | sort) | tr '\n' ' ')
SID_CHANGED=$(join -t'|' -j1 <(awk -F'|' '{print $1"|"$2"|"$8}' "$SNAP/running.tsv" | sort) <(awk -F'|' '{print $1"|"$8}' "$SNAP/running-after.tsv" | sort) \
  | awk -F'|' '$3!="" && $4!="" && $3!=$4 {print $2}' | tr '\n' ' ')
ERRORS=$(awk -v t="$(date -u -v-5M +%FT%T)" '$0 >= t' "$HOME/.config/agents-manager/daemon.log" | sed 's/\x1b\[[0-9;]*m//g' | grep -c ' ERROR ')
log "after: $(wc -l < "$SNAP/running-after.tsv") running; missing: ${MISSING:-none}; session changed: ${SID_CHANGED:-none}; daemon ERROR (5m): $ERRORS"
[ -z "$MISSING" ] || finish FAIL "升級完成但有 bot 沒接回：$MISSING"
finish OK "herdr 0.9.0 上線；running $(wc -l < "$SNAP/running.tsv")→$(wc -l < "$SNAP/running-after.tsv")，session 換掉的：${SID_CHANGED:-無}，daemon ERROR $ERRORS"
