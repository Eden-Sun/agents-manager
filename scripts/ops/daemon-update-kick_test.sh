#!/bin/bash
# daemon-update-kick.sh 的隔離測試。
#
# 完全不碰正式 daemon、正式 repo 或正式 AGM 目錄：每個 case 自己開一個暫存 git repo 與一支
# 假的 `bin/agm`，用環境變數餵回應，再檢查腳本做了什麼決定（log 與它送出的指令）。
# 測的是**決策**——什麼時候不派、什麼時候申請核准、拿不到窗口時會不會硬做。
#
#   bash scripts/ops/daemon-update-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-update-kick.sh"
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" AGM_BUILD_BOT="bot-build" AM_AGENT_NAME="test-owner"
  export AGM_TEST_MINUTE="00"   # 預設當成整點那一輪；門檻的 case 自己覆寫
  mkdir -p "$AGM_DIR/bin"
  echo "TASK-BODY 建置說明" > "$AGM_DIR/daemon-update-task.md"
  printf '%s' '{"manager_bot_id":"bot-manager","bot_id":"legacy-not-manager"}' > "$AGM_DIR/runtime.json"
  # 一個有 origin/main 的最小 repo。
  /usr/bin/git init -q "$AGM_REPO"
  ( cd "$AGM_REPO" && /usr/bin/git config user.email t@t && /usr/bin/git config user.name t \
      && mkdir -p daemon docs/goals scripts && echo x > daemon/main.rs && echo p > docs/goals/agm-supervisor-persona.md \
      && /usr/bin/git add -A && /usr/bin/git commit -qm init && /usr/bin/git branch -qf origin-main \
      && /usr/bin/git update-ref refs/remotes/origin/main HEAD ) >/dev/null 2>&1
  # 假的 agm：把呼叫寫進 calls.log，回應從 STUB_* 環境變數讀（在各 case 覆寫）。
  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
# 第一個不是 --flag 的參數是子命令，下一個是它的 op。
sub=""; op=""
for a in "$@"; do
  case "$a" in --*) continue;; esac
  if [ -z "$sub" ]; then sub="$a"; elif [ -z "$op" ]; then op="$a"; fi
done
case "$sub:$op" in
  build-inputs:*)    printf '%s' "$STUB_BUILD_INPUTS" ;;
  state:*)           printf '%s' "$STUB_STATE" ;;
  assignments:*)     printf '%s' "$STUB_ASSIGNMENTS" ;;
  lease:safety)      printf '%s' "$STUB_SAFETY" ;;
  lease:acquire)     printf '%s' "$STUB_ACQUIRE" ;;
  lease:release)     printf '%s' '{"released":true}' ;;
  approval:request)  printf '%s' "$STUB_APPROVAL" ;;
  approval:list)     printf '%s' "$STUB_APPROVAL_LIST" ;;
  approval:)         printf '%s\n' "${STUB_APPROVAL_HELP- --supersedes APPROVAL_ID}" ;;   # `approval --help`
  assign:*)          for i in $(seq 1 $#); do
                       eval "a=\${$i}"
                       case "$a" in --text-file) eval "f=\${$((i+1))}"; cat "$f" >> "$AGM_DIR/assign-body.txt" ;; esac
                     done
                     [ -n "$STUB_ASSIGN_FAIL" ] && exit 1; printf '%s' '{"id":"a-1"}' ;;
  responder:show)    [ -n "${STUB_RESPONDER:-}" ] && printf '%s' "$STUB_RESPONDER" || printf '%s' '{}' ;;
  *)                 printf '%s' '{}' ;;
esac
STUB
  chmod +x "$AGM_DIR/bin/agm"
  : > "$AGM_DIR/calls.log"
  : > "$AGM_DIR/assign-body.txt"
  # 預設是「一路順」，各 case 只覆寫自己要測的那一項。
  export STUB_BUILD_INPUTS='{"paths":["daemon","web","Cargo.toml","docs/goals/agm-supervisor-persona.md","scripts/agm.py"]}'
  export STUB_STATE='{"bots":[{"id":"bot-build","name":"build"}]}'
  export STUB_ASSIGNMENTS='{"assignments":[]}'
  export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
  export STUB_ACQUIRE='{"lease":{"fence":7,"resource":"rebuild"}}'
  export STUB_APPROVAL='{"id":"ap-1","status":"pending"}'
  export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"approved"}]}'
  export STUB_ASSIGN_FAIL=""
}

teardown() { rm -rf "$ROOT"; unset AGM_FAIL_ALERT_AFTER AGM_BUILD_BOT AGM_TEST_MINUTE AGM_REBUILD_THRESHOLD AGM_REBUILD_MAX_WAIT_MIN; }

check() { # check <描述> <要出現的字串> <檔案>
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  fi
}

check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "FAIL - $1"; echo "      不該出現 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else
    echo "ok   - $1"; PASS=$((PASS + 1))
  fi
}

# 1. 一路順的情形：申請核准 → 取得窗口 → 派工，而且未結案查詢用的是 --open。
setup
bash "$SCRIPT"
check "順利時會派工" "已派工 agm-daemon-update-" "$AGM_DIR/daemon-update.log"
check "未結案判斷用 --open（含 awaiting_review）" "assignments --open" "$AGM_DIR/calls.log"
check "先申請核准" "approval request" "$AGM_DIR/calls.log"
check "再取得 rebuild 窗口" "lease acquire rebuild" "$AGM_DIR/calls.log"
check "派工帶 ownership" "--owns daemon" "$AGM_DIR/calls.log"
teardown

# 2. 上一筆還沒結案（awaiting_review）：不要再疊一筆。舊版只看 completed/failed 會在這裡出錯。
setup
export STUB_ASSIGNMENTS='{"assignments":[{"client_request_id":"agm-daemon-update-abc","status":"awaiting_review"}]}'
bash "$SCRIPT"
check "上一筆等驗收時不再派" "還沒結案" "$AGM_DIR/daemon-update.log"
check_no "而且不會去申請核准" "approval request" "$AGM_DIR/calls.log"
teardown

# 3. 有人在跑：不取窗口、不派工。**核准照樣先申請**——「等太久就縮小封鎖面」的計時是從
# 自己這筆核准被核准的時刻起算（SPEC §18.10），不先申請的話那個時鐘永遠不會開始走。
setup
export STUB_SAFETY='{"safe":false,"working":[{"bot_id":"bot-busy","name":"bot-busy"}],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "有人在跑就不派" "還有人在跑（bot-busy）" "$AGM_DIR/daemon-update.log"
check_no "有人在跑不取窗口" "lease acquire" "$AGM_DIR/calls.log"
check_no "有人在跑不派工" "assign --bot" "$AGM_DIR/calls.log"
check "safety 帶著自己那筆核准問" "lease safety --approval" "$AGM_DIR/calls.log"
teardown

# 4. AGM 還沒核准：停在這裡，不硬做，也不去拿窗口。
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
check "沒核准就停住" "核准狀態是 pending" "$AGM_DIR/daemon-update.log"
check_no "沒核准不會去拿窗口" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 5. 窗口被別人拿走：不派工（這正是兩個執行者同時「等空檔」時的那一半）。
setup
export STUB_ACQUIRE='{"error":"conflict"}'
bash "$SCRIPT"
check "拿不到窗口就不派" "拿不到 rebuild 窗口" "$AGM_DIR/daemon-update.log"
check_no "不會硬派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

# 6. 派工失敗要把窗口還回去，不然下一輪永遠卡著。
setup
export STUB_ASSIGN_FAIL=yes
bash "$SCRIPT"
check "派工失敗會交還窗口" "lease release rebuild" "$AGM_DIR/calls.log"
teardown

# 6b. 連續沒能完成：過閘的那幾輪累計，連續 N 輪推 ops_alert；非整點輪不動計數；完整跑完清零。
setup
export STUB_STATE='{"bots":[]}' AGM_FAIL_ALERT_AFTER=3
bash "$SCRIPT"; AGM_TEST_MINUTE=37 bash "$SCRIPT"; bash "$SCRIPT"
check_no "連續 2 輪（中間夾一輪非整點）還不喊人" "ops-alert" "$AGM_DIR/calls.log"
bash "$SCRIPT"
check "連續 3 個過閘輪推 check_failing" "ops-alert.*check_failing" "$AGM_DIR/calls.log"
export STUB_STATE='{"bots":[{"id":"bot-build","name":"build"}]}'
bash "$SCRIPT"
[ ! -e "$AGM_DIR/daemon-update.fails" ] && { echo "ok   - 完整跑完一輪清零"; PASS=$((PASS + 1)); } || { echo "FAIL - 完整跑完一輪清零"; FAIL=$((FAIL + 1)); }
unset AGM_FAIL_ALERT_AFTER
teardown

# 6c. 任務說明檔不在：不申請核准、不拿租約、不派（以前會拿了租約派出一則沒有說明的交辦）。
setup
rm -f "$AGM_DIR/daemon-update-task.md"
bash "$SCRIPT"
check "任務檔不在有記 log" "daemon-update-task.md" "$AGM_DIR/daemon-update.log"
check_no "不申請核准" "approval request" "$AGM_DIR/calls.log"
check_no "不拿租約" "lease acquire" "$AGM_DIR/calls.log"
check_no "不派工" "assign" "$AGM_DIR/calls.log"
teardown

# 7. 沒設建置 child 就整支跳過——絕不改派給使用者的 bot。
setup
unset AGM_BUILD_BOT
bash "$SCRIPT"
check "沒有建置 child 就跳過" "沒設 AGM_BUILD_BOT" "$AGM_DIR/daemon-update.log"
teardown

# 8. Real asynchronous decision: the next run must reuse the first request, not create ap-2.
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL='{"id":"ap-2","status":"pending"}'
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"approved"}]}'
bash "$SCRIPT"
check_no "跨次執行不再申請新 ID" "approval request" "$AGM_DIR/calls.log"
check "接續原核准取得租約" "--approval ap-1" "$AGM_DIR/calls.log"
check "核准後才派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

# 9. Transport failures and malformed replies cannot mean there is no pending work.
setup
export STUB_ASSIGNMENTS='{"error":"unavailable"}'
bash "$SCRIPT"
check "查派工失敗會停住" "無法確認未結案派工" "$AGM_DIR/daemon-update.log"
check_no "查派工失敗不申請" "approval request" "$AGM_DIR/calls.log"
teardown

# 10. A denial is durable, not a reason to spam AGM with another approval next hour.
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"denied"}]}'
bash "$SCRIPT"
: > "$AGM_DIR/calls.log"
bash "$SCRIPT"
check_no "拒絕後不重複申請" "approval request" "$AGM_DIR/calls.log"
check_no "拒絕後不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 11. Expiry permits a new request on the next run, never using the expired approval.
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"approved","expires_at":"2000-01-01T00:00:00Z"}]}'
bash "$SCRIPT"
check_no "過期不取租約" "lease acquire" "$AGM_DIR/calls.log"
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL='{"id":"ap-2","status":"pending"}'
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-2","status":"pending"}]}'
bash "$SCRIPT"
check "過期後可重新申請" "approval request" "$AGM_DIR/calls.log"
teardown

# 12. A lost/corrupt approval list cannot authorize execution or create another request.
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL_LIST='{"approvals":null}'
bash "$SCRIPT"
check "核准讀取失敗會停住" "ALERT approval_missing" "$AGM_DIR/daemon-update.log"
# 只寫 log 會永久靜默停住（review 2026-09-16 c1 M1）：要推一則 durable 事件給 AGM。
check "核准查不到會喊人" "ops-alert --source test-owner --reason approval_missing" "$AGM_DIR/calls.log"
check_no "不拿新申請繞過讀取錯誤" "approval request" "$AGM_DIR/calls.log"
check_no "讀取錯誤不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 13. Overlapping invocations stop before making any API mutations.
setup
mkdir "$AGM_DIR/daemon-update.lock"
printf '%s %s\n' "$$" "$(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"   # $$ = 這支測試，command 含 daemon-update-kick
bash "$SCRIPT"
check "重疊執行停止" "已有執行者" "$AGM_DIR/daemon-update.log"
check_no "重疊執行不申請" "approval request" "$AGM_DIR/calls.log"
check_no "重疊執行不喊人" "ops-alert" "$AGM_DIR/calls.log"
teardown

# 13b. 殘留鎖（執行者已經不在：強制關機、SIGKILL）：回收後照常做這一輪，不再永久停住。
setup
mkdir "$AGM_DIR/daemon-update.lock"
printf '%s %s\n' 999999 "$(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"   # 不存在的 pid
touch -t 202601010000 "$AGM_DIR/daemon-update.lock"                           # 而且已經放很久
bash "$SCRIPT"
check "殘留鎖被回收" "清掉殘留鎖" "$AGM_DIR/daemon-update.log"
check "回收後照常派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

# 13c. 執行者還活著但卡了太久：不搶它的鎖，改喊人。
setup
mkdir "$AGM_DIR/daemon-update.lock"
printf '%s %s\n' "$$" "$(date +%s)" > "$AGM_DIR/daemon-update.lock/owner"
touch -t 202601010000 "$AGM_DIR/daemon-update.lock"
AGM_LOCK_HUNG_SECS=60 bash "$SCRIPT"
check "卡住的執行者會喊人" "ops-alert --source test-owner --reason runner_hung" "$AGM_DIR/calls.log"
check_no "卡住時不搶鎖" "approval request" "$AGM_DIR/calls.log"
teardown

# 13d. 核准狀態檔壞掉：一樣喊人，不自己繞過。
setup
printf '%s' 'not json' > "$AGM_DIR/daemon-update.approval.json"
bash "$SCRIPT"
check "狀態檔壞掉會喊人" "ops-alert --source test-owner --reason state_corrupt" "$AGM_DIR/calls.log"
check_no "狀態檔壞掉不申請" "approval request" "$AGM_DIR/calls.log"
teardown

# 14. The daemon filtered AGM and builder activity; consume its result and forward both IDs.
setup
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "只有 AGM 與建置者忙碌仍可派工" "已派工" "$AGM_DIR/daemon-update.log"
check "acquire 帶同一份兩顆排除名單" "--exclude-bot bot-build --exclude-bot bot-manager" "$AGM_DIR/calls.log"
check_no "不得拿相容 bot_id 當管理員" "--exclude-bot legacy-not-manager" "$AGM_DIR/calls.log"
teardown

# 15. Exclude identities, not names: an unrelated bot also named AGM is still protected.
setup
export STUB_SAFETY='{"safe":false,"working":[{"bot_id":"user-bot","name":"AGM"}],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check_no "其他同名 bot 忙碌時不取租約" "lease acquire" "$AGM_DIR/calls.log"
check "仍回報有人在跑" "還有人在跑（AGM）" "$AGM_DIR/daemon-update.log"
teardown

# 16. The in-flight check protects user turns even when working is empty.
setup
export STUB_SAFETY='{"safe":false,"working":[],"in_flight":[{"bot_id":"user-bot","turn_id":"t3"}],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check_no "其他 bot 的 in-flight 不可被排除" "lease acquire" "$AGM_DIR/calls.log"
check "in-flight 理由可見" "還有人在跑（in_flight）" "$AGM_DIR/daemon-update.log"
teardown

# 17. Do not hide failed reads, including reads of the excluded manager itself.
setup
export STUB_SAFETY='{"safe":false,"working":[],"in_flight":[],"unreadable":[{"bot_id":"bot-manager"}],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check_no "讀取失敗不取租約" "lease acquire" "$AGM_DIR/calls.log"
check "unreadable 理由可見" "還有人在跑（unreadable）" "$AGM_DIR/daemon-update.log"
teardown

# 18. A missing/corrupt manager identity must not turn into a guessed exclusion.
for runtime in '{}' '{"manager_bot_id":null}' '{"manager_bot_id":" "}' 'broken'; do
  setup
  printf '%s' "$runtime" > "$AGM_DIR/runtime.json"
  bash "$SCRIPT"
  check "無效 runtime 不會猜 AGM 身分" "無法從 runtime.json 取得 manager_bot_id" "$AGM_DIR/daemon-update.log"
  check_no "無效 runtime 不取租約" "lease acquire" "$AGM_DIR/calls.log"
  teardown
done

# 19. Filtering must not turn malformed safety responses into an empty safe window.
for safety in '{}' '{"safe":false}' '{"safe":false,"working":[],"in_flight":[],"unreadable":[]}' '{"safe":true,"working":[],"in_flight":{},"unreadable":[]}'; do
  setup
  export STUB_SAFETY="$safety"
  bash "$SCRIPT"
  check_no "錯誤或無法解釋的 safety 不取租約" "lease acquire" "$AGM_DIR/calls.log"
  teardown
done

# 20. Upgraded daemon applies exclusions server-side and echoes the applied IDs.
setup
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "safety 傳兩顆排除 ID" "lease safety --approval ap-1 --exclude-bot bot-build --exclude-bot bot-manager" "$AGM_DIR/calls.log"
check "新版 daemon 確認安全後派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

# 21. Never override a new daemon's refusal with client-side filtering.
setup
export STUB_SAFETY='{"safe":false,"working":[{"bot_id":"bot-manager","name":"AGM"}],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check_no "新版 daemon 判不安全時不靠過濾繞過" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 22. The server must confirm the exact exclusions before its preflight can be trusted.
setup
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","unrelated-bot"]}'
bash "$SCRIPT"
check_no "排除名單不一致時停止" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 23. An old daemon that did not apply exclusions cannot authorize this script.
setup
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[]}'
bash "$SCRIPT"
check_no "舊端點沒有回排除名單時不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 24. AGM 雙角色：協調者也排除；以 runtime.json 的角色派工，巡檢目錄就是 patrol 驗收。
setup
printf '%s' '{"manager_bot_id":"bot-manager","role":"patrol","self_bot_id":"bot-manager"}' > "$AGM_DIR/runtime.json"
export STUB_RESPONDER='{"configured":true,"bot_id":"bot-resp"}'
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager","bot-resp"]}'
bash "$SCRIPT"
check "safety 也排除協調者" "lease safety --approval ap-1 --exclude-bot bot-build --exclude-bot bot-manager --exclude-bot bot-resp" "$AGM_DIR/calls.log"
check "派工帶上巡檢角色" "--review-by patrol" "$AGM_DIR/calls.log"
check "雙角色下照常派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown
unset STUB_RESPONDER

# 25. 舊部署（runtime 沒有 role、沒有協調者）：不帶 --review-by，舊 CLI 才不會拒絕整筆派工。
setup
bash "$SCRIPT"
check_no "舊部署不帶 --review-by" "--review-by" "$AGM_DIR/calls.log"
check "舊部署照常派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

echo "----"
# 7b. 預設門檻是 3（使用者 2026-09-16 從 5 降下來）：兩筆還要等整點，第三筆一到就不等。
setup
export AGM_TEST_MINUTE="37"
two='{"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a2","purpose":"rebuild","status":"pending","requester":"bot-2","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"}'
export STUB_APPROVAL_LIST="{\"approvals\":[$two]}"
bash "$SCRIPT"
check "兩筆還不夠" "非整點且重建申請只有 2/3" "$AGM_DIR/daemon-update.log"
check_no "兩筆不會去拿窗口" "lease acquire" "$AGM_DIR/calls.log"
teardown

setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST="{\"approvals\":[{\"id\":\"ap-1\",\"status\":\"approved\"},$two,
  {\"id\":\"a3\",\"purpose\":\"rebuild\",\"status\":\"pending\",\"requester\":\"bot-3\",\"target_commit\":\"c2\",\"created_at\":\"2099-01-01T00:00:00.000Z\"}]}"
bash "$SCRIPT"
check "第三筆就不等整點" "重建申請 3/3，不等整點" "$AGM_DIR/daemon-update.log"
check "照樣派工" "已派工 agm-daemon-update-" "$AGM_DIR/daemon-update.log"
teardown

echo "----"
# 8. 非整點又沒有累積夠的重建申請：整輪跳過，連 fetch 之後的判斷都不做（使用者 2026-09-14）。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[]}'
bash "$SCRIPT"
check "非整點且請求不足就不檢查" "非整點且重建申請只有 0/3" "$AGM_DIR/daemon-update.log"
check_no "不會申請核准" "approval request" "$AGM_DIR/calls.log"
check_no "不會去拿窗口" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 9. 非整點但申請集滿門檻：照樣走完整流程。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"ap-1","status":"approved"},
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a2","purpose":"rebuild","status":"approved","requester":"bot-2","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a3","purpose":"rebuild","status":"pending","requester":"bot-3","target_commit":"c2","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a4","purpose":"rebuild","status":"pending","requester":"bot-4","target_commit":"c2","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a5","purpose":"rebuild","status":"pending","requester":"bot-4","target_commit":"c2","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a6","purpose":"rebuild","status":"pending","requester":"bot-5","target_commit":"c3","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a7","purpose":"restart","status":"pending","requester":"bot-9","target_commit":"c9","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a8","purpose":"rebuild","status":"denied","requester":"bot-8","target_commit":"c8","created_at":"2099-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "集滿門檻就不等整點" "重建申請 5/3，不等整點" "$AGM_DIR/daemon-update.log"
check "照樣派工" "已派工 agm-daemon-update-" "$AGM_DIR/daemon-update.log"
teardown

# 10. 門檻可調：AGM_REBUILD_THRESHOLD=2 時兩筆就夠。
setup
export AGM_TEST_MINUTE="37" AGM_REBUILD_THRESHOLD=2
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"ap-1","status":"approved"},
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"},
  {"id":"a2","purpose":"rebuild","status":"pending","requester":"bot-2","target_commit":"c1","created_at":"2099-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "門檻可以調小" "重建申請 2/2，不等整點" "$AGM_DIR/daemon-update.log"
teardown

# 11. 上次上線之後才算：built 之前建立的申請不列入。
setup
export AGM_TEST_MINUTE="37"
printf '%s' "$(cd "$AGM_REPO" && /usr/bin/git rev-parse --short HEAD)x" > "$AGM_DIR/daemon-update.built"
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"a2","purpose":"rebuild","status":"pending","requester":"bot-2","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"a3","purpose":"rebuild","status":"pending","requester":"bot-3","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"a4","purpose":"rebuild","status":"pending","requester":"bot-4","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"a5","purpose":"rebuild","status":"pending","requester":"bot-5","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "上次上線之前的申請不算" "非整點且重建申請只有 0/3" "$AGM_DIR/daemon-update.log"
teardown

# 12. 申請沒湊滿，但最早一筆已經等超過 30 分鐘：不等整點（使用者 2026-09-15）。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"ap-1","status":"approved"},
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "等超過上限就不等整點" "最早一筆重建申請已等" "$AGM_DIR/daemon-update.log"
check "照樣派工" "已派工 agm-daemon-update-" "$AGM_DIR/daemon-update.log"
teardown

# 13. 申請還沒等滿 30 分鐘（剛建立）而且沒湊滿：照舊等整點。
setup
export AGM_TEST_MINUTE="37"
now_iso=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
export STUB_APPROVAL_LIST="{\"approvals\":[{\"id\":\"a1\",\"purpose\":\"rebuild\",\"status\":\"pending\",\"requester\":\"bot-1\",\"target_commit\":\"c1\",\"created_at\":\"$now_iso\"}]}"
bash "$SCRIPT"
check "剛建立的申請不觸發" "非整點且重建申請只有 1/3（最早一筆等了 0 分鐘）" "$AGM_DIR/daemon-update.log"
check_no "不會去拿窗口" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 14. 等待上限可調：AGM_REBUILD_MAX_WAIT_MIN 設很大時，舊申請也不觸發。
setup
export AGM_TEST_MINUTE="37" AGM_REBUILD_MAX_WAIT_MIN=99999999
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "等待上限可以調大" "非整點且重建申請只有 1/3" "$AGM_DIR/daemon-update.log"
teardown

echo "----"
# 15. 縮小封鎖面（SPEC §18.10）：daemon 判 safe，即使還有 bot 在 working 也照換，log 與派工正文寫明是升級後才換的。
setup
export STUB_SAFETY='{"safe":true,"escalated":true,"waited_secs":2700,"working":[{"bot_id":"b9","name":"wits-pro"}],"in_flight":[{"bot_id":"b9","turn_id":"t9"}],"unreadable":[],"delivering":[],"held_leases":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "升級後照樣拿窗口" "lease acquire" "$AGM_DIR/calls.log"
check "log 寫明是升級後才換" "安全窗口是升級後才成立的：核准後已等 45 分鐘" "$AGM_DIR/daemon-update.log"
check "派工正文也寫明" "這次是升級後才換：核准後已等 45 分鐘" "$AGM_DIR/assign-body.txt"
teardown

# 16. 升級歸升級，daemon 說不安全就是不安全——而且理由要指出真正擋住的那一項。
setup
export STUB_SAFETY='{"safe":false,"escalated":true,"waited_secs":2700,"working":[{"bot_id":"b9","name":"wits-pro"}],"in_flight":[],"unreadable":[],"delivering":[{"bot_id":"b1","name":"AM-1-XH","turn_id":"t1"}],"held_leases":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "送達中就是不換" "還有人在跑（送達中:AM-1-XH）" "$AGM_DIR/daemon-update.log"
check_no "送達中不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

setup
export STUB_SAFETY='{"safe":false,"escalated":true,"waited_secs":2700,"working":[],"in_flight":[],"unreadable":[],"delivering":[],"held_leases":[{"resource":"restart","owner":"someone"}],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "別人握著租約就是不換" "還有人在跑（租約:restart）" "$AGM_DIR/daemon-update.log"
check_no "有租約不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 自己握的租約不擋自己（SPEC §18.10）：safety 要用跟 acquire 同一個 owner 問，daemon 判安全就照常派。
setup
export STUB_SAFETY='{"safe":true,"escalated":true,"waited_secs":2700,"working":[{"bot_id":"b9","name":"wits-pro"}],"in_flight":[],"unreadable":[],"delivering":[],"held_leases":[{"resource":"rebuild","owner":"test-owner","own":true}],"owner":"test-owner","excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "safety 以自己的 owner 問" "--exclude-bot bot-manager --owner test-owner" "$AGM_DIR/calls.log"
check "自己的租約不擋，照常取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 就算 daemon 判不安全，理由裡也不把自己的那把列成「租約」——看 log 的人才不會以為是別人卡住。
setup
export STUB_SAFETY='{"safe":false,"escalated":true,"waited_secs":2700,"working":[],"in_flight":[],"unreadable":[],"delivering":[{"bot_id":"b1","name":"AM-1-XH","turn_id":"t1"}],"held_leases":[{"resource":"rebuild","owner":"test-owner","own":true}],"owner":"test-owner","excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "擋人的理由是送達中" "還有人在跑（送達中:AM-1-XH）" "$AGM_DIR/daemon-update.log"
check_no "自己的租約不出現在理由裡" "租約:rebuild" "$AGM_DIR/daemon-update.log"
teardown

# 17. 沒升級時照舊：daemon 判不安全，理由還是那顆在跑的 bot。
setup
export STUB_SAFETY='{"safe":false,"escalated":false,"waited_secs":null,"working":[{"bot_id":"b9","name":"wits-pro"}],"in_flight":[],"unreadable":[],"delivering":[],"held_leases":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "沒升級照舊擋" "還有人在跑（wits-pro）" "$AGM_DIR/daemon-update.log"
check_no "沒升級不取租約" "lease acquire" "$AGM_DIR/calls.log"
check_no "沒升級不寫升級 log" "升級後才成立" "$AGM_DIR/daemon-update.log"
teardown

# 18. 舊 daemon（沒有 escalated／delivering 欄位）：行為完全照舊，也不會誤寫升級紀錄。
setup
export STUB_SAFETY='{"safe":true,"working":[],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "舊 daemon 照樣派工" "已派工" "$AGM_DIR/daemon-update.log"
check_no "舊 daemon 不寫升級紀錄" "升級後" "$AGM_DIR/assign-body.txt"
teardown

echo "----"
# 26. 租約憑證（SPEC §18.10）：token 不進派工正文（會出現在 assignments API、child 的對話紀錄與這份 log），
# 寫進只有本人讀得到的檔案，正文只給路徑（review2 sup #5）。
setup
export AGM_TEST_MINUTE="0"
export STUB_ACQUIRE='{"lease":{"fence":9,"resource":"rebuild"},"lease_token":"tok-abc123"}'
bash "$SCRIPT"
check "派工正文用檔案帶 lease-token" "--lease-token \"\$(cat $AGM_DIR/daemon-update.lease-token)\"" "$AGM_DIR/assign-body.txt"
check_no "派工正文沒有 token 本身" "tok-abc123" "$AGM_DIR/assign-body.txt"
check_no "log 裡也沒有" "tok-abc123" "$AGM_DIR/daemon-update.log"
check "token 檔的內容" "^tok-abc123$" "$AGM_DIR/daemon-update.lease-token"
if [ "$(stat -f %Lp "$AGM_DIR/daemon-update.lease-token" 2>/dev/null || stat -c %a "$AGM_DIR/daemon-update.lease-token")" = "600" ]; then
  echo "ok   - token 檔只有本人讀得到"; PASS=$((PASS + 1))
else
  echo "FAIL - token 檔權限不是 600"; FAIL=$((FAIL + 1))
fi
teardown

# 派工失敗要交還窗口，一樣要出示憑證。
setup
export AGM_TEST_MINUTE="0"
export STUB_ACQUIRE='{"lease":{"fence":9,"resource":"rebuild"},"lease_token":"tok-abc123"}'
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
check "交還窗口帶 lease-token" "lease release rebuild --owner .* --fence 9 --lease-token tok-abc123" "$AGM_DIR/calls.log"
teardown

# 舊 daemon 沒有 lease_token：照舊不帶，不要送出空的旗標。
setup
export AGM_TEST_MINUTE="0"
export STUB_ACQUIRE='{"lease":{"fence":9,"resource":"rebuild"}}'
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
check_no "舊 daemon 不帶空旗標" "--lease-token " "$AGM_DIR/calls.log"
check "舊 daemon 照樣交還窗口" "lease release rebuild --owner" "$AGM_DIR/calls.log"
teardown

echo "----"
# 27. 申請計數不算自己的、也不算過期的（review2 sup 新發現 2）：自己先申請、30 分鐘後再被自己觸發，
# 每 5 分鐘一輪、main 一動就對協調者再開一筆。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"m1","purpose":"rebuild","status":"denied","requester":"test-owner","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"x1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z","expires_at":"2000-01-01T01:00:00Z"},
  {"id":"x2","purpose":"rebuild","status":"approved","requester":"bot-2","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z","expires_at":"2000-01-01T01:00:00Z"},
  {"id":"x3","purpose":"rebuild","status":"pending","requester":"bot-3","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z","expires_at":"2000-01-01T01:00:00Z"}
]}'
bash "$SCRIPT"
check "過期的申請不算、也不觸發等太久" "非整點且重建申請只有 0/3（最早一筆等了 0 分鐘）" "$AGM_DIR/daemon-update.log"
teardown

setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"ap-1","purpose":"rebuild","status":"approved","requester":"test-owner","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"m2","purpose":"rebuild","status":"pending","requester":"test-owner","target_commit":"c2","created_at":"2000-01-01T00:00:00.000Z"},
  {"id":"m3","purpose":"rebuild","status":"pending","requester":"test-owner","target_commit":"c3","created_at":"2000-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check_no "自己的申請不算進門檻" "重建申請 3/3" "$AGM_DIR/daemon-update.log"
check_no "自己的申請不觸發等太久" "最早一筆重建申請已等" "$AGM_DIR/daemon-update.log"
check "自己還有在等的核准就照常往下跑" "自己的重建核准還在等（裁示或安全窗口），不等整點（別人的申請 0/3）" "$AGM_DIR/daemon-update.log"
teardown

# 28. main 只動到不進 binary 的檔：沿用原本的核准（commit 對原本那顆），不為 docs-only 再叫醒協調者。
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
H1=$(cd "$AGM_REPO" && /usr/bin/git rev-parse HEAD)
( cd "$AGM_REPO" && echo more >> docs/goals/notes.md && /usr/bin/git add -A && /usr/bin/git commit -qm docs \
    && /usr/bin/git update-ref refs/remotes/origin/main HEAD ) >/dev/null 2>&1
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"approved"}]}'
bash "$SCRIPT"
check_no "docs-only 不重新申請" "approval request" "$AGM_DIR/calls.log"
check "沿用原核准、commit 對原本那顆" "lease acquire rebuild --approval ap-1 --commit $H1" "$AGM_DIR/calls.log"
check "log 寫明沿用" "只動到不進 binary 的檔，沿用核准 ap-1" "$AGM_DIR/daemon-update.log"
check "照常派工" "已派工" "$AGM_DIR/daemon-update.log"
teardown

# 29. main 動到要建的東西：新申請取代舊的（--supersedes），等待時間由 daemon 接過去。
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"approved"}]}'
export STUB_SAFETY='{"safe":false,"working":[{"bot_id":"b9","name":"busy"}],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
( cd "$AGM_REPO" && echo y >> daemon/main.rs && /usr/bin/git add -A && /usr/bin/git commit -qm code \
    && /usr/bin/git update-ref refs/remotes/origin/main HEAD ) >/dev/null 2>&1
H2=$(cd "$AGM_REPO" && /usr/bin/git rev-parse HEAD)
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL='{"id":"ap-2","status":"pending"}'
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"superseded"},{"id":"ap-2","status":"pending"}]}'
bash "$SCRIPT"
check "換了要建的東西就取代舊申請" "approval request --requester test-owner --purpose rebuild .* --commit $H2 --expires-in 5400 --supersedes ap-1" "$AGM_DIR/calls.log"
check "log 寫明取代誰" "已申請核准 ap-2（commit ${H2}，取代 ap-1）" "$AGM_DIR/daemon-update.log"
teardown

# 舊的 bin/agm 不認得 --supersedes：照舊開一筆新的，不要讓整筆申請被 argparse 拒絕。
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
( cd "$AGM_REPO" && echo y >> daemon/main.rs && /usr/bin/git add -A && /usr/bin/git commit -qm code \
    && /usr/bin/git update-ref refs/remotes/origin/main HEAD ) >/dev/null 2>&1
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL_HELP="usage: agm approval [--requester R]"
export STUB_APPROVAL='{"id":"ap-2","status":"pending"}'
bash "$SCRIPT"
check "舊 CLI 照樣申請" "approval request" "$AGM_DIR/calls.log"
check_no "舊 CLI 不帶 --supersedes" "--supersedes" "$AGM_DIR/calls.log"
unset STUB_APPROVAL_HELP
teardown

# 30. 舊的核准已經被用掉（上一個窗口過期沒交還，daemon 當場消耗）：同一輪就重新申請，不白等到下一個整點。
setup
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"pending"}]}'
bash "$SCRIPT"
: > "$AGM_DIR/calls.log"
export STUB_APPROVAL='{"id":"ap-2","status":"pending"}'
export STUB_APPROVAL_LIST='{"approvals":[{"id":"ap-1","status":"consumed"},{"id":"ap-2","status":"approved"}]}'
bash "$SCRIPT"
check "用掉的核准當場重新申請" "approval request" "$AGM_DIR/calls.log"
check "用新的那筆拿窗口" "lease acquire rebuild --approval ap-2" "$AGM_DIR/calls.log"
check "log 寫明為什麼重申請" "核准 ap-1 已經不能用（consumed），重新申請" "$AGM_DIR/daemon-update.log"
teardown

# 31. 讀不到重建申請數不能當 0（當 0＝沒人申請＝非整點整輪跳過，換版流程靜默停擺）：這輪照常往下檢查、記成失敗、
#     連續幾輪就喊人；不是「非整點且重建申請只有 0/3，這輪不檢查」。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='not json at all'
bash "$SCRIPT"
check "讀不到申請數要講清楚" "讀不到重建申請數" "$AGM_DIR/daemon-update.log"
check_no "不能當成 0 個申請而跳過這輪" "非整點且重建申請只有 0/3" "$AGM_DIR/daemon-update.log"
check "照常往下檢查（過了閘，走到申請核准）" "已申請核准" "$AGM_DIR/daemon-update.log"
teardown

setup
export AGM_TEST_MINUTE="37" AGM_FAIL_ALERT_AFTER=1
export STUB_APPROVAL_LIST='not json at all'
bash "$SCRIPT"
check "讀不到申請數算失敗、連續幾輪會喊人" "ops-alert.*check_failing" "$AGM_DIR/calls.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
