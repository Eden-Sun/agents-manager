#!/bin/bash
set -u
PATH="${AGM_EXTRA_PATH-/opt/homebrew/bin:/usr/local/bin}:$PATH"; export PATH

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
AGM="$DIR/bin/agm"
LOG="$DIR/ci-watch.log"
TASK="$DIR/ci-watch-task.md"
STATE="$DIR/ci-watch.state.json"
OWNER="${AM_AGENT_NAME:-ci-watch-kick}"
LABEL="ci-red"

FAILS="$DIR/ci-watch.fails"
ERRS="$DIR/ci-watch.errors"
ERROR_ALERT_AFTER=${AGM_ERROR_ALERT_AFTER:-3}
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-6}

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  _a="${AGM:-$DIR/bin/agm}"
  [ -x "$_a" ] && "$_a" --compact ops-alert --source "${OWNER:-${0##*/}}" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }

fail_run() {
  log "$1"
  _n=$(cat "$FAILS" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$((_n + 1))
  echo "$_n" > "$FAILS"
  [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "main CI 盯哨連續 ${_n} 輪沒能完成檢查（等於沒在盯）：${1}"
  exit 0
}
ran_ok() { rm -f "$FAILS" 2>/dev/null; return 0; }

command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），main CI 盯哨停住"; exit 0; }
command -v gh >/dev/null 2>&1 || { alert missing_dependency "找不到 gh（PATH=${PATH}），main CI 盯哨停住"; exit 0; }
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -d "$REPO" ] || { log "找不到 ${REPO}，跳過"; exit 0; }

LOCK="$DIR/ci-watch.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}
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

WORK=$(mktemp -d "${TMPDIR:-/tmp}/agm-ci-watch.XXXXXX") || { log "建不了暫存目錄，跳過"; exit 0; }
cd "$REPO" 2>/dev/null || { log "進不了 ${REPO}，跳過"; exit 0; }

SOURCE="${AGM_CI_SOURCE:-ubuntu-ci}"
RUNS="$WORK/runs.json"
if [ "$SOURCE" = ubuntu-ci ]; then
  if ! python3 -c '
import json, subprocess, sys
def gh(path):
    return json.loads(subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout)
runs = []
latest_state = ""   # 最新一個有 ubuntu-ci status 的 commit 的 state（error＝基礎設施問題，不是測試紅）
commits = gh("repos/{owner}/{repo}/commits?sha=main&per_page=20")
json.dump(commits, open(sys.argv[3], "w"))   # 判定用的 commit 順序（#1287）
for c in commits:
    sha = c["sha"]
    st = [x for x in gh("repos/{owner}/{repo}/commits/%s/status" % sha).get("statuses", []) if x.get("context") == "ubuntu-ci"]
    if not st:
        continue
    state = st[0].get("state")
    if not runs:
        latest_state = state or ""
    runs.append({"databaseId": sha[:8], "headSha": sha, "createdAt": st[0].get("created_at") or "",
                 "url": st[0].get("target_url") or c.get("html_url") or "",
                 # error（磁碟不足、被訊號中斷、腳本中斷）不是測試紅：當未完成，不開票也不改狀態（#885）。
                 "status": "completed" if state in ("success", "failure") else "in_progress",
                 "conclusion": {"success": "success", "failure": "failure"}.get(state, "")})
json.dump(runs, open(sys.argv[1], "w"))
open(sys.argv[2], "w").write(latest_state)
' "$RUNS" "$WORK/latest-state" "$WORK/commits.json" 2> "$WORK/err"; then
    fail_run "讀 ubuntu-ci commit status 失敗，這輪不動：$(tail -c 200 "$WORK/err" | tr '\n' ' ')"
  fi
  if [ "$(cat "$WORK/latest-state" 2>/dev/null)" = error ]; then
    _e=$(cat "$ERRS" 2>/dev/null)
    case "$_e" in ''|*[!0-9]*) _e=0 ;; esac
    _e=$((_e + 1))
    echo "$_e" > "$ERRS"
    [ "$_e" -ge "$ERROR_ALERT_AFTER" ] && alert ci_infra_error "ubuntu-ci 最新 sha 連續回 error（磁碟不足／中斷），不是測試紅；看 ~/.cache/agents-manager/ci/status.json"
  else
    rm -f "$ERRS" 2>/dev/null
  fi
elif ! gh run list --branch main --workflow CI -L 20 --json databaseId,conclusion,status,headSha,createdAt,url > "$RUNS" 2> "$WORK/err"; then
  fail_run "gh run list 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
fi
if [ "$SOURCE" != ubuntu-ci ] && ! gh api "repos/{owner}/{repo}/commits?sha=main&per_page=20" > "$WORK/commits.json" 2> "$WORK/err"; then
  fail_run "讀不到 main 的 commit 順序，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
fi

ANALYSIS=$(python3 -c '
import json, sys
RED = {"failure", "timed_out", "startup_failure"}
try:
    runs = json.load(open(sys.argv[1]))
    assert isinstance(runs, list)
    # main 的 commit 順序（新的在前）。判定只看這個順序：run／status 的建立時間只反映「什麼時候寫進來」，
    # 較舊的 commit 可能晚補狀態（delayed、retry、unposted 補寫），按時間排會把舊的結果當成目前的結論（#1287）。
    order = {c["sha"]: i for i, c in enumerate(json.load(open(sys.argv[2])))}
except Exception:
    sys.exit(3)
done = [r for r in runs if r.get("status") == "completed" and (r.get("conclusion") in RED or r.get("conclusion") == "success")]
done.sort(key=lambda r: order.get(r.get("headSha"), len(order)))   # 不在這份順序裡的排最後（順序不明，不假裝知道）
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
' "$RUNS" "$WORK/commits.json") || fail_run "gh run list 的輸出看不懂，這輪不動"
field() { printf '%s\n' "$ANALYSIS" | sed -n "s/^$1=//p" | head -1; }
VERDICT=$(field verdict)
RUN_ID=$(field run); RUN_SHA=$(field sha); RUN_URL=$(field url)

state_get() { python3 -c '
import json, sys
try:
    v = json.load(open(sys.argv[1])).get(sys.argv[2])
except Exception:
    v = None
print("" if v is None else (json.dumps(v, ensure_ascii=False) if isinstance(v, (list, dict)) else str(v).lower() if isinstance(v, bool) else v))
' "$STATE" "$1" 2>/dev/null; }
state_write() {
  python3 -c '
import json, sys
sha, run, issue, assigned, failures = sys.argv[2:7]
json.dump({"first_red_sha": sha, "first_red_run": run, "issue": int(issue) if issue.isdigit() else None,
           "assigned": assigned == "true", "failures": json.loads(failures)}, open(sys.argv[1] + ".tmp", "w"), ensure_ascii=False)
' "$STATE" "$1" "$2" "$3" "$4" "$5" && mv "$STATE.tmp" "$STATE"
}

S_SHA=$(state_get first_red_sha)

case "$VERDICT" in
  none) ran_ok; exit 0 ;;
  green)
    [ -n "$S_SHA" ] || { ran_ok; exit 0; }
    S_ISSUE=$(state_get issue)
    if [ -n "$S_ISSUE" ]; then
      if ! gh issue comment "$S_ISSUE" --body "${RUN_SHA} 起恢復綠，run ${RUN_ID}${RUN_URL:+（${RUN_URL}）}。這張 issue 不會自動關，請修的人確認後關掉。" >> "$LOG" 2>&1; then
        fail_run "issue #${S_ISSUE} 留言失敗，狀態先不清，下一輪再試"
      fi
    fi
    rm -f "$STATE"; ran_ok
    log "恢復綠：${RUN_SHA:0:8}（run ${RUN_ID}）${S_ISSUE:+，已在 issue #${S_ISSUE} 留言}"
    exit 0 ;;
  red) ;;
  *) log "看不懂的判定 '${VERDICT}'，這輪不動"; exit 0 ;;
esac

FIRST_SHA=$(field first_sha); FIRST_RUN=$(field first_run); FIRST_URL=$(field first_url); PREV_GREEN=$(field prev_green_sha)

if [ "$SOURCE" = ubuntu-ci ]; then
  _clog="${AGM_CI_LOG_DIR:-${HOME:-/nonexistent}/.cache/agents-manager/ci/logs}/${RUN_SHA}.log"
  if [ -r "$_clog" ]; then
    cp "$_clog" "$WORK/failed.log"
  elif [ -z "${AGM_CI_HOST-ubuntu@agm-host}" ]; then
    fail_run "本機沒有 ubuntu-ci 的 log（${_clog}），也沒設 AGM_CI_HOST，這輪不動"
  elif ! ssh -o BatchMode=yes -o ConnectTimeout=15 "${AGM_CI_HOST-ubuntu@agm-host}" "cat ~/.cache/agents-manager/ci/logs/${RUN_SHA}.log" > "$WORK/failed.log" 2> "$WORK/err"; then
    fail_run "讀不到 ubuntu-ci 的 log（${RUN_SHA}），這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
  fi
elif ! gh run view "$RUN_ID" --log-failed > "$WORK/failed.log" 2> "$WORK/err"; then
  fail_run "gh run view ${RUN_ID} --log-failed 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
fi
FAILURES=$(python3 -c '
import json, re, sys
seen, out = set(), []
def add(n):
    # 這些字串來自 CI log（跑的是 PR／分支上的程式碼，不可信），之後會被放進 issue 內文的 `…` 與派給 bot 的交辦正文：
    # 反引號會跳出 code span、控制字元與過長的行會把整張票灌滿別人的字。反引號換成單引號、控制字元換成空白、限 200 字。
    n = re.sub(r"[\x00-\x1f\x7f\u202a-\u202e\u2066-\u2069]", " ", n.replace(chr(96), chr(39)))
    n = re.sub(r"\s+", " ", n).strip()[:200]
    if n and n not in seen:
        seen.add(n); out.append(n)
in_block = False
# bash 測試 harness 的失敗長 `FAIL - <中文描述>`，而描述跨檔不保證唯一，所以記住最近一個
# `==> ops: <path>` 當前綴（issue #449）。
src = ""
for raw in open(sys.argv[1], errors="replace"):
    line = raw.rstrip("\n").split("\t")[-1]
    line = re.sub(r"^\d{4}-\d\d-\d\dT[\d:.]+Z ", "", line)
    m = re.match(r"^==> \w+: (\S+)$", line)
    if m:
        src = m.group(1).rsplit("/", 1)[-1]; continue
    # `scripts/*_test.sh` 全部都是這個格式；行尾的全形括號是每輪會變的實際值（`（是 '1871'，
    # 預期 '0'）`），留著會讓「失敗清單變了」每輪都誤判成有新失敗，所以去掉。
    m = re.match(r"^FAIL\s+-\s+(.+)$", line)
    if m:
        desc = re.sub(r"（[^（）]*）\s*$", "", m.group(1)).strip()
        if desc:
            add("%s: %s" % (src, desc) if src else desc)
        continue
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
# 上限（#1174）：整樹大量紅時，清單會放進 issue 內文（GitHub 上限 65536 字）、交辦正文、狀態檔，還逐條跑 git log -S。
# 先排序再截：log 裡 FAILED 的順序每輪不同，不排序的話前 100 條每輪都不一樣。說明不含數字，條數一變也不會被當成新失敗。
MAX_NAMES = 100
if len(out) > MAX_NAMES:
    out = sorted(out)[:MAX_NAMES] + ["（失敗超過 %d 條，只列排序後的前 %d 條；完整清單見 CI log）" % (MAX_NAMES, MAX_NAMES)]
print(json.dumps(out, ensure_ascii=False))
' "$WORK/failed.log") || FAILURES="[]"
NFAIL=$(python3 -c 'import json,sys; print(len(json.loads(sys.argv[1])))' "$FAILURES")
fail_lines() { python3 -c '
import json, sys
names = json.loads(sys.argv[1])
print("\n".join("- `%s`" % n for n in names) if names else "（log 裡抽不出測試名，請看連結）")
' "$1"; }

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
dispatch() {
  find_bot
  [ -n "$BOT" ] || { log "找不到要派給誰（AGM_CI_BOT／runtime.json 的 ci_bot_id 或 responder_bot_id），issue #${1} 先開著，下一輪再派"; return 1; }
  _body="$WORK/assign.md"
  {
    cat "$TASK"
    printf '\n\n---\n本次：issue #%s（標籤 `%s`），第一個紅的 sha `%s`，run %s。\n\n失敗的測試：\n%s\n' \
      "$1" "$LABEL" "$FIRST_SHA" "$FIRST_RUN" "$(fail_lines "$FAILURES")"
  } > "$_body"
  "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$_body" \
      --request-id "ci-red-${FIRST_SHA}" >> "$LOG" 2>&1
}

SKIP_ADOPT=""
if [ -n "$S_SHA" ] && [ -n "$PREV_GREEN" ] && [ "$FIRST_SHA" != "$S_SHA" ]; then
  S_ISSUE=$(state_get issue)
  if [ -n "$S_ISSUE" ]; then
    if ! gh issue comment "$S_ISSUE" --body "${PREV_GREEN} 起恢復綠（中間那輪沒看到，這輪才補記）。run ${RUN_ID}（\`${FIRST_SHA}\`）起是新的一段紅，會另外開票。這張 issue 不會自動關，請修的人確認後關掉。" >> "$LOG" 2>&1; then
      fail_run "issue #${S_ISSUE} 補記轉綠失敗，狀態先留著，下一輪再試"
    fi
  fi
  SKIP_ADOPT="${S_ISSUE:-}"
  log "上一段紅（${S_SHA:0:8}）中間轉過綠（${PREV_GREEN:0:8}），這段從 ${FIRST_SHA:0:8} 另開"
  rm -f "$STATE"
  S_SHA=""
fi

if [ -n "$S_SHA" ]; then
  S_ISSUE=$(state_get issue); S_ASSIGNED=$(state_get assigned); S_FAILS=$(state_get failures); [ -n "$S_FAILS" ] || S_FAILS="[]"
  FIRST_SHA="$S_SHA"; FIRST_RUN=$(state_get first_red_run)
  if [ "$S_ASSIGNED" != "true" ] && [ -n "$S_ISSUE" ]; then
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
  ran_ok; exit 0
fi

if ! EXISTING=$(gh issue list --label "$LABEL" --state open --json number --limit 1 -q '.[0].number // empty' 2> "$WORK/err"); then
  fail_run "gh issue list 失敗，這輪不動：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
fi
if [ -n "$EXISTING" ] && [ "$EXISTING" = "${SKIP_ADOPT:-}" ]; then
  EXISTING=""
fi
if [ -n "$EXISTING" ]; then
  if ! gh issue comment "$EXISTING" --body "$(printf '新的一段紅，接手既有 issue #%s（不另開）。\n\n- run %s（`%s`）\n%s\n\n失敗的測試：\n%s\n' \
      "$EXISTING" "$RUN_ID" "$FIRST_SHA" "${RUN_URL}" "$(fail_lines "$FAILURES")")" >> "$LOG" 2>&1; then
    fail_run "issue #${EXISTING} 留言失敗，這輪不寫狀態，下一輪再試"
  fi
  ASSIGNED=false
  if dispatch "$EXISTING"; then
    ASSIGNED=true
    log "接手既有 ${LABEL} issue #${EXISTING}（${FIRST_SHA:0:8}），已留言並派工"
  else
    log "接手既有 ${LABEL} issue #${EXISTING}（${FIRST_SHA:0:8}），已留言，派工失敗，下一輪補派"
  fi
  state_write "$FIRST_SHA" "$FIRST_RUN" "$EXISTING" "$ASSIGNED" "$FAILURES"
  ran_ok
  exit 0
fi

gh label create "$LABEL" --color B60205 --description "main 的 CI 紅了（ci-watch-kick 自動開）" >/dev/null 2>&1 || true

if [ -n "$PREV_GREEN" ]; then SUSPECTS=$(git log --oneline -n 30 "${PREV_GREEN}..${FIRST_SHA}" 2>/dev/null)
else SUSPECTS=$(git log --oneline -n 10 "$FIRST_SHA" 2>/dev/null); fi
[ -n "$SUSPECTS" ] || SUSPECTS="（本機 repo 查不到 ${PREV_GREEN:-?}..${FIRST_SHA:0:8}，請先 git fetch）"

: > "${WORK}/names.txt"; : > "${WORK}/intro.txt"; : > "${WORK}/inrange.txt"
python3 -c 'import json,sys; print("\n".join(json.loads(sys.argv[1])))' "$FAILURES" > "${WORK}/names.txt" 2>/dev/null || :
if [ -n "$PREV_GREEN" ]; then git rev-list "${PREV_GREEN}..${FIRST_SHA}" > "${WORK}/range.txt" 2>/dev/null || : > "${WORK}/range.txt"
else : > "${WORK}/range.txt"; fi
while IFS= read -r TNAME; do
  [ -n "$TNAME" ] || continue
  case "$TNAME" in "（"*) continue ;; esac
  case "$TNAME" in
    *::*) NEEDLE=${TNAME##*::}; NEEDLE=${NEEDLE%% *}; NEEDLE=${NEEDLE%%(*} ;;
    *": "*) NEEDLE=${TNAME#*": "} ;;
    *) NEEDLE=${TNAME%% *} ;;
  esac
  [ -n "$NEEDLE" ] || continue
  INTRO_LINE=$(git log -S"$NEEDLE" --oneline -n 1 2>/dev/null)
  if [ -z "$INTRO_LINE" ]; then
    printf -- '- `%s` ← 查不到（本機 repo 可能落後，先 git fetch）\n' "$TNAME" >> "${WORK}/intro.txt"
    continue
  fi
  INTRO_SHA=${INTRO_LINE%% *}
  if [ -s "${WORK}/range.txt" ] && grep -q "^${INTRO_SHA}" "${WORK}/range.txt" 2>/dev/null; then
    printf -- '- `%s` ← `%s`（**在本段紅的範圍內，優先看這個**）\n' "$TNAME" "$INTRO_LINE" >> "${WORK}/intro.txt"
    echo 1 >> "${WORK}/inrange.txt"
  else
    printf -- '- `%s` ← `%s`（在上一個綠之前就有了）\n' "$TNAME" "$INTRO_LINE" >> "${WORK}/intro.txt"
  fi
done < "${WORK}/names.txt"
INTRO=$(cat "${WORK}/intro.txt")
[ -n "$INTRO" ] || INTRO="（沒有抽到測試名，或本機 repo 查不到）"
FLAKY=""
if [ "$NFAIL" -gt 0 ] && [ -n "$PREV_GREEN" ] && [ ! -s "${WORK}/inrange.txt" ] && [ -s "${WORK}/names.txt" ]; then
  FLAKY="**疑似間歇紅（flaky）**：這些測試在上一個綠的 run（\`${PREV_GREEN:0:8}\`）裡就已經存在而且通過了，下面那串嫌疑 commit 多半不是兇手。先當成 flaky 查（排序鍵、時間戳精度、平行測試共用狀態），本機重跑不出來**不代表**不是它。"
fi
if [ "$NFAIL" -gt 100 ]; then TITLE="CI 紅了：${FIRST_SHA:0:8} 起超過 100 條失敗"
elif [ "$NFAIL" -gt 0 ]; then TITLE="CI 紅了：${FIRST_SHA:0:8} 起 ${NFAIL} 條失敗"
else TITLE="CI 紅了：${FIRST_SHA:0:8} 起失敗"; fi
{
  printf 'main 的 CI 從這個 run 起是紅的（ci-watch-kick 自動開；恢復綠時會在這裡留言，但不會自動關）。\n\n'
  printf -- '- 第一個紅的 run：%s（run %s，`%s`）\n' "${FIRST_URL:-run ${FIRST_RUN}}" "$FIRST_RUN" "$FIRST_SHA"
  printf -- '- 目前最新一個紅的 run：%s（run %s）\n\n' "${RUN_URL:-}" "$RUN_ID"
  printf '## 失敗的測試（最新 run）\n%s\n\n' "$(fail_lines "$FAILURES")"
  printf '## 引入這些測試的 commit（`git log -S`）\n%s\n\n' "$INTRO"
  [ -n "$FLAKY" ] && printf '%s\n\n' "$FLAKY"
  printf '## 上一個綠的 `%s` 之後的 commit\n（一次紅到底的回歸看這裡；**間歇紅時這串不可靠**，見上面）\n```\n%s\n```\n' "${PREV_GREEN:0:8}" "$SUSPECTS"
} > "$WORK/issue.md"
if ! URL=$(gh issue create --title "$TITLE" --label "$LABEL" --body-file "$WORK/issue.md" 2> "$WORK/err"); then
  fail_run "開 issue 失敗，下一輪再試：$(head -c 200 "$WORK/err" | tr '\n' ' ')"
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
ran_ok
exit 0
