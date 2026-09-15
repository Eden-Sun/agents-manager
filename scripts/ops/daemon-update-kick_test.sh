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
  assign:*)          [ -n "$STUB_ASSIGN_FAIL" ] && exit 1; printf '%s' '{"id":"a-1"}' ;;
  responder:show)    [ -n "${STUB_RESPONDER:-}" ] && printf '%s' "$STUB_RESPONDER" || printf '%s' '{}' ;;
  *)                 printf '%s' '{}' ;;
esac
STUB
  chmod +x "$AGM_DIR/bin/agm"
  : > "$AGM_DIR/calls.log"
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

teardown() { rm -rf "$ROOT"; unset AGM_BUILD_BOT AGM_TEST_MINUTE AGM_REBUILD_THRESHOLD AGM_REBUILD_MAX_WAIT_MIN; }

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

# 3. 有人在跑：連核准都不申請。
setup
export STUB_SAFETY='{"safe":false,"working":[{"bot_id":"bot-busy","name":"bot-busy"}],"in_flight":[],"unreadable":[],"excluded_bot_ids":["bot-build","bot-manager"]}'
bash "$SCRIPT"
check "有人在跑就不派" "還有人在跑（bot-busy）" "$AGM_DIR/daemon-update.log"
check_no "不會申請核准" "approval request" "$AGM_DIR/calls.log"
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
check "核准讀取失敗會停住" "無法確認核准" "$AGM_DIR/daemon-update.log"
check_no "不拿新申請繞過讀取錯誤" "approval request" "$AGM_DIR/calls.log"
check_no "讀取錯誤不取租約" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 13. Overlapping invocations stop before making any API mutations.
setup
mkdir "$AGM_DIR/daemon-update.lock"
bash "$SCRIPT"
check "重疊執行停止" "已有執行者或殘留鎖" "$AGM_DIR/daemon-update.log"
check_no "重疊執行不申請" "approval request" "$AGM_DIR/calls.log"
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
check "safety 傳兩顆排除 ID" "lease safety --exclude-bot bot-build --exclude-bot bot-manager" "$AGM_DIR/calls.log"
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
check "safety 也排除協調者" "lease safety --exclude-bot bot-build --exclude-bot bot-manager --exclude-bot bot-resp" "$AGM_DIR/calls.log"
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
# 8. 非整點又沒有累積夠的重建申請：整輪跳過，連 fetch 之後的判斷都不做（使用者 2026-09-14）。
setup
export AGM_TEST_MINUTE="37"
export STUB_APPROVAL_LIST='{"approvals":[]}'
bash "$SCRIPT"
check "非整點且請求不足就不檢查" "非整點且重建申請只有 0/5" "$AGM_DIR/daemon-update.log"
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
check "集滿門檻就不等整點" "重建申請 5/5，不等整點" "$AGM_DIR/daemon-update.log"
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
check "上次上線之前的申請不算" "非整點且重建申請只有 0/5" "$AGM_DIR/daemon-update.log"
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
check "剛建立的申請不觸發" "非整點且重建申請只有 1/5（最早一筆等了 0 分鐘）" "$AGM_DIR/daemon-update.log"
check_no "不會去拿窗口" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 14. 等待上限可調：AGM_REBUILD_MAX_WAIT_MIN 設很大時，舊申請也不觸發。
setup
export AGM_TEST_MINUTE="37" AGM_REBUILD_MAX_WAIT_MIN=99999999
export STUB_APPROVAL_LIST='{"approvals":[
  {"id":"a1","purpose":"rebuild","status":"pending","requester":"bot-1","target_commit":"c1","created_at":"2000-01-01T00:00:00.000Z"}
]}'
bash "$SCRIPT"
check "等待上限可以調大" "非整點且重建申請只有 1/5" "$AGM_DIR/daemon-update.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
