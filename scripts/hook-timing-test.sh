#!/usr/bin/env bash
# M4 fake-hook timing tests (SPEC §4.4, §6.7, appendix D / M4).
#
# Drives the running daemon with hand-rolled `curl` hooks so the hook/Turn matching
# rules can be checked without waiting on a real agent:
#
#   A  hook arrives before the prompt RPC returns  -> one assistant message, Turn completed
#   B  the same native turn id twice               -> still one assistant message
#   C  SessionStart / a non-turn event             -> no new Turn, no new Message
#   D  daemon unreachable                          -> <= 3 s, exit 0, empty stdout, spool +1
#   E  a Stop with no in-flight Turn               -> origin=external Turn + assistant message
#
# Requires: a daemon on 127.0.0.1:<port> and one bot with a *running* Run.
# Usage:
#   scripts/hook-timing-test.sh [--bot BOT_ID] [--port N] [--bot-token T]
#                               [--ui-token T] [--binary PATH] [--keep]
# Anything not given is discovered from ~/.config/agents-manager/.
set -uo pipefail

DATA_DIR="${AM_DATA_DIR:-$HOME/.config/agents-manager}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOT_ID=""; BOT_TOKEN=""; UI_TOKEN=""; PORT=""; BINARY=""; KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --bot)       BOT_ID="$2"; shift 2 ;;
    --bot-token) BOT_TOKEN="$2"; shift 2 ;;
    --ui-token)  UI_TOKEN="$2"; shift 2 ;;
    --port)      PORT="$2"; shift 2 ;;
    --binary)    BINARY="$2"; shift 2 ;;
    --keep)      KEEP=1; shift ;;
    -h|--help)   sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

PASS=0; FAIL=0; SKIP=0
pass() { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2"; }
skip() { SKIP=$((SKIP+1)); printf '  \033[33mSKIP\033[0m %s — %s\n' "$1" "$2"; }
info() { printf '  ·    %s\n' "$1"; }
head1() { printf '\n\033[1m%s\033[0m\n' "$1"; }
die()  { printf '\033[31mfatal:\033[0m %s\n' "$1" >&2; exit 1; }

command -v python3 >/dev/null || die "python3 is required"
command -v curl    >/dev/null || die "curl is required"

# Evaluate `expr` with `d` = the JSON document on stdin and `a` = the extra argv.
pyq() { python3 -c 'import sys,json;d=json.load(sys.stdin);a=sys.argv[2:];print(eval(sys.argv[1],{"d":d,"a":a,"json":json,"len":len,"sum":sum,"set":set,"sorted":sorted}))' "$@"; }
now_iso() { python3 -c 'import datetime;print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3]+"Z")'; }
rand_id() { python3 -c 'import uuid;print(uuid.uuid4())'; }
epoch()   { python3 -c 'import time;print(time.time())'; }

# ---------------------------------------------------------------- discovery

if [ -z "$PORT" ]; then
  PORT="$(python3 - "$DATA_DIR/config.toml" <<'PY'
import re, sys
try:
    t = open(sys.argv[1]).read()
except OSError:
    t = ""
m = re.search(r'^\s*listen\s*=\s*"[^":]*:(\d+)"', t, re.M)
print(m.group(1) if m else "7788")
PY
)"
fi
BASE="http://127.0.0.1:$PORT"

if [ -z "$UI_TOKEN" ] && [ -r "$DATA_DIR/ui-token" ]; then
  UI_TOKEN="$(tr -d '[:space:]' < "$DATA_DIR/ui-token")"
fi
[ -n "$UI_TOKEN" ] || die "no UI token; pass --ui-token or create $DATA_DIR/ui-token"

if [ -z "$BINARY" ]; then
  for c in "$REPO_ROOT/target/debug/agents-managerd" "$REPO_ROOT/target/release/agents-managerd" "$(command -v agents-managerd 2>/dev/null)"; do
    [ -n "$c" ] && [ -x "$c" ] && { BINARY="$c"; break; }
  done
fi

api() { curl -s --max-time 30 -H "X-AM-Token: $UI_TOKEN" "$@"; }

STATE="$(api "$BASE/api/state")" || die "cannot reach daemon at $BASE"
echo "$STATE" | python3 -c 'import sys,json;json.load(sys.stdin)' 2>/dev/null \
  || die "bad /api/state response (is the daemon up on $PORT? is the UI token stale?): $STATE"

if [ -z "$BOT_ID" ]; then
  BOT_ID="$(printf '%s' "$STATE" | pyq '([b["id"] for p in d["projects"] for b in p["bots"] if (b.get("run") or {}).get("state")=="running"] or [""])[0]')"
fi
[ -n "$BOT_ID" ] || die "no bot with a running Run; start one first (POST /api/bots/:id/start) or pass --bot"

BOT_JSON="$(printf '%s' "$STATE" | pyq 'json.dumps(([b for p in d["projects"] for b in p["bots"] if b["id"]==a[0]] or [{}])[0])' "$BOT_ID")"
BOT_NAME="$(printf '%s' "$BOT_JSON" | pyq 'd.get("name","?")')"
KIND="$(printf '%s' "$BOT_JSON" | pyq 'd.get("kind","claude")')"
RUN_STATE="$(printf '%s' "$BOT_JSON" | pyq '(d.get("run") or {}).get("state","")')"
INJECT="$(printf '%s' "$BOT_JSON" | pyq 'd.get("inject_hooks", True)')"
[ "$RUN_STATE" = "running" ] || die "bot $BOT_NAME is '$RUN_STATE', not running"

if [ -z "$BOT_TOKEN" ]; then
  if command -v sqlite3 >/dev/null; then
    BOT_TOKEN="$(sqlite3 "file:$DATA_DIR/agents-manager.sqlite3?mode=ro" \
      "SELECT hook_token FROM bots WHERE id='$BOT_ID';" 2>/dev/null | tr -d '[:space:]')"
  fi
fi
[ -n "$BOT_TOKEN" ] || die "no per-bot hook token; pass --bot-token (or install sqlite3)"

printf '\033[1mM4 hook timing test\033[0m\n'
info "daemon    $BASE"
info "bot       $BOT_NAME ($BOT_ID), kind=$KIND, inject_hooks=$INJECT"
info "binary    ${BINARY:-<not found — scenario D will be skipped>}"
if [ "$INJECT" = "True" ] || [ "$INJECT" = "true" ]; then
  info "note: this bot injects real hooks, so the live agent may add its own"
  info "      external Turns during the run; assertions are scoped to our own Turn."
fi

# ---------------------------------------------------------------- helpers

# The native session id we pretend to be. Reusing the run's real one would collide
# with the live agent's own dedup key, so we use a distinct synthetic session.
SESSION_ID="timing-test-$(rand_id)"

msgs() { api "$BASE/api/bots/$BOT_ID/messages?limit=500"; }
count_msgs()  { msgs | pyq 'len(d["messages"])'; }
count_turns() { msgs | pyq 'len(d["turns"])'; }

# assistant messages attached to a given turn id
assistants_of() { msgs | pyq 'len([m for m in d["messages"] if m.get("turn_id")==a[0] and m["role"]=="assistant"])' "$1"; }
turn_field()    { msgs | pyq '(([t for t in d["turns"] if t["id"]==a[0]] or [{}])[0]).get(a[1],"")' "$1" "$2"; }

# $1 = native turn id, $2 = assistant text
turn_payload() {
  if [ "$KIND" = "codex" ]; then
    python3 -c 'import json,sys;print(json.dumps({"type":"agent-turn-complete","thread-id":sys.argv[1],"turn-id":sys.argv[2],"cwd":"/","input-messages":[],"last-assistant-message":sys.argv[3]}))' "$SESSION_ID" "$1" "$2"
  else
    python3 -c 'import json,sys;print(json.dumps({"hook_event_name":"Stop","session_id":sys.argv[1],"transcript_path":"/dev/null","prompt_id":sys.argv[2],"last_assistant_message":sys.argv[3],"stop_hook_active":False}))' "$SESSION_ID" "$1" "$2"
  fi
}
session_payload() {
  if [ "$KIND" = "codex" ]; then
    # Codex has no SessionStart; an unrelated event type must be ignored the same way.
    python3 -c 'import json,sys;print(json.dumps({"type":"agent-turn-started","thread-id":sys.argv[1],"turn-id":"ignored"}))' "$SESSION_ID"
  else
    python3 -c 'import json,sys;print(json.dumps({"hook_event_name":"SessionStart","session_id":sys.argv[1],"transcript_path":"/dev/null","cwd":"/"}))' "$SESSION_ID"
  fi
}

# POST a hook body exactly as the child process would. Prints the HTTP status.
post_hook() {
  local payload="$1" body
  body="$(python3 -c 'import json,sys;print(json.dumps({"bot_id":sys.argv[1],"provider":sys.argv[2],"payload":json.loads(sys.argv[3]),"received_at":sys.argv[4],"truncated":False}))' \
    "$BOT_ID" "$KIND" "$payload" "$(now_iso)")"
  curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST "$BASE/hook/$KIND" \
    -H "X-AM-Bot-Token: $BOT_TOKEN" -H 'Content-Type: application/json' -d "$body"
}

# Wait until `turn_field <id> status` stops being in_flight (or the timeout expires).
wait_turn_settled() {
  local id="$1" deadline=$(( $(date +%s) + ${2:-25} )) st
  while [ "$(date +%s)" -lt "$deadline" ]; do
    st="$(turn_field "$id" status)"
    [ -n "$st" ] && [ "$st" != "in_flight" ] && { echo "$st"; return 0; }
    sleep 0.5
  done
  echo "$(turn_field "$id" status)"
}

wait_no_in_flight() {
  local deadline=$(( $(date +%s) + ${1:-30} ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    [ "$(msgs | pyq 'len([t for t in d["turns"] if t["status"]=="in_flight"])')" = "0" ] && return 0
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------- A

head1 "A. hook lands before the prompt RPC returns"
if ! wait_no_in_flight 20; then
  skip "A" "the bot already has an in-flight Turn; abandon it and retry"
  TURN_A=""
else
  NATIVE_A="timing-a-$(rand_id)"
  CRID="timing-a-$(rand_id)"
  PROMPT_OUT="$(mktemp)"
  # Fire the prompt without waiting for the RPC to come back...
  api -X POST "$BASE/api/bots/$BOT_ID/prompt" -H 'Content-Type: application/json' \
      -d "{\"text\":\"[hook timing test A] ignore this\",\"client_request_id\":\"$CRID\"}" \
      > "$PROMPT_OUT" 2>&1 &
  PROMPT_PID=$!
  # ...and immediately deliver the Stop hook. The Turn is already in_flight (SPEC §6.3.3),
  # so this must match rather than create an external Turn.
  sleep 0.05
  CODE_A="$(post_hook "$(turn_payload "$NATIVE_A" "PONG-A")")"
  wait "$PROMPT_PID" 2>/dev/null
  PROMPT_BODY="$(cat "$PROMPT_OUT")"; rm -f "$PROMPT_OUT"
  TURN_A="$(printf '%s' "$PROMPT_BODY" | pyq 'd.get("turn_id","")' 2>/dev/null)"

  if [ "$CODE_A" != "200" ]; then
    fail "A hook accepted" "POST /hook/$KIND returned $CODE_A (token wrong?)"
  else
    pass "A hook accepted (200)"
  fi
  if [ -z "$TURN_A" ]; then
    fail "A prompt created a Turn" "prompt response had no turn_id: $PROMPT_BODY"
  else
    info "turn_id $TURN_A"
    ST_A="$(wait_turn_settled "$TURN_A" 25)"
    [ "$ST_A" = "completed" ] && pass "A Turn completed" \
      || fail "A Turn completed" "status=$ST_A (expected completed; completed_fallback means the hook lost the race)"
    N_A="$(assistants_of "$TURN_A")"
    [ "$N_A" = "1" ] && pass "A exactly one assistant message" \
      || fail "A exactly one assistant message" "found $N_A"
  fi
fi

# ---------------------------------------------------------------- B

head1 "B. the same native turn id twice"
if [ -z "${TURN_A:-}" ]; then
  skip "B" "scenario A did not produce a Turn"
else
  T_BEFORE="$(count_turns)"; M_BEFORE="$(count_msgs)"
  CODE_B="$(post_hook "$(turn_payload "$NATIVE_A" "PONG-A-DUPLICATE")")"
  sleep 1.5
  T_AFTER="$(count_turns)"; M_AFTER="$(count_msgs)"
  N_B="$(assistants_of "$TURN_A")"
  [ "$CODE_B" = "200" ] && pass "B duplicate hook accepted (200)" || fail "B duplicate hook accepted" "status $CODE_B"
  [ "$N_B" = "1" ] && pass "B still exactly one assistant message" || fail "B still exactly one assistant message" "found $N_B"
  if [ "$T_AFTER" = "$T_BEFORE" ] && [ "$M_AFTER" = "$M_BEFORE" ]; then
    pass "B no Turn / Message added by the duplicate"
  else
    fail "B no Turn / Message added by the duplicate" "turns $T_BEFORE->$T_AFTER, messages $M_BEFORE->$M_AFTER"
  fi
fi

# ---------------------------------------------------------------- C

head1 "C. SessionStart (claude) / non-turn event (codex) creates nothing"
wait_no_in_flight 20 >/dev/null
T_BEFORE="$(count_turns)"; M_BEFORE="$(count_msgs)"
CODE_C="$(post_hook "$(session_payload)")"
sleep 1.5
T_AFTER="$(count_turns)"; M_AFTER="$(count_msgs)"
[ "$CODE_C" = "200" ] && pass "C hook accepted (200)" || fail "C hook accepted" "status $CODE_C"
if [ "$T_AFTER" = "$T_BEFORE" ] && [ "$M_AFTER" = "$M_BEFORE" ]; then
  pass "C no Turn and no Message created (turns=$T_AFTER, messages=$M_AFTER)"
else
  fail "C no Turn and no Message created" "turns $T_BEFORE->$T_AFTER, messages $M_BEFORE->$M_AFTER"
fi

# ---------------------------------------------------------------- D

head1 "D. daemon unreachable: <= 3 s, exit 0, empty stdout, one spool line"
if [ -z "$BINARY" ]; then
  skip "D" "agents-managerd binary not found (pass --binary)"
else
  # A closed port is indistinguishable from a stopped daemon for the child process,
  # and it lets the rest of the suite keep running. A throwaway bot id keeps the
  # spool out of any real bot's directory (the daemon only replays live bots).
  DEAD_PORT=1
  for p in 45231 45232 45233 45234; do
    nc -z 127.0.0.1 "$p" >/dev/null 2>&1 || { DEAD_PORT="$p"; break; }
  done
  FAKE_BOT="timing-test-$(rand_id)"
  SPOOL="$DATA_DIR/bots/$FAKE_BOT/hook-spool.jsonl"
  OUT="$(mktemp)"; ERR="$(mktemp)"
  T0="$(epoch)"
  printf '%s' "$(turn_payload "timing-d-$(rand_id)" "PONG-D")" \
    | "$BINARY" hook "$KIND" --bot "$FAKE_BOT" --token "$BOT_TOKEN" --port "$DEAD_PORT" \
      >"$OUT" 2>"$ERR"
  EC=$?
  T1="$(epoch)"
  ELAPSED="$(python3 -c "print('%.3f' % ($T1-$T0))")"
  OUT_BYTES="$(wc -c < "$OUT" | tr -d ' ')"

  info "elapsed ${ELAPSED}s, exit $EC, stdout ${OUT_BYTES}B, stderr: $(head -c 160 "$ERR")"
  python3 -c "import sys;sys.exit(0 if $ELAPSED <= 3.0 else 1)" \
    && pass "D wall-clock ${ELAPSED}s <= 3 s" || fail "D wall-clock" "${ELAPSED}s > 3 s"
  [ "$EC" = "0" ] && pass "D exit 0" || fail "D exit 0" "exit $EC"
  [ "$OUT_BYTES" = "0" ] && pass "D stdout empty" || fail "D stdout empty" "$OUT_BYTES bytes: $(head -c 200 "$OUT")"
  if [ -s "$SPOOL" ] && [ "$(wc -l < "$SPOOL" | tr -d ' ')" = "1" ]; then
    if [ "$(pyq 'd["bot_id"]' < "$SPOOL")" = "$FAKE_BOT" ] \
       && [ "$(pyq 'd["provider"]' < "$SPOOL")" = "$KIND" ] \
       && [ "$(pyq '"payload" in d and "received_at" in d and "truncated" in d' < "$SPOOL")" = "True" ]; then
      pass "D one well-formed spool line"
    else
      fail "D one well-formed spool line" "unexpected body: $(head -c 200 "$SPOOL")"
    fi
  else
    fail "D one spool line" "expected exactly 1 line in $SPOOL"
  fi
  rm -f "$OUT" "$ERR"
  if [ "$KEEP" = "0" ]; then rm -rf "$DATA_DIR/bots/$FAKE_BOT"; else info "kept $DATA_DIR/bots/$FAKE_BOT"; fi
fi

# ---------------------------------------------------------------- E

head1 "E. Stop with no in-flight Turn -> origin=external"
if ! wait_no_in_flight 25; then
  skip "E" "a Turn is still in flight"
else
  BEFORE_FILE="$(mktemp)"
  msgs | pyq 'json.dumps([t["id"] for t in d["turns"]])' > "$BEFORE_FILE"
  NATIVE_E="timing-e-$(rand_id)"
  CODE_E="$(post_hook "$(turn_payload "$NATIVE_E" "PONG-E")")"
  [ "$CODE_E" = "200" ] && pass "E hook accepted (200)" || fail "E hook accepted" "status $CODE_E"
  TURN_E=""
  DEADLINE=$(( $(date +%s) + 15 ))
  while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    TURN_E="$(msgs | pyq '([t["id"] for t in d["turns"] if t["id"] not in set(json.load(open(a[0])))] or [""])[0]' "$BEFORE_FILE")"
    [ -n "$TURN_E" ] && break
    sleep 0.5
  done
  rm -f "$BEFORE_FILE"
  if [ -z "$TURN_E" ]; then
    fail "E external Turn created" "no new Turn appeared within 15 s"
  else
    info "turn_id $TURN_E"
    O_E="$(turn_field "$TURN_E" origin)"; S_E="$(turn_field "$TURN_E" status)"
    [ "$O_E" = "external" ] && pass "E origin=external" || fail "E origin=external" "origin=$O_E"
    [ "$S_E" = "completed" ] && pass "E status=completed" || fail "E status=completed" "status=$S_E"
    N_E="$(assistants_of "$TURN_E")"
    [ "$N_E" = "1" ] && pass "E one assistant message" || fail "E one assistant message" "found $N_E"
  fi
fi

# ---------------------------------------------------------------- summary

head1 "summary"
printf '  %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" = "0" ] || exit 1
exit 0
