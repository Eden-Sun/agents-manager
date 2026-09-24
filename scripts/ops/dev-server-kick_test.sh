#!/bin/bash
# dev-server-kick.ts 的隔離測試（issue #418）：假的 lsof／ps／git／bun／node，測試用的 port 與
# 暫存目錄當 REPO，**絕對不碰真的 5173 與真的 vite**。
#
# 安全規則（issue #422）：
# - 副本的 PORT 換成測試 port，跑之前先斷言副本裡一個 5173 都不剩。
# - 這支腳本用 `Bun.spawn` 叫外部指令，**shell 函式攔不住**，所以危險指令一律靠 PATH 上的替身，
#   而且假 lsof 只肯回報「測試自己 spawn 出來的 pid」（記在 fix/spawned）——腳本的 process.kill
#   因此永遠只可能殺到測試自己的 sleep，殺不到任何真的行程。
#
#   bash scripts/ops/dev-server-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${HERE}/dev-server-kick.ts"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}

# 真 binary 的絕對路徑要先抓：跑腳本時 PATH 換成 fakebin，用名字找會拿到假的
# （第一版就是這樣讓 `bun "$SCRIPT"` 打到假 bun，整組測試空跑成假綠）。
BUN="$(command -v bun || true)"
PY3="$(command -v python3 || true)"
[ -n "$BUN" ] || { echo "skip - 沒有 bun，跳過 dev-server-kick.ts 的測試"; echo "0 passed, 0 failed"; exit 0; }
[ -n "$PY3" ] || { echo "skip - 沒有 python3（測試用它當假的 HTTP server）"; echo "0 passed, 0 failed"; exit 0; }

for pat in "const REPO = '/Users/m4p/project/agents-manager-main'" 'const PORT = 5173' \
           "const pinned = '/Users/m4p/.local/bin/node'"; do
  grep -q -- "${pat}" "${SRC}" || { echo "FAIL - 原腳本找不到 '${pat}'，測試的替換要更新"; exit 1; }
done

# 測試用 port：挑一個沒人在聽的。5173 絕對不准用。
PORT=""
for p in 53173 53174 53175 53176; do
  if ! lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then PORT="$p"; break; fi
done
[ -n "$PORT" ] || { echo "skip - 找不到空的測試 port"; echo "0 passed, 0 failed"; exit 0; }

setup() {
  ROOT="$(mktemp -d)"
  FIX="$ROOT/fix"; BIN="$ROOT/fakebin"; REPO="$ROOT/repo"
  mkdir -p "$FIX" "$BIN" "$REPO/web/node_modules/vite/bin" "$ROOT/agm/bin"
  LOG="$ROOT/agm/dev-server.log"; : > "$LOG"
  : > "$FIX/spawned"; : > "$FIX/lsof.txt"; : > "$FIX/calls.log"
  SCRIPT="$ROOT/agm/bin/dev-server-kick.ts"
  sed -e "s#const REPO = '/Users/m4p/project/agents-manager-main'#const REPO = '${REPO}'#" \
      -e "s#const PORT = 5173#const PORT = ${PORT}#" \
      -e "s#const pinned = '/Users/m4p/.local/bin/node'#const pinned = '${BIN}/node'#" \
      "$SRC" > "$SCRIPT"

  # 假 lsof：只回報 fix/lsof.txt 裡、而且「還活著且是測試自己 spawn 的」pid。
  # 這是硬守衛：腳本的 process.kill 只可能打到測試自己的行程。
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
exit 0
STUB
  # 假 node：記下 argv；帶 vite 參數時真的起一個 HTTP server（讓 alive() 成立）。
  cat > "$BIN/node" <<STUB
#!/bin/bash
echo "node \$*" >> "${FIX}/calls.log"
case "\$1" in --version) echo "v22.0.0"; exit 0 ;; esac
echo \$\$ >> "${FIX}/spawned"
exec "${PY3}" -m http.server "${PORT}" --bind 127.0.0.1 --directory "${ROOT}"
STUB
  chmod 755 "$BIN"/*
  echo "aaaaaaa" > "$FIX/head"; echo "lock1" > "$FIX/lock"
  : > "$REPO/web/node_modules/vite/bin/vite.js"
  ( cd "$REPO" && mkdir -p .git )
}
# 只殺測試自己登記過的 pid。不用 `pkill -f <字串>`：那種通用比對會掃到別顆 agent
# 的同名行程（issue #422 的規則之一）。
teardown() {
  while read -r pid; do [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null; done < "$FIX/spawned"
  rm -rf "$ROOT"
}
# 起一個測試自己的行程當「占用 port 的人」，pid 登記進 spawned。
fake_listener() { # <addr> [ppid] [cmd]
  # stdout／stderr 一定要導掉：這個函式常在 `$( )` 裡呼叫，背景行程若還握著
  # command substitution 的管線，外層會一直等到它結束（sleep 300 就是卡 5 分鐘）。
  sleep 300 >/dev/null 2>&1 & local pid=$!
  disown 2>/dev/null || true   # teardown kill -9 時不要噴 job control 訊息
  echo "$pid" >> "$FIX/spawned"
  echo "$pid $1" >> "$FIX/lsof.txt"
  printf '%s %s\n' "${2:-1}" "${3:-node /x/vite.js --host 0.0.0.0}" > "$FIX/ps.$pid"
  echo "$pid"
}
serve() { "$PY3" -m http.server "$PORT" --bind 127.0.0.1 --directory "$ROOT" >/dev/null 2>&1 & local pid=$!; disown 2>/dev/null || true; echo "$pid" >> "$FIX/spawned"; echo "$pid"; }
# 等它真的在聽才能往下跑。原本固定 sleep 1，在 CI 的 runner 上不夠——server 還沒 bind，
# 被測腳本的 alive() 就回 false，於是「健康」那組變成「沒人聽」，測試假紅（run 35958418022）。
wait_ready() {
  local i
  for i in $(seq 1 60); do
    curl -sf -o /dev/null "http://127.0.0.1:${PORT}/" 2>/dev/null && return 0
    sleep 0.5
  done
  echo "      （等不到測試用的 HTTP server 在 ${PORT} 上回應）" >&2
  return 1
}
run() { ( cd "$ROOT" && PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BUN" "$SCRIPT" >/dev/null 2>&1; echo $? ); }

# 0. 安全前提：副本裡不准再有 5173，假 lsof 只回報自己 spawn 的 pid。
setup
# PORT 是唯一的 port 來源（URL 與 lsof 都從它來），所以只要這一行換掉，
# 副本就不可能碰到真的 5173；其餘提到 5173 的是註解與一行 log 字串。
equals "副本的 const PORT 換成測試 port" "$(grep -c "^const PORT = ${PORT}\$" "$SCRIPT")" "1"
equals "副本裡沒有 const PORT = 5173" "$(grep -c '^const PORT = 5173$' "$SCRIPT")" "0"
equals "副本裡剩下的 5173 都不是可執行的 port（只在註解與 log 字串）" \
  "$(grep -n '5173' "$SCRIPT" | grep -vE ':[[:space:]]*(//|/\*\*|\*)' | grep -vc 'log(`')" "0"
equals "副本的 REPO 指到暫存目錄" "$(grep -c "const REPO = '${REPO}'" "$SCRIPT")" "1"
echo "99999" > "$FIX/lsof.txt"   # 沒登記、也不存在的 pid
equals "假 lsof 不回報沒登記的 pid" "$(PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" "$BIN/lsof" -nP -iTCP:"$PORT" -sTCP:LISTEN -Fpn | wc -l | tr -d ' ')" "0"
teardown

# 1. 健康（綁 *、有 HTTP 回應、HEAD 沒變）→ 什麼都不做、log 不寫。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
equals "健康時 exit 0" "$(run)" "0"
equals "健康時不寫 log" "$(wc -c < "$LOG" | tr -d ' ')" "0"
check_no "健康時不會去起 vite" "vite.js --host" "$FIX/calls.log"
teardown

# 2. port 被非 vite 的程序占用 → 只記錄，絕不殺。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 1 "python3 -m http.server")
equals "非 vite 占用時 exit 0" "$(run)" "0"
check "記下被非 vite 占用" "被非 vite 的 pid ${PID}" "$LOG"
equals "非 vite 的程序沒被殺" "$(kill -0 "$PID" 2>/dev/null && echo alive)" "alive"
teardown

# 3. loopback-only 但父程序還活著（ppid≠1）→ 某個 bot 正在用，只記錄「需人工處理」。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 4242 "node /x/vite.js")
equals "有父程序時 exit 0" "$(run)" "0"
check "記下需人工處理" "需人工處理" "$LOG"
equals "別人的 vite 沒被殺" "$(kill -0 "$PID" 2>/dev/null && echo alive)" "alive"
teardown

# 4. loopback-only 的孤兒 vite（ppid=1）→ 收掉換一顆綁 0.0.0.0 的。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
PID=$(fake_listener "127.0.0.1:${PORT}" 1 "node /x/vite.js")
run >/dev/null
check "記下收掉孤兒 loopback-only vite" "收掉孤兒 loopback-only vite pid ${PID}" "$LOG"
sleep 1
equals "孤兒 vite 真的被殺掉" "$(kill -0 "$PID" 2>/dev/null && echo alive || echo gone)" "gone"
check "接著用 node 拉起 vite" "用 node v22.0.0 拉起 vite" "$LOG"
teardown

# 5. 缺依賴：找不到 node → 明確放棄這輪，不拿 bun 代跑。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
: > "$FIX/no-node"; rm -f "$BIN/node"
equals "找不到 node 時 exit 0" "$(run)" "0"
check "明講不用 bun 代跑" "找不到 node，不用 bun 代跑" "$LOG"
check_no "沒有真的去 spawn 任何東西" "vite.js --host" "$FIX/calls.log"
teardown

# 6. 缺依賴：node 在、但 web/ 還沒 install（vite.js 不存在）→ 放棄這輪並指出缺的檔。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
rm -f "$REPO/web/node_modules/vite/bin/vite.js"
equals "找不到 vite.js 時 exit 0" "$(run)" "0"
check "log 指出缺的是 vite.js" "vite/bin/vite.js" "$LOG"
check_no "不會硬起" "拉起 vite" "$LOG"
teardown

# 7. 沒人聽 → 用 node 拉起，參數要綁 0.0.0.0＋strictPort，起來了要寫進 log。
setup
echo "aaaaaaa" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
equals "沒人聽時 exit 0" "$(run)" "0"
check "用 node 拉起" "用 node v22.0.0 拉起 vite" "$LOG"
check "綁 0.0.0.0 供 LAN 存取" "\-\-host 0.0.0.0" "$FIX/calls.log"
check "帶 --strictPort（port 搶不到就失敗，不換 port）" "\-\-strictPort" "$FIX/calls.log"
check "帶測試用的 port" "\-\-port ${PORT}" "$FIX/calls.log"
check "確認起來了才收工" "dev server 已起來" "$LOG"
teardown

# 8. 同步：REPO 不是 git worktree → 記一行跳過，不碰 git。
setup
rm -rf "$REPO/.git"
run >/dev/null
check "不是 git worktree 就跳過同步" "不是 git worktree，跳過同步" "$LOG"
check_no "跳過時不會叫 git" "git fetch" "$FIX/calls.log"
teardown

# 9. 同步：HEAD 變了但 bun.lock 沒變 → 只記同步，不 bun install（靠 HMR）。
setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock1" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
run >/dev/null
check "記下同步到 origin/main" "同步到 origin/main aaaaaaa..bbbbbbb" "$LOG"
check_no "lock 沒變就不 bun install" "bun install" "$FIX/calls.log"
teardown

# 10. 同步：bun.lock 也變了 → bun install 並重啟 vite。
setup
echo "bbbbbbb" > "$FIX/head.after"; echo "lock2" > "$FIX/lock.after"
fake_listener "*:${PORT}" 1 "node /x/vite.js --host 0.0.0.0" >/dev/null
serve >/dev/null; wait_ready
run >/dev/null
check "lock 變了就 bun install" "bun install --frozen-lockfile" "$FIX/calls.log"
check "log 說明要重啟 vite" "web/bun.lock 變了" "$LOG"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
