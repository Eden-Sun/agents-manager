#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${HERE}/dev-server-kick.ts"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}

BUN="$(command -v bun || true)"
PY3="$(command -v python3 || true)"
[ -n "$BUN" ] || { echo "skip - 沒有 bun，跳過 dev-server-kick.ts 的測試"; echo "0 passed, 0 failed"; exit 0; }
[ -n "$PY3" ] || { echo "skip - 沒有 python3（測試用它當假的 HTTP server）"; echo "0 passed, 0 failed"; exit 0; }

for pat in 'const PORT = 5173' "const pinned = join(HOME, '.local/bin/node')"; do
  grep -q -- "${pat}" "${SRC}" || { echo "FAIL - 原腳本找不到 '${pat}'，測試的替換要更新"; exit 1; }
done

PORT_LOCK=""
release_port() { [ -n "$PORT_LOCK" ] && rm -rf "$PORT_LOCK"; }
trap release_port EXIT
claim_port() {
  local lock="${TMPDIR:-/tmp}/agm-dev-server-kick_test.$1.lock" owner
  if ! mkdir "$lock" 2>/dev/null; then
    owner="$(cat "$lock/pid" 2>/dev/null)"
    [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null || return 1
    rm -rf "$lock"
    mkdir "$lock" 2>/dev/null || return 1
  fi
  echo "$$" > "$lock/pid"
  if lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1; then rm -rf "$lock"; return 1; fi
  PORT_LOCK="$lock"
}
PORT=""
for _ in $(seq 1 120); do
  for p in 53173 53174 53175 53176; do
    if claim_port "$p"; then PORT="$p"; break 2; fi
  done
  sleep 1
done
[ -n "$PORT" ] || { echo "skip - 找不到空的測試 port"; echo "0 passed, 0 failed"; exit 0; }

setup() {
  ROOT="$(mktemp -d)"
  FIX="$ROOT/fix"; BIN="$ROOT/fakebin"; REPO="$ROOT/repo"
  mkdir -p "$FIX" "$BIN" "$REPO/web/node_modules/vite/bin" "$ROOT/agm/bin"
  LOG="$ROOT/agm/dev-server.log"; : > "$LOG"
  : > "$FIX/spawned"; : > "$FIX/lsof.txt"; : > "$FIX/calls.log"
  SCRIPT="$ROOT/agm/bin/dev-server-kick.ts"
  sed -e "s#const PORT = 5173#const PORT = ${PORT}#" \
      -e "s#const pinned = join(HOME, '.local/bin/node')#const pinned = '${BIN}/node'#" \
      "$SRC" > "$SCRIPT"

  cat > "$BIN/lsof" <<STUB
#!/bin/bash
while read -r pid addr; do
  [ -n "\$pid" ] || continue
  grep -qx "\$pid" "${FIX}/spawned" || continue
  kill -0 "\$pid" 2>/dev/null || continue
  echo "p\$pid"
  echo "n\$addr"
done < "${FIX}/lsof.txt"
exit 0
STUB
  cat > "$BIN/ss" <<STUB
#!/bin/bash
while read -r pid addr; do
  [ -n "\$pid" ] || continue
  grep -qx "\$pid" "${FIX}/spawned" || continue
  kill -0 "\$pid" 2>/dev/null || continue
  case "\$addr" in '*:'*) addr="0.0.0.0:\${addr#*:}" ;; esac
  echo "LISTEN 0      511    \$addr      0.0.0.0:*    users:((\"node\",pid=\$pid,fd=21))"
done < "${FIX}/lsof.txt"
exit 0
STUB
  cat > "$BIN/ps" <<STUB
#!/bin/bash
cat "${FIX}/ps.\$4" 2>/dev/null
exit 0
STUB
  cat > "$BIN/which" <<STUB
#!/bin/bash
[ -f "${FIX}/no-node" ] && exit 1
echo "${BIN}/node"
STUB
  cat > "$BIN/git" <<STUB
#!/bin/bash
echo "git \$*" >> "${FIX}/calls.log"
shift 2   # -C <repo>
case "\$1 \${2:-}" in
  "rev-parse HEAD") cat "${FIX}/head" ;;
  "rev-parse HEAD:web/bun.lock") [ -f "${FIX}/nolock" ] && exit 1; cat "${FIX}/lock" ;;
  "fetch"*) cp "${FIX}/head.after" "${FIX}/head" 2>/dev/null; cp "${FIX}/lock.after" "${FIX}/lock" 2>/dev/null ;;
  "reset"*) : ;;
esac
exit 0
STUB
  cat > "$BIN/bun" <<STUB
#!/bin/bash
echo "bun \$*" >> "${FIX}/calls.log"
[ -f "${FIX}/install-fail" ] && exit 1
exit 0
STUB
  cat > "$BIN/node" <<STUB
#!/bin/bash
echo "node \$*" >> "${FIX}/calls.log"
case "\$1" in --version) echo "v22.0.0"; exit 0 ;; esac
echo \$\$ >> "${FIX}/spawned"
exec "${PY3}" -m http.server "${PORT}" --bind 127.0.0.1 --directory "${ROOT}"
STUB
  cat > "$BIN/agm" <<STUB
#!/bin/bash
echo "agm \$*" >> "${FIX}/agm.log"
exit 0
STUB
  : > "$FIX/agm.log"
  chmod 755 "$BIN"/*
  case "${TOOL:-}" in lsof) rm -f "$BIN/ss" ;; ss) rm -f "$BIN/lsof" ;; esac
  echo "aaaaaaa" > "$FIX/head"; echo "lock1" > "$FIX/lock"
  : > "$REPO/web/node_modules/vite/bin/vite.js"
  ( cd "$REPO" && mkdir -p .git )
}
teardown() {
  while read -r pid; do [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null; done < "$FIX/spawned"
  rm -rf "$ROOT"
}
fake_listener() {
  sleep 300 >/dev/null 2>&1 & local pid=$!
  disown 2>/dev/null || true
  echo "$pid" >> "$FIX/spawned"
  echo "$pid $1" >> "$FIX/lsof.txt"
  printf '%s %s\n' "${2:-1}" "${3:-node /x/vite.js --host 0.0.0.0}" > "$FIX/ps.$pid"
  echo "$pid"
}
serve() { "$PY3" -m http.server "$PORT" --bind 127.0.0.1 --directory "$ROOT" >/dev/null 2>&1 & local pid=$!; disown 2>/dev/null || true; echo "$pid" >> "$FIX/spawned"; echo "$pid"; }
wait_ready() {
  local i
  for i in $(seq 1 60); do
    curl -sf -o /dev/null "http://127.0.0.1:${PORT}/" 2>/dev/null && return 0
    sleep 0.5
  done
  echo "      （等不到測試用的 HTTP server 在 ${PORT} 上回應）" >&2
  return 1
}
run() { ( cd "$ROOT" && AGM_BIN="$BIN/agm" AGM_DEV_LISTEN_TOOL="$TOOL" AGM_DEV_REPO="$REPO" PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BUN" "$SCRIPT" >/dev/null 2>&1; echo $? ); }

setup
equals "副本的 const PORT 換成測試 port" "$(grep -c "^const PORT = ${PORT}\$" "$SCRIPT")" "1"
equals "副本裡沒有 const PORT = 5173" "$(grep -c '^const PORT = 5173$' "$SCRIPT")" "0"
equals "副本裡剩下的 5173 都不是可執行的 port（只在註解與 log 字串）" \
  "$(grep -n '5173' "$SCRIPT" | grep -vE ':[[:space:]]*(//|/\*\*|\*)' | grep -vc 'log(`')" "0"
equals "副本裡沒有寫死的 /Users 路徑" "$(grep -v '^ *//' "$SCRIPT" | grep -c '/Users/')" "0"
echo "99999" > "$FIX/lsof.txt"
equals "假 lsof 不回報沒登記的 pid" "$(PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BIN/lsof" -nP -iTCP:"$PORT" -sTCP:LISTEN -Fpn | wc -l | tr -d ' ')" "0"
equals "假 ss 不回報沒登記的 pid" "$(PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BIN/ss" -Hltnp | wc -l | tr -d ' ')" "0"
teardown

for TOOL in lsof ss; do
echo "# listen tool: ${TOOL}"

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
equals "健康時 exit 0" "$(run)" "0"
equals "健康時不寫 log" "$(wc -c < "$LOG" | tr -d ' ')" "0"
check_no "健康時不會去起 vite" "vite.js --host" "$FIX/calls.log"
equals "健康時不呼叫 agm（#859）" "$(wc -c < "$FIX/agm.log" | tr -d ' ')" "0"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 1 "python3 -m http.server")
equals "非 vite 占用時 exit 0" "$(run)" "0"
check "記下被非 vite 占用" "被非 vite 的 pid ${PID}" "$LOG"
equals "非 vite 的程序沒被殺" "$(kill -0 "$PID" 2>/dev/null && echo alive)" "alive"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 4242 "node /x/vite.js")
equals "有父程序時 exit 0" "$(run)" "0"
check "記下需人工處理" "需人工處理" "$LOG"
equals "別人的 vite 沒被殺" "$(kill -0 "$PID" 2>/dev/null && echo alive)" "alive"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 1 "node /x/vite.js")
run >/dev/null
check "記下收掉孤兒 loopback-only vite" "收掉孤兒 loopback-only vite pid ${PID}" "$LOG"
sleep 1
equals "孤兒 vite 真的被殺掉" "$(kill -0 "$PID" 2>/dev/null && echo alive || echo gone)" "gone"
check "接著用 node 拉起 vite" "用 node v22.0.0 拉起 vite" "$LOG"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 1 "node /x/vite.js")
serve >/dev/null; wait_ready
run >/dev/null
check "本機有回應的 loopback-only 孤兒也收掉" "收掉孤兒 loopback-only vite pid ${PID}" "$LOG"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 4242 "node /x/vite.js")
echo "/usr/lib/systemd/systemd --user" > "$FIX/ps.4242"
run >/dev/null
check "父程序是 systemd --user 的 loopback-only vite 當孤兒收掉" "收掉孤兒 loopback-only vite pid ${PID}" "$LOG"
check_no "不再寫需人工處理" "需人工處理" "$LOG"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 4242 "node /x/vite.js")
echo "-zsh" > "$FIX/ps.4242"
run >/dev/null
check "父程序不是 systemd --user 時照舊只記錄" "需人工處理" "$LOG"
equals "別人的 vite 沒被殺" "$(kill -0 "$PID" 2>/dev/null && echo alive)" "alive"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
: > "$FIX/no-node"; rm -f "$BIN/node"
equals "找不到 node 時 exit 0" "$(run)" "0"
check "明講不用 bun 代跑" "找不到 node，不用 bun 代跑" "$LOG"
check_no "沒有真的去 spawn 任何東西" "vite.js --host" "$FIX/calls.log"
check "找不到 node 有喊人（#859）" "agm --compact ops-alert --source dev-server --reason dev_node_missing" "$FIX/agm.log"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
rm -f "$REPO/web/node_modules/vite/bin/vite.js"
equals "找不到 vite.js 時 exit 0" "$(run)" "0"
check "log 指出缺的是 vite.js" "vite/bin/vite.js" "$LOG"
check_no "不會硬起" "拉起 vite" "$LOG"
check "找不到 vite.js 有喊人（#859）" "agm --compact ops-alert --source dev-server --reason dev_vite_missing" "$FIX/agm.log"
equals "只喊一次" "$(grep -c 'ops-alert' "$FIX/agm.log")" "1"
teardown

setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
equals "沒人聽時 exit 0" "$(run)" "0"
check "用 node 拉起" "用 node v22.0.0 拉起 vite" "$LOG"
check "綁 0.0.0.0 供 LAN 存取" "\-\-host 0.0.0.0" "$FIX/calls.log"
check "帶 --strictPort（port 搶不到就失敗，不換 port）" "\-\-strictPort" "$FIX/calls.log"
check "帶測試用的 port" "\-\-port ${PORT}" "$FIX/calls.log"
check "確認起來了才收工" "dev server 已起來" "$LOG"
teardown

setup
rm -rf "$REPO/.git"
run >/dev/null
check "不是 git worktree 就跳過同步" "不是 git worktree，跳過同步" "$LOG"
check_no "跳過時不會叫 git" "git fetch" "$FIX/calls.log"
check "worktree 不在有喊人（#859）" "agm --compact ops-alert --source dev-server --reason dev_worktree_missing" "$FIX/agm.log"
teardown

setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
run >/dev/null
check "記下同步到 origin/main" "同步到 origin/main aaaaaaa..bbbbbbb" "$LOG"
check_no "lock 沒變就不 bun install" "bun install" "$FIX/calls.log"
teardown

setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock2" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
run >/dev/null
check "lock 變了就 bun install" "bun install --frozen-lockfile" "$FIX/calls.log"
check "log 說明要重啟 vite" "web/bun.lock 變了" "$LOG"
teardown

setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock2" > "$FIX/lock.after"
PID=$(fake_listener "*:${PORT}" 4242 "node /x/vite.js --host 0.0.0.0")
echo "/usr/lib/systemd/systemd --user" > "$FIX/ps.4242"
serve >/dev/null; wait_ready
run >/dev/null
check "log 說明要重啟 vite" "web/bun.lock 變了" "$LOG"
sleep 1
equals "舊的 vite 真的被收掉" "$(kill -0 "$PID" 2>/dev/null && echo alive || echo gone)" "gone"
check_no "不寫需人工處理" "需人工處理" "$LOG"
teardown

setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock2" > "$FIX/lock.after"
: > "$FIX/install-fail"
pid=$(fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0")
serve >/dev/null; wait_ready
run >/dev/null
check "install 失敗有記 log" "不重啟還在跑的 vite" "$LOG"
kill -0 "$pid" 2>/dev/null && { echo "ok   - install 失敗沒砍 vite"; PASS=$((PASS + 1)); } || { echo "FAIL - install 失敗沒砍 vite"; FAIL=$((FAIL + 1)); }
[ -f "$ROOT/agm/dev-server.pending-install" ] && { echo "ok   - 留下待安裝標記"; PASS=$((PASS + 1)); } || { echo "FAIL - 留下待安裝標記"; FAIL=$((FAIL + 1)); }
rm -f "$FIX/install-fail"
run >/dev/null
equals "HEAD 沒變也再裝一次" "$(grep -c 'bun install --frozen-lockfile' "$FIX/calls.log")" "2"
[ ! -f "$ROOT/agm/dev-server.pending-install" ] && { echo "ok   - 裝成功就清標記"; PASS=$((PASS + 1)); } || { echo "FAIL - 裝成功就清標記"; FAIL=$((FAIL + 1)); }
teardown

setup
( cd "$ROOT" && AGM_BIN="$BIN/agm" AGM_DEV_LISTEN_TOOL="$TOOL" AGM_REPO="$ROOT/shared" PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BUN" "$SCRIPT" >/dev/null 2>&1 )
check "REPO＝AGM_REPO 旁邊的 -main" "$ROOT/shared-main 不是 git worktree" "$LOG"
check "AGM_DEV_REPO 指到不存在的目錄也喊 dev_worktree_missing（#859）" "reason dev_worktree_missing" "$FIX/agm.log"
check_no "沒指定時不會去碰測試 REPO" "git -C $REPO" "$FIX/calls.log"
teardown
done

TOOL=ss; setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
fake_listener "[::]:${PORT}" 1 "node /x/vite.js --host ::" >/dev/null
serve >/dev/null; wait_ready
equals "[::] 健康時 exit 0" "$(run)" "0"
equals "[::] 健康時不寫 log" "$(wc -c < "$LOG" | tr -d ' ')" "0"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
