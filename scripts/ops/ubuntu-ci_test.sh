#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
if ! command -v flock >/dev/null 2>&1; then echo "skip - 沒有 flock（ubuntu-ci 只在 Linux 跑）"; exit 0; fi

setup() {
  ROOT=$(mktemp -d)
  export HOME="$ROOT/home"; mkdir -p "$HOME"
  export GIT_CONFIG_GLOBAL="$ROOT/gitconfig" GIT_CONFIG_SYSTEM=/dev/null
  git config --global user.email t@t; git config --global user.name t; git config --global init.defaultBranch main
  git init -q --bare "$ROOT/origin.git"
  git clone -q "$ROOT/origin.git" "$ROOT/work" 2>/dev/null
  mkdir -p "$ROOT/work/scripts"
  cp "$HERE/../ci-changed-parts.sh" "$ROOT/work/scripts/ci-changed-parts.sh"
  cp "$HERE/../ci-rules.tsv" "$ROOT/work/scripts/ci-rules.tsv"
  cat > "$ROOT/work/scripts/check.sh" <<'CK'
#!/bin/bash
part="$1"
echo "==> fake $part"
[ -e "$FIX/title.$part" ] && cat "$FIX/title.$part"
if [ -e "$FIX/hang.$part" ]; then trap '' TERM; sleep 30 & wait; fi
exit "$(cat "$FIX/rc.$part" 2>/dev/null || echo 0)"
CK
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
  cat > "$ROOT/bin/cargo" <<'CARGO'
#!/bin/bash
echo "AM_TEST_REAL_HERDR=${AM_TEST_REAL_HERDR:-} AM_REAL_HERDR=${AM_REAL_HERDR:-} $*" >> "$FIX/cargo.log"
rc_file="$FIX/cargo.rc"
case " $* " in
  *" the_generated_argv_parses_with_the_real_herdr "*)
    [ -f "$FIX/parser.rc" ] && rc_file="$FIX/parser.rc" ;;
esac
exit "$(cat "$rc_file" 2>/dev/null || echo 0)"
CARGO
  chmod +x "$ROOT/bin/cargo"
  cat > "$ROOT/bin/herdr" <<'HERDR'
#!/bin/bash
exit 0
HERDR
  chmod +x "$ROOT/bin/herdr"
  export PATH="$ROOT/bin:$PATH"
  export AGM_CI_WEB_BUNDLE_CMD='echo bundle >> "$FIX/bundle.log"; mkdir -p web/dist && echo x > web/dist/index.html; exit "$(cat "$FIX/bundle.rc" 2>/dev/null || echo 0)"'
  unset AGM_CI_FULL_INTERVAL
  export AGM_CI_DEPLOY_DIR="$ROOT/agm"; mkdir -p "$AGM_CI_DEPLOY_DIR"; unset AGM_CI_DEPLOY_WAIT AGM_CI_DEPLOY_POLL
  export AGM_CI_ROOT="$ROOT/ci" AGM_CI_REPO_URL="$ROOT/origin.git" AGM_CI_GH_REPO=o/r AGM_CI_TIMEOUT=30s AGM_CI_KILL_AFTER=1s AGM_CI_MIN_FREE_GB=0
  CI="$AGM_CI_ROOT"; : > "$FIX/gh.log"; : > "$FIX/gh.attempts"; export AGM_CI_STATUS_RETRY_SLEEP=0
}
teardown() { rm -rf "$ROOT"; }
run() { bash "$HERE/ubuntu-ci.sh" >"$ROOT/out" 2>&1; echo $?; }

setup
equals "全綠 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
check "先送 pending" "state=pending" "$FIX/gh.log"
check "最後送 success" "state=success" "$FIX/gh.log"
check "status.json 是 success" '"state":"success"' "$CI/status.json"
equals "last-sha 記下這個 sha" "$(cat "$CI/last-sha")" "$SHA"
check "log 有四段" "\[ubuntu-ci\] daemon rc=0" "$CI/logs/$SHA.log"

: > "$FIX/gh.log"
equals "同 sha 再叫一次 exit 0" "$(run)" "0"
equals "沒有任何 gh 呼叫" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"

(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
echo 3 > "$FIX/rc.ops"
: > "$FIX/gh.log"
equals "有一段紅仍 exit 0（結果在 status）" "$(run)" "0"
check "送 failure" "state=failure" "$FIX/gh.log"
check "description 點名 ops" "紅：ops" "$FIX/gh.log"
check "status.json 記 rc=3" '"rc":3' "$CI/status.json"
check "其他段照樣跑完" "\[ubuntu-ci\] daemon rc=0" "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
teardown

setup
export AM_REAL_HERDR=""
equals "只有 PATH shim 時全綠" "$(run)" "0"
check_no "只有 PATH shim 時略過真 herdr parser 測試" "the_generated_argv_parses_with_the_real_herdr" "$FIX/cargo.log"
export AM_REAL_HERDR=/bin/true
echo 1 > "$ROOT/work/next"
(cd "$ROOT/work" && git add next && git commit -q -m c2 && git push -q origin main)
equals "找到真 herdr binary 時全綠" "$(run)" "0"
check "只對 argv parser 測試設 AM_TEST_REAL_HERDR=1" "AM_TEST_REAL_HERDR=1 AM_REAL_HERDR=/bin/true test -p agents-managerd --locked -- the_generated_argv_parses_with_the_real_herdr" "$FIX/cargo.log"
check_no "不啟用會連正式 pane 的 delivery 測試" "the_scan_source_is_one_real_herdr_accepts" "$FIX/cargo.log"
teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
printf '==> step "q" C:\\dir\\x\n' > "$FIX/title.ops"
echo 1 > "$FIX/rc.ops"
run >/dev/null
equals "status.json 是合法 JSON" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$CI/status.json" 2>&1)" "failure"
check "description 保留了反斜線之後的字" "dir" "$CI/status.json"
teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
printf '==> daemon: cargo test -p agents-managerd\ntest ok::fine ... ok\ntest cli_update::tests::flaky_one ... FAILED\ntest other::tests::flaky_two ... FAILED\n' > "$FIX/title.daemon"
echo 101 > "$FIX/rc.daemon"
run >/dev/null
check "description 點名第一條紅的測試" "cli_update::tests::flaky_one" "$FIX/gh.log"
check "description 說明還有幾條" "2 條測試紅" "$FIX/gh.log"
check_no "綠的測試不進 description" "ok::fine" "$FIX/gh.log"
teardown

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

setup
mkdir -p "$CI"
( exec 9>"$CI/lock"; flock -n 9; sleep 5 ) &
HOLDER=$!
sleep 0.5
equals "鎖被占著 exit 0" "$(run)" "0"
equals "沒有 gh 呼叫" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null
teardown

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

setup
: > "$FIX/hang.web"
export AGM_CI_TIMEOUT=1s
start=$(date +%s)
equals "超時的段也跑得完" "$(run)" "0"
elapsed=$(( $(date +%s) - start ))
equals "沒有被卡住（遠小於 sleep 30）" "$([ "$elapsed" -lt 20 ] && echo fast)" "fast"
check "第 1 次超時記成中斷 error" "中斷：web(rc=137)，會重跑" "$FIX/gh.log"
check_no "第 1 次超時不送 failure" "state=failure" "$FIX/gh.log"
equals "第 2 次超時也跑得完" "$(run)" "0"
check_no "第 2 次超時還不是 failure" "state=failure" "$FIX/gh.log"
equals "第 3 次超時也跑得完" "$(run)" "0"
check "連續 3 次超時的段記成紅" "連續中斷 3 次：紅：web" "$FIX/gh.log"
teardown

U1=01ARZ3NDEKTSV4RRFFQ69G5FAV; U2=01ARZ3NDEKTSV4RRFFQ69G5FAW; U3=01ARZ3NDEKTSV4RRFFQ69G5FAX
U4=01ARZ3NDEKTSV4RRFFQ69G5FAY; U5=01ARZ3NDEKTSV4RRFFQ69G5FAZ; U6=01ARZ3NDEKTSV4RRFFQ69G5FB0
U7=Z1ARZ3NDEKTSV4RRFFQ69G5FAV
OLD="$(date -d '8 hours ago' +%Y%m%d%H%M)"
setup_tmp() {
  T="$ROOT/tmp"; mkdir -p "$T"
  mkdir -p "$T/am-stale-$U1/sub" "$T/agm-$U2" "$T/am-busy-$U3/sub" "$T/am-fresh-$U4" "$T/am-ops-test" "$T/am-invalid-ulid-$U7/sub" "$T/herdr-$U5" "$T/claude-1000" "$T/not-am-$U6" "$ROOT/outside"
  echo keep > "$ROOT/outside/precious"; echo x > "$T/am-stale-$U1/sub/f"; echo busy > "$T/am-busy-$U3/sub/live"
  echo keep > "$T/am-invalid-ulid-$U7/sub/file"
  ln -s "$ROOT/outside" "$T/am-link-$U6"
  : > "$T/am-origin-$U5.jsonl"
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

setup
echo 4 > "$FIX/gh.failn"
equals "status 寫不上去時 CI 本身 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
equals "last-sha 照常前進（結果是真的跑出來的）" "$(cat "$CI/last-sha")" "$SHA"
check "最後的 success 有送到（重試或補送）" "state=success" "$FIX/gh.log"
teardown

setup
echo 99 > "$FIX/gh.failn"
equals "GitHub 整輪連不上 CI 也 exit 0" "$(run)" "0"
SHA1=$(git -C "$ROOT/work" rev-parse HEAD)
check "結果記下來等補送" "$SHA1\|success\|" "$CI/unposted"
[ -s "$CI/unposted" ] && echo "ok   - 有 unposted 記錄" && PASS=$((PASS + 1)) || { echo "FAIL - 沒有 unposted 記錄"; FAIL=$((FAIL + 1)); }
check_no "GitHub 沒收到任何 status" "state=" "$FIX/gh.log"

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

setup
BASE_SHA=$(git -C "$ROOT/work" rev-parse HEAD)
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
mkdir -p "$CI"
echo "$BASE_SHA" > "$CI/last-sha"
printf '%s\tfailure\told offline result\n' "$SHA" > "$CI/unposted"
echo 3 > "$FIX/gh.failn"
equals "同 SHA 重跑時新結果 exit 0" "$(run)" "0"
check "同 SHA 的新 success 有送出" "statuses/$SHA.*state=success" "$FIX/gh.log"
equals "下一輪不再補送被新 success 取代的舊 failure" "$(run)" "0"
check_no "最後的 commit status 沒被舊 failure 倒灌" "statuses/$SHA.*state=failure" "$FIX/gh.log"
[ ! -e "$CI/unposted" ] && echo "ok   - 同 SHA 的舊結果已清除" && PASS=$((PASS + 1)) || { echo "FAIL - 同 SHA 的舊結果還在"; FAIL=$((FAIL + 1)); }
teardown

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

setup
echo 143 > "$FIX/rc.daemon"
echo 1 > "$FIX/rc.web"
run >/dev/null
check "普通失敗混中斷仍是 failure" "state=failure" "$FIX/gh.log"
teardown

setup
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
printf '==> daemon: cargo test\nerror: No space left on device (os error 28)\ntest a::b ... FAILED\n' > "$FIX/title.daemon"
echo 1 > "$FIX/rc.daemon"
equals "磁碟滿第 1 次 exit 0" "$(run)" "0"
check "送 error" "state=error" "$FIX/gh.log"
check "說明磁碟滿、會重跑" "磁碟滿：.*會重跑" "$FIX/gh.log"
check_no "不送 failure" "state=failure" "$FIX/gh.log"
check "status.json 是 error／disk_full" '"reason":"disk_full"' "$CI/status.json"
[ ! -e "$CI/last-sha" ] && echo "ok   - 磁碟滿不寫 last-sha" && PASS=$((PASS + 1)) || { echo "FAIL - 磁碟滿不該寫 last-sha"; FAIL=$((FAIL + 1)); }
equals "計數是 1" "$(cat "$CI/interrupted-$SHA")" "1"
equals "磁碟滿第 2 次 exit 0（同一個 sha 重跑）" "$(run)" "0"
equals "計數是 2" "$(cat "$CI/interrupted-$SHA")" "2"
check_no "兩次都還不是 failure" "state=failure" "$FIX/gh.log"
equals "磁碟滿第 3 次 exit 0" "$(run)" "0"
check "連續 3 次才當 failure" "連續磁碟滿 3 次：紅：daemon" "$FIX/gh.log"
equals "failure 寫 last-sha" "$(cat "$CI/last-sha")" "$SHA"
[ ! -e "$CI/interrupted-$SHA" ] && echo "ok   - 計數檔已刪" && PASS=$((PASS + 1)) || { echo "FAIL - 計數檔還在"; FAIL=$((FAIL + 1)); }
teardown

setup
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
printf '!! 失敗時磁碟只剩 1G\n' > "$FIX/title.ops"
echo 1 > "$FIX/rc.ops"
run >/dev/null
check "check.sh 的磁碟證據：error／disk_full" '"reason":"disk_full"' "$CI/status.json"
check_no "不送 failure" "state=failure" "$FIX/gh.log"
teardown

setup
echo 1 > "$FIX/rc.web"
export AGM_CI_MIN_FREE_GB=0
cat > "$ROOT/bin/df" <<'DF'
#!/bin/sh
# 預檢那一次（log 還不存在）回很多、跑完之後回 1 KB。
if [ -e "$AGM_CI_ROOT/logs/$(git -C "$AGM_CI_ROOT/repo" rev-parse HEAD 2>/dev/null).log" ] && [ -s "$AGM_CI_ROOT/logs/$(git -C "$AGM_CI_ROOT/repo" rev-parse HEAD).log" ]; then
  printf 'Filesystem 1024-blocks Used Available Capacity Mounted\nx 100 99 1 99%% /\n'
else
  printf 'Filesystem 1024-blocks Used Available Capacity Mounted\nx 999999999 1 999999998 1%% /\n'
fi
DF
chmod +x "$ROOT/bin/df"
export AGM_CI_MIN_FREE_GB=5
run >/dev/null
check "跑完磁碟已低於門檻：error／disk_full" '"reason":"disk_full"' "$CI/status.json"
check_no "不送 failure" "state=failure" "$FIX/gh.log"
export AGM_CI_MIN_FREE_GB=0
teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
mkdir -p "$CI"; mkdir "$CI/status.json"
export AGM_CI_MIN_FREE_GB=99999999
equals "status.json 寫不進去也 exit 0" "$(run)" "0"
equals "disk-low-sha 照樣記下" "$(cat "$CI/disk-low-sha")" "$SHA"
: > "$FIX/gh.log"
equals "下一輪 exit 0" "$(run)" "0"
equals "同一個 sha 不重複送 error" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
export AGM_CI_MIN_FREE_GB=0
teardown

setup
REAL_GIT=$(command -v git)
cat > "$ROOT/fakegit" <<GIT
#!/bin/bash
[ "\$1" = fetch ] && { sleep 30; exit 0; }
exec "$REAL_GIT" "\$@"
GIT
chmod +x "$ROOT/fakegit"
export AGM_CI_GIT_BIN="$ROOT/fakegit" AGM_CI_FETCH_TIMEOUT=1
start=$(date +%s)
rc=$(run)
elapsed=$(( $(date +%s) - start ))
equals "fetch 逾時：整輪以非零結束" "$([ "$rc" != 0 ] && echo nonzero)" "nonzero"
equals "5 秒內結束（不是等 30 秒）" "$([ "$elapsed" -lt 5 ] && echo fast)" "fast"
equals "鎖已放掉、可以再拿到" "$( ( exec 9>"$CI/lock"; flock -n 9 && echo got ) )" "got"
check_no "沒有送出任何 status（還沒開始驗）" "state=" "$FIX/gh.log"
[ -s "$CI/running-since" ] && echo "ok   - 拿到鎖後有寫 running-since" && PASS=$((PASS + 1)) || { echo "FAIL - 沒有 running-since"; FAIL=$((FAIL + 1)); }
unset AGM_CI_GIT_BIN AGM_CI_FETCH_TIMEOUT
teardown

setup
git clone -q "$ROOT/origin.git" "$CI/repo" 2>/dev/null
mkdir -p "$CI"
( exec 9>"$CI/lock"; flock -n 9; sleep 6 ) &
HOLDER=$!
sleep 0.5
date +%s > "$CI/running-since"
equals "占鎖還沒超過門檻 exit 0" "$(run)" "0"
[ ! -e "$FIX/agm.log" ] && echo "ok   - 還沒超過門檻：不喊人" && PASS=$((PASS + 1)) || { echo "FAIL - 不該喊人"; FAIL=$((FAIL + 1)); }
echo 1 > "$CI/running-since"
equals "占鎖超過門檻 exit 0" "$(run)" "0"
check "喊 ops-alert ci_hung" "ops-alert --source ubuntu-ci --reason ci_hung" "$FIX/agm.log"
equals "沒有 gh 呼叫（沒碰 status）" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null
teardown

setup
mkdir -p "$CI"
( exec 9>"$CI/lock"; flock -n 9; sleep 4 ) &
HOLDER=$!
sleep 0.5
echo 1 > "$CI/running-since"
equals "clone 還沒有：不喊也 exit 0" "$(run)" "0"
git clone -q "$ROOT/origin.git" "$CI/repo" 2>/dev/null
echo 9 > "$FIX/agm.rc"
equals "喊人失敗（agm.py exit 9）仍 exit 0" "$(run)" "0"
check "有嘗試喊" "ci_hung" "$FIX/agm.log"
kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null
teardown

setup
NAME86=$(printf 'a%.0s' $(seq 1 86))
printf 'test %s ... FAILED\n' "$NAME86" > "$FIX/title.daemon"
echo 1 > "$FIX/rc.daemon"
equals "紅燈仍 exit 0" "$(run)" "0"
equals "status.json 是合法 UTF-8" "$(python3 -c 'import sys; open(sys.argv[1],"rb").read().decode("utf-8"); print("ok")' "$CI/status.json" 2>/dev/null)" "ok"
equals "送出去的 description 是合法 UTF-8" "$(python3 -c 'import sys; open(sys.argv[1],"rb").read().decode("utf-8"); print("ok")' "$FIX/gh.log" 2>/dev/null)" "ok"
check "description 完整（103 個字，不到 120，不該被截）" "條測試紅（${NAME86}）" "$CI/status.json"
teardown

parts_ok() { python3 -c 'import json,sys
p = json.load(open(sys.argv[1])).get("parts")
ok = isinstance(p, dict) and set(p) == {"ob", "ops", "web", "daemon"} and all(type(v) is int and v >= 0 for v in p.values())
print("ok" if ok else "bad")' "$1" 2>/dev/null; }
setup
equals "全綠 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
check 'log 的 ob 段有 secs（綠）' '^==> \[ubuntu-ci\] ob rc=0 secs=[0-9][0-9]*$' "$CI/logs/$SHA.log"
check 'log 的 daemon 段有 secs（綠）' '^==> \[ubuntu-ci\] daemon rc=0 secs=[0-9][0-9]*$' "$CI/logs/$SHA.log"
equals "綠：status.json 有四段 parts" "$(parts_ok "$CI/status.json")" "ok"
teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
echo 1 > "$FIX/rc.web"
equals "紅段仍 exit 0" "$(run)" "0"
SHA=$(git -C "$ROOT/work" rev-parse HEAD)
check 'log 的紅段（web）也有 secs' '^==> \[ubuntu-ci\] web rc=1 secs=[0-9][0-9]*$' "$CI/logs/$SHA.log"
check "紅：description 仍是既有格式（點名紅的段）" "紅：web" "$FIX/gh.log"
equals "紅：status.json 有四段 parts" "$(parts_ok "$CI/status.json")" "ok"
teardown

setup
(cd "$ROOT/work" && echo 1 > f && git add f && git commit -q -m c2 && git push -q origin main)
echo 124 > "$FIX/rc.ops"
equals "中斷（rc=124）仍 exit 0" "$(run)" "0"
check "中斷：error 的 reason 是 interrupted" '"reason":"interrupted"' "$CI/status.json"
equals "中斷：status.json 也有四段 parts" "$(parts_ok "$CI/status.json")" "ok"
teardown

setup
equals "沒有完整基準先全跑" "$(run)" "0"
BASE=$(git -C "$ROOT/work" rev-parse HEAD)
equals "全綠建立完整基準" "$(cat "$CI/full-success-sha")" "$BASE"
push_file() {
  mkdir -p "$(dirname "$ROOT/work/$1")"
  printf '%s\n' "$2" > "$ROOT/work/$1"
  (cd "$ROOT/work" && git add -A && git commit -qm "$2" && git push -q origin main)
}
push_file docs/review.md docs
: > "$FIX/gh.log"
equals "純文件 exit 0" "$(run)" "0"
DOCS=$(git -C "$ROOT/work" rev-parse HEAD)
check "純文件送 success 並標記 skip" 'state=success.*docs-only skip' "$FIX/gh.log"
check_no "純文件沒有跑四段" '==> fake' "$CI/logs/$DOCS.log"
equals "skip 不前進完整基準" "$(cat "$CI/full-success-sha")" "$BASE"
equals "skip 更新 last-sha" "$(cat "$CI/last-sha")" "$DOCS"
push_file docs/review.md more-docs
equals "連續文件 SHA 沿用完整基準" "$(run)" "0"
equals "完整基準仍不前進" "$(cat "$CI/full-success-sha")" "$BASE"
push_file web/src/change.ts code
echo 1 > "$FIX/rc.web"
run >/dev/null
equals "紅燈不前進完整基準" "$(cat "$CI/full-success-sha")" "$BASE"
push_file docs/review.md mixed
run >/dev/null
MIXED=$(git -C "$ROOT/work" rev-parse HEAD)
check "文件提交仍重驗前一個紅的 web" '==> fake web' "$CI/logs/$MIXED.log"
check_no "累積差異與 daemon 無關：daemon 沿用" '==> fake daemon' "$CI/logs/$MIXED.log"
check "文件不能掩蓋前一個 code 紅燈" '"state":"failure"' "$CI/status.json"
rm "$FIX/rc.web"
push_file unknown-input opaque
run >/dev/null
UNKNOWN=$(git -C "$ROOT/work" rev-parse HEAD)
check "未知路徑仍全跑" '==> fake daemon' "$CI/logs/$UNKNOWN.log"
equals "完整成功才前進基準" "$(cat "$CI/full-success-sha")" "$UNKNOWN"
UNRELATED=$(printf unrelated | git -C "$ROOT/work" commit-tree "$(git -C "$ROOT/work" rev-parse HEAD^{tree})")
printf '%s\n' "$UNRELATED" > "$CI/full-success-sha"
push_file docs/review.md unrelated
run >/dev/null
check "非祖先基準仍全跑" '==> fake daemon' "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
printf '%040d\n' 0 > "$CI/full-success-sha"
push_file docs/review.md missing-base
run >/dev/null
check "缺基準 object 仍全跑" '==> fake daemon' "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
(cd "$ROOT/work" && git mv web/src/change.ts docs/moved.md && git commit -qm rename && git push -q origin main)
run >/dev/null
check "程式改名到 docs 仍重驗被改名的 web 程式" '==> fake web' "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
push_file scripts/ci-changed-parts.sh '#!/bin/bash
exit 1'
run >/dev/null
push_file docs/review.md classifier-failed
run >/dev/null
check "分類器失敗仍全跑" '==> fake daemon' "$CI/logs/$(git -C "$ROOT/work" rev-parse HEAD).log"
teardown

push_file() {
  mkdir -p "$(dirname "$ROOT/work/$1")"
  printf '%s\n' "$2" > "$ROOT/work/$1"
  (cd "$ROOT/work" && git add -A && git commit -qm "$2" && git push -q origin main)
}
cur() { git -C "$ROOT/work" rev-parse HEAD; }
ran() { grep -c "^==> fake $1$" "$CI/logs/$(cur).log"; }
json_get() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1]))
for k in sys.argv[2].split("."): d=d[k]
print(d)' "$CI/status.json" "$1" 2>&1; }
new_baseline() { setup; run >/dev/null; BASE=$(cur); B8=${BASE:0:8}; }

new_baseline
equals "完整四段建立各段基準" "$(cat "$CI/part-success/daemon")" "$BASE"
rm -f "$FIX/bundle.log"
push_file web/src/a.ts web1
: > "$FIX/gh.log"
equals "web-only exit 0" "$(run)" "0"
equals "只跑 web" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1000"
check "description 是 scoped web（字面 scoped 前綴）" "state=success.*scoped web（沿用完整成功基準 ${B8}）" "$FIX/gh.log"
equals "部分成功不前進完整基準" "$(cat "$CI/full-success-sha")" "$BASE"
equals "web 基準前進" "$(cat "$CI/part-success/web")" "$(cur)"
equals "沒跑的 daemon 基準不動" "$(cat "$CI/part-success/daemon")" "$BASE"
equals "status.json mode" "$(json_get mode)" "scoped"
equals "status.json 標 web 真的跑" "$(json_get part_state.web)" "run"
equals "status.json 標 daemon 是 reused" "$(json_get part_state.daemon)" "reused"
equals "reused 的段附上沿用基準" "$(json_get part_base.daemon)" "$BASE"
equals "沒跑的段 secs 是 null 不是 0" "$(json_get parts.daemon)" "None"
equals "跑過的段有秒數" "$(json_get parts.web | grep -c '^[0-9][0-9]*$')" "1"
[ ! -e "$FIX/bundle.log" ] && echo "ok   - web-only 不重建 bundle" && PASS=$((PASS + 1)) || { echo "FAIL - web-only 不該重建 bundle"; FAIL=$((FAIL + 1)); }

push_file daemon/src/x.rs rust1
: > "$FIX/gh.log"
equals "rust-only exit 0" "$(run)" "0"
equals "只跑 daemon" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "0001"
check "description 是 scoped daemon" "scoped daemon（沿用完整成功基準 ${B8}）" "$FIX/gh.log"
equals "daemon 前重建過 bundle" "$(wc -l < "$FIX/bundle.log" | tr -d ' ')" "1"
equals "web 基準仍是上一個 web 提交（沒因為別段而前進）" "$(cat "$CI/part-success/web")" "$(git -C "$ROOT/work" rev-parse HEAD~1)"

push_file docs/n.md docs1
: > "$FIX/gh.log"
equals "docs exit 0" "$(run)" "0"
equals "沒有任何段執行" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "0000"
check "A..C 含 web 程式碼：不是 docs-only，用 scoped none" "state=success.*scoped none（沿用完整成功基準 ${B8}）" "$FIX/gh.log"
check_no "不標 docs-only skip" "docs-only skip" "$FIX/gh.log"
equals "status.json mode 是 reuse" "$(json_get mode)" "reuse"
equals "完整基準仍不前進" "$(cat "$CI/full-success-sha")" "$BASE"
teardown

new_baseline
push_file daemon/src/y.rs rust-red
echo 1 > "$FIX/rc.daemon"
: > "$FIX/gh.log"
run >/dev/null
check "daemon 紅 → failure" "state=failure" "$FIX/gh.log"
equals "紅的 daemon 基準不前進" "$(cat "$CI/part-success/daemon")" "$BASE"
rm "$FIX/rc.daemon"
push_file web/src/b.ts web-after-red
: > "$FIX/gh.log"
run >/dev/null
equals "web+daemon 都跑（按 ob/ops/web/daemon 順序）" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1001"
check "description 照段落順序 scoped web+daemon" "scoped web+daemon（沿用完整成功基準 ${B8}）" "$FIX/gh.log"
equals "紅過又補驗成功：daemon 基準前進" "$(cat "$CI/part-success/daemon")" "$(cur)"
equals "仍不前進完整基準" "$(cat "$CI/full-success-sha")" "$BASE"
push_file web/src/c.ts web-ok
push_file daemon/src/z.rs rust-red2
echo 1 > "$FIX/rc.daemon"
run >/dev/null
equals "同輪成功的 web 基準前進" "$(cat "$CI/part-success/web")" "$(cur)"
equals "同輪紅的 daemon 基準不動" "$(cat "$CI/part-success/daemon")" "$(git -C "$ROOT/work" rev-parse HEAD~2)"
teardown

new_baseline
push_file scripts/check.sh '#!/bin/bash'"
"'part="$1"; echo "==> fake $part"; exit 0'
: > "$FIX/gh.log"
run >/dev/null
equals "check.sh 改動跑四段" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
check "description 是完整成功" "ob+ops+web+daemon 全綠" "$FIX/gh.log"
push_file opaque-unknown x
run >/dev/null
equals "未知路徑四段全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
equals "全跑成功才前進完整基準" "$(cat "$CI/full-success-sha")" "$(cur)"
check "full 成功 description 不變" "ob+ops+web+daemon 全綠" "$FIX/gh.log"
teardown

new_baseline
rm "$CI/part-success/web"
push_file docs/n.md d1
push_file web/src/m.ts m1
run >/dev/null
equals "缺一段基準：四段全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
push_file web/src/m.ts m2
echo garbage > "$CI/part-success/ops"
run >/dev/null
equals "基準讀錯：四段全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
UNREL=$(printf unrel | git -C "$ROOT/work" commit-tree "$(git -C "$ROOT/work" rev-parse 'HEAD^{tree}')")
git -C "$ROOT/work" push -q origin "$UNREL:refs/heads/other" 2>/dev/null
printf '%s\n' "$UNREL" > "$CI/part-success/ob"
push_file web/src/m.ts m3
run >/dev/null
equals "基準非祖先：四段全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
teardown

new_baseline
export AM_REAL_HERDR=/bin/true
: > "$FIX/cargo.log"
push_file web/src/q.ts q1
run >/dev/null
check_no "web-only 不跑 real-herdr smoke" "the_generated_argv_parses_with_the_real_herdr" "$FIX/cargo.log"
push_file daemon/src/q.rs q2
echo 1 > "$FIX/bundle.rc"
: > "$FIX/gh.log"
run >/dev/null
check "bundle 重建失敗 → daemon 紅" "state=failure.*紅：daemon" "$FIX/gh.log"
equals "bundle 失敗時 daemon 測試不跑" "$(ran daemon)" "0"
equals "daemon 基準不動" "$(cat "$CI/part-success/daemon")" "$BASE"
rm "$FIX/bundle.rc"
rm -rf "$ROOT/work/web/dist"
echo 1 > "$FIX/cargo.rc"
push_file daemon/src/q.rs q3
: > "$FIX/gh.log"
run >/dev/null
check "daemon 跑了（含 real-herdr smoke）" "the_generated_argv_parses_with_the_real_herdr" "$FIX/cargo.log"
check "herdr smoke 失敗 → daemon 紅" "state=failure.*紅：daemon" "$FIX/gh.log"
equals "herdr smoke 失敗不記 daemon 成功" "$(cat "$CI/part-success/daemon")" "$BASE"
rm -f "$FIX/cargo.rc"; export AM_REAL_HERDR=""
teardown

new_baseline
export AM_REAL_HERDR=/bin/true
echo 101 > "$FIX/rc.daemon"
echo 0 > "$FIX/parser.rc"
push_file daemon/src/rc-priority.rs main-fails-parser-passes
SHA=$(cur)
run >/dev/null
check "101+0 保留主檢查 failure" 'state=failure.*紅：daemon' "$FIX/gh.log"
check "101+0 保留主檢查 rc" '"rc":101' "$CI/status.json"
check "101+0 記錄主檢查 rc" 'daemon primary check rc=101' "$CI/logs/$SHA.log"
check "101+0 記錄 parser rc" 'herdr argv parser rc=0' "$CI/logs/$SHA.log"

echo 124 > "$FIX/parser.rc"
push_file daemon/src/rc-priority.rs main-fails-parser-times-out
SHA=$(cur)
: > "$FIX/gh.log"
run >/dev/null
check "101+124 保留程式失敗" 'state=failure.*紅：daemon' "$FIX/gh.log"
check "101+124 status rc 不被覆蓋" '"rc":101' "$CI/status.json"
check "101+124 附上 parser 中斷證據" 'herdr argv parser rc=124 result=interrupted' "$CI/logs/$SHA.log"
check "101+124 最終 daemon rc 保留 101" 'daemon rc=101' "$CI/logs/$SHA.log"

echo 137 > "$FIX/parser.rc"
push_file daemon/src/rc-priority.rs main-fails-parser-killed
SHA=$(cur)
: > "$FIX/gh.log"
run >/dev/null
check "101+137 保留程式失敗" 'state=failure.*紅：daemon' "$FIX/gh.log"
check "101+137 status rc 不被覆蓋" '"rc":101' "$CI/status.json"
check "101+137 附上 parser 中斷證據" 'herdr argv parser rc=137 result=interrupted' "$CI/logs/$SHA.log"
check "101+137 最終 daemon rc 保留 101" 'daemon rc=101' "$CI/logs/$SHA.log"

echo 0 > "$FIX/rc.daemon"
echo 124 > "$FIX/parser.rc"
PRIOR=$(cat "$CI/last-sha")
push_file daemon/src/rc-priority.rs main-passes-parser-times-out
SHA=$(cur)
: > "$FIX/gh.log"
run >/dev/null
check "0+124 純 parser 中斷仍重試" '中斷：daemon(rc=124)，會重跑' "$FIX/gh.log"
check "0+124 仍分類為 interrupted" '"reason":"interrupted"' "$CI/status.json"
check "0+124 主檢查為成功" 'daemon primary check rc=0' "$CI/logs/$SHA.log"
check "0+124 parser 中斷有記錄" 'herdr argv parser rc=124' "$CI/logs/$SHA.log"
equals "0+124 不前進 last-sha" "$(cat "$CI/last-sha")" "$PRIOR"

echo 124 > "$FIX/rc.daemon"
echo 0 > "$FIX/parser.rc"
push_file daemon/src/rc-priority.rs main-times-out-parser-passes
SHA=$(cur)
: > "$FIX/gh.log"
run >/dev/null
check "124+0 純主檢查中斷仍重試" '中斷：daemon(rc=124)，會重跑' "$FIX/gh.log"
check "124+0 仍分類為 interrupted" '"reason":"interrupted"' "$CI/status.json"
check "124+0 主檢查中斷有記錄" 'daemon primary check rc=124' "$CI/logs/$SHA.log"
check "124+0 parser 成功有記錄" 'herdr argv parser rc=0' "$CI/logs/$SHA.log"
teardown

new_baseline
: > "$FIX/gh.log"
run >/dev/null
equals "沒到期的同 sha 不重跑" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
echo 1 > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
equals "到期的同 sha exit 0" "$(run)" "0"
check "到期補跑完整四段並送 success" "state=success.*ob+ops+web+daemon 全綠" "$FIX/gh.log"
equals "補跑重新計時" "$(( $(date +%s) - $(cat "$CI/full-success-at") < 100 ))" "1"
echo 1 > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
echo 1 > "$FIX/rc.web"
: > "$FIX/gh.log"
run >/dev/null
check "到期補跑失敗送 failure" "state=failure" "$FIX/gh.log"
equals "完整基準沒被失敗輪前進" "$(cat "$CI/full-success-sha")" "$BASE"
: > "$FIX/gh.log"
run >/dev/null
equals "失敗後下一輪不重跑" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
rm "$FIX/rc.web"; echo 1 > "$CI/full-success-at"; echo 1 > "$CI/full-attempt-at"
push_file web/src/p.ts p1
run >/dev/null
equals "到期時新提交也跑完整四段" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
teardown

new_baseline
echo 1 > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
: > "$FIX/gh.log"
AGM_CI_FULL_INTERVAL=abc run >/dev/null
check "非法間隔退回預設、到期照補" "state=success" "$FIX/gh.log"
check "警告非法間隔" "不是非負整數" "$ROOT/out"
echo 1 > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
: > "$FIX/gh.log"
AGM_CI_FULL_INTERVAL=-5 run >/dev/null
check "負值間隔也退回預設" "state=success" "$FIX/gh.log"
echo $(( $(date +%s) + 99999 )) > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
touch -d '2 days ago' "$CI/full-success-sha"
: > "$FIX/gh.log"
run >/dev/null
check "未來時間戳視為沒有紀錄、照補" "state=success" "$FIX/gh.log"
echo 1 > "$CI/full-success-at"; rm -f "$CI/full-attempt-at"
: > "$FIX/gh.log"
AGM_CI_FULL_INTERVAL=0 run >/dev/null
equals "間隔 0＝明確關閉" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
teardown

new_baseline
printf '08\n' > "$CI/full-success-at"; printf '09\n' > "$CI/full-attempt-at"
touch -d '2 days ago' "$CI/full-success-sha"
: > "$FIX/gh.log"
equals "壞時間戳 exit 0" "$(run)" "0"
check "壞時間戳視為沒有紀錄、補跑完整並 success" "state=success.*全綠" "$FIX/gh.log"
check_no "不是 error" "state=error" "$FIX/gh.log"
teardown

setup
run >/dev/null
rm -rf "$CI/part-success" "$CI/full-success-at" "$CI/full-attempt-at"
push_file web/src/o.ts o1
run >/dev/null
equals "未知舊狀態全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1111"
teardown

new_baseline
echo 1 > "$CI/full-success-at"; echo 1 > "$CI/full-attempt-at"
echo 1 > "$FIX/rc.ops"
run >/dev/null
equals "定期全量 ops 失敗" "$(json_get state)" "failure"
push_file web/src/retry.ts web-after-periodic-failure
run >/dev/null
equals "舊 ops 基準仍相同也必須補驗 ops+web" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1010"
equals "ops 持續失敗不得被 web 成功蓋掉" "$(json_get state)" "failure"
push_file docs/retry.md docs-after-periodic-failure
run >/dev/null
equals "文件提交仍補驗失敗的 ops" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "0010"
equals "文件不能假綠" "$(json_get state)" "failure"
: > "$FIX/gh.log"
run >/dev/null
equals "持續失敗仍保留同 SHA 節流" "$(wc -l < "$FIX/gh.log" | tr -d ' ')" "0"
rm "$FIX/rc.ops"
push_file docs/retry.md docs-recover
run >/dev/null
equals "新提交真的補驗 ops 成功才恢復" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0010:success"
push_file web/src/retry.ts web-after-recovery
run >/dev/null
equals "恢復後不再重跑無關的 ops" "$(ran web)$(ran ob)$(ran ops)$(ran daemon)" "1000"
teardown

new_baseline
echo 1 > "$CI/full-success-at"; echo 1 > "$CI/full-attempt-at"
echo 1 > "$FIX/rc.ops"; echo 1 > "$FIX/rc.daemon"
run >/dev/null
push_file docs/recover.md docs-with-two-failures
run >/dev/null
equals "有效完整基準也不能走 docs-only 掩蓋兩段失敗" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0011:failure"
rm "$FIX/rc.ops"
push_file docs/recover.md docs-one-recovers
run >/dev/null
equals "ops 恢復仍要保留 daemon 失敗" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0011:failure"
rm "$FIX/rc.daemon"
push_file docs/recover.md docs-both-recover
run >/dev/null
equals "下一筆只補驗尚未恢復的 daemon" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0001:success"
push_file docs/recover.md docs-skip-after-recovery
run >/dev/null
equals "全部恢復後文件才能零測試成功" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get mode):$(json_get state)" "0000:docs_only:success"
teardown

new_baseline
rm "$CI/part-outcomes-v1"
echo 1 > "$FIX/rc.daemon"
push_file docs/upgrade.md legacy-failure
run >/dev/null
equals "舊版成功基準不能證明沒有後續失敗：首次仍全跑" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "1111:failure"
push_file docs/upgrade.md after-upgrade
run >/dev/null
equals "建立新狀態後只補驗未恢復的 daemon" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0001:failure"
rm "$FIX/rc.daemon"
push_file docs/upgrade.md recovered-after-upgrade
run >/dev/null
equals "失敗真的恢復後才能成功" "$(ran web)$(ran ob)$(ran ops)$(ran daemon):$(json_get state)" "0001:success"
teardown

hold_lock() {
  mkdir -p "$AGM_CI_DEPLOY_DIR/daemon-update.lock"; echo "$1 $(date +%s)" > "$AGM_CI_DEPLOY_DIR/daemon-update.lock/owner"
}
setup
export AGM_CI_DEPLOY_WAIT=30 AGM_CI_DEPLOY_POLL=1
sleep 30 & HOLDER=$!
hold_lock "$HOLDER"
( sleep 3; rm -rf "$AGM_CI_DEPLOY_DIR/daemon-update.lock" ) &
RELEASER=$!
equals "部署鎖被持有：等到釋放後照常跑、exit 0" "$(run)" "0"
check "等待期間印一行說明" "CI 讓路" "$ROOT/out"
check "釋放後繼續" "部署已結束" "$ROOT/out"
check "釋放後 CI 跑完是 success" "state=success" "$FIX/gh.log"
wait "$RELEASER" 2>/dev/null
kill "$HOLDER" 2>/dev/null
teardown

setup
export AGM_CI_DEPLOY_WAIT=2 AGM_CI_DEPLOY_POLL=1
sleep 30 & HOLDER=$!
hold_lock "$HOLDER"
equals "部署鎖一直不放：逾時後照跑、exit 0" "$(run)" "0"
check "逾時印原因" "不再讓路，照跑" "$ROOT/out"
check "逾時後 CI 照跑完成" "state=success" "$FIX/gh.log"
kill "$HOLDER" 2>/dev/null
teardown

setup
export AGM_CI_DEPLOY_WAIT=30 AGM_CI_DEPLOY_POLL=1
hold_lock 999999
equals "鎖的 owner 已死（殘鎖）：不等" "$(run)" "0"
check_no "沒有讓路訊息" "CI 讓路" "$ROOT/out"
teardown

setup
export AGM_CI_DEPLOY_WAIT=0
sleep 30 & HOLDER=$!
hold_lock "$HOLDER"
equals "AGM_CI_DEPLOY_WAIT=0：不讓路" "$(run)" "0"
check_no "沒有讓路訊息" "CI 讓路" "$ROOT/out"
kill "$HOLDER" 2>/dev/null
teardown

echo "ubuntu-ci_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
