#!/bin/bash
# herdr-upgrade.sh 的 daemon API 身分（issue #556）：launchd 的 service principal，不冒充 bot、不退回 UI token。
# 不跑升級本身（要真的 herdr／brew）；把 `svc` 函式抽出來對一顆假 daemon 打，另外對原始碼做靜態檢查。
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/herdr-upgrade.sh"
PASS=0; FAIL=0
ok() { echo "PASS - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }
check_eq() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1（期望 [$2]，實際 [$3]）"; fi; }
check_no() { if grep -qF -- "$2" "$3"; then bad "$1（不該出現：$2）"; else ok "$1"; fi; }

command -v python3 >/dev/null 2>&1 || { echo "SKIP - 沒有 python3"; exit 0; }

ROOT=$(mktemp -d)
# 收掉自己起的假 daemon 並等它結束（不 wait 的話 bash 會在結尾印一行「Terminated」，看起來像出錯）。
trap '[ -n "${SRV_PID:-}" ] && { kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; }; rm -rf "$ROOT"' EXIT

# 假 daemon：記下每個請求的 method、path 與身分標頭，/api/supervisor/health 回 200，其他 404。
cat > "$ROOT/server.py" <<'PY'
import http.server, json, socketserver, sys
log, portfile = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def handle_one(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode() if n else ""
        with open(log, "a") as f:
            f.write(json.dumps({"method": method, "path": self.path, "body": body,
                "service_id": self.headers.get("X-AM-Service-Id"), "service_token": self.headers.get("X-AM-Service-Token"),
                "ui_token": self.headers.get("X-AM-Token"), "bot_id": self.headers.get("X-AM-Bot-Id")}) + "\n")
        code = 200 if self.path in ("/api/supervisor/health", "/api/services/herdr-upgrade/notify") else 404
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.end_headers()
        self.wfile.write(b'{"ok":true}')
    def do_GET(self): self.handle_one("GET")
    def do_POST(self): self.handle_one("POST")
    def log_message(self, *a): pass
class Server(http.server.HTTPServer):
    # HTTPServer.server_bind 會 socket.getfqdn("127.0.0.1") 反解主機名；CI runner 上反解可能卡好幾秒，
    # port 檔就遲遲寫不出來（#571）。只綁 socket，不反解。
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]
s = Server(("127.0.0.1", 0), H)
open(portfile, "w").write(str(s.server_address[1]))
s.serve_forever()
PY
# canary-gap: 假 daemon 只用 http.server 回固定 JSON、寫請求紀錄，沒有任何 subprocess／破壞性指令
python3 "$ROOT/server.py" "$ROOT/requests.log" "$ROOT/port" 2> "$ROOT/server.err" & SRV_PID=$!
# 等到 port 真的連得上才往下：總時限 30 秒，假 daemon 中途死掉就不必再等。
up=""
deadline=$((SECONDS + 30))
while [ "$SECONDS" -lt "$deadline" ]; do
  kill -0 "$SRV_PID" 2>/dev/null || break
  if [ -s "$ROOT/port" ] && (exec 3<>"/dev/tcp/127.0.0.1/$(cat "$ROOT/port")") 2>/dev/null; then up=1; break; fi
  sleep 0.1
done
[ -n "$up" ] || { echo "FAIL - 假 daemon 30 秒內連不上"; sed 's/^/      /' "$ROOT/server.err"; exit 1; }

# issue #677：只適用 macOS。只跑開頭到 `R=` 之前那段（守衛就在裡面），平台由假 uname 決定——
# 不執行升級流程本身；守衛被拿掉時這段只會正常結束，測試就看得出來。
sed -n '1,/^R=/p' "$SCRIPT" | sed '$d' > "$ROOT/guard.sh"
mkdir -p "$ROOT/fakebin"; printf '#!/bin/sh\necho "$AM_FAKE_UNAME"\n' > "$ROOT/fakebin/uname"; chmod 755 "$ROOT/fakebin/uname"
guard_rc() { AM_FAKE_UNAME="$1" PATH="$ROOT/fakebin:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" bash "$ROOT/guard.sh" 2>"$ROOT/guard.err"; echo $?; }
check_eq "Linux 上一開始就拒絕（exit 2）" "2" "$(guard_rc Linux)"
if grep -q "只適用 macOS" "$ROOT/guard.err"; then ok "拒絕時說明原因"; else bad "拒絕時說明原因（stderr：$(cat "$ROOT/guard.err")）"; fi
check_eq "macOS 照常往下（守衛放行）" "0" "$(guard_rc Darwin)"

# 從腳本抽出 svc／svc_code（到下一個頂層 `}` 為止），不執行升級流程本身。
sed -n '/^svc() {$/,/^}$/p; /^svc_code() /p' "$SCRIPT" > "$ROOT/svc.sh"
grep -q '^svc() {' "$ROOT/svc.sh" || { echo "FAIL - 抽不到 svc()"; exit 1; }
# shellcheck disable=SC1091
. "$ROOT/svc.sh"
API_BASE="http://127.0.0.1:$(cat "$ROOT/port")"

fresh_creds() { # fresh_creds <dir mode> <file mode>
  rm -rf "$ROOT/data"; mkdir -p "$ROOT/data/service-tokens"
  printf 'svc-test-token\n' > "$ROOT/data/service-tokens/herdr-upgrade.token"
  chmod "$1" "$ROOT/data/service-tokens"; chmod "$2" "$ROOT/data/service-tokens/herdr-upgrade.token"
  SERVICE_TOKEN_FILE="$ROOT/data/service-tokens/herdr-upgrade.token"
  : > "$ROOT/requests.log"
}

# 1. 正常：帶 service 身分、不帶 UI token／bot 身分；token 不在 argv（python 讀檔）。
fresh_creds 700 600
check_eq "health 回 200" "200" "$(svc_code GET /api/supervisor/health)"
last=$(tail -n 1 "$ROOT/requests.log")
check_eq "帶 service id" "herdr-upgrade" "$(printf '%s' "$last" | python3 -c 'import json,sys; print(json.load(sys.stdin)["service_id"])')"
check_eq "帶 service token" "svc-test-token" "$(printf '%s' "$last" | python3 -c 'import json,sys; print(json.load(sys.stdin)["service_token"])')"
check_eq "不帶 UI token" "None" "$(printf '%s' "$last" | python3 -c 'import json,sys; print(json.load(sys.stdin)["ui_token"])')"
check_eq "不帶 bot 身分" "None" "$(printf '%s' "$last" | python3 -c 'import json,sys; print(json.load(sys.stdin)["bot_id"])')"

# 2. notify 只送 text，不自己宣告來源。
printf '{"text":"hi"}' > "$ROOT/notify.json"
check_eq "notify 回 200" "200" "$(svc_code POST /api/services/herdr-upgrade/notify "$ROOT/resp.json" "$ROOT/notify.json")"
check_no "notify body 不帶 relay_from" "relay_from" "$ROOT/requests.log"

# 3. 憑證不安全：一個請求都不送。
for case in "file 644:700 644" "dir 755:755 600"; do
  name=${case%%:*}; modes=${case#*:}
  fresh_creds ${modes}
  check_eq "憑證 ${name} → CRED" "CRED" "$(svc_code GET /api/supervisor/health)"
  check_eq "憑證 ${name} → 沒有送出請求" "0" "$(wc -l < "$ROOT/requests.log" | tr -d ' ')"
done
fresh_creds 700 600
mv "$SERVICE_TOKEN_FILE" "$ROOT/data/real.token"; ln -s "$ROOT/data/real.token" "$SERVICE_TOKEN_FILE"
check_eq "symlink 憑證 → CRED" "CRED" "$(svc_code GET /api/supervisor/health)"
rm -f "$SERVICE_TOKEN_FILE"
check_eq "沒有憑證 → CRED" "CRED" "$(svc_code GET /api/supervisor/health)"
check_eq "沒有憑證 → 沒有送出請求" "0" "$(wc -l < "$ROOT/requests.log" | tr -d ' ')"

# 4. 原始碼：不再碰共用 UI token、不冒充 bot、不走通用 bot 控制路由、token 不經 curl argv。
check_no "不讀 ui-token 檔" "ui-token" "$SCRIPT"
check_no "不取 /api/session" "/api/session" "$SCRIPT"
check_no "不送 X-AM-Token" "X-AM-Token" "$SCRIPT"
check_no "不自稱 relay_from" '"relay_from"' "$SCRIPT"
check_no "不打通用 prompt" "/prompt" "$SCRIPT"
check_no "不打通用 start" "start?resume" "$SCRIPT"
check_no "不讀含 env 的 /api/state" '/api/state"' "$SCRIPT"
check_no "service token 不放 curl 參數" "curl -s -m 20 -H \"X-AM-Service-Token" "$SCRIPT"

# 重啟前所有 active run 都要避開 working、blocked 與回合狀態仍 in_flight 的 bot。
# 後者可能在 agent_status 更新前短暫仍顯示 idle；只看 status 會把回合中途的 pane 一起砍掉。
if grep -q '^busy_bots() {' "$SCRIPT"; then
  sed -n '/^busy_bots() {$/,/^}$/p' "$SCRIPT" > "$ROOT/busy.sh"
  printf 'w|worker|claude|user|running|working|p1|s1|0\nb|blocked|claude|user|running|blocked|p2|s2|0\ni|inflight|claude|user|running|idle|p3|s3|1\ni0|idle|claude|user|running|idle|p4|s4|0\n' > "$ROOT/running.tsv"
  got=$(bash -c '. "$1"; busy_bots "$2"' _ "$ROOT/busy.sh" "$ROOT/running.tsv" | sed 's/[[:space:]]*$//')
  check_eq "working、blocked、in-flight 都列為 busy，idle 不列" "worker blocked inflight" "$got"
else
  bad "有可測的 busy_bots 判斷（需涵蓋 working／blocked／in-flight）"
fi
if grep -q "FROM turns" "$SCRIPT" && grep -q 'busy_bots.*running.tsv' "$SCRIPT"; then
  ok "active-run snapshot 帶入 in-flight turn 並交給 busy 判斷"
else
  bad "active-run snapshot 帶入 in-flight turn 並交給 busy 判斷"
fi

# sqlite 讀快照失敗時不能因 set -u 以外沒有 set -e 而把空檔當「沒有 bot」繼續升級。
if grep -q '^read_running_snapshot() {' "$SCRIPT"; then
  sed -n '/^read_running_snapshot() {$/,/^}$/p' "$SCRIPT" > "$ROOT/read-snapshot.sh"
  mkdir -p "$ROOT/sqlitebin"
  cat > "$ROOT/sqlitebin/sqlite3" <<'STUB'
#!/bin/sh
[ -n "${STUB_SQLITE_FAIL:-}" ] && exit 19
printf 'snapshot-row\n'
STUB
  chmod +x "$ROOT/sqlitebin/sqlite3"
  check_eq "快照命令錯誤能傳回非零" "19" "$(PATH="$ROOT/sqlitebin:$PATH" STUB_SQLITE_FAIL=1 bash -c '. "$1"; read_running_snapshot db' _ "$ROOT/read-snapshot.sh" >/dev/null; echo $?)"
  check_eq "快照命令成功保留內容" "snapshot-row" "$(PATH="$ROOT/sqlitebin:$PATH" bash -c '. "$1"; read_running_snapshot db' _ "$ROOT/read-snapshot.sh")"
else
  bad "running snapshot 有可測的 sqlite 包裝（失敗不得當成空名單）"
fi
if grep -q 'if ! read_running_snapshot ' "$SCRIPT"; then
  ok "sqlite 快照失敗會中止升級"
else
  bad "sqlite 快照失敗會中止升級"
fi

# 失敗／成功都會呼叫 notify；通知 endpoint 不可用時，至少要把送達失敗留在本機升級 log。
sed -n '/^notify() {$/,/^}$/p' "$SCRIPT" > "$ROOT/notify.sh"
mkdir -p "$ROOT/notify-snap"
NOTIFY_LOG="$ROOT/notify.log" STUB_NOTIFY_CODE=503 bash -c '
  . "$1"
  SNAP="$2"; API_BASE=http://unused; SERVICE_TOKEN_FILE=unused
  log() { printf "%s\n" "$*" >> "$NOTIFY_LOG"; }
  svc_code() { printf "%s\n" "$STUB_NOTIFY_CODE"; }
  notify test-notification
' _ "$ROOT/notify.sh" "$ROOT/notify-snap"
if grep -q 'notify.*503\|503.*notify' "$ROOT/notify.log" 2>/dev/null; then
  ok "notify 非 2xx 會記錄送達失敗"
else
  bad "notify 非 2xx 會記錄送達失敗"
fi

# #671：行首 ANSI 不能讓 5 分鐘內的 ERROR 被算成 0。先去色再比時間。
LOGF="$ROOT/daemon.log"
CUTOFF="2026-09-27T00:05:00"
{
  printf '\033[2m2026-09-27T00:06:00Z\033[0m ERROR boom\n'
  printf '2026-09-27T00:06:01Z \033[31mERROR\033[0m plain-marker\n'
  printf '2026-09-27T00:01:00Z ERROR too-old\n'
} > "$LOGF"
# 第二行色碼包住 ERROR、前後沒有空格時，比對用的是「 ERROR 」這個帶空格的片段；補上空格才算同一種 log。
# 上面第二行是 `Z \033[31mERROR\033[0m`，去掉色碼後是 `Z ERROR `。
awk '/^count_daemon_errors\(\)/,/^}/' "$SCRIPT" > "$ROOT/count.sh"
printf '\ncount_daemon_errors "$1" "$2"\n' >> "$ROOT/count.sh"
got=$(bash "$ROOT/count.sh" "$LOGF" "$CUTOFF")
check_eq "ANSI 行與純文字行都算，太舊的不算" "2" "$got"

# BSD（macOS）的 `wc -l` 會在數字前面補空白（"       3"）：log 與通知字串裡的行數要去掉，不然變成「      3 running」。
# 抽出 count_lines()，PATH 前面放一個照 BSD 補空白的假 wc。
sed -n '/^count_lines() {$/,/^}$/p' "$SCRIPT" > "$ROOT/count_lines.sh"
if grep -q '^count_lines() {' "$ROOT/count_lines.sh"; then ok "腳本有 count_lines()"; else bad "腳本沒有 count_lines()（行數字串沒處理 BSD 的前導空白）"; fi
mkdir -p "$ROOT/bsdbin"
printf '#!/bin/sh\nn=$(/usr/bin/wc "$@" | tr -d " ")\nprintf "       %%s\\n" "$n"\n' > "$ROOT/bsdbin/wc"; chmod 755 "$ROOT/bsdbin/wc"
printf 'a\nb\nc\n' > "$ROOT/three.txt"
got=$(PATH="$ROOT/bsdbin:$PATH" bash -c '. "$1"; count_lines "$2"' _ "$ROOT/count_lines.sh" "$ROOT/three.txt" 2>/dev/null)
check_eq "BSD 風格的 wc 輸出被去掉前導空白" "3" "$got"
check_eq "假 wc 真的會補空白（測試有效）" "       3" "$(PATH="$ROOT/bsdbin:$PATH" wc -l < "$ROOT/three.txt")"
# 腳本裡不再有直接把 wc -l 塞進字串的地方。
if grep -n 'wc -l' "$SCRIPT" | grep -v 'count_lines\|tr -d' | grep -q .; then bad "還有沒處理前導空白的 wc -l"; else ok "沒有裸的 wc -l 進字串"; fi

# resume 非 2xx 必須讓整支腳本失敗。只寫 log 再 finish OK 時，DB 裡還標 running 的 bot 會被當成已經接回。
if grep -A 12 'POST "/api/services/herdr-upgrade/resume/' "$SCRIPT" | grep -q 'resume_fail=1' \
  && grep -q 'resume 有呼叫失敗' "$SCRIPT"; then
  ok "resume 非 2xx 會讓升級失敗"
else
  bad "resume 非 2xx 會讓升級失敗"
fi

echo "herdr-upgrade_test: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" = 0 ]
