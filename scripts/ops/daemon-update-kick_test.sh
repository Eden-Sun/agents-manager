#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-update-kick.sh"
GITBIN=$(command -v git)
export GITBIN
PASS=0
FAIL=0

check() {
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
check_eq() {
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1))
  fi
}
count() { grep -c -- "$1" "$2" 2>/dev/null || true; }

commit() {
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
  mkdir -p "$AGM_DIR/bin" "$ROOT/bin" "$ROOT/ci" "$ROOT/cistate"
  export AGM_CI_ROOT="$ROOT/cistate"
  "$GITBIN" init -q --bare -b main "$ORIGIN"
  "$GITBIN" clone -q "$ORIGIN" "$WORK" 2>/dev/null
  "$GITBIN" -C "$WORK" config user.email t@t; "$GITBIN" -C "$WORK" config user.name t
  mkdir -p "$WORK/web"; : > "$WORK/web/.keep"
  C0=$(commit daemon/src/a.rs "live")
  C1=$(commit daemon/src/a.rs "fix one")
  C2=$(commit docs/x.md "docs only")
  C3=$(commit web/src/b.ts "web change")
  export C0 C1 C2 C3
  "$GITBIN" clone -q "$ORIGIN" "$AGM_REPO" 2>/dev/null
  mkdir -p "$AGM_REPO/target/release"
  printf 'old-binary\n' > "$AGM_REPO/target/release/agents-managerd"
  echo "$(echo "$C0" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"

  export GIT_BIN="$ROOT/bin/git" GH_BIN="$ROOT/bin/gh" BUN_BIN="$ROOT/bin/bun" CARGO_BIN="$ROOT/bin/cargo"
  export AGM_SWAP_SCRIPT="$ROOT/bin/swap.sh" AM_AGENT_NAME=daemon-update-kick
  export STUB_GH_FAIL="" STUB_BUILD_INPUTS_FAIL="" STUB_SWAP_RC=0 STUB_BUILT_BLOCK="" STUB_CARGO_FAIL="" STUB_BUN_FAIL="" STUB_BUN_SLEEP="" STUB_GIT_CLONE_FAIL=""
  export AGM_FAIL_ALERT_AFTER=3 AGM_CI_LOOKBACK=30

  cat > "$ROOT/bin/git" <<'STUB'
#!/bin/bash
if [ "${STUB_GIT_CLONE_FAIL:-}" = 1 ]; then
  case " $* " in *" clone "*)
    for dst do :; done
    mkdir -p "$dst"
    echo partial-clone > "$dst/partial"
    exit 1
    ;; esac
fi
exec "$GITBIN" "$@"
STUB
  chmod +x "$ROOT/bin/git"

  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/agm.log"
case "$*" in
  *build-inputs*) [ -z "${STUB_BUILD_INPUTS_FAIL:-}" ] || exit 1; printf '{"paths":["daemon","web","Cargo.toml","Cargo.lock"]}' ;;
  *supervisor*) [ -f "$AGM_DIR/live.json" ] && cat "$AGM_DIR/live.json" || printf '{}' ;;
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
if [ "${1:-}" = run ]; then
  [ "${3:-}" = --outDir ] || exit 2
  mkdir -p "$4/assets"
  printf '<html>bundle</html>\n' > "$4/index.html"
  printf 'asset\n' > "$4/assets/app.js"
fi
exit 0
STUB
  cat > "$ROOT/bin/cargo" <<'STUB'
#!/bin/bash
echo "cargo $* @ $(pwd) AM_REAL_CARGO=${AM_REAL_CARGO:-} PATH_HEAD=${PATH%%:*} LTO=${CARGO_PROFILE_RELEASE_LTO:-unset} INC=${CARGO_PROFILE_RELEASE_INCREMENTAL:-unset}" >> "$AGM_DIR/build.log"
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
teardown() { [ -n "${KEEP:-}" ] && echo "KEPT $ROOT" && return; rm -rf "$ROOT"; }
run() { bash "$SCRIPT" >/dev/null 2>"$AGM_DIR/stderr.log"; echo $?; }
ci() { echo "$2" > "$ROOT/ci/$1"; }
LOG() { echo "$AGM_DIR/daemon-update.log"; }

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

setup
ci "$C3" success
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "問的是 ubuntu-ci 的 commit status" "repos/Eden-Sun/agents-manager/commits/$C3/status" "$AGM_DIR/gh.log"
REAL_WEB=$(cd "$ROOT/deploy/web" && pwd -P)
check "web 建置在專用 checkout" "bun run build --outDir $REAL_WEB/.dist-stage-" "$AGM_DIR/build.log"
check "cargo 建置在專用 checkout" "cargo build --release -p agents-managerd @ $ROOT/deploy" "$AGM_DIR/build.log"
check "cargo 帶 AM_REAL_CARGO 與 cargo 的 PATH" "AM_REAL_CARGO=$ROOT/bin/cargo PATH_HEAD=$ROOT/bin" "$AGM_DIR/build.log"
check_eq "專用 checkout 停在目標 sha" "$C3" "$("$GITBIN" -C "$ROOT/deploy" rev-parse HEAD)"
check "預設部署建置關 LTO、開 incremental" "LTO=false INC=true" "$AGM_DIR/build.log"
check "換版的 sha" "--sha $C3" "$AGM_DIR/swap.log"
check "換版的舊版是 .built" "--old $(echo "$C0" | cut -c1-8)" "$AGM_DIR/swap.log"
check "換版帶舊 binary 的 hash" "--old-hash $(shasum -a 256 "$AGM_REPO/target/release/agents-managerd" | cut -c1-16)" "$AGM_DIR/swap.log"
check "換版指向專用 checkout" "--checkout $ROOT/deploy" "$AGM_DIR/swap.log"
check_no "不帶核准單" "approval" "$AGM_DIR/swap.log"
check_no "不申請核准、不派工" "approval\|assign\|lease" "$AGM_DIR/agm.log"
check_eq "主樹 HEAD 沒被動" "$C3" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
teardown

setup
ci "$C3" success
export AGM_DEPLOY_FAST_BUILD=0
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "開關為 0 時兩個環境變數都不帶" "LTO=unset INC=unset" "$AGM_DIR/build.log"
unset AGM_DEPLOY_FAST_BUILD
teardown

setup
ci "$C3" success
export STUB_GIT_CLONE_FAIL=1
run >/dev/null
check_eq "clone 失敗不留下正式 deploy-checkout" "no" "$([ -e "$ROOT/deploy" ] && echo yes || echo no)"
check_eq "clone 失敗清掉 staging 目錄" "0" "$(find "$ROOT" -maxdepth 1 -name 'deploy.clone.*' -print | wc -l | tr -d ' ')"
teardown

setup
FORK_ORIGIN="$ROOT/fork.git"; FORK_WORK="$ROOT/fork-work"
"$GITBIN" init -q --bare -b main "$FORK_ORIGIN"
"$GITBIN" -C "$WORK" push -q "$FORK_ORIGIN" "$C0:refs/heads/main"
"$GITBIN" clone -q "$FORK_ORIGIN" "$FORK_WORK"
"$GITBIN" -C "$FORK_WORK" config user.email t@t; "$GITBIN" -C "$FORK_WORK" config user.name t
echo fork-only > "$FORK_WORK/daemon/src/a.rs"
"$GITBIN" -C "$FORK_WORK" add -A; "$GITBIN" -C "$FORK_WORK" commit -q -m fork-only
"$GITBIN" -C "$FORK_WORK" push -q origin HEAD:main
FORK_SHA=$("$GITBIN" -C "$FORK_WORK" rev-parse HEAD)
ci "$FORK_SHA" success
"$GITBIN" clone -q --no-checkout "$ORIGIN" "$ROOT/deploy"
"$GITBIN" -C "$ROOT/deploy" remote set-url origin "$FORK_ORIGIN"
run >/dev/null
check_eq "不同 upstream 的 checkout 不讀 fork CI" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_eq "不同 upstream 的 checkout 不建置" "0" "$(count 'cargo build' "$AGM_DIR/build.log")"
check_eq "不同 upstream 的 checkout 不部署" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "不同 upstream 明確告警" "deploy_checkout_origin_mismatch" "$AGM_DIR/alerts.log"
teardown

setup
ci "$C3" pending
ci "$C1" success
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "HEAD 不是綠燈就往前找" "${C3}.*pending" "$(LOG)"
check "換的是綠燈那顆" "--sha $C1" "$AGM_DIR/swap.log"
teardown

setup
ci "$C3" failure
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "沒建置" "0" "$(wc -l < "$AGM_DIR/build.log" | tr -d ' ')"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "沒有失敗計數" "no" "$([ -f "$AGM_DIR/daemon-update.fails" ] && echo yes || echo no)"
teardown

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

setup
ci "$C3" success
export STUB_SWAP_RC=7
mkdir "$AGM_DIR/daemon-update.rejected"
run >/dev/null
check "回滾後 rejected 寫入失敗有明確 log" "rejected 清單寫入失敗" "$(LOG)"
check "回滾通知指出下一輪可能重試" "可能重新挑選" "$(LOG)"
check_eq "rejected 寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=6
run >/dev/null
check_eq ".built 補上" "$(echo "$C3" | cut -c1-8)" "$(cat "$AGM_DIR/daemon-update.built")"
check "推 swap_forward_fixed" "swap_forward_fixed" "$AGM_DIR/alerts.log"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=6 STUB_BUILT_BLOCK=1
run >/dev/null
check "built marker 寫入失敗有明確 log" "daemon-update.built 寫入失敗" "$(LOG)"
check "built marker 寫入失敗推 ops_alert" "swap_built_marker_failed" "$AGM_DIR/alerts.log"
check_eq "built marker 寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=11 STUB_BUILT_BLOCK=1
run >/dev/null
check "swap rc=11 標記寫入失敗有明確 log" "daemon-update.built 寫入失敗" "$(LOG)"
check "swap rc=11 標記寫入失敗推 ops_alert" "swap_built_marker_failed" "$AGM_DIR/alerts.log"
check_eq "swap rc=11 標記寫入失敗計入失敗輪次" "1" "$(cat "$AGM_DIR/daemon-update.fails" 2>/dev/null || echo 0)"
teardown

setup
ci "$C3" success
export STUB_CARGO_FAIL=1
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "log 記 cargo build 失敗" "cargo build 失敗" "$(LOG)"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
teardown

setup
ci "$C3" pending
printf '{"sha":"%s","live_sha":"x","requested_at":"%s","requested_by":"ui"}' "$C1" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" > "$AGM_DIR/daemon-update.now.json"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "換的是請求的那顆" "--sha $C1" "$AGM_DIR/swap.log"
check_eq "立即部署不問 GitHub" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_eq "做完刪請求檔" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_no "不帶核准單" "approval" "$AGM_DIR/swap.log"
teardown

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
printf '{"sha":"%s","requested_at":"2000-01-01T00:00:00.000Z"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "推 now_request_expired" "now_request_expired" "$AGM_DIR/alerts.log"
check_eq "過期請求收掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "過期請求不換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
echo "$(echo "$C1" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
printf '{"sha":"%s"}' "$C2" > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "說已經是最新" "只動到不進 binary" "$(LOG)"
check_eq "請求檔收掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
echo "$(echo "$C3" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
run >/dev/null
check "推 now_target_older" "now_target_older" "$AGM_DIR/alerts.log"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
rm -f "$AGM_DIR/daemon-update.built"
ci "$C3" success
run >/dev/null
check "推 built_unknown" "built_unknown" "$AGM_DIR/alerts.log"
check_eq "沒換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=9
run >/dev/null
check "推 swap_daemon_too_old" "swap_daemon_too_old" "$AGM_DIR/alerts.log"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=10
run >/dev/null
check "推 swap_binary_sha_mismatch" "swap_binary_sha_mismatch" "$AGM_DIR/alerts.log"
check_eq "有失敗計數" "1" "$(cat "$AGM_DIR/daemon-update.fails")"
check_no "sha 不符不記進 rejected" "$C3" "$AGM_DIR/daemon-update.rejected"
teardown

setup
ci "$C3" success
export STUB_SWAP_RC=10
run >/dev/null
check_eq "rc=10 後 .built-for 不在" "no" "$([ -f "$AGM_DEPLOY_CHECKOUT/target/release/.built-for" ] && echo yes || echo no)"
run >/dev/null
check_eq "rc=10 的下一輪重新建置" "2" "$(count 'cargo build' "$AGM_DIR/build.log")"
teardown

setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
export STUB_SWAP_RC=3
run >/dev/null
check_eq "第 1 次失敗請求檔還在" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "失敗計數是 1" "$C1 1" "$(cat "$AGM_DIR/daemon-update.now.fails")"
check_eq "首次失敗沒有缺計數檔的 stderr" "" "$(cat "$AGM_DIR/stderr.log")"
run >/dev/null
check_eq "第 2 次失敗請求檔還在" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "失敗計數是 2" "$C1 2" "$(cat "$AGM_DIR/daemon-update.now.fails")"
check_no "還沒到上限不推 now_failed" "now_failed" "$AGM_DIR/alerts.log"
run >/dev/null
check_eq "第 3 次失敗收掉請求檔" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "到上限只推一次 now_failed" "1" "$(count '^now_failed$' "$AGM_DIR/alerts.log")"
check_eq "收掉後計數檔也清掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.fails" ] && echo yes || echo no)"
teardown

setup
echo "$C1 2" > "$AGM_DIR/daemon-update.now.fails"
printf '{"sha":"%s"}' "$C3" > "$AGM_DIR/daemon-update.now.json"
export STUB_SWAP_RC=3
run >/dev/null
check_eq "新 sha 的失敗次數從 1 開始" "$C3 1" "$(cat "$AGM_DIR/daemon-update.now.fails")"
check_no "新 sha 不因舊計數提早推 now_failed" "now_failed" "$AGM_DIR/alerts.log"
export STUB_SWAP_RC=0
run >/dev/null
check_eq "成功後請求檔清掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "成功後失敗計數清掉" "no" "$([ -f "$AGM_DIR/daemon-update.now.fails" ] && echo yes || echo no)"
teardown

setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
mkdir "$AGM_DIR/daemon-update.now.fails"
export STUB_SWAP_RC=3
run >/dev/null
check "計數路徑是目錄時 read 錯誤可見" "[Ii]s a directory" "$AGM_DIR/stderr.log"
check "目錄讀取失敗仍記原 swap 失敗" "daemon-swap 失敗 rc=3" "$(LOG)"
check_eq "原 mv 把第一次計數放進目錄" "$C1 1" "$(cat "$AGM_DIR/daemon-update.now.fails/daemon-update.now.fails.tmp")"
run >/dev/null; run >/dev/null; run >/dev/null
check "重試目錄讀取錯誤仍可見" "[Ii]s a directory" "$AGM_DIR/stderr.log"
check_eq "目錄故障每輪仍只寫計數 1（既有行為）" "$C1 1" "$(cat "$AGM_DIR/daemon-update.now.fails/daemon-update.now.fails.tmp")"
check_eq "四輪仍各記原 swap 失敗" "4" "$(count '^[0-9-]* [0-9:]* daemon-swap 失敗 rc=3' "$(LOG)")"
check "目錄故障連續失敗仍推原 check_failing" "^check_failing$" "$AGM_DIR/alerts.log"
check_no "目錄計數無法累加而未推 now_failed（既有行為）" "^now_failed$" "$AGM_DIR/alerts.log"
check_eq "目錄故障四輪後仍留請求（既有行為）" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
teardown

setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
export STUB_SWAP_RC=4
run >/dev/null; run >/dev/null; run >/dev/null
check_eq "rc=4 三輪請求檔仍在" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_eq "rc=4 不累計失敗" "no" "$([ -f "$AGM_DIR/daemon-update.now.fails" ] && echo yes || echo no)"
check_no "rc=4 不推 now_failed" "now_failed" "$AGM_DIR/alerts.log"
echo "$C1 2" > "$AGM_DIR/daemon-update.now.fails"
run >/dev/null
check_eq "rc=4 清掉已有失敗計數" "no" "$([ -f "$AGM_DIR/daemon-update.now.fails" ] && echo yes || echo no)"
check_eq "rc=4 有舊計數時仍保留請求" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check_no "rc=4 有舊計數也不推 now_failed" "now_failed" "$AGM_DIR/alerts.log"
teardown

setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
export STUB_SWAP_RC=9
run >/dev/null
check_eq "rc=9 立刻收掉請求檔" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check "仍推 swap_daemon_too_old" "swap_daemon_too_old" "$AGM_DIR/alerts.log"
teardown

setup
printf '{"sha":"%s"}' "$C1" > "$AGM_DIR/daemon-update.now.json"
export STUB_CARGO_FAIL=1
run >/dev/null; run >/dev/null
check_eq "建置失敗 2 次請求檔還在" "yes" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
run >/dev/null
check_eq "建置失敗 3 次收掉請求檔" "no" "$([ -f "$AGM_DIR/daemon-update.now.json" ] && echo yes || echo no)"
check "建置失敗推 now_failed" "now_failed" "$AGM_DIR/alerts.log"
teardown

setup
ci "$C3" success
echo 3 > "$AGM_DIR/daemon-swap.probe-skips"
export STUB_SWAP_RC=0
run >/dev/null
check "略過 3 次推 swap_probe_skipped" "swap_probe_skipped" "$AGM_DIR/alerts.log"
check "alert 內容帶次數" "連續 3 次略過" "$(LOG)"
teardown
setup
ci "$C3" success
echo 2 > "$AGM_DIR/daemon-swap.probe-skips"
run >/dev/null
check_no "略過 2 次不推" "swap_probe_skipped" "$AGM_DIR/alerts.log"
teardown

setup
ci "$C3" success
echo 3 > "$AGM_DIR/daemon-swap.probe-skips"
echo "目標不能收 prompt" > "$AGM_DIR/daemon-swap.probe-skips.why"
export STUB_SWAP_RC=0
run >/dev/null
check "blocked 略過 3 次推 swap_probe_skipped" "swap_probe_skipped" "$AGM_DIR/alerts.log"
check "alert 內容講卡在 blocked／busy" "卡在 blocked／busy" "$(LOG)"
teardown

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

setup
ci "$C3" success
printf 'preserve outside guard target\n' > "$ROOT/guard-target"
ln -s "$ROOT/guard-target" "$AGM_DIR/daemon-update.lock.guard"
run >/dev/null
check_eq "guard symlink 目標原封不動" "preserve outside guard target" "$(cat "$ROOT/guard-target")"
check "guard symlink 推 lock_unavailable" "lock_unavailable" "$AGM_DIR/alerts.log"
check_eq "guard symlink 下不部署" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

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

setup
ci "$C3" success
mkdir -p "$AGM_DIR/daemon-update.lock"
echo "999999 $(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"
python3 -c 'import os,sys,time; t=time.time()+3600; os.utime(sys.argv[1], (t,t))' "$AGM_DIR/daemon-update.lock"
AGM_LOCK_STALE_SECS=0 run >/dev/null
check "未來 mtime 的死 runner 鎖推時間異常告警" "runner_clock_skew" "$AGM_DIR/alerts.log"
check "未來 mtime 的鎖有明確記錄" "時間在未來" "$(LOG)"
check_eq "未來 mtime 的鎖不重複部署" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
ci "$C3" success
bash -c 'sleep 30; : # daemon-update-kick' & SLEEPER=$!
mkdir -p "$AGM_DIR/daemon-update.lock"; echo "$SLEEPER $(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"
AGM_LOCK_STALE_SECS=0 run >/dev/null
check_eq "重用 PID 的 command line 含名稱也要回收" "1" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "重用 PID 不再被判成活 runner" "清掉殘留鎖" "$(LOG)"
kill "$SLEEPER" 2>/dev/null; wait "$SLEEPER" 2>/dev/null
teardown

check "認得立即部署請求檔（kick_ready 靠這個字）" "daemon-update.now.json" "$SCRIPT"
seed_ops() {
  mkdir -p "$WORK/scripts/ops"
  cp "$HERE/ops-install.sh" "$WORK/scripts/ops/ops-install.sh"
  printf 'scripts/ops/x-kick.sh  bin/x-kick.sh\n' > "$WORK/scripts/ops/install-manifest.tsv"
  printf '#!/bin/bash\necho x-v1\n' > "$WORK/scripts/ops/x-kick.sh"
  "$GITBIN" -C "$WORK" add -A >/dev/null; "$GITBIN" -C "$WORK" commit -q -m "seed ops"; "$GITBIN" -C "$WORK" push -q origin HEAD:main
  install -m 755 "$WORK/scripts/ops/x-kick.sh" "$AGM_DIR/bin/x-kick.sh"
}
bump_x() {
  printf '%s' "$1" > "$WORK/scripts/ops/x-kick.sh"
  "$GITBIN" -C "$WORK" add -A >/dev/null; "$GITBIN" -C "$WORK" commit -q -m "bump x"; "$GITBIN" -C "$WORK" push -q origin HEAD:main
  "$GITBIN" -C "$WORK" rev-parse HEAD
}
only_ops_since_built() {
  "$GITBIN" -C "$WORK" reset -q --hard "$C0"; "$GITBIN" -C "$WORK" push -q -f origin HEAD:main
  seed_ops
  "$GITBIN" -C "$AGM_REPO" reset -q --hard "$C0"
}
x_line() { sed -n 2p "$AGM_DIR/bin/x-kick.sh"; }

setup
only_ops_since_built
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
check_eq "沒問 GitHub" "0" "$(wc -l < "$AGM_DIR/gh.log" | tr -d ' ')"
check_no "log 沒提 ops 自動安裝" "ops 自動安裝" "$(LOG)"
teardown

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

setup
only_ops_since_built
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" pending
check_eq "rc=0" "0" "$(run)"
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
check "log 說等綠燈" "等綠燈" "$(LOG)"
teardown

setup
seed_ops
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
export AGM_OPS_AUTO_INSTALL=1
check_eq "rc=0" "0" "$(run)"
unset AGM_OPS_AUTO_INSTALL
check "有換版" "--sha $H" "$AGM_DIR/swap.log"
check_eq "換版後 ops 腳本也換了" "echo x-v2" "$(x_line)"
teardown

setup
seed_ops
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\necho x-v2\n'); ci "$H" success
STUB_SWAP_RC=7 run >/dev/null
check_eq "安裝端沒動" "echo x-v1" "$(x_line)"
teardown

setup
seed_ops
touch "$AGM_DIR/ops-auto-install.enabled"
H=$(bump_x $'#!/bin/bash\nif then fi (\n'); ci "$H" success
check_eq "rc=0" "0" "$(run)"
check_eq "壞的新版沒留在安裝端" "echo x-v1" "$(x_line)"
check "推了 ops_install_failed" "ops_install_failed" "$AGM_DIR/alerts.log"
check "部署照常完成" "已換上" "$(LOG)"
teardown

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

setup
"$GITBIN" -C "$WORK" reset -q --hard "$C0"; "$GITBIN" -C "$WORK" push -q -f origin HEAD:main
E=$(commit crates/am-share/src/x.rs "crate only")
ci "$E" success
export STUB_BUILD_INPUTS_FAIL=1
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_no "不能說沒有會進 binary 的差異" "沒有會進 binary 的差異" "$(LOG)"
check "有建置" "cargo build --release -p agents-managerd" "$AGM_DIR/build.log"
check "換上那顆" "--sha ${E}" "$AGM_DIR/swap.log"
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

bulk() {
  local i
  for i in $(seq 1 "$1"); do
    echo "bulk $i" >> "$WORK/daemon/src/a.rs"
    "$GITBIN" -C "$WORK" add -A >/dev/null
    "$GITBIN" -C "$WORK" commit -q -m "bulk $i"
  done
  "$GITBIN" -C "$WORK" push -q origin HEAD:main
}
setup
bulk 40
S35=$("$GITBIN" -C "$WORK" rev-parse HEAD~34)
ci "$S35" success
echo "$S35" > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check "挑到 last-sha 那顆（回看範圍之外）" "--sha $S35" "$AGM_DIR/swap.log"
check "log 說來源是 last-sha" "來自 ubuntu-ci 的 last-sha" "$(LOG)"
teardown

setup
bulk 40
ci "$("$GITBIN" -C "$WORK" rev-parse HEAD~34)" success
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "沒換版（綠燈在回看範圍之外、也沒有 last-sha）" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "log 講明是回看上限用完" "回看上限 30 顆用完" "$(LOG)"
teardown

setup
bulk 40
S35=$("$GITBIN" -C "$WORK" rev-parse HEAD~34)
ci "$S35" failure
echo "$S35" > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "紅燈的 last-sha 不換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check "log 講明是回看上限用完" "回看上限 30 顆用完" "$(LOG)"
teardown

setup
ci "$C3" success
echo "not-a-sha" > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "垃圾內容 rc=0" "0" "$rc"
check "垃圾內容時照舊走迴圈挑到 HEAD" "--sha $C3" "$AGM_DIR/swap.log"
teardown

setup
ci "$C3" success
printf '%040d\n' 0 > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "不存在的 40 碼 rc=0" "0" "$rc"
check "不存在的 40 碼也忽略，照舊挑到 HEAD" "--sha $C3" "$AGM_DIR/swap.log"
teardown

setup
"$GITBIN" -C "$WORK" checkout -q -b side "$C0"
echo "side" >> "$WORK/daemon/src/side.rs"
"$GITBIN" -C "$WORK" add -A >/dev/null; "$GITBIN" -C "$WORK" commit -q -m "side"
SIDE=$("$GITBIN" -C "$WORK" rev-parse HEAD)
"$GITBIN" -C "$WORK" push -q origin HEAD:refs/heads/side
"$GITBIN" -C "$WORK" checkout -q main
ci "$SIDE" success
echo "$SIDE" > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "不是 main 祖先的 last-sha 不換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
bulk 40
S35=$("$GITBIN" -C "$WORK" rev-parse HEAD~34)
ci "$S35" success
echo "$S35" > "$ROOT/cistate/last-sha"
echo "$S35" > "$AGM_DIR/daemon-update.rejected"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_eq "被回滾過的 last-sha 不換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
teardown

setup
ci "$C3" pending
echo "$C0" > "$ROOT/cistate/last-sha"
rc=$(run)
check_eq "rc=0" "0" "$rc"
check_no "線上那顆不問 GitHub" "commits/$C0/status" "$AGM_DIR/gh.log"
teardown

setup
ci "$C3" success
cp "$HERE/daemon-update-kick.sh" "$AGM_DIR/bin/daemon-update-kick.sh"
cp "$HERE/kick-common.sh" "$AGM_DIR/bin/kick-common.sh"
cp "$HERE/build-web-bundle.py" "$AGM_DIR/bin/build-web-bundle.py"
SOURCE_SCRIPT="$SCRIPT"
SCRIPT="$AGM_DIR/bin/daemon-update-kick.sh"
rc=$(run)
check_eq "installed kick rc=0" "0" "$rc"
check "installed paired helper published bundle" "web bundle changed: published" "$(LOG)"
check "cargo follows successful publication" "cargo build --release" "$AGM_DIR/build.log"
check_eq "target checkout has no helper" "no" "$([ -e "$ROOT/deploy/scripts/ops/build-web-bundle.py" ] && echo yes || echo no)"
SCRIPT="$SOURCE_SCRIPT"
teardown

setup
ci "$C3" success
cp "$HERE/daemon-update-kick.sh" "$AGM_DIR/bin/daemon-update-kick.sh"
cp "$HERE/kick-common.sh" "$AGM_DIR/bin/kick-common.sh"
SCRIPT="$AGM_DIR/bin/daemon-update-kick.sh"
rc=$(run)
check_eq "missing helper is handled" "0" "$rc"
check "missing paired helper is explicit" "找不到配對的 web helper" "$(LOG)"
check_no "missing helper never falls back to bun" "bun " "$AGM_DIR/build.log"
check_no "missing helper prevents cargo" "cargo " "$AGM_DIR/build.log"
check_eq "missing helper prevents swap" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
SCRIPT="$SOURCE_SCRIPT"
teardown

setup
ci "$C3" success
export STUB_BUN_FAIL=1
rc=$(run)
check "helper build failure is explicit" "web 建置失敗" "$(LOG)"
check_no "helper build failure prevents cargo" "cargo " "$AGM_DIR/build.log"
check_eq "helper build failure prevents swap" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "helper build failure removes staging" "0" "$(find "$ROOT/deploy/web" -name '.dist-stage-*' | wc -l | tr -d ' ')"
teardown

sync_setup() {
  setup
  ci "$C3" success
  "$GITBIN" -C "$AGM_REPO" reset -q --hard "$C0"
}
sync_setup
rc=$(run)
check_eq "快轉 rc=0" "0" "$rc"
check_eq "快轉：主樹到 origin/main" "$C3" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
check "快轉：log 說快轉" "主樹快轉到" "$(LOG)"
teardown

sync_setup
echo "fix one" >> "$AGM_REPO/daemon/src/a.rs"
"$GITBIN" -C "$AGM_REPO" add daemon >/dev/null
"$GITBIN" -C "$AGM_REPO" -c user.name=t -c user.email=t@t commit -q -m "local copy of fix one"
rc=$(run)
check_eq "已 rebase rc=0" "0" "$rc"
check_eq "已 rebase：主樹對齊到 origin/main" "$C3" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
check "已 rebase：log 說對齊" "對齊到" "$(LOG)"
teardown

sync_setup
echo "local only" > "$AGM_REPO/daemon/src/local.rs"
"$GITBIN" -C "$AGM_REPO" add daemon >/dev/null
"$GITBIN" -C "$AGM_REPO" -c user.name=t -c user.email=t@t commit -q -m "local only"
LOCAL=$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)
rc=$(run)
check_eq "有本地獨有 commit rc=0" "0" "$rc"
check_eq "有本地獨有 commit：主樹不動" "$LOCAL" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
check "有本地獨有 commit：推 main_tree_diverged" "main_tree_diverged" "$AGM_DIR/alerts.log"
teardown

sync_setup
echo "$(echo "$C3" | cut -c1-8)" > "$AGM_DIR/daemon-update.built"
rc=$(run)
check_eq "no-diff rc=0" "0" "$rc"
check_eq "no-diff：沒有換版" "0" "$(wc -l < "$AGM_DIR/swap.log" | tr -d ' ')"
check_eq "no-diff：主樹快轉到 origin/main" "$C3" "$("$GITBIN" -C "$AGM_REPO" rev-parse HEAD)"
check "no-diff：log 說快轉" "主樹快轉到" "$(LOG)"
teardown

fake_live() {
  local sha="${1%-dirty}" dirty=false
  [ "$sha" = "$1" ] || dirty=true
  printf '{"last_deploy":{"sha":"%s","sha_full":"%s","dirty":%s}}' "$(echo "$sha" | cut -c1-8)" "$sha" "$dirty" > "$AGM_DIR/live.json"
}
fake_disk() {
  printf '#!/bin/sh\necho "agents-managerd 0.2.0 %s"\n' "$1" > "$AGM_REPO/target/release/agents-managerd"
  chmod +x "$AGM_REPO/target/release/agents-managerd"
}
short() { echo "$1" | cut -c1-8; }

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_live "$C1"
ci "$C3" success
rc=$(STUB_SWAP_RC=3 run)
check_eq "過期 .built：rc=0" "0" "$rc"
check_eq "過期 .built：更正成線上的 short sha" "$(short "$C1")" "$(cat "$AGM_DIR/daemon-update.built")"
check "過期 .built：換版的舊版是線上的 sha" "--old $(short "$C1")" "$AGM_DIR/swap.log"
check "過期 .built：推 built_reconciled" "built_reconciled" "$AGM_DIR/alerts.log"
check "過期 .built：log 說以線上為準" "已以線上為準更新" "$(LOG)"
teardown

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_live "$(printf 'b%.0s' $(seq 1 40))"
ci "$C3" success
STUB_SWAP_RC=3 run >/dev/null
check_eq "線上 sha 不在 checkout：.built 不動" "$(short "$C0")" "$(cat "$AGM_DIR/daemon-update.built")"
check "線上 sha 不在 checkout：沿用 .built 的舊版" "--old $(short "$C0")" "$AGM_DIR/swap.log"
check "線上 sha 不在 checkout：log 說沿用" "沿用" "$(LOG)"
check_no "線上 sha 不在 checkout：不推 built_reconciled" "built_reconciled" "$AGM_DIR/alerts.log"
teardown

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_live "$C1-dirty"
ci "$C3" success
STUB_SWAP_RC=3 run >/dev/null
check_eq "髒樹 binary：.built 不動" "$(short "$C0")" "$(cat "$AGM_DIR/daemon-update.built")"
check "髒樹 binary：沿用 .built 的舊版" "--old $(short "$C0")" "$AGM_DIR/swap.log"
teardown

setup
rm -f "$AGM_DIR/daemon-update.built"
fake_live "$C1"
ci "$C3" success
rc=$(STUB_SWAP_RC=3 run)
check_eq ".built 缺檔、線上可辨認：rc=0" "0" "$rc"
check_eq ".built 缺檔：由線上補回" "$(short "$C1")" "$(cat "$AGM_DIR/daemon-update.built")"
check_no ".built 缺檔：不推 built_unknown" "built_unknown" "$AGM_DIR/alerts.log"
teardown

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_live "$C0"
ci "$C3" success
STUB_SWAP_RC=3 run >/dev/null
check "換版 rc=3（窗口後線上已改）：舊版是 C0" "--old $(short "$C0")" "$AGM_DIR/swap.log"
fake_live "$C1"
STUB_SWAP_RC=0 run >/dev/null
check "下一輪自癒：舊版改成重新讀到的線上 C1" "--old $(short "$C1")" "$AGM_DIR/swap.log"
check_eq "下一輪自癒：.built 也對上線上" "$(short "$C3")" "$(cat "$AGM_DIR/daemon-update.built")"
teardown

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_live "$C0"
fake_disk "$C1"
ci "$C3" success
STUB_SWAP_RC=3 run >/dev/null
check_eq "磁碟上重建了 binary、daemon 沒重啟：.built 不動" "$(short "$C0")" "$(cat "$AGM_DIR/daemon-update.built")"
check "磁碟上重建了 binary：換版的舊版是執行中的 C0" "--old $(short "$C0")" "$AGM_DIR/swap.log"
check_no "磁碟上重建了 binary：不推 built_reconciled" "built_reconciled" "$AGM_DIR/alerts.log"
teardown

setup
echo "$(short "$C0")" > "$AGM_DIR/daemon-update.built"
fake_disk "$C1"
ci "$C3" success
STUB_SWAP_RC=3 run >/dev/null
check_eq "問不到執行中的 daemon：不改讀磁碟 binary，.built 不動" "$(short "$C0")" "$(cat "$AGM_DIR/daemon-update.built")"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
