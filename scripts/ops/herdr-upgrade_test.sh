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
trap 'kill "${SRV_PID:-}" 2>/dev/null; rm -rf "$ROOT"' EXIT

# 假 daemon：記下每個請求的 method、path 與身分標頭，/api/supervisor/health 回 200，其他 404。
cat > "$ROOT/server.py" <<'PY'
import http.server, json, sys
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
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(portfile, "w").write(str(s.server_address[1]))
s.serve_forever()
PY
# canary-gap: 假 daemon 只用 http.server 回固定 JSON、寫請求紀錄，沒有任何 subprocess／破壞性指令
python3 "$ROOT/server.py" "$ROOT/requests.log" "$ROOT/port" & SRV_PID=$!
for _ in $(seq 1 50); do [ -s "$ROOT/port" ] && break; sleep 0.1; done
[ -s "$ROOT/port" ] || { echo "FAIL - 假 daemon 沒起來"; exit 1; }

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

echo "herdr-upgrade_test: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" = 0 ]
