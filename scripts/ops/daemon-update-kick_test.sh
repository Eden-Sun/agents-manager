#!/bin/bash
# daemon-update-kick.sh 的隔離測試（例行自動部署，使用者 2026-09-29 簡化）。
#
# 完全不碰正式 daemon／AGM 目錄／GitHub：每個 case 開暫存目錄，放一個假 origin（真的 git）、
# 假主樹、假 AGM 目錄，再把 gh／bun／cargo／agm／daemon-swap.sh 換成 stub，從它們的 log 檢查腳本做了什麼決定。
#
# 釘住的行為：
#   1. 沿 first-parent 往回找最新一顆 ubuntu-ci＝success 的 sha；沒有會進 binary 的差異就不問 GitHub、不建置。
#   2. 在專用 checkout 建置，不動主樹；同一顆等安全窗口那幾輪不重建。
#   3. 換版交給 daemon-swap.sh，**不帶核准單**、也不派工給任何 bot。
#   4. 立即部署請求檔：部署那顆 sha（不等 ubuntu-ci），做完才刪；等不到窗口就留著。
#   5. 失敗照舊推 ops_alert；被回滾的 sha 不再挑。
#
#   bash scripts/ops/daemon-update-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-update-kick.sh"
GITBIN=$(command -v git)
BASE_PATH="$PATH"
PASS=0
FAIL=0

check() { # check <描述> <要出現的字串> <檔案>
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1))
  fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "FAIL - $1"; echo "      不該出現 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else
    echo "ok   - $1"; PASS=$((PASS + 1))
  fi
}
check_eq() { # check_eq <描述> <期望> <實際>
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1))
  fi
}
count() { grep -c -- "$1" "$2" 2>/dev/null || true; }

commit() { # commit <檔案> <訊息>：在 WORK 提交並推到 origin，回 sha
  mkdir -p "$(dirname "$WORK/$1")"
  echo "$2" >> "$WORK/$1"
  "$GITBIN" -C "$WORK" add -A >/dev/null
  "$GITBIN" -C "$WORK" commit -q -m "$2"
  "$GITBIN" -C "$WORK" push -q origin HEAD:main
  "$GITBIN" -C "$WORK" rev-parse HEAD
}

setup() {
  ROOT=$(mktemp -d); export ROOT
  ORIGIN="$ROOT/origin.git"; WORK="$ROOT/work"
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" AGM_DEPLOY_CHECKOUT="$ROOT/deploy"
  mkdir -p "$AGM_DIR/bin" "$ROOT/bin" "$ROOT/ci"
  "$GITBIN" init -q --bare -b main "$ORIGIN"
  "$GITBIN" clone -q "$ORIGIN" "$WORK" 2>/dev/null
  "$GITBIN" -C "$WORK" config user.email t@t; "$GITBIN" -C "$WORK" config user.name t
  mkdir -p "$WORK/web"; : > "$WORK/web/.keep"     # 每顆 commit 都要有 web/（腳本會 cd 進去建置）
  C0=$(commit daemon/src/a.rs "live")            # 線上那顆
  C1=$(commit daemon/src/a.rs "fix one")          # 會進 binary
  C2=$(commit docs/x.md "docs only")              # 不進 binary
  C3=$(commit web/src/b.ts "web change")          # 會進 binary（HEAD）
  export C0 C1 C2 C3
  # 主樹：只用來取 origin URL 與放線上 binary。
  "$GITBIN" clone -q "$ORIGIN" "$AGM_REPO" 2>/dev/null
  mkdir -p "$AGM_REPO/target/release"
  printf 'old-binary\n' > "$AGM_REPO/target/release/agents-managerd"
  echo "$(echo "$C0" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"

  export PATH="$ROOT/bin:$BASE_PATH"
  export GIT_BIN="$GITBIN" GH_BIN="$ROOT/bin/gh" BUN_BIN="$ROOT/bin/bun"
  export AGM_SWAP_SCRIPT="$ROOT/bin/swap.sh" AM_AGENT_NAME=daemon-update-kick
  export STUB_GH_FAIL="" STUB_SWAP_RC=0 STUB_BUILT_BLOCK="" STUB_CARGO_FAIL="" STUB_BUN_FAIL="" STUB_BUN_SLEEP=""
  export AGM_FAIL_ALERT_AFTER=3 AGM_CI_LOOKBACK=30

  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/agm.log"
case "$*" in
  *build-inputs*) printf '{"paths":["daemon","web","Cargo.toml","Cargo.lock"]}' ;;
  *ops-alert*) reason=""; nxt=0
      for a in "$@"; do [ "$nxt" = 1 ] && { reason="$a"; nxt=0; }; [ "$a" = "--reason" ] && nxt=1; done
      echo "$reason" >> "$AGM_DIR/alerts.log" ;;
  *) printf '{}' ;;
esac
STUB
  cat > "$ROOT/bin/gh" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/gh.log"
[ -z "$STUB_GH_FAIL" ] || { echo "gh: network down" >&2; exit 1; }
sha=$(echo "$*" | sed -n 's|.*commits/\([0-9a-f]*\)/status.*|\1|p')
[ -f "$ROOT/ci/$sha" ] && cat "$ROOT/ci/$sha"
exit 0
STUB
  cat > "$ROOT/bin/bun" <<'STUB'
#!/bin/bash
echo "bun $* @ $(pwd)" >> "$AGM_DIR/build.log"
[ -z "$STUB_BUN_FAIL" ] || exit 1
[ -z "$STUB_BUN_SLEEP" ] || sleep "$STUB_BUN_SLEEP"
exit 0
STUB
  cat > "$ROOT/bin/cargo" <<'STUB'
#!/bin/bash
echo "cargo $* @ $(pwd) AM_REAL_CARGO=${AM_REAL_CARGO:-} PATH_HEAD=${PATH%%:*} CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-}" >> "$AGM_DIR/build.log"
[ -z "$STUB_CARGO_FAIL" ] || exit 1
mkdir -p target/release
git rev-parse HEAD > target/release/agents-managerd
chmod +x target/release/agents-managerd
STUB
  cat > "$ROOT/bin/swap.sh" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/swap.log"
[ "$STUB_SWAP_RC" != 0 ] || echo "$(echo "$2" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
[ -z "$STUB_BUILT_BLOCK" ] || { rm -f "$AGM_DIR/daemon-update.built"; mkdir "$AGM_DIR/daemon-update.built"; }
exit "$STUB_SWAP_RC"
STUB
  chmod +x "$AGM_DIR/bin/agm" "$ROOT/bin/"*
  : > "$AGM_DIR/agm.log"; : > "$AGM_DIR/gh.log"; : > "$AGM_DIR/build.log"; : > "$AGM_DIR/swap.log"; : > "$AGM_DIR/alerts.log"
  : > "$AGM_DIR/daemon-update.log"
}
teardown() { [ -n "${KEEP:-}" ] && echo "KEPT $ROOT" && return; rm -rf "$ROOT"; PATH="$BASE_PATH"; export PATH; }
run() { ( unset AM_REAL_CARGO; bash "$SCRIPT" >/dev/null 2>&1 ); echo $?; }
ci() { echo "$2" > "$ROOT/ci/$1"; }   # ci <sha> <state>
LOG() { echo "$AGM_DIR/daemon-update.log"; }

# 1. 沒有會進 binary 的差異：不問 GitHub、不建置、不換版。
setup
"$GITBIN" -C "$WORK" reset -q --hard "$C0"; "$GITBIN" -C "$WORK" push -q -f origin HEAD:main
D=$(commit docs/y.md "docs only again")
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "log 說只動到不進 binary 的檔" "沒有會進 binary 的差異" "$(LOG)"
check_eq "沒問 GitHub" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_eq "沒建置" "0" "$(wc -l < "$AGM_DIR/build.log" | tr -d ' ')"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

# 2. HEAD 綠燈：專用 checkout 建置、換版，不帶核准單、不派工。
setup
ci "$C3" success
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "問的是 ubuntu-ci 的 commit status" "repos/Eden-Sun/agents-manager/commits/$C3/status" "$AGM_DIR/gh.log"
check "web 建置在專用 checkout" "bun run build @ $ROOT/deploy/web" "$AGM_DIR/build.log"
check "cargo 建置在專用 checkout" "cargo build --release -p agents-managerd @ $ROOT/deploy" "$AGM_DIR/build.log"
check "cargo uses PATH shim without AM_REAL_CARGO" "AM_REAL_CARGO= PATH_HEAD=$ROOT/bin CARGO_BUILD_JOBS=2" "$AGM_DIR/build.log"
check_eq "專用 checkout 停在目標 sha" "$C3" "$("$GITBIN" -C "$ROOT/deploy" rev-parse HEAD)"
check "換版的 sha" "--sha $C3" "$AGM_DIR/swap.log"
check "換版的舊版是 .built" "--old $(echo "$C0" | cut -c1-8)" "$AGM_DIR/swap.log"
check "換版帶舊 binary 的 hash" "--old-hash $(shasum -a 256 "$AGM_REPO/target/release/agents-managerd" | cut -c1-16)" "$AGM_DIR/swap.log"
check "換版指向專用 checkout" "--checkout $ROOT/deploy" "$AGM_DIR/swap.log"
check_no "不帶核准單" "approval" "$AGM_DIR/swap.log"
check_no "不申請核准、不派工" "approval\|assign\|lease" "$AGM_DIR/agm.log"
check_eq "主樹 HEAD 沒被動" "$C3" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
teardown

# 3. HEAD 還在跑（pending）：往前一顆找到綠燈的那顆；中間 docs-only 不會被當成候選。
setup
ci "$C3" pending
ci "$C1" success
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "HEAD 不是綠燈就往前找" "${C3}.*pending" "$(LOG)"
check "換的是綠燈那顆" "--sha $C1" "$AGM_DIR/swap.log"
teardown

# 4. 沒有任何綠燈：不建置、不換版，也不算失敗（等下一輪）。
setup
ci "$C3" failure
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "沒建置" "0" "$(wc -l < "$AGM_DIR/build.log" | tr -d ' ')"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "沒有失敗計數" "no" "$([ -f "$AGM_DIR/daemon-update.fails" ] && echo yes || echo no)"
teardown

# 5. GitHub 問不到：這輪不動、記成失敗；連續 3 輪推 ops_alert。
setup
export STUB_GH_FAIL=1
for _ in 1 2; do run >/dev/null; done
check_no "兩輪還不喊人" "check_failing" "$AGM_DIR/alerts.log"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "第三輪推 check_failing" "check_failing" "$AGM_DIR/alerts.log"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
export STUB_GH_FAIL=""
ci "$C3" success
run >/dev/null
check_eq "恢復後失敗計數清零" "no" "$([ -f "$AGM_DIR/daemon-update.fails" ] && echo yes || echo no)"
teardown

# 6. 等安全窗口（swap rc=4）：不算失敗、不重建；下一輪同一顆直接再換。
setup
ci "$C3" success
export STUB_SWAP_RC=4
run >/dev/null
check "有人在忙記 log" "還有人在忙" "$(LOG)"
check_eq "沒有失敗計數" "no" "$([ -f "$AGM_DIR/daemon-update.fails" ] && echo yes || echo no)"
export STUB_SWAP_RC=0
run >/dev/null
check_eq "只建置一次" "1" "$(count 'cargo build' "$AGM_DIR/build.log")"
check_eq "換版試了兩次" "2" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "第二輪說已經建好" "已經建好" "$(LOG)"
teardown

# 7. 換上去被回滾（swap rc=7）：推 ops_alert，之後不再挑那顆；往前一顆綠燈就退而求其次也不行（那顆才是被拒的）。
setup
ci "$C3" success
export STUB_SWAP_RC=7
run >/dev/null
check "推 swap_rolled_back" "swap_rolled_back" "$AGM_DIR/alerts.log"
check "記進 rejected" "$C3" "$AGM_DIR/daemon-update.rejected"
: > "$AGM_DIR/swap.log"
ci "$C1" success
export STUB_SWAP_RC=0
run >/dev/null
check "下一輪略過被回滾的那顆" "之前換上去被回滾過" "$(LOG)"
check "改換往前的綠燈" "--sha $C1" "$AGM_DIR/swap.log"
teardown

# 7b. rollback list writes are durable deploy state: report a failed rejection write instead of claiming this sha is safely skipped.
setup
ci "$C3" success
export STUB_SWAP_RC=7
mkdir "$AGM_DIR/daemon-update.rejected"
run >/dev/null
check "回滾後 rejected 寫入失敗有明確 log" "rejected 清單寫入失敗" "$(LOG)"
check "回滾通知指出下一輪可能重試" "可能重新挑選" "$(LOG)"
check_eq "rejected 寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

# 8. 往前修（rc=6）：補寫 .built，推 ops_alert。
setup
ci "$C3" success
export STUB_SWAP_RC=6
run >/dev/null
check_eq ".built 補上" "$(echo "$C3" | cut -c1-8)" "$(cat "$AGM_DIR/daemon-update.built")"
check "推 swap_forward_fixed" "swap_forward_fixed" "$AGM_DIR/alerts.log"
teardown

# 8b. Forward-fix succeeded but the .built marker cannot be updated: raise an actionable alert and keep the failure visible.
setup
ci "$C3" success
export STUB_SWAP_RC=6 STUB_BUILT_BLOCK=1
run >/dev/null
check "built marker 寫入失敗有明確 log" "daemon-update.built 寫入失敗" "$(LOG)"
check "built marker 寫入失敗推 ops_alert" "swap_built_marker_failed" "$AGM_DIR/alerts.log"
check_eq "built marker 寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

# 8c. daemon-swap reports marker persistence failure after successful verification; kick retries the marker and alerts if it still cannot persist.
setup
ci "$C3" success
export STUB_SWAP_RC=11 STUB_BUILT_BLOCK=1
run >/dev/null
check "swap rc=11 標記寫入失敗有明確 log" "daemon-update.built 寫入失敗" "$(LOG)"
check "swap rc=11 標記寫入失敗推 ops_alert" "swap_built_marker_failed" "$AGM_DIR/alerts.log"
check_eq "swap rc=11 標記寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

# 8d. rollback cannot safely restore the old binary; preserve DB state and raise manual-recovery alert.
setup
ci "$C3" success
export STUB_SWAP_RC=12
run >/dev/null
check "rollback incomplete 有明確 log" "舊 binary 無法安全恢復" "$(LOG)"
check "rollback incomplete 推 ops_alert" "swap_rollback_incomplete" "$AGM_DIR/alerts.log"
check_eq "rollback incomplete 計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

# 9. 建置失敗：不換版、記失敗。
setup
ci "$C3" success
export STUB_CARGO_FAIL=1
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "log 記 cargo build 失敗" "cargo build 失敗" "$(LOG)"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
teardown

# 10. 立即部署：不等 ubuntu-ci（那顆是 pending），換完才刪請求檔。
setup
ci "$C3" pending
printf '{"sha":"%s","live_sha":"x","requested_by":"ui"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "換的是請求的那顆" "--sha $C1" "$AGM_DIR/swap.log"
check_eq "立即部署不問 GitHub" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_eq "做完刪請求檔" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_no "不帶核准單" "approval" "$AGM_DIR/swap.log"
teardown

# 11. 立即部署等不到窗口（rc=4）：請求檔留著，下一輪再試。
setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
export STUB_SWAP_RC=4
run >/dev/null
check_eq "請求檔留著" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
export STUB_SWAP_RC=0
run >/dev/null
check_eq "下一輪換完才刪" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "只建置一次" "1" "$(count 'cargo build' "$AGM_DIR/build.log")"
teardown

# 12. 立即部署：sha 不在 origin/main、壞掉的請求檔 → alert 並收掉，不建置。
setup
printf '{"sha":"deadbeef00000000000000000000000000000000"}' > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "推 now_target_invalid" "now_target_invalid" "$AGM_DIR/alerts.log"
check_eq "請求檔收掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
printf 'not json' > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "推 now_request_corrupt" "now_request_corrupt" "$AGM_DIR/alerts.log"
check_eq "壞檔收掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
teardown

# 13. 立即部署：只動到不進 binary 的檔（C2 相對線上 C0 之間有 C1，所以改成把線上設成 C1 再要 C2）。
setup
echo "$(echo "$C1" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
printf '{"sha":"%s"}' "$C2" > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "說已經是最新" "只動到不進 binary" "$(LOG)"
check_eq "請求檔收掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

# 14. 立即部署：目標比線上舊 → 拒絕降版。
setup
echo "$(echo "$C3" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "推 now_target_older" "now_target_older" "$AGM_DIR/alerts.log"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

# 15. 讀不到線上版本（.built 不在）：喊人，不動。
setup
rm -f "$AGM_DIR/daemon-update.built"
ci "$C3" success
run >/dev/null
check "推 built_unknown" "built_unknown" "$AGM_DIR/alerts.log"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

# 16. daemon 太舊（swap rc=9）：推 swap_daemon_too_old，算失敗。
setup
ci "$C3" success
export STUB_SWAP_RC=9
run >/dev/null
check "推 swap_daemon_too_old" "swap_daemon_too_old" "$AGM_DIR/alerts.log"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
teardown

# 16b. 新 binary 內嵌的 sha 不是核准的那顆（swap rc=10，換版之前就擋下、什麼都沒動）：推 swap_binary_sha_mismatch，算失敗，
#      但不記進 rejected（commit 本身沒問題，是這次建出來的 binary 不對，下一輪重建就可能好）。
setup
ci "$C3" success
export STUB_SWAP_RC=10
run >/dev/null
check "推 swap_binary_sha_mismatch" "swap_binary_sha_mismatch" "$AGM_DIR/alerts.log"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
check_no "sha 不符不記進 rejected" "$C3" "$AGM_DIR/daemon-update.rejected"
teardown

# 17. 殘留鎖（執行者已不在）超過門檻就回收；還活著的執行者則跳過。
setup
ci "$C3" success
mkdir -p "$AGM_DIR/daemon-update.lock"; echo "999999 1" > "$AGM_DIR/daemon-update.lock/owner"
AGM_LOCK_STALE_SECS=0 run >/dev/null
check "清掉殘留鎖" "清掉殘留鎖" "$(LOG)"
check_eq "接手後有換版" "1" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown
setup
ci "$C3" success
export STUB_BUN_SLEEP=30
bash "$SCRIPT" > "$ROOT/live-runner.log" 2>&1 & SLEEPER=$!
_wait=0
while [ ! -s "$AGM_DIR/daemon-update.lock/owner" ] && [ "$_wait" -lt 100 ]; do
  sleep 0.05
  _wait=$((_wait + 1))
done
[ -s "$AGM_DIR/daemon-update.lock/owner" ] || { echo "FAIL - 活 runner 沒拿到鎖"; FAIL=$((FAIL + 1)); }
_wait=0
while [ "$(count 'bun install' "$AGM_DIR/build.log" | tr -d ' ')" = 0 ] && [ "$_wait" -lt 100 ]; do
  sleep 0.05
  _wait=$((_wait + 1))
done
run >/dev/null
check_no "正常重疊的活 runner 安靜跳過" "更新檢查已有執行者" "$(LOG)"
kill "$SLEEPER" 2>/dev/null; wait "$SLEEPER" 2>/dev/null
check_eq "活著的執行者：這輪不動" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "活著的執行者：沒有重複建置" "1" "$(count 'bun install' "$AGM_DIR/build.log")"
teardown

# guard 路徑被換成 symlink 時 fail closed，不可截斷其目標或繼續部署。
setup
ci "$C3" success
printf 'preserve outside guard target\n' > "$ROOT/guard-target"
ln -s "$ROOT/guard-target" "$AGM_DIR/daemon-update.lock.guard"
run >/dev/null
check_eq "guard symlink 目標原封不動" "preserve outside guard target" "$(cat "$ROOT/guard-target")"
check "guard symlink 推 lock_unavailable" "lock_unavailable" "$AGM_DIR/alerts.log"
check_eq "guard symlink 下不部署" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

# A child left holding the advisory lock after the runner PID disappeared must not turn
# into an endless quiet skip: aged, unverified guard ownership raises an alert.
setup
ci "$C3" success
mkdir -p "$AGM_DIR/daemon-update.lock"
echo "999999 $(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"w"); fcntl.flock(f,fcntl.LOCK_EX); time.sleep(30)' "$AGM_DIR/daemon-update.lock.guard" & GUARD_HOLDER=$!
sleep 0.1
AGM_LOCK_HUNG_SECS=0 run >/dev/null
check "未驗證但持有 OS lock 的舊 runner 會喊人" "runner_hung" "$AGM_DIR/alerts.log"
check_eq "guard 持有人未知時不重複部署" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
kill "$GUARD_HOLDER" 2>/dev/null; wait "$GUARD_HOLDER" 2>/dev/null
teardown

# 活 PID 的 command line 只含同名片語、不是真正 kick：不能把被重用的 PID 誤認成執行者而永遠保留鎖。
setup
ci "$C3" success
bash -c 'sleep 30; : # daemon-update-kick' & SLEEPER=$!
mkdir -p "$AGM_DIR/daemon-update.lock"; echo "$SLEEPER $(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"
AGM_LOCK_STALE_SECS=0 run >/dev/null
check_eq "重用 PID 的 command line 含名稱也要回收" "1" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "重用 PID 不再被判成活 runner" "清掉殘留鎖" "$(LOG)"
kill "$SLEEPER" 2>/dev/null; wait "$SLEEPER" 2>/dev/null
teardown

# 18. 腳本本身：不再有建置 child／核准／門檻這些東西；daemon 的 kick_ready 靠這個檔名判斷；全形標點前要有大括號。
check "認得立即部署請求檔（kick_ready 靠這個字）" "daemon-update.now.json" "$SCRIPT"
# ── ops 腳本自動換新（旗標，預設關）：假 repo 裡放真的 ops-install.sh＋一份對照表＋一支 x-kick.sh；安裝端先裝好 v1 ──
seed_ops() { # 在 WORK 加上 ops-install.sh、對照表、x-kick.sh（v1）並推上去；安裝端的 x-kick.sh 是 v1
  mkdir -p "$WORK/scripts/ops"
  cp "$HERE/ops-install.sh" "$WORK/scripts/ops/ops-install.sh"
  printf 'scripts/ops/x-kick.sh  bin/x-kick.sh\n' > "$WORK/scripts/ops/install-manifest.tsv"
  printf '#!/bin/bash\necho x-v1\n' > "$WORK/scripts/ops/x-kick.sh"
  "$GITBIN" -C "$WORK" add -A >/dev/null; "$GITBIN" -C "$WORK" commit -q -m "seed ops"; "$GITBIN" -C "$WORK" push -q origin HEAD:main
  install -m 755 "$WORK/scripts/ops/x-kick.sh" "$AGM_DIR/bin/x-kick.sh"
}
bump_x() { # bump_x <內容>：改 x-kick.sh 並推上去，回 sha
  printf '%s' "$1" > "$WORK/scripts/ops/x-kick.sh"
  "$GITBIN" -C "$WORK" add -A >/dev/null; "$GITBIN" -C "$WORK" commit -q -m "bump x"; "$GITBIN" -C "$WORK" push -q origin HEAD:main
  "$GITBIN" -C "$WORK" rev-parse HEAD
}
only_ops_since_built() { # 線上那顆之後只剩 ops 的改動（不進 binary）
  "$GITBIN" -C "$WORK" reset -q --hard "$C0"; "$GITBIN" -C "$WORK" push -q -f origin HEAD:main
  seed_ops
}
x_line() { sed -n 2p "$AGM_DIR/bin/x-kick.sh"; }

# 8. 旗標沒開（預設）：ops 腳本改了也不動安裝端，也不為它問 GitHub。
setup
only_ops_since_built
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
check_eq "沒問 GitHub" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_no "log 沒提 ops 自動安裝" "ops 自動安裝" "$(LOG)"
teardown

# 9. 旗標開（檔案）、沒有會進 binary 的差異、HEAD 綠燈：只換 ops 腳本，不建置、不換版；先備份。
setup
only_ops_since_built
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "安裝端換成 v2" "echo x-v2" "$(x_line)"
check "log 記下裝了哪支" "ops-install: installed bin/x-kick.sh" "$(LOG)"
check_eq "備份是舊的 v1" "echo x-v1" "$(sed -n 2p "$(ls -d "$AGM_DIR"/ops-install-backups/*/ | head -1)bin/x-kick.sh")"
check_eq "沒建置" "0" "$(wc -l < "$AGM_DIR/build.log" | tr -d ' ')"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "沒有 ops_alert" "0" "$(wc -l < "$AGM_DIR/alerts.log" | tr -d ' ')"
teardown

# 10. 旗標開、HEAD 的 ubuntu-ci 還沒綠：等，不裝。
setup
only_ops_since_built
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" pending
check_eq "rc=0" "0" "$(run)"
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
check "log 說等綠燈" "等綠燈" "$(LOG)"
teardown

# 11. 旗標開（環境變數）、有 binary 要換：換版成功之後才裝 ops 腳本；順序是先換版。
setup
seed_ops
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
export AGM_OPS_AUTO_INSTALL=1
check_eq "rc=0" "0" "$(run)"
unset AGM_OPS_AUTO_INSTALL
check "有換版" "--sha $H" "$AGM_DIR/swap.log"
check_eq "換版後 ops 腳本也換了" "echo x-v2" "$(x_line)"
teardown

# 12. 換版沒成功（daemon-swap rc=7 回滾）：不裝 ops 腳本。
setup
seed_ops
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
STUB_SWAP_RC=7 run >/dev/null
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
teardown

# 13. 新版自檢沒過：還原舊版、推 ops_install_failed，部署本身的結果不受影響（照常寫「已換上」）。
setup
seed_ops
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\nif then fi (\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "壞的新版沒留在安裝端" "echo x-v1" "$(x_line)"
check "推了 ops_install_failed" "ops_install_failed" "$AGM_DIR/alerts.log"
check "部署照常完成" "已換上" "$(LOG)"
teardown

# 14. 安裝端的檔被人手改過（不是 repo 任何一版）：不覆蓋、推 ops_install_drift 叫人看，部署照常完成、不算失敗。
setup
seed_ops
touch "$AGM_DIR/ops-auto-install.enabled"
printf '#!/bin/bash\necho x-hand-edited\n' > "$AGM_DIR/bin/x-kick.sh"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "手改的檔沒被蓋掉" "echo x-hand-edited" "$(x_line)"
check "推了 ops_install_drift" "ops_install_drift" "$AGM_DIR/alerts.log"
check_no "不是 ops_install_failed" "ops_install_failed" "$AGM_DIR/alerts.log"
check "部署照常完成" "已換上" "$(LOG)"
teardown

for gone in AGM_BUILD_BOT AGM_REBUILD_THRESHOLD "approval request" "lease acquire" "agm assign\|assign --bot" "AGM_REBUILD_MAX_WAIT_MIN"; do
  check_no "已拿掉：${gone}" "$gone" "$SCRIPT"
done
if LC_ALL=C grep -nE '\$[A-Za-z_][A-Za-z0-9_]*[^ -~]' "$SCRIPT" > "/tmp/daemon-update-unbraced.$$" 2>/dev/null; then
  echo "FAIL - 變數後面接非 ASCII 字元要用 \${VAR}"; sed 's/^/      /' "/tmp/daemon-update-unbraced.$$"; FAIL=$((FAIL + 1))
else
  echo "ok   - 變數後面接非 ASCII 字元都有大括號"; PASS=$((PASS + 1))
fi
rm -f "/tmp/daemon-update-unbraced.$$"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
