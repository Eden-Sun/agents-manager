#!/bin/bash
# ubuntu-ci.sh 的隔離測試：origin 是暫存目錄裡的本地 bare repo（裡面放一支假的 scripts/check.sh），
# gh 是假的（只記錄呼叫），CI_ROOT 在暫存目錄；不碰網路、不碰真的 ~/.cache/agents-manager/ci。
# 測的是：綠／紅各寫對 commit status 與 status.json、同一個 sha 不重跑、鎖被占著就退出，
# 還有「跑到一半失敗不能讓 pending 永遠掛著」與「step 不理 TERM 也要被收掉、不能卡住鎖」。
#
#   bash scripts/ops/ubuntu-ci_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
if ! command -v flock >/dev/null 2>&1; then echo "skip - 沒有 flock（ubuntu-ci 只在 Linux 跑）"; exit 0; fi
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}

setup() {
  ROOT=$(mktemp -d)
  export HOME="$ROOT/home"; mkdir -p "$HOME"
  export GIT_CONFIG_GLOBAL="$ROOT/gitconfig" GIT_CONFIG_SYSTEM=/dev/null
  git config --global user.email t@t; git config --global user.name t; git config --global init.defaultBranch main
  git init -q --bare "$ROOT/origin.git"
  git clone -q "$ROOT/origin.git" "$ROOT/work" 2>/dev/null
  mkdir -p "$ROOT/work/scripts"
  # 假 check.sh：每段看 $FIX/rc.<段> 決定 rc，預設 0；$FIX/hang.<段> 存在就忽略 TERM 睡很久。
  cat > "$ROOT/work/scripts/check.sh" <<'CK'
#!/bin/bash
part="$1"
echo "==> fake $part"
[ -e "$FIX/title.$part" ] && cat "$FIX/title.$part"
if [ -e "$FIX/hang.$part" ]; then trap '' TERM; sleep 30 & wait; fi
exit "$(cat "$FIX/rc.$part" 2>/dev/null || echo 0)"
CK
  (cd "$ROOT/work" && git add -A && git commit -q -m c1 && git push -q origin main)
  FIX="$ROOT/fix"; mkdir -p "$FIX" "$ROOT/bin"
  export FIX
  # 假 gh：記下呼叫；$FIX/gh.rc 決定回傳碼。
  cat > "$ROOT/bin/gh" <<'GH'
#!/bin/bash
echo "gh $*" >> "$FIX/gh.log"
exit "$(cat "$FIX/gh.rc" 2>/dev/null || echo 0)"
GH
  chmod +x "$ROOT/bin/gh"
  export PATH="$ROOT/bin:$PATH"
  export AGM_CI_ROOT="$ROOT/ci" AGM_CI_REPO_URL="$ROOT/origin.git" AGM_CI_GH_REPO=o/r AGM_CI_TIMEOUT=30s AGM_CI_KILL_AFTER=1s
  CI="$AGM_CI_ROOT"; : > "$FIX/gh.log"
}
teardown() { rm -rf "$ROOT"; }
run() { bash "$HERE/ubuntu-ci.sh" >"$ROOT/out" 2>&1; echo $?; }

# 1. 綠：pending → success，last-sha 與 status.json 寫好。
setup
equals "全綠 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
check "先送 pending" "state=pending" "$FIX/gh.log"
check "最後送 success" "state=success" "$FIX/gh.log"
check "status.json 是 success" '"state":"success"' "$CI/status.json"
equals "last-sha 記下這個 sha" "$(cat "$CI/last-sha")" "$SHA"
check "log 有四段" "\[ubuntu-ci\] daemon rc=0" "$CI/logs/$SHA.log"

# 2. 同一個 sha 不重跑。
: > "$FIX/gh.log"
equals "同 sha 再叫一次 exit 0" "$(run)" "0"
equals "沒有任何 gh 呼叫" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"

# 3. 新 commit 且 ops 紅：failure，description 點名紅的段。
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
echo 3 > "$FIX/rc.ops"
: > "$FIX/gh.log"
equals "有一段紅仍 exit 0（結果在 status）" "$(run)" "0"
check "送 failure" "state=failure" "$FIX/gh.log"
check "description 點名 ops" "紅：ops" "$FIX/gh.log"
check "status.json 記 rc=3" '"rc":3' "$CI/status.json"
check "其他段照樣跑完" "\[ubuntu-ci\] daemon rc=0" "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
teardown

# 3b. 紅的 step 標題含引號與反斜線：status.json 仍要是合法 JSON，description 內容不能被吃掉。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
printf '==> step "q" C:\\dir\\x\n' > "$FIX/title.ops"
echo 1 > "$FIX/rc.ops"
run >/dev/null
equals "status.json 是合法 JSON" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$CI/status.json" 2>&1)" "failure"
check "description 保留了反斜線之後的字" "dir" "$CI/status.json"
teardown

# 4. 鎖被占著：直接退出、不碰 git／gh。
setup
mkdir -p "$CI"
( exec 9>"$CI/lock"; flock -n 9; sleep 5 ) &
HOLDER=$!
sleep 0.5
equals "鎖被占著 exit 0" "$(run)" "0"
equals "沒有 gh 呼叫" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null
teardown

# 5. pending 之後腳本自己失敗（這裡讓 git checkout 失敗）：不能讓 pending 掛著，要補一個 error，last-sha 不前進。
setup
REAL_GIT=$(command -v git)
cat > "$ROOT/bin/git" <<GIT
#!/bin/bash
[ "\$1" = checkout ] && { echo "fatal: simulated checkout failure" >&2; exit 1; }
exec "$REAL_GIT" "\$@"
GIT
chmod +x "$ROOT/bin/git"
rc=$(run)
equals "失敗時 exit 非零" "$([ "$rc" != 0 ] && echo nonzero)" "nonzero"
check "pending 有送" "state=pending" "$FIX/gh.log"
check "之後補 error，不留 pending" "state=error" "$FIX/gh.log"
check "status.json 不是 running" '"state":"error"' "$CI/status.json"
equals "last-sha 沒前進（下一輪會重試）" "$(cat "$CI/last-sha" 2>/dev/null)" ""
teardown

# 6. step 忽略 TERM：timeout 要補 KILL，不然鎖永遠不放。
setup
: > "$FIX/hang.web"
export AGM_CI_TIMEOUT=1s
start=$(date +%s)
equals "超時的段也跑得完" "$(run)" "0"
elapsed=$(( $(date +%s) - start ))
equals "沒有被卡住（遠小於 sleep 30）" "$([ "$elapsed" -lt 20 ] && echo fast)" "fast"
check "超時的段記成紅" "紅：web" "$FIX/gh.log"
teardown

echo "ubuntu-ci_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
