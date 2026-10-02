#!/bin/bash
# cutover-to-host.sh 的隔離測試（issue #721）。
#
# 不碰正式 daemon／DB／agm-host：兩顆假 daemon（python http.server，各自一份 state、記下每個寫入請求）、
# 假 ssh（把「遠端指令」在本機的暫存目錄裡跑，PATH 前面放 ss／systemd-run／curl 的 stub）、
# 假 rsync／launchctl／lsof／project-transfer／transcript-transfer，daemon 行程是改名成 agents-managerd 的 sleep。
# 從各自的 log 檢查腳本做了什麼、順序對不對。
#
#   bash scripts/ops/cutover-to-host_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/cutover-to-host.sh"
HELPER="$HERE/cutover-helper.py"
PASS=0
FAIL=0

check() { # check <描述> <要出現的字串> <檔案>
    if grep -q -- "$2" "$3" 2>/dev/null; then
        echo "ok   - $1"; PASS=$((PASS + 1))
    else
        echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | tail -40; FAIL=$((FAIL + 1))
    fi
}
check_no() {
    if grep -q -- "$2" "$3" 2>/dev/null; then
        echo "FAIL - $1"; echo "      不該出現 '$2'："; grep -n -- "$2" "$3" | sed 's/^/      /'; FAIL=$((FAIL + 1))
    else
        echo "ok   - $1"; PASS=$((PASS + 1))
    fi
}
check_eq() {
    if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1)); else echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1)); fi
}
line_of() { grep -n -- "$1" "$2" | head -1 | cut -d: -f1; }
check_before() { # check_before <描述> <先出現的> <後出現的> <檔案>
    local a b; a=$(line_of "$2" "$4"); b=$(line_of "$3" "$4")
    if [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]; then
        echo "ok   - $1"; PASS=$((PASS + 1))
    else
        echo "FAIL - $1（'$2' 在第 ${a:-無} 行，'$3' 在第 ${b:-無} 行）"; sed 's/^/      /' "$4" | tail -40; FAIL=$((FAIL + 1))
    fi
}

PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$HERE/host-state-transfer_test.py"

# ---------------------------------------------------------------- 假 daemon

FAKE_DAEMON='
import http.server, json, os, re, sys, threading
state_path, log_path, port_path = sys.argv[1:4]
lock = threading.Lock()
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        raw = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw))); self.end_headers(); self.wfile.write(raw)
    def do_GET(self):
        if self.path == "/api/session":
            return self._send(200, {"token": "x"})
        if self.path == "/api/state":
            with lock, open(state_path) as f:
                return self._send(200, json.load(f))
        self._send(404, {"error": "nope"})
    def _write(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode() if n else ""
        with lock:
            with open(log_path, "a") as f:
                tok = self.headers.get("X-AM-Token")
                f.write(f"{self.command} {self.path} {body} tok={tok}\n")
            m = re.fullmatch(r"/api/bots/([^/]+)/stop", self.path)
            nostop = open(state_path + ".nostop").read().split() if os.path.exists(state_path + ".nostop") else []
            if m and m.group(1) not in nostop:
                st = json.load(open(state_path))
                for p in st["projects"]:
                    for b in p["bots"]:
                        if b["id"] == m.group(1):
                            b["run"] = None
                json.dump(st, open(state_path, "w"))
        if os.path.exists(state_path + ".fail") and self.path in open(state_path + ".fail").read().split():
            return self._send(500, {"error": "boom"})
        if "/start" in self.path and os.path.exists(state_path + ".409"):
            return self._send(409, {"error": "already running"})
        self._send(200, {})
    do_POST = do_PATCH = do_PUT = _write
class S(http.server.ThreadingHTTPServer):
    def server_bind(self):  # 不做 getfqdn 反解（CI 上會卡 35 秒）
        import socketserver
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "127.0.0.1", self.server_address[1]
s = S(("127.0.0.1", 0), H)
open(port_path + ".tmp", "w").write(str(s.server_address[1])); os.replace(port_path + ".tmp", port_path)
s.serve_forever()
'

start_daemon() { # start_daemon <名字>：起假 daemon，port 寫在 $ROOT/<名字>.port
    # stdout 也要接走：這個函式在 $(…) 裡叫，背景行程握著那條 pipe 的話 $(…) 會等到它結束。
    python3 -c "$FAKE_DAEMON" "$ROOT/$1-state.json" "$ROOT/$1-api.log" "$ROOT/$1.port" >/dev/null 2>>"$ROOT/$1-daemon.err" &
    PIDS="$PIDS $!"
    local i
    for i in $(seq 100); do [ -s "$ROOT/$1.port" ] && break; sleep 0.05; done
}

# state：Agents Manager（P1 在跑、P2 沒跑、P1 的 child C1 在跑）＋ AGM-DM-GRUP（AGM、巡檢、browser-gc 在跑，build 沒跑）。
write_state() { # write_state <檔> <src|dst>
    python3 - "$1" "$2" "$SRC_REPO" "$DST_REPO" "$SRC_DATA" "$DST_DATA" <<'EOF'
import json, sys
out, side, src_repo, dst_repo, src_data, dst_data = sys.argv[1:7]
dst = side == "dst"
run = {"state": "running"}
def bot(i, name, managed="user", parent=None, running=False):
    return {"id": i, "name": name, "kind": "claude", "managed_by": managed, "parent_bot_id": parent, "cwd": None,
            "run": dict(run) if running else None}
am = {"id": "PAM", "label": "Agents Manager", "host": "local", "handed_off_to": None,
      "path": dst_repo if dst else src_repo,
      "bots": [bot("P1", "AM-1", running=True), bot("P2", "AM-2"), bot("C1", "c1", "child", "P1", running=not dst)]}
agm = {"id": "PAGM", "label": "AGM-DM-GRUP", "host": "local", "handed_off_to": None,
       "path": (dst_data if dst else src_data) + "/supervisor/AGM",
       "bots": [bot("A1", "AGM", running=True), bot("A2", "AGM-responder", running=True),
                bot("A3", "agm-pxf2pv-browser-gc", running=not dst), bot("A4", "build")]}
other = {"id": "PHUB", "label": "pt-hub", "host": "local", "handed_off_to": "agm-host", "path": "/x",
         "bots": [bot("H1", "hub", running=True)]}
json.dump({"projects": [am, agm, other]}, open(out, "w"))
EOF
}

# 「daemon 行程」在子 shell 裡起：父行程馬上結束，它被 init 收養，被殺之後不會變成測試 shell 的殭屍
# （殭屍 kill -0 仍然成功，lsof／ss stub 會以為它還在聽）。
spawn_daemon() {
    ( "$ROOT/bin/agents-managerd" 300 > /dev/null 2>&1 & echo $! )
}

setup() {
    ROOT=$(mktemp -d); export ROOT
    PIDS=""
    export HOME="$ROOT/home"
    SRC_REPO="$ROOT/src-repo"; DST_REPO="$ROOT/dst-repo"; SRC_DATA="$ROOT/src-data"; DST_DATA="$ROOT/dst-data"
    mkdir -p "$HOME/Library/LaunchAgents" "$SRC_REPO" "$DST_REPO/target/release" "$DST_REPO/scripts/ops" \
        "$SRC_DATA/supervisor/AGM" "$SRC_DATA/outbox/AM" "$DST_DATA/outbox" "$DST_DATA" "$ROOT/bin" "$ROOT/rbin"
    echo tok-src > "$SRC_DATA/ui-token"; echo tok-dst > "$DST_DATA/ui-token"
    echo migrated-report > "$SRC_DATA/outbox/AM/report.txt"
    echo "x" > "$SRC_DATA/supervisor/AGM/handoff.md"
    : > "$DST_DATA/agents-manager.sqlite3"; echo '[server]' > "$DST_DATA/config.toml"
    : > "$DST_DATA/daemon.lock"
    printf '[server]\n[judge]\nenabled = true\n' > "$SRC_DATA/config.toml"
    printf 'line\n' > "$DST_DATA/daemon.log"
    : > "$DST_REPO/target/release/agents-managerd"; chmod +x "$DST_REPO/target/release/agents-managerd"
    : > "$DST_REPO/scripts/ops/daemon-start.py"
    for l in com.agm.dev-server com.agm.daemon-update com.agm.not-loaded; do : > "$HOME/Library/LaunchAgents/$l.plist"; done
    printf 'com.agm.dev-server\ncom.agm.daemon-update\n' > "$ROOT/launchd-loaded"
    write_state "$ROOT/src-state.json" src
    write_state "$ROOT/dst-state.json" dst
    start_daemon src; start_daemon dst
    SRC_PORT=$(cat "$ROOT/src.port"); DST_PORT=$(cat "$ROOT/dst.port")

    # 兩顆「daemon 行程」：改名的 sleep，ps 的 comm 會是 agents-managerd。
    cp /bin/sleep "$ROOT/bin/agents-managerd"
    # macOS 複製出來的系統 binary 簽章對不上會被 SIGKILL，重簽 ad-hoc。
    command -v codesign > /dev/null && codesign -s - -f "$ROOT/bin/agents-managerd" 2>/dev/null
    SRC_PID=$(spawn_daemon); DST_PID=$(spawn_daemon)
    export SRC_PID DST_PID

    cat > "$ROOT/bin/ssh" <<'EOF'
#!/bin/bash
# 假 ssh：最後一個參數是遠端指令，在本機跑（PATH 前面是遠端才有的 stub）。
args=("$@"); last="${args[$((${#args[@]} - 1))]}"
printf '%s\n' "$last" >> "$ROOT/ssh.log"
# check.sh 的 destructive-canary 把 systemd-run／systemctl 匯出成 bash 函式（優先於 PATH），rbin 的 stub 會被蓋掉；
# 這裡拿掉函式，PATH 上的 canary shim 仍在（rbin 沒 stub 的照樣擋）。
unset -f systemd-run systemctl 2>/dev/null
PATH="$ROOT/rbin:$PATH" exec bash -c "$last"
EOF
    cat > "$ROOT/bin/rsync" <<'EOF'
#!/bin/bash
echo "rsync $*" >> "$ROOT/rsync.log"
srcs=(); while [ $# -gt 0 ]; do case "$1" in -a) ;; --exclude) shift ;; *) srcs+=("$1") ;; esac; shift; done
n=${#srcs[@]}; dest="${srcs[$((n - 1))]}"; dest="${dest#*:}"; unset "srcs[$((n - 1))]"
# 跟真 rsync 一樣：目的地不以 / 結尾、來源只有一個檔＝複製成那個名字。
if [ "${#srcs[@]}" = 1 ] && [ -f "${srcs[0]}" ] && [ "${dest%/}" = "$dest" ]; then
    mkdir -p "$(dirname "$dest")"; cp "${srcs[0]}" "$dest"; exit 0
fi
mkdir -p "$dest"
for s in "${srcs[@]}"; do case "$s" in */) cp -R "$s." "$dest/" ;; *) cp -R "$s" "$dest/" ;; esac; done
EOF
    cat > "$ROOT/bin/launchctl" <<'EOF'
#!/bin/bash
echo "launchctl $*" >> "$ROOT/launchctl.log"
[ "$1" = list ] && { grep -qx "$2" "$ROOT/launchd-loaded"; exit $?; }
exit 0
EOF
    cat > "$ROOT/bin/lsof" <<'EOF'
#!/bin/bash
kill -0 "$SRC_PID" 2>/dev/null && echo "$SRC_PID"
exit 0
EOF
    cat > "$ROOT/bin/curl" <<'EOF'
#!/bin/bash
echo "curl $*" >> "$ROOT/curl.log"; exit 0
EOF
    cat > "$ROOT/rbin/ss" <<'EOF'
#!/bin/bash
kill -0 "$DST_PID" 2>/dev/null && echo "LISTEN 0 128 0.0.0.0:7788 0.0.0.0:* users:((\"agents-managerd\",pid=$DST_PID,fd=45))"
exit 0
EOF
    cat > "$ROOT/rbin/systemd-run" <<'EOF'
#!/bin/bash
echo "systemd-run $*" >> "$ROOT/systemd-run.log"
[ -n "${DST_ERROR_AFTER_START:-}" ] && printf '2026-09-28T00:00:00Z ERROR boom\n' >> "$DST_DATA_T/daemon.log"
exit 0
EOF
    # preflight 只用 `systemctl --user cat herdr@.service` 看有沒有裝，當作有裝。
    cat > "$ROOT/rbin/systemctl" <<'EOF'
#!/bin/bash
echo "systemctl $*" >> "$ROOT/systemctl.log"; exit 0
EOF
    cp "$ROOT/bin/curl" "$ROOT/rbin/curl"
    # stub 故意不叫 project-transfer：傳到目標要改成固定檔名，import 才叫得到。
    cat > "$ROOT/bin/pt-stub" <<'EOF'
#!/usr/bin/env python3
import json, os, sys
root = os.environ["ROOT"]
with open(os.path.join(root, "pt.log"), "a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
a = sys.argv[1:]
if "--help" in a:
    print("usage: export ... --with-supervisor" if not os.environ.get("PT_OLD") else "usage: export ...")
    sys.exit(0)
if a[0] == "export":
    import gzip
    pid = a[a.index("--project") + 1]
    bots = {"PAM": ["P1", "P2", "C1"], "PAGM": ["A1", "A2", "A3", "A4"]}[pid]
    runs = {"PAM": [("P1", "s-old", "1"), ("P1", "s-new", "2")], "PAGM": [("A1", "s-agm", "1")]}[pid]
    bundle = {"tables": {"bots": {"rows": [{"id": b, "name": {"P1": "AM-1"}.get(b, b)} for b in bots]},
                         "runs": {"rows": [{"bot_id": b, "native_session_id": s, "started_at": t} for b, s, t in runs]}}}
    with gzip.open(a[a.index("--out") + 1], "wb") as f:
        f.write(json.dumps(bundle).encode())
    print(json.dumps({"counts": {}, "missing_files": []}))
    sys.exit(0)
dry = "--dry-run" in a
if not dry:
    with open(os.path.join(root, "import-config.log"), "a") as f:
        f.write(open(a[a.index("--config") + 1]).read())
n_gone = os.environ.get("PT_WARN_TRANSCRIPT") if "PAM-moved" in a[a.index("--bundle") + 1] else None
warn = [f"{n_gone} 段原生對話的 transcript 不在這台（例：/x）"] if dry and n_gone else []
cfg = a[a.index("--config") + 1]
n = "1" if "PAM-moved" in a[a.index("--bundle") + 1] else "2"   # 每次 import 各有一份備份：1＝第一次（切換前）
print(json.dumps({"dry_run": dry, "backups": [] if dry else [cfg.replace("config.toml", "agents-manager.sqlite3") + ".pre-transfer-" + n, cfg + ".pre-transfer-" + n],
                  "warnings": warn}))
EOF
    cat > "$ROOT/bin/transcript-transfer" <<'EOF'
#!/usr/bin/env python3
import json, os, shutil, sys
a = sys.argv[1:]
with open(os.path.join(os.environ["ROOT"], "tt.log"), "a") as f:
    f.write(" ".join(a) + "\n")
import gzip
src = a[a.index("--bundle") + 1]
shutil.copy(src, a[a.index("--out") + 1])
have = {r["native_session_id"] for r in json.loads(gzip.open(src).read())["tables"]["runs"]["rows"]}
missing = [{"kind": "claude", "session": x, "candidates": []} for x in os.environ.get("TT_MISSING", "").split() if x in have]
conflicts = [".claude/projects/k/s-new.jsonl"] if os.environ.get("TT_CONFLICT") and "s-new" in have else []
print(json.dumps({"sessions": [], "files_written": 0, "missing": missing, "refused": [], "conflicts": conflicts}))
if os.environ.get("TT_RC"):
    sys.exit(int(os.environ["TT_RC"]))
sys.exit(2 if missing or conflicts else 0)
EOF
    chmod +x "$ROOT"/bin/* "$ROOT"/rbin/*
    export SSH_BIN="$ROOT/bin/ssh" RSYNC_BIN="$ROOT/bin/rsync" LAUNCHCTL_BIN="$ROOT/bin/launchctl" LSOF_BIN="$ROOT/bin/lsof" CURL_BIN="$ROOT/bin/curl"
    export PROJECT_TRANSFER="$ROOT/bin/pt-stub" TRANSCRIPT_TRANSFER="$ROOT/bin/transcript-transfer"
    export CUTOVER_TARGET=fake@target CUTOVER_SRC_REPO="$SRC_REPO" CUTOVER_DST_REPO="$DST_REPO"
    export CUTOVER_SRC_DATA="$SRC_DATA" CUTOVER_DST_DATA="$DST_DATA" DST_DATA_T="$DST_DATA"
    export CUTOVER_SRC_API="http://127.0.0.1:$SRC_PORT" CUTOVER_DST_API="http://127.0.0.1:$DST_PORT"
    export CUTOVER_DAEMON_WAIT_SECS=5 CUTOVER_STOP_WAIT_SECS=10
    unset CUTOVER_DETACHED PT_OLD PT_WARN_TRANSCRIPT TT_RC TT_MISSING TT_CONFLICT DST_ERROR_AFTER_START
}

teardown() {
    local p
    for p in $PIDS $SRC_PID $DST_PID; do kill "$p" 2>/dev/null; done
    wait 2>/dev/null
    rm -rf "$ROOT"
}

run() { # run <輸出檔> <參數…>
    local out="$1"; shift
    bash "$SCRIPT" "$@" > "$out" 2>&1
    echo $? > "$out.rc"
}

# ---------------------------------------------------------------- dry-run 一個字都不寫

echo "# dry-run：只讀、只印"
setup
run "$ROOT/out" cutover
check_eq "dry-run 結束碼 0" 0 "$(cat "$ROOT/out.rc")"
check_no "來源 daemon 沒收到任何寫入" "POST\|PATCH" "$ROOT/src-api.log"
check_no "目標 daemon 沒收到任何寫入" "POST\|PATCH" "$ROOT/dst-api.log"
check_no "沒 bootout launchd" "bootout" "$ROOT/launchctl.log"
check_no "沒在目標起 daemon" "systemd-run --user" "$ROOT/ssh.log"
check_no "沒在目標 import" "import --bundle" "$ROOT/ssh.log"
check_no "沒有 export" "export --db" "$ROOT/pt.log"
check_no "沒搬對話" "--bundle" "$ROOT/tt.log"
kill -0 "$SRC_PID" 2>/dev/null; check_eq "來源 daemon 行程還活著" 0 $?
kill -0 "$DST_PID" 2>/dev/null; check_eq "目標 daemon 行程還活著" 0 $?
check "dry-run 列出會停的 bot" "\[dry-run\] shelper api POST /api/bots/C1/stop" "$ROOT/out"
check "dry-run 列出目標接回" "start?resume=native" "$ROOT/out"
check "在跑名單：接回 user bot，不含 browser-gc" "接回 3 顆：AM-1, AGM, AGM-responder" "$ROOT/out"
check "在跑的 child 列出來" "在跑的 child 1 顆" "$ROOT/out"
check "已移交的 hub 不在名單" "Agents Manager（PAM" "$ROOT/out"
check "dry-run 列出完整設定移交" "host-state-transfer.py" "$ROOT/out"
check_no "不再要求手動補來源 config 段落" "切換後在目標補上" "$ROOT/out"
check_no "已移交的 hub 不碰" "H1" "$ROOT/out"
teardown

# ---------------------------------------------------------------- 完整切換

echo "# --execute：完整切換的順序與內容"
setup
run "$ROOT/out" cutover --execute --foreground
check_eq "切換結束碼 0" 0 "$(cat "$ROOT/out.rc")"
S="$ROOT/src-api.log"
check_before "協調者先標不要跑，才停 bot" "POST /api/supervisor/stop" "POST /api/bots/" "$S"
check_before "巡檢也先標不要跑" "POST /api/supervisor/responder/stop" "POST /api/bots/" "$S"
check_before "child 先停，再停它的 parent" "POST /api/bots/C1/stop" "POST /api/bots/P1/stop" "$S"
check "停在跑的 browser-gc（Mac 上不能留著）" "POST /api/bots/A3/stop" "$S"
check_no "沒在跑的不停" "POST /api/bots/P2/stop" "$S"
check_no "已移交的 hub 不停" "/api/bots/H1" "$S"
check_before "bot 都停了才設 handed_off_to" "POST /api/bots/P1/stop" "PATCH /api/projects/PAM" "$S"
check "handed_off_to 設成目標主機" '"handed_off_to": "agm-host"' "$S"
check "兩個專案都設" "PATCH /api/projects/PAGM" "$S"
check "UI token 用來源的" "tok=tok-src" "$S"
check "launchd 的 com.agm.* 有載入的才 bootout" "bootout gui/$(id -u)/com.agm.dev-server" "$ROOT/launchctl.log"
check_no "沒載入的不 bootout" "bootout gui/$(id -u)/com.agm.not-loaded" "$ROOT/launchctl.log"
kill -0 "$SRC_PID" 2>/dev/null; check_eq "來源 daemon 停了" 1 $?
check "只有協調者專案 export 帶 --with-supervisor" "project PAGM .*--with-supervisor" "$ROOT/pt.log"
check_no "Agents Manager 不帶 --with-supervisor" "project PAM .*--with-supervisor" "$ROOT/pt.log"
check "對話搬移的 map 含 repo" "--map $SRC_REPO=$DST_REPO" "$ROOT/tt.log"
check "對話搬移的 map 含資料目錄" "--map $SRC_DATA=$DST_DATA" "$ROOT/tt.log"
check "supervisor 目錄 rsync 到目標" "$SRC_DATA/supervisor/ fake@target:$DST_DATA/supervisor/" "$ROOT/rsync.log"
check "host-state bundle 與工具送到目標" "$DST_DATA/cutover/" "$ROOT/rsync.log"
check_before "host-state install 在正式 project import 前執行" "host-state-transfer.py.*install.*host-state-backup" "project-transfer.*import.*--config.*config.toml[[:space:]]*$" "$ROOT/ssh.log"
check "source ui-token 套用到目標" "tok=tok-src" "$ROOT/dst-api.log"
check "outbox 檔案搬到目標" "migrated-report" "$DST_DATA/outbox/AM/report.txt"
check "完整非 project config 在正式 import 前套用" "enabled = true" "$ROOT/import-config.log"
compgen -G "$ROOT/src-data/cutover/*/*.json.gz" > /dev/null; check_eq "Mac 上的 bundle 傳完就刪" 1 $?
kill -0 "$DST_PID" 2>/dev/null; check_eq "目標 daemon 停過" 1 $?
check_before "import 先 --dry-run" "import .*PAM-moved.json.gz.*--dry-run" "import .*PAM-moved.json.gz --host local .*config.toml *$" "$ROOT/pt.log"
check "import 是本機＋路徑改寫" "--host local --path-map $SRC_REPO=$DST_REPO --path-map $SRC_DATA=$DST_DATA" "$ROOT/pt.log"
check "協調者專案 import 帶 --with-supervisor" "PAGM-moved.json.gz .*--with-supervisor" "$ROOT/pt.log"
check "目標 daemon 經 systemd-run 起" "systemd-run --user --collect --unit=agents-managerd-" "$ROOT/systemd-run.log"
check "Type=forking＋KillMode=process（#677）" "-p Type=forking -p KillMode=process" "$ROOT/systemd-run.log"
D="$ROOT/dst-api.log"
check_before "setup 重寫部署檔在接回之前" "POST /api/supervisor/setup" "start?resume=native" "$D"
check "接回 AM-1" "POST /api/bots/P1/start?resume=native" "$D"
check "接回 AGM" "POST /api/bots/A1/start?resume=native" "$D"
check_no "沒跑的不接回" "/api/bots/P2/start" "$D"
check_no "child 不單獨接回" "/api/bots/C1/start" "$D"
check_no "browser-gc 不在目標接回" "/api/bots/A3/start" "$D"
check_before "接回之後才標協調者要跑" "POST /api/bots/A1/start" "POST /api/supervisor/start" "$D"
check "巡檢標回要跑" "POST /api/supervisor/responder/start" "$D"
check "目標用移交後的來源 token" "tok=tok-src" "$D"
check "驗證通過" "驗證通過" "$ROOT/out"
check "印出停機窗口" "停機窗口：" "$ROOT/out"
check "記下每步耗時" "	import	" "$(ls -d "$ROOT"/src-data/cutover/*)/timings.tsv"
teardown

# ---------------------------------------------------------------- 閘門

echo "# 閘門：#720 還沒進來就不做"
setup
export PT_OLD=1
run "$ROOT/out" cutover --execute --foreground
check_eq "沒有 --with-supervisor 時 --execute 失敗" 1 "$(cat "$ROOT/out.rc")"
check "講明原因" "#720" "$ROOT/out"
check_no "什麼都沒停" "POST" "$ROOT/src-api.log"
check_no "什麼都沒 bootout" "bootout" "$ROOT/launchctl.log"
teardown

echo "# 閘門：import --dry-run 看到 transcript 不在（對話沒搬到）就停，不正式 import、不起目標"
setup
export PT_WARN_TRANSCRIPT=1
run "$ROOT/out" cutover --execute --foreground
check_eq "結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "講明 transcript" "1 段 transcript 不在，對話搬移只放行 0 段" "$ROOT/out"
check_no "沒正式 import" "config.toml *$" "$ROOT/pt.log"
check_no "目標 daemon 沒被起來" "systemd-run --user" "$ROOT/ssh.log"
teardown

echo "# 閘門：只有舊 session 找不到（不是要接回的最後一段）→ 放行，import 的段數對得上"
setup
export TT_MISSING=s-old PT_WARN_TRANSCRIPT=1
run "$ROOT/out" cutover --execute --foreground
check_eq "放行：結束碼 0" 0 "$(cat "$ROOT/out.rc")"
check "警告放行了幾段" "PAM：1 段舊 session" "$ROOT/out"
check "正式 import 了" "PAM-moved.json.gz --host local .*config.toml *$" "$ROOT/pt.log"
teardown
setup
export TT_MISSING=s-old PT_WARN_TRANSCRIPT=2
run "$ROOT/out" cutover --execute --foreground
check_eq "import 看到的比放行的多：結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check_no "沒正式 import" "config.toml *$" "$ROOT/pt.log"
teardown

echo "# 閘門：要接回的 bot 最後一段找不到、或目標已有不同內容的同名檔 → 停在 ship 之前"
for case in "TT_MISSING=s-new|要接回的 bot 最後一段對話找不到：AM-1" "TT_CONFLICT=1|目標已有內容不同的同名檔" "TT_RC=1|結束碼 1"; do
    setup
    export "${case%%|*}"
    run "$ROOT/out" cutover --execute --foreground
    check_eq "${case%%|*}：結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
    check "${case%%|*}：講明原因" "${case#*|}" "$ROOT/out"
    check_no "${case%%|*}：bundle 沒傳到目標" "moved.json.gz" "$ROOT/rsync.log"
    kill -0 "$DST_PID" 2>/dev/null; check_eq "${case%%|*}：目標 daemon 沒被停" 0 $?
    teardown
done

echo "# 閘門：bot 沒停下來就不往下（不設 handed_off_to、不停 daemon）"
setup
export CUTOVER_STOP_WAIT_SECS=2
echo P1 > "$ROOT/src-state.json.nostop"
run "$ROOT/out" cutover --execute --foreground
check_eq "結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "講明誰沒停" "還有 1 顆在跑：AM-1" "$ROOT/out"
check_no "沒設 handed_off_to" "PATCH" "$ROOT/src-api.log"
kill -0 "$SRC_PID" 2>/dev/null; check_eq "來源 daemon 沒被停" 0 $?
teardown

echo "# 閘門：已經設了 handed_off_to 的專案＝上一次沒做完，不從頭再來"
setup
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); s["projects"][0]["handed_off_to"]="agm-host"; json.dump(s, open(sys.argv[1],"w"))' "$ROOT/src-state.json"
run "$ROOT/out" cutover --execute --foreground
check_eq "結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "提示用 --from 或 rollback" "--from" "$ROOT/out"
check_no "什麼都沒停" "POST" "$ROOT/src-api.log"
teardown

echo "# 驗證：目標少了 user bot、或 daemon.log 有新的 ERROR 都算沒過"
setup
python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); s["projects"][0]["bots"]=[b for b in s["projects"][0]["bots"] if b["id"]!="P2"]; json.dump(s, open(sys.argv[1],"w"))' "$ROOT/dst-state.json"
run "$ROOT/out" cutover --execute --foreground
check_eq "少 bot：結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "指出少了哪顆" "少了 user bot：AM-2" "$ROOT/out"
check "提示 rollback" "rollback --state-dir" "$ROOT/out"
teardown
setup
export DST_ERROR_AFTER_START=1
printf '2026-09-27T00:00:00Z ERROR old one before the cutover\n' >> "$DST_DATA/daemon.log"
run "$ROOT/out" cutover --execute --foreground
check_eq "新 ERROR：結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "只數起來之後的 ERROR" "起來後有 1 行 ERROR" "$ROOT/out"
teardown

echo "# --from：從中間接著做，前面的步驟不重跑"
setup
run "$ROOT/out" cutover --execute --foreground
ST=$(ls -d "$ROOT"/src-data/cutover/*)
: > "$ROOT/src-api.log"; : > "$ROOT/pt.log"; : > "$ROOT/dst-api.log"
DST_PID=$(spawn_daemon); export DST_PID
run "$ROOT/out2" cutover --execute --foreground --from start-dst --state-dir "$ST"
check_eq "--from start-dst 結束碼 0" 0 "$(cat "$ROOT/out2.rc")"
check_no "不再停來源 bot" "POST /api/bots" "$ROOT/src-api.log"
check_no "不再 export／import" "export --db\|import --bundle" "$ROOT/pt.log"
check "接著接回" "start?resume=native" "$ROOT/dst-api.log"
run "$ROOT/out3" cutover --execute --foreground --from nope --state-dir "$ST"
check_eq "不存在的步驟直接拒絕" 1 "$(cat "$ROOT/out3.rc")"
teardown

# ---------------------------------------------------------------- 背景

echo "# --execute 預設脫離成背景（會停掉叫它起來的 bot pane）"
setup
export PT_OLD=1   # 讓背景那一份在 preflight 就停，不必真的跑完
bash "$SCRIPT" cutover --execute > "$ROOT/out" 2>&1
check "印出背景 pid 與 log" "背景執行（pid" "$ROOT/out"
ST=$(ls -d "$ROOT"/src-data/cutover/*)
for i in $(seq 50); do grep -q FATAL "$ST/run.log" 2>/dev/null && break; sleep 0.1; done
check "背景那一份帶著同一個狀態目錄在跑" "#720" "$ST/run.log"
teardown

# ---------------------------------------------------------------- rollback

echo "# rollback：還原目標到第一次 import 之前、Mac 接回"
setup
run "$ROOT/out" cutover --execute --foreground
ST=$(ls -d "$ROOT"/src-data/cutover/*)
echo "db-before" > "$DST_DATA/agents-manager.sqlite3.pre-transfer-1"; echo "cfg-before" > "$DST_DATA/config.toml.pre-transfer-1"
echo "db-mid" > "$DST_DATA/agents-manager.sqlite3.pre-transfer-2"; echo "cfg-mid" > "$DST_DATA/config.toml.pre-transfer-2"
echo "db-after" > "$DST_DATA/agents-manager.sqlite3"; : > "$DST_DATA/agents-manager.sqlite3-wal"
DST_PID=$(spawn_daemon); export DST_PID
: > "$ROOT/src-api.log"; : > "$ROOT/launchctl.log"
run "$ROOT/rb" rollback --state-dir "$ST"
check_eq "rollback dry-run 結束碼 0" 0 "$(cat "$ROOT/rb.rc")"
check_eq "rollback dry-run 不動目標 DB" db-after "$(cat "$DST_DATA/agents-manager.sqlite3")"
check_no "rollback dry-run 不打 API" "PATCH\|POST" "$ROOT/src-api.log"
run "$ROOT/rb" rollback --state-dir "$ST" --execute --foreground
check_eq "rollback 結束碼 0" 0 "$(cat "$ROOT/rb.rc")"
check_eq "目標 DB 放回第一次 import 前的備份" db-before "$(cat "$DST_DATA/agents-manager.sqlite3")"
[ -e "$DST_DATA/agents-manager.sqlite3-wal" ]; check_eq "舊的 -wal 清掉" 1 $?
check_eq "目標 config 放回完整切換前內容" '[server]' "$(cat "$DST_DATA/config.toml")"
check_eq "目標 ui-token rollback" tok-dst "$(cat "$DST_DATA/ui-token")"
check_no "rollback 移除搬入 outbox 檔" "migrated-report" "$DST_DATA/outbox/AM/report.txt"
check "Mac daemon 經 launchctl submit 起" "submit -l am-cutover-rollback-" "$ROOT/launchctl.log"
check "清 handed_off_to" '"handed_off_to": null' "$ROOT/src-api.log"
check_before "先收回再接回" "PATCH /api/projects/PAM" "POST /api/bots/P1/start?resume=native" "$ROOT/src-api.log"
check "Mac 協調者標回要跑" "POST /api/supervisor/start" "$ROOT/src-api.log"
check "bootstrap 回原本載入的 com.agm.*" "bootstrap gui/$(id -u) $HOME/Library/LaunchAgents/com.agm.dev-server.plist" "$ROOT/launchctl.log"
check_no "原本沒載入的不 bootstrap" "com.agm.not-loaded.plist" "$ROOT/launchctl.log"
teardown

echo "# rollback 要 --state-dir"
setup
run "$ROOT/out" rollback --execute --foreground
check_eq "沒有 --state-dir 就拒絕" 1 "$(cat "$ROOT/out.rc")"
teardown

# ---------------------------------------------------------------- drill

echo "# drill：目標 config 有 data_dir（複本會寫回正式資料目錄）就不做，做完一定清掉"
setup
printf '[server]\ndata_dir = "/somewhere"\n' > "$DST_DATA/config.toml"
run "$ROOT/out" drill
check_eq "結束碼非 0" 1 "$(cat "$ROOT/out.rc")"
check "講明 data_dir" "data_dir" "$ROOT/out"
compgen -G "$ROOT/src-data/cutover/drill-*" > /dev/null; check_eq "Mac 狀態目錄清掉" 1 $?
compgen -G "$HOME/agm-drill-*" > /dev/null; check_eq "目標沒留演練目錄" 1 $?
teardown

# ---------------------------------------------------------------- helper

echo "# helper：lock-free 認得 daemon 拿著的鎖"
T=$(mktemp -d)
python3 "$HELPER" lock-free "$T"; check_eq "沒有 daemon.lock＝空的" 0 $?
python3 -c 'import fcntl,os,sys,time; fd=os.open(sys.argv[1]+"/daemon.lock", os.O_RDWR|os.O_CREAT); fcntl.flock(fd, fcntl.LOCK_EX); open(sys.argv[1]+"/held","w").close(); time.sleep(30)' "$T" &
HOLD=$!
for i in $(seq 50); do [ -e "$T/held" ] && break; sleep 0.1; done
python3 "$HELPER" lock-free "$T"; check_eq "被拿著＝daemon 在跑" 1 $?
kill "$HOLD"; wait "$HOLD" 2>/dev/null
python3 "$HELPER" lock-free "$T"; check_eq "放開之後＝空的" 0 $?
rm -rf "$T"

echo "# helper：drill-verify 抓得到沒換的路徑與不在的 transcript"
T=$(mktemp -d)
python3 - "$T" <<'EOF'
import gzip, json, os, sqlite3, sys
t = sys.argv[1]
os.makedirs(f"{t}/home", exist_ok=True)
open(f"{t}/home/s1.jsonl", "w").close()
db = sqlite3.connect(f"{t}/db.sqlite3")
db.executescript("""
CREATE TABLE projects (id TEXT PRIMARY KEY, host TEXT, path TEXT, deleted_at TEXT);
CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), cwd TEXT, deleted_at TEXT);
CREATE TABLE runs (id TEXT PRIMARY KEY, bot_id TEXT REFERENCES bots(id), native_session_id TEXT, transcript_path TEXT, started_at TEXT);
""")
db.execute("INSERT INTO projects VALUES ('P', 'local', '/dst/repo', NULL)")
db.execute("INSERT INTO bots VALUES ('B1', 'P', '/dst/repo/.claude/worktrees/x', NULL)")
db.execute("INSERT INTO bots VALUES ('B2', 'P', NULL, NULL)")
db.execute("INSERT INTO runs VALUES ('R1', 'B1', 's1', ?, '1')", (f"{t}/home/s1.jsonl",))
db.commit()
bundle = {"tables": {"projects": {"rows": [{"id": "P", "path": "/src/repo", "label": "x"}]},
                     "bots": {"rows": [{"id": "B1"}, {"id": "B2"}]}}}
with gzip.open(f"{t}/b.json.gz", "wb") as f:
    f.write(json.dumps(bundle).encode())
open(f"{t}/config.toml", "w").write('[[projects]]\nid = "P"\npath = "/dst/repo"\nhost = "local"\n')
EOF
dv() { python3 "$HELPER" drill-verify --db "$T/db.sqlite3" --config "$T/config.toml" --bundle "$T/b.json.gz" --map /src/repo=/dst/repo > "$T/out" 2>&1; echo $?; }
check_eq "全部對得上＝0" 0 "$(dv)"
sqlite3 "$T/db.sqlite3" "UPDATE bots SET cwd = '/src/repo/x' WHERE id = 'B1'"
check_eq "cwd 還是來源路徑＝1" 1 "$(dv)"
check "指出 cwd" "cwd 還是來源路徑" "$T/out"
sqlite3 "$T/db.sqlite3" "UPDATE bots SET cwd = NULL WHERE id = 'B1'"
sqlite3 "$T/db.sqlite3" "INSERT INTO runs VALUES ('R0', 'B1', 's0', '$T/home/s0-gone.jsonl', '0')"
check_eq "更早的舊 session 不在＝0（只列數字）" 0 "$(dv)"
check "舊 session 列數字" '"transcripts_missing_old": 1' "$T/out"
rm "$T/home/s1.jsonl"
check_eq "最後一段 transcript 不在＝1" 1 "$(dv)"
check "指出 transcript" "1 顆 bot 最後一段對話的 transcript 不在" "$T/out"
touch "$T/home/s1.jsonl"
sqlite3 "$T/db.sqlite3" "UPDATE projects SET path = '/src/repo'"
check_eq "專案 path 沒換＝1" 1 "$(dv)"
sqlite3 "$T/db.sqlite3" "UPDATE projects SET path = '/dst/repo'; UPDATE bots SET deleted_at = 'x' WHERE id = 'B2'"
check_eq "bot 少一顆＝1" 1 "$(dv)"
sqlite3 "$T/db.sqlite3" "UPDATE bots SET deleted_at = NULL"
printf '[[projects]]\nid = "P"\npath = "/dst/repo"\nhost = "local"\n[[projects.bots]]\nautostart = true\n' > "$T/config.toml"
check_eq "config 有 autostart＝1" 1 "$(dv)"
rm -rf "$T"

# print_times 在沒跑任何步驟（TIMES 空）時不能死：macOS 的 /bin/bash 3.2 對 set -u 下空陣列的 "${arr[@]}" 會 unbound variable。
# 用系統 /bin/bash 跑（Linux 上是新版 bash，等於只驗輸出；在 Mac 上才真的守住）。
pt_out=$(/bin/bash -c 'set -u; log() { echo "$*"; }; TIMES=(); '"$(sed -n '/^print_times() {/,/^}/p' "$SCRIPT")"'; print_times' 2>&1)
check_eq "沒跑任何步驟：print_times 印出合計、不死" 1 "$(printf '%s\n' "$pt_out" | grep -c '合計')"
unset pt_out

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" = 0 ]
