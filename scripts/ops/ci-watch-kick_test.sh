#!/bin/bash
# ci-watch-kick.sh 的隔離測試：假的 `gh`、假的 `bin/agm`，完全不碰 GitHub、正式 AGM 或 launchd。
# 測的是決策：什麼時候開 issue、派幾次、失敗清單變了留言、恢復綠、狀態檔遺失、gh 失敗、鎖回收、最小 PATH。
#
#   bash scripts/ops/ci-watch-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/ci-watch-kick.sh"
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" GHDIR="$ROOT/gh"
  mkdir -p "$AGM_DIR/bin" "$AGM_REPO" "$GHDIR" "$ROOT/fakebin"
  cp "$HERE/fixtures/patrol-runtime.json" "$AGM_DIR/runtime.json"
  echo "TASK-BODY-MARKER 交辦正文" > "$AGM_DIR/ci-watch-task.md"
  : > "$AGM_DIR/calls.log"; : > "$GHDIR/calls.log"; : > "$GHDIR/created.log"; : > "$GHDIR/comments.log"; : > "$AGM_DIR/assign-body.txt"

  # 假 agm：assign／ops-alert；未知旗標一律 exit 2（2026-09-16 `--request-id` 拼錯事故的教訓）。
  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
for a in "$@"; do
  case "$a" in
    --compact|--bot|--review-by|--text-file|--request-id|--source|--reason|--detail|assign|ops-alert) ;;
    --*) echo "agm: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
case "$*" in
  *" assign "*)
    [ -n "${STUB_ASSIGN_FAIL:-}" ] && exit 1
    for i in $(seq 1 $#); do
      eval "a=\${$i}"
      case "$a" in --text-file) eval "f=\${$((i+1))}"; cat "$f" >> "$AGM_DIR/assign-body.txt" ;; esac
    done
    printf '%s' '{"id":"a-1"}' ;;
  *ops-alert*) printf '%s' '{"ok":true}' ;;
esac
STUB
  chmod +x "$AGM_DIR/bin/agm"

  # 假 gh：run list 吐 $GHDIR/runs.json；run view --log-failed 吐 $GHDIR/log-<id>.txt；issue list 吐 $GHDIR/issues.txt（編號，可空）；
  # issue create／comment／label create 只記錄。STUB_GH_FAIL=1 全部失敗；未知子命令或旗標 exit 2。
  cat > "$ROOT/fakebin/gh" <<'STUB'
#!/bin/bash
echo "$*" >> "$GHDIR/calls.log"
[ -n "${STUB_GH_FAIL:-}" ] && { echo "HTTP 403: API rate limit exceeded" >&2; exit 1; }
case "$1 $2" in
  "run list")
    case "$*" in *"--json databaseId,conclusion,status,headSha,createdAt"*) ;; *) echo "gh: bad --json" >&2; exit 2 ;; esac
    case "$*" in *"--branch main"*"--workflow CI"*) ;; *) echo "gh: bad filter" >&2; exit 2 ;; esac
    cat "$GHDIR/runs.json" ;;
  "run view")
    [ "$4" = "--log-failed" ] || exit 2
    [ -n "${STUB_LOG_FAIL:-}" ] && exit 1
    cat "$GHDIR/log-$3.txt" 2>/dev/null ;;
  "issue list")
    case "$*" in *"--label ci-red"*"--state open"*) ;; *) exit 2 ;; esac
    cat "$GHDIR/issues.txt" 2>/dev/null; exit 0 ;;
  "issue create")
    [ -n "${STUB_CREATE_FAIL:-}" ] && exit 1
    n=$(( $(grep -c '^TITLE ' "$GHDIR/created.log") + 100 ))
    shift 2
    while [ $# -gt 0 ]; do
      case "$1" in
        --title) echo "TITLE $2" >> "$GHDIR/created.log"; shift 2 ;;
        --label) echo "LABEL $2" >> "$GHDIR/created.log"; shift 2 ;;
        --body-file) sed 's/^/BODY /' "$2" >> "$GHDIR/created.log"; shift 2 ;;
        *) echo "gh: unknown flag $1" >&2; exit 2 ;;
      esac
    done
    echo "https://github.com/o/r/issues/$n" ;;
  "issue comment") echo "COMMENT $3 :: $5" >> "$GHDIR/comments.log" ;;
  "label create") echo "label" >> "$GHDIR/labels.log" ;;
  *) echo "gh: unknown $*" >&2; exit 2 ;;
esac
STUB
  chmod +x "$ROOT/fakebin/gh"
  export PATH="$ROOT/fakebin:$PATH" AGM_EXTRA_PATH=""   # 不讓腳本把真的 /opt/homebrew/bin/gh 排到假 gh 前面
  unset AGM_CI_BOT STUB_GH_FAIL STUB_LOG_FAIL STUB_CREATE_FAIL STUB_ASSIGN_FAIL
}
teardown() {
  rm -rf "$ROOT"
  unset AGM_DIR AGM_REPO GHDIR AGM_CI_BOT AGM_LOCK_STALE_SECS AGM_LOCK_HUNG_SECS AGM_EXTRA_PATH STUB_GH_FAIL STUB_LOG_FAIL STUB_CREATE_FAIL STUB_ASSIGN_FAIL
  export PATH="${PATH#"$ROOT/fakebin:"}"
}

# mk_runs <id:conclusion:status>…：最新的排前面，headSha 用 sha<id>，createdAt 隨 id 遞增。
mk_runs() {
  python3 - "$GHDIR/runs.json" "$@" <<'PY'
import json, sys
path, *specs = sys.argv[1:]
out = []
for s in specs:
    i, concl, status = (s.split(":") + ["completed"])[:3]
    out.append({"databaseId": int(i), "conclusion": concl, "status": status, "headSha": f"sha{i}",
                "createdAt": "2026-09-19T00:%02d:00Z" % int(i), "url": f"https://github.com/o/r/actions/runs/{i}"})
out.sort(key=lambda r: r["createdAt"], reverse=True)
json.dump(out, open(path, "w"))
PY
}
# mk_log <run-id> <name…>：cargo 那一種＋一條 python 的。
mk_log() {
  local id="$1"; shift
  : > "$GHDIR/log-$id.txt"
  for n in "$@"; do
    printf 'daemon\tRun tests\t2026-09-19T01:00:00.0000000Z test %s ... FAILED\n' "$n" >> "$GHDIR/log-$id.txt"
  done
  printf 'daemon\tRun tests\t2026-09-19T01:00:01.0000000Z failures:\n' >> "$GHDIR/log-$id.txt"
  for n in "$@"; do printf 'daemon\tRun tests\t2026-09-19T01:00:01.0000000Z     %s\n' "$n" >> "$GHDIR/log-$id.txt"; done
  printf 'daemon\tRun tests\t2026-09-19T01:00:02.0000000Z \n' >> "$GHDIR/log-$id.txt"
}

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
assigns() { grep -c ' assign ' "$AGM_DIR/calls.log" 2>/dev/null | tr -d ' '; }
creates() { grep -c '^TITLE ' "$GHDIR/created.log" 2>/dev/null | tr -d ' '; }
comments() { grep -c '^COMMENT' "$GHDIR/comments.log" 2>/dev/null | tr -d ' '; }
state() { python3 -c 'import json,sys; v=json.load(open(sys.argv[1])).get(sys.argv[2]); print(json.dumps(v) if isinstance(v,(list,bool)) else v)' "$AGM_DIR/ci-watch.state.json" "$1" 2>/dev/null; }

# 1. 一直綠：安靜（不寫 log、不開、不派、不留言）。
setup
mk_runs 3:success 2:success 1:success
bash "$SCRIPT"
equals "綠：不開" "$(creates)" "0"
equals "綠：不派" "$(assigns)" "0"
equals "綠：不寫 log" "$(cat "$AGM_DIR/ci-watch.log" 2>/dev/null)" ""
[ ! -e "$AGM_DIR/ci-watch.state.json" ] && { echo "ok   - 綠：不建狀態檔"; PASS=$((PASS + 1)); } || { echo "FAIL - 綠：不建狀態檔"; FAIL=$((FAIL + 1)); }
teardown

# 2. 綠→紅：開一張＋派一次；標題、標籤、內文、request-id、派給誰、狀態檔。
setup
( cd "$AGM_REPO" && git init -q && git config user.email t@t && git config user.name t &&
  for n in 1 2 3; do echo $n > f; git add f; git commit -q -m "commit $n"; done )
G=$(git -C "$AGM_REPO" rev-parse HEAD~2); F=$(git -C "$AGM_REPO" rev-parse HEAD)
mk_runs 5:failure 4:failure 3:success
python3 - "$GHDIR/runs.json" "$G" "$F" <<'PY'
import json, sys
p, g, f = sys.argv[1:]; d = json.load(open(p))
for r in d:
    if r["databaseId"] == 3: r["headSha"] = g
    if r["databaseId"] == 4: r["headSha"] = f
json.dump(d, open(p, "w"))
PY
mk_log 5 mod::a mod::b
bash "$SCRIPT"
equals "開一張 issue" "$(creates)" "1"
check "標題：sha 前 8 碼＋條數" "^TITLE CI 紅了：${F:0:8} 起 2 條失敗" "$GHDIR/created.log"
check "標籤 ci-red" "^LABEL ci-red" "$GHDIR/created.log"
check "內文有第一個紅的 run 連結" "actions/runs/4" "$GHDIR/created.log"
check "內文有失敗測試" 'BODY - `mod::a`' "$GHDIR/created.log"
check "內文有嫌疑 commit（上一個綠之後）" "commit 3" "$GHDIR/created.log"
check_no "嫌疑 commit 不含上一個綠自己" "BODY .*commit 1" "$GHDIR/created.log"
equals "派一次" "$(assigns)" "1"
check "request-id 帶第一個紅的 sha" "\-\-request-id ci-red-${F}" "$AGM_DIR/calls.log"
check "派給協調者" "\-\-bot bot-resp" "$AGM_DIR/calls.log"
check_no "不派給巡檢自己" "\-\-bot bot-agm" "$AGM_DIR/calls.log"
check "交辦帶正文" "TASK-BODY-MARKER" "$AGM_DIR/assign-body.txt"
check "交辦帶失敗測試" "mod::b" "$AGM_DIR/assign-body.txt"
equals "狀態：first_red_sha 是那段紅的第一個" "$(state first_red_sha)" "$F"
equals "狀態：issue 編號" "$(state issue)" "100"
equals "狀態：assigned" "$(state assigned)" "true"
check "有補建標籤" "label" "$GHDIR/labels.log"
teardown

# 3. 同一段紅連跑三輪：只有一張、一派、零留言。
setup
mk_runs 5:failure 4:failure 3:success; mk_log 5 mod::a
bash "$SCRIPT"; bash "$SCRIPT"; bash "$SCRIPT"
equals "三輪只開一張" "$(creates)" "1"
equals "三輪只派一次" "$(assigns)" "1"
equals "沒變就不留言" "$(comments)" "0"
teardown

# 4. 失敗清單變了（多了新的測試）：同一張 issue 留言一次；再跑不重複留言。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash "$SCRIPT"
mk_runs 6:failure 5:failure 4:success; mk_log 6 mod::a mod::c
bash "$SCRIPT"; bash "$SCRIPT"
equals "清單變了：留言一次" "$(comments)" "1"
check "留言在原本那張" 'COMMENT 100 :: run 6' "$GHDIR/comments.log"
check "留言含新的測試" 'mod::c' "$GHDIR/comments.log"
equals "仍只一張、一派" "$(creates)$(assigns)" "11"
check "狀態記下新清單" "mod::c" "$AGM_DIR/ci-watch.state.json"
teardown

# 5. 紅→綠：在 issue 留言「sha 起恢復綠」並清狀態、不關 issue；之後綠安靜。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash "$SCRIPT"
mk_runs 6:success 5:failure 4:success
bash "$SCRIPT"
check "綠了留言（sha＋run）" "COMMENT 100 :: sha6 起恢復綠，run 6" "$GHDIR/comments.log"
[ ! -e "$AGM_DIR/ci-watch.state.json" ] && { echo "ok   - 綠了清狀態"; PASS=$((PASS + 1)); } || { echo "FAIL - 綠了清狀態"; FAIL=$((FAIL + 1)); }
check_no "不自動關 issue" "issue close" "$GHDIR/calls.log"
bash "$SCRIPT"
equals "之後再綠不重複留言" "$(comments)" "1"
# 再紅一次是新的一段：重新開一張、重新派（request-id 是新的 sha）。
mk_runs 8:failure 7:success 6:success; mk_log 8 mod::z
bash "$SCRIPT"
equals "新的一段紅：再開一張" "$(creates)" "2"
check "新的一段紅：新 request-id" "\-\-request-id ci-red-sha8" "$AGM_DIR/calls.log"
teardown

# 6. 狀態檔遺失但 issue 還開著：不重開、不重派，接手。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
echo 77 > "$GHDIR/issues.txt"
bash "$SCRIPT"
equals "狀態檔遺失：不重開" "$(creates)" "0"
equals "狀態檔遺失：不重派" "$(assigns)" "0"
equals "接手現成的 issue" "$(state issue)" "77"
check "有記 log" "已有開著的 ci-red issue #77" "$AGM_DIR/ci-watch.log"
teardown

# 7. gh 失敗（rate limit）：什麼都不做、不改狀態、記一行 log。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash "$SCRIPT"
BEFORE=$(cat "$AGM_DIR/ci-watch.state.json")
mk_runs 6:success 5:failure
export STUB_GH_FAIL=1
bash "$SCRIPT"; equals "gh 失敗：exit 0" "$?" "0"
equals "gh 失敗：狀態不動" "$(cat "$AGM_DIR/ci-watch.state.json")" "$BEFORE"
equals "gh 失敗：不留言（不誤報綠）" "$(comments)" "0"
equals "gh 失敗：記一行 log" "$(grep -c 'gh run list 失敗' "$AGM_DIR/ci-watch.log")" "1"
teardown
setup
mk_runs 5:failure 4:success
export STUB_GH_FAIL=1
bash "$SCRIPT"
equals "gh 失敗且沒狀態：不開" "$(creates)" "0"
[ ! -e "$AGM_DIR/ci-watch.state.json" ] && { echo "ok   - gh 失敗：不誤建狀態"; PASS=$((PASS + 1)); } || { echo "FAIL - gh 失敗：不誤建狀態"; FAIL=$((FAIL + 1)); }
teardown
# 7b. 取失敗 log 失敗、開 issue 失敗：不動狀態，下一輪再來。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
export STUB_LOG_FAIL=1
bash "$SCRIPT"
equals "取不到失敗 log：不開" "$(creates)" "0"
unset STUB_LOG_FAIL; export STUB_CREATE_FAIL=1
bash "$SCRIPT"
[ ! -e "$AGM_DIR/ci-watch.state.json" ] && { echo "ok   - 開 issue 失敗：不寫狀態"; PASS=$((PASS + 1)); } || { echo "FAIL - 開 issue 失敗：不寫狀態"; FAIL=$((FAIL + 1)); }
unset STUB_CREATE_FAIL
bash "$SCRIPT"
equals "下一輪重試就開成功" "$(creates)" "1"
teardown

# 8. cancelled／skipped／進行中不算紅也不算綠。
setup
mk_runs 6:cancelled 5:skipped 4:success 3:failure:in_progress
bash "$SCRIPT"
equals "只有 cancelled／skipped／進行中：不開" "$(creates)" "0"
teardown
setup
mk_runs 7:failure 6:cancelled 5:failure 4:success; mk_log 7 mod::a
bash "$SCRIPT"
check "cancelled 夾在中間，紅段起點跳過它（第一個紅是 5）" "^TITLE CI 紅了：sha5" "$GHDIR/created.log"
teardown
setup
mk_runs 6:failure 5:success; mk_log 6 mod::a
bash "$SCRIPT"
mk_runs 7:cancelled 6:failure 5:success
bash "$SCRIPT"
equals "紅段中來了 cancelled：不當綠、不留言" "$(comments)" "0"
[ -e "$AGM_DIR/ci-watch.state.json" ] && { echo "ok   - cancelled：狀態還在"; PASS=$((PASS + 1)); } || { echo "FAIL - cancelled：狀態還在"; FAIL=$((FAIL + 1)); }
teardown

# 9. 派工失敗：issue 已開、狀態記 assigned=false，下一輪補派（同 request-id），不重開。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
equals "派工失敗：issue 已開" "$(creates)" "1"
equals "派工失敗：assigned=false" "$(state assigned)" "false"
unset STUB_ASSIGN_FAIL
bash "$SCRIPT"
equals "下一輪補派" "$(assigns)" "2"
equals "補派後 assigned=true" "$(state assigned)" "true"
equals "補派不重開" "$(creates)" "1"
check "補派用同一個 request-id" "ci-red-sha5" "$AGM_DIR/calls.log"
teardown

# 10. 派給誰：AGM_CI_BOT ＞ runtime.json 的 ci_bot_id ＞ responder_bot_id；找不到就不派（issue 仍開）。
setup
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["ci_bot_id"]="bot-ci"; json.dump(d,open(sys.argv[1],"w"))' "$AGM_DIR/runtime.json"
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash "$SCRIPT"
check "優先派給 ci_bot_id" "\-\-bot bot-ci" "$AGM_DIR/calls.log"
teardown
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
export AGM_CI_BOT=bot-override
bash "$SCRIPT"
check "env 覆寫優先" "\-\-bot bot-override" "$AGM_DIR/calls.log"
teardown
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
rm -f "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "找不到對象：記 log" "找不到要派給誰" "$AGM_DIR/ci-watch.log"
equals "找不到對象：不亂派" "$(assigns)" "0"
equals "找不到對象：issue 仍開" "$(creates)" "1"
teardown

# 11. 殘留鎖（pid 不存在）：回收並接手；剛建立的鎖先不動；活鎖擋下、卡太久喊人；pid 被別的程序重用視為殘留。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
mkdir "$AGM_DIR/ci-watch.lock"; echo "999999 1" > "$AGM_DIR/ci-watch.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "殘留鎖：回收後照開" "$(creates)" "1"
check "殘留鎖：有記回收 log" "清掉殘留鎖" "$AGM_DIR/ci-watch.log"
[ ! -d "$AGM_DIR/ci-watch.lock" ] && { echo "ok   - 跑完鎖有釋放"; PASS=$((PASS + 1)); } || { echo "FAIL - 跑完鎖有釋放"; FAIL=$((FAIL + 1)); }
teardown
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
mkdir "$AGM_DIR/ci-watch.lock"
bash "$SCRIPT"
equals "剛建立、讀不到執行者的鎖：這輪跳過" "$(creates)" "0"
teardown
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash -c 'sleep 30; : # ci-watch-kick' & LIVE=$!
sleep 0.3
mkdir "$AGM_DIR/ci-watch.lock"; echo "$LIVE $(date +%s)" > "$AGM_DIR/ci-watch.lock/owner"
bash "$SCRIPT"
equals "活鎖：不開" "$(creates)" "0"
check_no "活鎖：沒到卡住門檻不喊人" "ops-alert" "$AGM_DIR/calls.log"
export AGM_LOCK_HUNG_SECS=0
bash "$SCRIPT"
check "活鎖卡太久：推 ops-alert" "ops-alert .*\-\-reason runner_hung" "$AGM_DIR/calls.log"
equals "活鎖卡太久：仍不搶鎖" "$(creates)" "0"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
bash -c 'sleep 30; :' & LIVE=$!
sleep 0.3
mkdir "$AGM_DIR/ci-watch.lock"; echo "$LIVE $(date +%s)" > "$AGM_DIR/ci-watch.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "pid 被別的程序重用：回收後照開" "$(creates)" "1"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

# 12. launchd 的最小環境：env -i、PATH 只有 /usr/bin:/bin（加上假 gh 的目錄），系統 /bin/bash 也要跑得起來。
setup
mk_runs 5:failure 4:success; mk_log 5 mod::a
env -i PATH="$ROOT/fakebin:/usr/bin:/bin" AGM_EXTRA_PATH="" AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" GHDIR="$GHDIR" /bin/bash "$SCRIPT"
equals "env -i 最小 PATH：照開" "$(creates)" "1"
equals "env -i 最小 PATH：照派" "$(assigns)" "1"
teardown
# 12b. 找不到 gh／python3：log＋ops_alert，不是靜默 exit 0。
setup
mk_runs 5:failure 4:success
mkdir "$ROOT/nogh"; for t in date cat seq dirname sed head cut; do ln -s "$(command -v $t)" "$ROOT/nogh/$t"; done
ln -s "$(command -v python3)" "$ROOT/nogh/python3"
env -i PATH="$ROOT/nogh" AGM_EXTRA_PATH="" AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" /bin/bash "$SCRIPT"
check "缺 gh：有記 log" "找不到 gh" "$AGM_DIR/ci-watch.log"
check "缺 gh：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
teardown
setup
mk_runs 5:failure 4:success
mkdir "$ROOT/nopy"; for t in date cat seq dirname; do ln -s "$(command -v $t)" "$ROOT/nopy/$t"; done; ln -s "$ROOT/fakebin/gh" "$ROOT/nopy/gh"
env -i PATH="$ROOT/nopy" AGM_EXTRA_PATH="" AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" /bin/bash "$SCRIPT"
check "缺 python3：有記 log" "找不到 python3" "$AGM_DIR/ci-watch.log"
check "缺 python3：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
teardown

# 13. 假 agm／gh 對未知旗標要 exit 2（守住 stub 本身）。
setup
"$AGM_DIR/bin/agm" --compact assign --bot x --client-request-id y 2>/dev/null; equals "agm stub 對未知旗標 exit 2" "$?" "2"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
