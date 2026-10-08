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
  # 假 agm.py：記下參數；rc 看 $FIX/agm.rc（預設 0）。ubuntu-ci 的 ops-sync 漂移檢查跑的就是 repo 裡這支。
  cat > "$ROOT/work/scripts/agm.py" <<'AGMPY'
import os, sys
fix = os.environ["FIX"]
open(fix + "/agm.log", "a").write(" ".join(sys.argv[1:]) + "\n")
print('{"in_sync": true}')
rc = fix + "/agm.rc"
sys.exit(int(open(rc).read()) if os.path.exists(rc) else 0)
AGMPY
  (cd "$ROOT/work" && git add -A && git commit -q -m c1 && git push -q origin main)
  FIX="$ROOT/fix"; mkdir -p "$FIX" "$ROOT/bin"
  export FIX
  # 假 gh：記下呼叫；$FIX/gh.rc 決定回傳碼。
  cat > "$ROOT/bin/gh" <<'GH'
#!/bin/bash
# $FIX/gh.failn：前 N 次呼叫失敗（每次減一）；$FIX/gh.rc：其餘呼叫的回傳碼（預設 0）。失敗的呼叫也記在 gh.attempts。
echo "gh $*" >> "$FIX/gh.attempts"
if [ -s "$FIX/gh.failn" ] && [ "$(cat "$FIX/gh.failn")" -gt 0 ]; then
  echo $(( $(cat "$FIX/gh.failn") - 1 )) > "$FIX/gh.failn"
  exit 1
fi
echo "gh $*" >> "$FIX/gh.log"
exit "$(cat "$FIX/gh.rc" 2>/dev/null || echo 0)"
GH
  chmod +x "$ROOT/bin/gh"
  export PATH="$ROOT/bin:$PATH"
  export AGM_CI_ROOT="$ROOT/ci" AGM_CI_REPO_URL="$ROOT/origin.git" AGM_CI_GH_REPO=o/r AGM_CI_TIMEOUT=30s AGM_CI_KILL_AFTER=1s AGM_CI_MIN_FREE_GB=0
  CI="$AGM_CI_ROOT"; : > "$FIX/gh.log"; : > "$FIX/gh.attempts"; export AGM_CI_STATUS_RETRY_SLEEP=0
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

# 3c. daemon 測試紅：description 要點出是哪一條測試，不只是「cargo test 紅了」（高負載偶發紅時才知道是誰）。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
printf '==> daemon: cargo test -p agents-managerd\ntest ok::fine ... ok\ntest cli_update::tests::flaky_one ... FAILED\ntest other::tests::flaky_two ... FAILED\n' > "$FIX/title.daemon"
echo 101 > "$FIX/rc.daemon"
run >/dev/null
check "description 點名第一條紅的測試" "cli_update::tests::flaky_one" "$FIX/gh.log"
check "description 說明還有幾條" "2 條測試紅" "$FIX/gh.log"
check_no "綠的測試不進 description" "ok::fine" "$FIX/gh.log"
teardown

# 3d. 磁碟不足：不跑任何一段、寫 error（說明含「磁碟不足，未執行」與剩餘空間）、last-sha 不前進；
#     同一個 sha 連續幾輪不重複送 status；空間恢復後同一個 sha 會跑。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
export AGM_CI_MIN_FREE_GB=99999999
equals "磁碟不足 exit 0" "$(run)" "0"
check "送 error" "state=error" "$FIX/gh.log"
check "說明磁碟不足，未執行" "磁碟不足，未執行" "$FIX/gh.log"
check "說明剩餘空間與門檻" "剩 .*G，門檻 99999999G" "$FIX/gh.log"
check_no "沒送 pending" "state=pending" "$FIX/gh.log"
check "status.json 是 error／disk_low" '"reason":"disk_low"' "$CI/status.json"
equals "last-sha 沒前進" "$(cat "$CI/last-sha" 2>/dev/null)" ""
[ ! -e "$CI/logs/$SHA.log" ] && echo "ok   - 沒有開始跑（沒有 log）" && PASS=$((PASS + 1)) || { echo "FAIL - 不該產生 log"; FAIL=$((FAIL + 1)); }
: > "$FIX/gh.log"
equals "同 sha 下一輪 exit 0" "$(run)" "0"
equals "同一個 sha 不重複送 status" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
export AGM_CI_MIN_FREE_GB=0
equals "空間恢復後 exit 0" "$(run)" "0"
check "同一個 sha 這時才跑、送 success" "state=success" "$FIX/gh.log"
equals "last-sha 前進到這個 sha" "$(cat "$CI/last-sha")" "$SHA"
teardown

# 3e. 已安裝的 ops 腳本與 origin/main 的漂移檢查（#418 的 `agm ops-sync --check --alert`）：以前只有文件寫「巡檢每天跑一次」，
#     沒有任何東西在排程它（outbox-gc.sh 停在舊版沒人發現）。ubuntu-ci 每輪順手問一次、每 AGM_CI_OPS_SYNC_INTERVAL 秒最多一次；
#     只偵測回報，結果不影響 CI 的 status／last-sha，檢查壞掉也不能讓 CI 跟著壞。
setup
export AGM_CI_OPS_SYNC_INTERVAL=21600
equals "漂移檢查：第一輪 exit 0" "$(run)" "0"
check "跑的是 ops-sync --check --alert，對 CI clone" "ops-sync --check --alert --repo $CI/repo" "$FIX/agm.log"
check "CI 本身照常 success" "state=success" "$FIX/gh.log"
check "結果留在 ops-sync.json" '"in_sync"' "$CI/ops-sync.json"
equals "同一個間隔內不重跑（同 sha 也不重跑）" "$(run >/dev/null; wc -l < "$FIX/agm.log" | tr -d ' ')" "1"
echo 1 > "$CI/ops-sync.last"
(cd "$ROOT/work" && echo 2 > g && git add g && git commit -q -m c2 && git push -q origin main)
echo 1 > "$FIX/agm.rc"
: > "$FIX/gh.log"
equals "間隔過了再跑一次；有落差（agm exit 1）CI 仍 exit 0" "$(run)" "0"
equals "又問了一次" "$(wc -l < "$FIX/agm.log" | tr -d ' ')" "2"
check "有落差不影響這個 sha 的 CI 結果" "state=success" "$FIX/gh.log"
echo 1 > "$CI/ops-sync.last"
echo 9 > "$FIX/agm.rc"
: > "$FIX/gh.log"
equals "檢查本身壞掉（agm exit 9）CI 也 exit 0" "$(run)" "0"
equals "沒有新 sha 時過了間隔照樣會問（不靠新 commit）" "$(wc -l < "$FIX/agm.log" | tr -d ' ')" "3"
teardown
export AGM_CI_OPS_SYNC_INTERVAL=0

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
# #885：超時（KILL 後 rc=137）算中斷，先記 error 重跑；同一個 sha 連續 3 次都超時才是真的紅（一直卡住的測試不能永遠 error）。
check "第 1 次超時記成中斷 error" "中斷：web(rc=137)，會重跑" "$FIX/gh.log"
check_no "第 1 次超時不送 failure" "state=failure" "$FIX/gh.log"
equals "第 2 次超時也跑得完" "$(run)" "0"
check_no "第 2 次超時還不是 failure" "state=failure" "$FIX/gh.log"
equals "第 3 次超時也跑得完" "$(run)" "0"
check "連續 3 次超時的段記成紅" "連續中斷 3 次：紅：web" "$FIX/gh.log"
teardown

# 7. 殘留暫存清理（#763）：只刪 $TMPDIR 底下「超過 6 小時沒動、名字以 26 碼 ULID 結尾的 am-*／agm-*」。
#    不跟 symlink 出去、不刪裡面還有新檔的（行程可能還在用）、不誤中別人的目錄、$TMPDIR 是 HOME 或相對路徑就整個不動，
#    而且磁碟不足的那一輪也要清（以前清理排在磁碟預檢之後：正是被這些目錄塞滿時永遠清不到）。
U1=01ARZ3NDEKTSV4RRFFQ69G5FAV; U2=01ARZ3NDEKTSV4RRFFQ69G5FAW; U3=01ARZ3NDEKTSV4RRFFQ69G5FAX
U4=01ARZ3NDEKTSV4RRFFQ69G5FAY; U5=01ARZ3NDEKTSV4RRFFQ69G5FAZ; U6=01ARZ3NDEKTSV4RRFFQ69G5FB0
U7=Z1ARZ3NDEKTSV4RRFFQ69G5FAV # ULID overflow: the first base32 digit may only be 0 through 7.
OLD="$(date -d '8 hours ago' +%Y%m%d%H%M)"
setup_tmp() {
  T="$ROOT/tmp"; mkdir -p "$T"
  mkdir -p "$T/am-stale-$U1/sub" "$T/agm-$U2" "$T/am-busy-$U3/sub" "$T/am-fresh-$U4" "$T/am-ops-test" "$T/am-invalid-ulid-$U7/sub" "$T/herdr-$U5" "$T/claude-1000" "$T/not-am-$U6" "$ROOT/outside"
  echo keep > "$ROOT/outside/precious"; echo x > "$T/am-stale-$U1/sub/f"; echo busy > "$T/am-busy-$U3/sub/live"
  echo keep > "$T/am-invalid-ulid-$U7/sub/file"
  ln -s "$ROOT/outside" "$T/am-link-$U6"
  : > "$T/am-origin-$U5.jsonl"
  # 內容都先做好再把時間倒回去（最後才動目錄本身）；busy 的目錄自己很舊、裡面有新檔。
  find "$T" -mindepth 2 -exec touch -t "$OLD" {} +
  touch -t "$(date +%Y%m%d%H%M)" "$T/am-busy-$U3/sub/live"
  for d in am-stale-$U1 agm-$U2 am-busy-$U3 am-invalid-ulid-$U7 am-ops-test herdr-$U5 claude-1000 not-am-$U6 am-origin-$U5.jsonl; do touch -t "$OLD" "$T/$d"; done
  touch -h -t "$OLD" "$T/am-link-$U6"
  export TMPDIR="$T"
}
exists() { [ -e "$1" ] || [ -L "$1" ]; }
gone() { if exists "$2"; then echo "FAIL - $1（還在）"; FAIL=$((FAIL + 1)); else echo "ok   - $1"; PASS=$((PASS + 1)); fi; }
kept() { if exists "$2"; then echo "ok   - $1"; PASS=$((PASS + 1)); else echo "FAIL - $1（被刪了）"; FAIL=$((FAIL + 1)); fi; }
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
setup_tmp
equals "清理那輪 exit 0" "$(run)" "0"
gone "超過 6 小時的 am-*-ULID 目錄被清掉" "$T/am-stale-$U1"
gone "超過 6 小時的 agm-ULID 目錄被清掉" "$T/agm-$U2"
gone "超過 6 小時的 am-*-ULID.jsonl 殘留檔被清掉" "$T/am-origin-$U5.jsonl"
kept "目錄自己很舊但裡面有新檔：行程可能還在用，不刪" "$T/am-busy-$U3/sub/live"
kept "新的目錄不刪" "$T/am-fresh-$U4"
kept "不是 ULID 結尾的 am-* 不刪" "$T/am-ops-test"
kept "超出 ULID 時間範圍的首字元不當成 ULID 刪除" "$T/am-invalid-ulid-$U7"
kept "不是 am-／agm- 開頭的不刪（herdr）" "$T/herdr-$U5"
kept "不刪 claude-*" "$T/claude-1000"
kept "名字只是含 am-、不是開頭的不刪" "$T/not-am-$U6"
kept "symlink 指向的目錄沒被跟進去刪" "$ROOT/outside/precious"
kept "symlink 本身也不動（只處理真的目錄與檔案）" "$T/am-link-$U6"
unset TMPDIR; teardown

# 7b. 磁碟不足的那一輪也要清（清完才量空間；不然被塞滿時永遠清不到）。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
setup_tmp
export AGM_CI_MIN_FREE_GB=99999999
equals "磁碟不足 exit 0" "$(run)" "0"
check "仍然是磁碟不足、沒跑" "state=error" "$FIX/gh.log"
gone "磁碟不足那一輪照樣清掉過期殘留" "$T/am-stale-$U1"
kept "磁碟不足那一輪也不誤刪新的" "$T/am-fresh-$U4"
export AGM_CI_MIN_FREE_GB=0
unset TMPDIR; teardown

# 7c. $TMPDIR 是 HOME 或相對路徑：整個不動（設錯就是寫錯目錄，不能照字面去刪）。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
setup_tmp
mkdir -p "$HOME/am-home-$U1"; touch -t "$OLD" "$HOME/am-home-$U1"
export TMPDIR="$HOME"
equals "TMPDIR=HOME exit 0" "$(run)" "0"
kept "TMPDIR 是 HOME：不清" "$HOME/am-home-$U1"
(cd "$ROOT" && export TMPDIR=tmp && run >/dev/null)
kept "TMPDIR 是相對路徑：不清" "$T/am-stale-$U1"
unset TMPDIR; teardown

# 7d. TMPDIR 不得藉由 symlink 或 HOME 子目錄把清理範圍導到使用者檔案。
setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
setup_tmp
ln -s "$T" "$ROOT/tmp-link"
export TMPDIR="$ROOT/tmp-link"
equals "TMPDIR 是 symlink 時 exit 0" "$(run)" "0"
kept "不沿 TMPDIR symlink 刪除目標中的暫存目錄" "$T/am-stale-$U1"
kept "TMPDIR symlink 本身不動" "$ROOT/tmp-link"
unset TMPDIR; teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
mkdir -p "$HOME/tmp"
mkdir -p "$HOME/tmp/am-home-child-$U1/sub"
echo keep > "$HOME/tmp/am-home-child-$U1/sub/file"
find "$HOME/tmp" -mindepth 2 -exec touch -t "$OLD" {} +
touch -t "$OLD" "$HOME/tmp/am-home-child-$U1"
export TMPDIR="$HOME/tmp"
equals "TMPDIR 在 HOME 底下時 exit 0" "$(run)" "0"
kept "不清理 HOME 子目錄裡的舊 ULID 暫存" "$HOME/tmp/am-home-child-$U1"
unset TMPDIR; teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
mkdir -p "$ROOT/am-root-child-$U1/sub"
echo keep > "$ROOT/am-root-child-$U1/sub/file"
find "$ROOT/am-root-child-$U1" -exec touch -t "$OLD" {} +
export TMPDIR="$ROOT"
equals "TMPDIR 是 HOME 祖先時 exit 0" "$(run)" "0"
kept "不清理 HOME 祖先底下的同名 ULID 暫存" "$ROOT/am-root-child-$U1"
unset TMPDIR; teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
mkdir -p "$ROOT/am-root-home-$U1/sub"
echo keep > "$ROOT/am-root-home-$U1/sub/file"
find "$ROOT/am-root-home-$U1" -exec touch -t "$OLD" {} +
export TMPDIR="$ROOT" HOME=/
equals "HOME=/ 時清理 exit 0" "$(run)" "0"
kept "HOME=/ 時不清理任何暫存路徑" "$ROOT/am-root-home-$U1"
unset TMPDIR; teardown

# 8. 最後一則 commit status 暫時寫不上去（GitHub 連不上）：以前只印一行就算了、last-sha 照樣前進，
#    這個 sha 在 GitHub 上永遠停在 pending（status.json 卻說 success）。現在先重試，還不行就記下來，下一輪補送（不重跑）。
setup
echo 4 > "$FIX/gh.failn"   # 前 4 次呼叫失敗：pending 那則整個送不出去、最後的 success 第一次也失敗，靠重試補上
equals "status 寫不上去時 CI 本身 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
equals "last-sha 照常前進（結果是真的跑出來的）" "$(cat "$CI/last-sha")" "$SHA"
check "最後的 success 有送到（重試或補送）" "state=success" "$FIX/gh.log"
teardown

setup
echo 99 > "$FIX/gh.failn"   # 整輪 GitHub 都連不上
equals "GitHub 整輪連不上 CI 也 exit 0" "$(run)" "0"
SHA1=$(git -C "$ROOT/work" rev-parse HEAD)
check "結果記下來等補送" "$SHA1\|success\|" "$CI/unposted"
[ -s "$CI/unposted" ] && echo "ok   - 有 unposted 記錄" && PASS=$((PASS + 1)) || { echo "FAIL - 沒有 unposted 記錄"; FAIL=$((FAIL + 1)); }
check_no "GitHub 沒收到任何 status" "state=" "$FIX/gh.log"

# Another commit can finish while GitHub is still unavailable. Keep both completed results queued;
# overwriting the single unposted row makes the earlier tested SHA stay pending forever.
(cd "$ROOT/work" && echo 2 > f && git add f && git commit -q -m c2 && git push -q origin main)
equals "第二個離線 SHA 的 CI exit 0" "$(run)" "0"
SHA2=$(git -C "$ROOT/work" rev-parse HEAD)
check "unposted 保留第一個已測 SHA" "$SHA1\|success\|" "$CI/unposted"
check "unposted 也記下第二個已測 SHA" "$SHA2\|success\|" "$CI/unposted"

echo 0 > "$FIX/gh.failn"
equals "GitHub 回來後下一輪 exit 0" "$(run)" "0"
check "補送了 success（同一個 sha，不重跑）" "state=success" "$FIX/gh.log"
check "補送第一個 SHA" "statuses/$SHA1" "$FIX/gh.log"
check "補送第二個 SHA" "statuses/$SHA2" "$FIX/gh.log"
[ ! -e "$CI/unposted" ] && echo "ok   - 補送完清掉記錄" && PASS=$((PASS + 1)) || { echo "FAIL - unposted 還在"; FAIL=$((FAIL + 1)); }
equals "沒有重跑第二個 SHA 的檢查（log 只有一份 daemon 結尾）" "$(grep -c '\[ubuntu-ci\] daemon rc=' "$CI/logs/$SHA2.log")" "1"
teardown

# A delayed status for a SHA must not overwrite a newer result for the same SHA. Re-run the
# current SHA while its older queued failure cannot post; the fresh success should retire that row.
setup
BASE_SHA=$(git -C "$ROOT/work" rev-parse HEAD)
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
mkdir -p "$CI"
echo "$BASE_SHA" > "$CI/last-sha"
printf '%s\tfailure\told offline result\n' "$SHA" > "$CI/unposted"
echo 3 > "$FIX/gh.failn"   # 補送舊 failure 的三次 retry 失敗；本輪的新 pending/success 可以寫入
equals "同 SHA 重跑時新結果 exit 0" "$(run)" "0"
check "同 SHA 的新 success 有送出" "statuses/$SHA.*state=success" "$FIX/gh.log"
equals "下一輪不再補送被新 success 取代的舊 failure" "$(run)" "0"
check_no "最後的 commit status 沒被舊 failure 倒灌" "statuses/$SHA.*state=failure" "$FIX/gh.log"
[ ! -e "$CI/unposted" ] && echo "ok   - 同 SHA 的舊結果已清除" && PASS=$((PASS + 1)) || { echo "FAIL - 同 SHA 的舊結果還在"; FAIL=$((FAIL + 1)); }
teardown

# 9. #885：段落被訊號收掉（TERM＝143）、log 沒有任何測試紅的證據 → 不是這個 commit 的錯。記 error、不前進 last-sha、不算 failure，
#    同一個 sha 下一輪重跑；連續 3 次才當 failure。
setup
echo 143 > "$FIX/rc.daemon"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
equals "第 1 次中斷 exit 0" "$(run)" "0"
check "中斷送 error" "state=error" "$FIX/gh.log"
check "description 點名中斷的段與 rc" "中斷：daemon(rc=143)，會重跑" "$FIX/gh.log"
check_no "中斷不送 failure" "state=failure" "$FIX/gh.log"
check "status.json 是 error／interrupted" '"reason":"interrupted"' "$CI/status.json"
check "status.json state 是 error" '"state":"error"' "$CI/status.json"
[ ! -e "$CI/last-sha" ] && echo "ok   - 中斷不寫 last-sha" && PASS=$((PASS + 1)) || { echo "FAIL - 中斷不該寫 last-sha"; FAIL=$((FAIL + 1)); }
equals "中斷計數是 1" "$(cat "$CI/interrupted-$SHA")" "1"
equals "第 2 次中斷 exit 0（同一個 sha 重跑）" "$(run)" "0"
equals "中斷計數是 2" "$(cat "$CI/interrupted-$SHA")" "2"
check_no "兩次中斷都還不算 failure" "state=failure" "$FIX/gh.log"
equals "第 3 次中斷 exit 0" "$(run)" "0"
check "第 3 次當 failure" "state=failure" "$FIX/gh.log"
check "description 有連續中斷前綴" "連續中斷 3 次：紅：daemon" "$FIX/gh.log"
equals "failure 寫 last-sha" "$(cat "$CI/last-sha")" "$SHA"
[ ! -e "$CI/interrupted-$SHA" ] && echo "ok   - 計數檔已刪" && PASS=$((PASS + 1)) || { echo "FAIL - 計數檔還在"; FAIL=$((FAIL + 1)); }
teardown

# 9b. 中斷之後正常跑完：計數檔要刪掉（下次同一個 sha 再中斷不能接著舊計數）。
setup
echo 143 > "$FIX/rc.daemon"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
run >/dev/null
echo 0 > "$FIX/rc.daemon"
equals "恢復後 exit 0" "$(run)" "0"
check "恢復後 success" "state=success" "$FIX/gh.log"
[ ! -e "$CI/interrupted-$SHA" ] && echo "ok   - success 刪掉計數檔" && PASS=$((PASS + 1)) || { echo "FAIL - success 後計數檔還在"; FAIL=$((FAIL + 1)); }
equals "success 寫 last-sha" "$(cat "$CI/last-sha")" "$SHA"
teardown

# 9c. 有真的測試紅（`test … FAILED`／`FAIL - `）時，即使有段落 exit 143 也是 failure，不當中斷重跑。
setup
printf 'test a::b ... FAILED\n' > "$FIX/title.daemon"
echo 143 > "$FIX/rc.daemon"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
equals "測試紅又 143 exit 0" "$(run)" "0"
check "直接 failure" "state=failure" "$FIX/gh.log"
check_no "不送 error" "state=error" "$FIX/gh.log"
equals "failure 寫 last-sha" "$(cat "$CI/last-sha")" "$SHA"
[ ! -e "$CI/interrupted-$SHA" ] && echo "ok   - 沒有計數檔" && PASS=$((PASS + 1)) || { echo "FAIL - 不該有計數檔"; FAIL=$((FAIL + 1)); }
teardown
setup
printf 'FAIL - some shell case\n' > "$FIX/title.ops"
echo 143 > "$FIX/rc.ops"
run >/dev/null
check "ops 的 FAIL - 行也算證據" "state=failure" "$FIX/gh.log"
teardown

# 9d. 另一段是普通失敗（rc=1）又有一段被中斷：真的紅，不重跑。
setup
echo 143 > "$FIX/rc.daemon"
echo 1 > "$FIX/rc.web"
run >/dev/null
check "普通失敗混中斷仍是 failure" "state=failure" "$FIX/gh.log"
teardown

echo "ubuntu-ci_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
