#!/bin/bash
# main 的 GitHub CI 盯哨（issue #211）：一紅就開 issue＋派工，同一段紅不重複派，綠回來在 issue 留言。
# 2026-09-16 起 main 的 CI 連紅好幾天沒人發現——規則只要求跑本機 check.sh，沒人看 GitHub 的結果。
# launchd `com.agm.ci-watch` 每 10 分鐘跑一次；唯讀（只開 issue、留言、派工），**不改程式、不重啟、不關 issue**。
#
# 形狀比照 release-triage-kick.sh：pid＋時間的鎖與殘留回收、開頭自補 PATH、缺依賴走 ops-alert、無事安靜。
# 「一段紅」＝從綠翻紅到下一次綠之間的連續失敗；狀態檔 ci-watch.state.json 記這一段：
#   {"first_red_sha","first_red_run","issue","assigned","failures":[…]}
# 只看已完成的 run；cancelled／skipped 等不算紅也不算綠（不改變狀態）。gh 失敗／rate limit 這輪什麼都不做。
#
#   AGM_DIR、AGM_REPO、AGM_CI_BOT、AGM_LOCK_STALE_SECS、AGM_LOCK_HUNG_SECS、AGM_EXTRA_PATH 可覆寫（測試用）。
set -u
PATH="${AGM_EXTRA_PATH-/opt/homebrew/bin:/usr/local/bin}:$PATH"; export PATH   # AGM_EXTRA_PATH 只給測試蓋掉

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
AGM="$DIR/bin/agm"
LOG="$DIR/ci-watch.log"
TASK="$DIR/ci-watch-task.md"
STATE="$DIR/ci-watch.state.json"
OWNER="${AM_AGENT_NAME:-ci-watch-kick}"
LABEL="ci-red"

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || exit 0

alert() { # alert <reason> <detail>：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}

# launchd 的預設 PATH 不含 Homebrew；缺依賴不能只靜默 exit 0，否則「已排程」但永遠不盯（#66 留言）。
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），main CI 盯哨停住"; exit 0; }
command -v gh >/dev/null 2>&1 || { alert missing_dependency "找不到 gh（PATH=${PATH}），main CI 盯哨停住"; exit 0; }
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -d "$REPO" ] || { log "找不到 ${REPO}，跳過"; exit 0; }

# 鎖：格式抄 release-triage-kick.sh（鎖裡寫 pid 與時間；執行者不在就回收，活著但卡太久才喊人）。
LOCK="$DIR/ci-watch.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}
lock_age() { # lock_age → 鎖建立到現在幾秒（讀不到就當 0）
  _born=$(python3 -c '
import os,sys
try:
    print(int(os.path.getmtime(sys.argv[1])))
except OSError:
    print(0)
' "$LOCK" 2>/dev/null) || _born=0
  case "$_born" in ''|*[!0-9]*) _born=0 ;; esac
  [ "$_born" = 0 ] && { echo 0; return; }
  echo $(( $(date +%s) - _born ))
}
WORK=""
cleanup() { rm -rf "$LOCK" 2>/dev/null || true; [ -n "$WORK" ] && rm -rf "$WORK" 2>/dev/null; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'ci-watch-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，main CI 盯哨停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "盯哨已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
    fi
    exit 0
  fi
  if [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），main CI 盯哨停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

WORK=$(mktemp -d -t agm-ci-watch) || { log "建不了暫存目錄，跳過"; exit 0; }
cd "$REPO" 2>/dev/null || { log "進不了 ${REPO}，跳過"; exit 0; }   # gh 靠這裡推得出是哪個 repo

# 1. 抓最近的 run。失敗（網路、rate limit、沒登入）＝這輪什麼都不做，不改狀態、不誤報紅或綠。
RUNS="$WORK/runs.json"
if ! gh run list --branch main --workflow CI -L 20 --json databaseId,conclusion,status,headSha,createdAt,url > "$RUNS" 2> "$WORK/err"; then
  log "gh run list 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
  exit 0
fi

# 2. 分析：只看已完成的；success 算綠、failure／timed_out／startup_failure 算紅，其他（cancelled、skipped…）當沒看到。
#    輸出 key=value 行：verdict（green／red／none）、run、sha、url，以及（紅時）這一段紅的起點與上一個綠。
ANALYSIS=$(python3 -c '
import json, sys
RED = {"failure", "timed_out", "startup_failure"}
try:
    runs = json.load(open(sys.argv[1]))
    assert isinstance(runs, list)
except Exception:
    sys.exit(3)
done = [r for r in runs if r.get("status") == "completed" and (r.get("conclusion") in RED or r.get("conclusion") == "success")]
done.sort(key=lambda r: r.get("createdAt") or "", reverse=True)
if not done:
    print("verdict=none"); sys.exit(0)
latest = done[0]
def kv(prefix, r):
    for k, v in (("run", r.get("databaseId")), ("sha", r.get("headSha")), ("url", r.get("url") or "")):
        print("%s%s=%s" % (prefix, k, v))
if latest["conclusion"] == "success":
    print("verdict=green"); kv("", latest); sys.exit(0)
i = 0
while i + 1 < len(done) and done[i + 1]["conclusion"] in RED:
    i += 1
first = done[i]
print("verdict=red"); kv("", latest); kv("first_", first)
prev = done[i + 1] if i + 1 < len(done) else {}
print("prev_green_sha=%s" % (prev.get("headSha") or ""))
print("red_runs=%d" % (i + 1))
' "$RUNS") || { log "gh run list 的輸出看不懂，這輪不動"; exit 0; }
field() { printf '%s\n' "$ANALYSIS" | sed -n "s/^$1=//p" | head -1; }
VERDICT=$(field verdict)
RUN_ID=$(field run); RUN_SHA=$(field sha); RUN_URL=$(field url)

# 狀態檔讀寫（壞掉的檔案當作沒有）。
state_get() { python3 -c '
import json, sys
try:
    v = json.load(open(sys.argv[1])).get(sys.argv[2])
except Exception:
    v = None
print("" if v is None else (json.dumps(v, ensure_ascii=False) if isinstance(v, (list, dict)) else str(v).lower() if isinstance(v, bool) else v))
' "$STATE" "$1" 2>/dev/null; }
state_write() { # state_write <first_red_sha> <first_red_run> <issue> <assigned true|false> <failures-json>
  python3 -c '
import json, sys
sha, run, issue, assigned, failures = sys.argv[2:7]
json.dump({"first_red_sha": sha, "first_red_run": run, "issue": int(issue) if issue.isdigit() else None,
           "assigned": assigned == "true", "failures": json.loads(failures)}, open(sys.argv[1] + ".tmp", "w"), ensure_ascii=False)
' "$STATE" "$1" "$2" "$3" "$4" "$5" && mv "$STATE.tmp" "$STATE"
}

S_SHA=$(state_get first_red_sha)

case "$VERDICT" in
  none) exit 0 ;;
  green)
    [ -n "$S_SHA" ] || exit 0    # 本來就綠：安靜
    S_ISSUE=$(state_get issue)
    if [ -n "$S_ISSUE" ]; then
      if ! gh issue comment "$S_ISSUE" --body "${RUN_SHA} 起恢復綠，run ${RUN_ID}${RUN_URL:+（${RUN_URL}）}。這張 issue 不會自動關，請修的人確認後關掉。" >> "$LOG" 2>&1; then
        log "issue #${S_ISSUE} 留言失敗，狀態先不清，下一輪再試"; exit 0
      fi
    fi
    rm -f "$STATE"
    log "恢復綠：${RUN_SHA:0:8}（run ${RUN_ID}）${S_ISSUE:+，已在 issue #${S_ISSUE} 留言}"
    exit 0 ;;
  red) ;;
  *) log "看不懂的判定 '${VERDICT}'，這輪不動"; exit 0 ;;
esac

FIRST_SHA=$(field first_sha); FIRST_RUN=$(field first_run); FIRST_URL=$(field first_url); PREV_GREEN=$(field prev_green_sha)

# 3. 失敗清單：從最新一個紅 run 的 --log-failed 抽。抓不到就這輪不動（沒有清單就開不出有用的 issue）。
if ! gh run view "$RUN_ID" --log-failed > "$WORK/failed.log" 2> "$WORK/err"; then
  log "gh run view ${RUN_ID} --log-failed 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
  exit 0
fi
FAILURES=$(python3 -c '
import json, re, sys
seen, out = set(), []
def add(n):
    n = n.strip()
    if n and n not in seen:
        seen.add(n); out.append(n)
in_block = False
for raw in open(sys.argv[1], errors="replace"):
    line = raw.rstrip("\n").split("\t")[-1]
    line = re.sub(r"^\d{4}-\d\d-\d\dT[\d:.]+Z ", "", line)
    m = re.match(r"^test (\S+) \.\.\. FAILED$", line)
    if m:
        add(m.group(1)); continue
    if line.strip() == "failures:":
        in_block = True; continue
    if in_block:
        m = re.match(r"^    ([A-Za-z_][\w:]*)$", line)
        if m:
            add(m.group(1)); continue
        if not line.strip() or line.startswith("test result"):
            in_block = False
        continue
    m = re.match(r"^(?:ERROR|FAIL): (.+)$", line)
    if m:
        add(m.group(1))
print(json.dumps(out, ensure_ascii=False))
' "$WORK/failed.log") || FAILURES="[]"
NFAIL=$(python3 -c 'import json,sys; print(len(json.loads(sys.argv[1])))' "$FAILURES")
fail_lines() { python3 -c '
import json, sys
names = json.loads(sys.argv[1])
print("\n".join("- `%s`" % n for n in names) if names else "（log 裡抽不出測試名，請看連結）")
' "$1"; }

# 派給誰：AGM_CI_BOT ＞ runtime.json 的 ci_bot_id ＞ responder_bot_id。**不能是巡檢自己**（daemon 擋總管對自己下交辦）。
find_bot() {
  BOT="${AGM_CI_BOT:-}"
  if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
    BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("ci_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
  fi
}
dispatch() { # dispatch <issue-number> → 0 派成功
  find_bot
  [ -n "$BOT" ] || { log "找不到要派給誰（AGM_CI_BOT／runtime.json 的 ci_bot_id 或 responder_bot_id），issue #${1} 先開著，下一輪再派"; return 1; }
  _body="$WORK/assign.md"
  {
    cat "$TASK"
    printf '\n\n---\n本次：issue #%s（標籤 `%s`），第一個紅的 sha `%s`，run %s。\n\n失敗的測試：\n%s\n' \
      "$1" "$LABEL" "$FIRST_SHA" "$FIRST_RUN" "$(fail_lines "$FAILURES")"
  } > "$_body"
  # 旗標叫 `--request-id`（不是 --client-request-id）：拼錯 argparse 會 exit 2，stub 吃掉未知旗標的測試看不出來。
  "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$_body" \
      --request-id "ci-red-${FIRST_SHA}" >> "$LOG" 2>&1
}

# 4a. 還在同一段紅裡：不重開、不重派；清單多了新的測試才在同一張 issue 留言一次。
if [ -n "$S_SHA" ]; then
  S_ISSUE=$(state_get issue); S_ASSIGNED=$(state_get assigned); S_FAILS=$(state_get failures); [ -n "$S_FAILS" ] || S_FAILS="[]"
  FIRST_SHA="$S_SHA"; FIRST_RUN=$(state_get first_red_run)
  if [ "$S_ASSIGNED" != "true" ] && [ -n "$S_ISSUE" ]; then       # 上一輪開了 issue 但派工沒成功：補派（同 request-id，daemon 去重）
    if dispatch "$S_ISSUE"; then S_ASSIGNED=true; log "補派成功：issue #${S_ISSUE} → ${BOT}"; else log "補派失敗（issue #${S_ISSUE}），下一輪再試"; fi
  fi
  NEW=$(python3 -c '
import json, sys
old = set(json.loads(sys.argv[1])); print(json.dumps([n for n in json.loads(sys.argv[2]) if n not in old], ensure_ascii=False))
' "$S_FAILS" "$FAILURES")
  if [ "$NEW" != "[]" ] && [ -n "$S_ISSUE" ]; then
    if gh issue comment "$S_ISSUE" --body "$(printf 'run %s 起又多了這些失敗的測試：\n%s\n\n%s' "$RUN_ID" "$(fail_lines "$NEW")" "${RUN_URL}")" >> "$LOG" 2>&1; then
      S_FAILS=$(python3 -c '
import json, sys
old = json.loads(sys.argv[1]); print(json.dumps(old + [n for n in json.loads(sys.argv[2]) if n not in old], ensure_ascii=False))
' "$S_FAILS" "$FAILURES")
      log "失敗清單變了，已在 issue #${S_ISSUE} 留言：${NEW}"
    else
      log "issue #${S_ISSUE} 留言失敗，下一輪再試"
    fi
  fi
  state_write "$S_SHA" "$FIRST_RUN" "${S_ISSUE:-}" "${S_ASSIGNED:-false}" "$S_FAILS"
  exit 0
fi

# 4b. 新的一段紅。狀態檔可能只是遺失：先看有沒有現成開著的 ci-red issue，有就接手、不重開、不重派。
if ! EXISTING=$(gh issue list --label "$LABEL" --state open --json number --limit 1 -q '.[0].number // empty' 2> "$WORK/err"); then
  log "gh issue list 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
  exit 0
fi
if [ -n "$EXISTING" ]; then
  state_write "$FIRST_SHA" "$FIRST_RUN" "$EXISTING" true "$FAILURES"
  log "已有開著的 ${LABEL} issue #${EXISTING}，接手這一段紅（${FIRST_SHA:0:8}），不重開、不重派"
  exit 0
fi

gh label create "$LABEL" --color B60205 --description "main 的 CI 紅了（ci-watch-kick 自動開）" >/dev/null 2>&1 || true   # 已存在會失敗，無妨

if [ -n "$PREV_GREEN" ]; then SUSPECTS=$(git log --oneline -n 30 "${PREV_GREEN}..${FIRST_SHA}" 2>/dev/null)
else SUSPECTS=$(git log --oneline -n 10 "$FIRST_SHA" 2>/dev/null); fi
[ -n "$SUSPECTS" ] || SUSPECTS="（本機 repo 查不到 ${PREV_GREEN:-?}..${FIRST_SHA:0:8}，請先 git fetch）"
if [ "$NFAIL" -gt 0 ]; then TITLE="CI 紅了：${FIRST_SHA:0:8} 起 ${NFAIL} 條失敗"; else TITLE="CI 紅了：${FIRST_SHA:0:8} 起失敗"; fi
{
  printf 'main 的 CI 從這個 run 起是紅的（ci-watch-kick 自動開；恢復綠時會在這裡留言，但不會自動關）。\n\n'
  printf -- '- 第一個紅的 run：%s（run %s，`%s`）\n' "${FIRST_URL:-run ${FIRST_RUN}}" "$FIRST_RUN" "$FIRST_SHA"
  printf -- '- 目前最新一個紅的 run：%s（run %s）\n\n' "${RUN_URL:-}" "$RUN_ID"
  printf '## 失敗的測試（最新 run）\n%s\n\n' "$(fail_lines "$FAILURES")"
  printf '## 嫌疑 commit（上一個綠的 `%s` 之後）\n```\n%s\n```\n' "${PREV_GREEN:0:8}" "$SUSPECTS"
} > "$WORK/issue.md"
if ! URL=$(gh issue create --title "$TITLE" --label "$LABEL" --body-file "$WORK/issue.md" 2> "$WORK/err"); then
  log "開 issue 失敗，下一輪再試：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
  exit 0
fi
ISSUE=$(printf '%s\n' "$URL" | tail -1 | sed 's|.*/||')
case "$ISSUE" in ''|*[!0-9]*) log "開了 issue 但讀不到編號（${URL}），狀態不寫，下一輪會用現成的 ${LABEL} issue 接手"; exit 0 ;; esac
state_write "$FIRST_SHA" "$FIRST_RUN" "$ISSUE" false "$FAILURES"
log "CI 紅了：${FIRST_SHA:0:8} 起，開 issue #${ISSUE}（${NFAIL} 條失敗）"
if dispatch "$ISSUE"; then
  state_write "$FIRST_SHA" "$FIRST_RUN" "$ISSUE" true "$FAILURES"
  log "issue #${ISSUE} 已派 ${BOT}"
else
  log "派工失敗（issue #${ISSUE}），下一輪補派"
fi
exit 0
