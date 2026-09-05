#!/bin/sh
# remote-loop-test.sh — automated R1–R3 (SPEC §11.8) against a loopback "remote".
#
# Brings up scripts/dev-sshd.sh on 127.0.0.1:2222, registers it as host `loop`
# (herdr session `am-loop`), creates a remote Project + Bot, starts it, answers the
# trust prompt, sends a prompt, checks the reply came from a **hook** over the reverse
# tunnel, then stops and removes everything it created. Safe to run repeatedly.
#
#   scripts/remote-loop-test.sh
#
# Environment:
#   AM_BASE=http://127.0.0.1:7788   daemon base URL
#   AM_KIND=claude|codex            agent to run (default claude)
#   AM_PROJECT_DIR=/tmp/...         reuse a fixed (already trusted) project directory
#   AM_KEEP=1                       keep the host / project / dev sshd afterwards
#   AM_HOOK_PORT=17788              reverse-forwarded port (must differ from the
#                                   daemon's own port: the "remote" is this machine)

set -eu

BASE="${AM_BASE:-http://127.0.0.1:7788}"
KIND="${AM_KIND:-claude}"
HOOK_PORT="${AM_HOOK_PORT:-17788}"
HOST=loop
SESSION=am-loop
# A fresh directory per run so the trust prompt (R2's `blocked` step) is actually exercised;
# pin it with AM_PROJECT_DIR to reuse an already-trusted folder.
PROJECT_DIR="${AM_PROJECT_DIR:-$(mktemp -d /tmp/am-loop-test.XXXXXX)}"
PROJECT_BASE=$(basename "$PROJECT_DIR")
BOT_NAME="loop-$KIND"
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DEV_SSHD="$HERE/dev-sshd.sh"
TMP="${TMPDIR:-/tmp}/am-loop-test.$$"
mkdir -p "$TMP"
trap 'rm -rf "$TMP"' EXIT

step()  { printf '\n=== %s\n' "$*"; }
fail()  { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
ok()    { printf 'ok: %s\n' "$*"; }

# py '<expression over the parsed json in `d`>'  < file
py() { python3 -c 'import json,sys
d=json.load(open(sys.argv[2]))
print(eval(sys.argv[1]))' "$1" "$2"; }

api() { # api METHOD PATH [JSON-BODY] -> body on stdout, status in $API_STATUS
  _m=$1; _p=$2; _b=${3:-}
  if [ -n "$_b" ]; then
    API_STATUS=$(curl -s -o "$TMP/resp" -w '%{http_code}' -X "$_m" \
      -H "X-AM-Token: $TOKEN" -H 'Content-Type: application/json' -d "$_b" "$BASE$_p")
  else
    API_STATUS=$(curl -s -o "$TMP/resp" -w '%{http_code}' -X "$_m" -H "X-AM-Token: $TOKEN" "$BASE$_p")
  fi
  cat "$TMP/resp"
}

# ---------------------------------------------------------------- 0. daemon

step "0. daemon reachable at $BASE"
curl -s -o "$TMP/sess" "$BASE/api/session" || fail "daemon not reachable at $BASE"
TOKEN=$(py 'd["token"]' "$TMP/sess") || fail "could not read the UI token"
ok "token acquired"

# ---------------------------------------------------------------- 1. dev sshd (R5)

step "1. dev sshd on 127.0.0.1:2222"
HOST_BODY=$("$DEV_SSHD" start | tail -1)
[ -n "$HOST_BODY" ] || fail "dev-sshd.sh start printed no host body"
# Override the hook port (the reverse forward lands on this very machine).
HOST_BODY=$(printf '%s' "$HOST_BODY" | python3 -c 'import json,sys,os
b=json.load(sys.stdin); b["hook_port"]=int(os.environ["HOOK_PORT"]); print(json.dumps(b))' )
ok "dev sshd up"

# ---------------------------------------------------------------- 2. host (R1)

step "2. POST /api/hosts ($HOST -> $SESSION)"
api POST /api/hosts "$HOST_BODY" > "$TMP/host"
[ "$API_STATUS" = 200 ] || { cat "$TMP/host"; fail "POST /api/hosts -> $API_STATUS"; }
CONNECTED=$(py 'd["connected"]' "$TMP/host")
[ "$CONNECTED" = True ] || { cat "$TMP/host"; fail "host did not connect"; }
ok "host $HOST connected"

step "2b. remote directory listing (§11.5)"
mkdir -p "$PROJECT_DIR"
api GET "/api/fs/dirs?host=$HOST&path=/tmp" > "$TMP/dirs"
[ "$API_STATUS" = 200 ] || { cat "$TMP/dirs"; fail "GET /api/fs/dirs -> $API_STATUS"; }
py '[e["name"] for e in d["entries"]].count("'"$PROJECT_BASE"'")' "$TMP/dirs" | grep -q '^1$' \
  || fail "remote listing of /tmp did not contain $PROJECT_BASE"
ok "remote listing works"

# ---------------------------------------------------------------- 3. clean slate

step "3. remove leftovers from a previous run"
api GET /api/state > "$TMP/state"
OLD=$(py '" ".join(p["id"] for p in d["projects"] if p["host"]=="'"$HOST"'")' "$TMP/state")
for pid in $OLD; do
  BOTS=$(py '" ".join(b["id"] for p in d["projects"] if p["id"]=="'"$pid"'" for b in p["bots"])' "$TMP/state")
  for b in $BOTS; do api POST "/api/bots/$b/stop" >/dev/null; api DELETE "/api/bots/$b" >/dev/null; done
  api DELETE "/api/projects/$pid" >/dev/null
  ok "removed old project $pid"
done

# ---------------------------------------------------------------- 4. project + bot (R2)

step "4. POST /api/projects (host=$HOST, path=$PROJECT_DIR)"
api POST /api/projects "{\"path\":\"$PROJECT_DIR\",\"label\":\"loop-test\",\"host\":\"$HOST\"}" > "$TMP/proj"
[ "$API_STATUS" = 200 ] || { cat "$TMP/proj"; fail "POST /api/projects -> $API_STATUS"; }
PID=$(py 'd["project_id"]' "$TMP/proj")
ok "project $PID"

api GET /api/state > "$TMP/state"
py '[p["host"] for p in d["projects"] if p["id"]=="'"$PID"'"][0]' "$TMP/state" | grep -qx "$HOST" \
  || fail "project host was not persisted as $HOST"
ok "projects[].host = $HOST in /api/state"

step "5. POST /api/projects/$PID/bots ($BOT_NAME, $KIND)"
api POST "/api/projects/$PID/bots" "{\"name\":\"$BOT_NAME\",\"kind\":\"$KIND\",\"auto_approve\":false}" > "$TMP/bot"
[ "$API_STATUS" = 200 ] || { cat "$TMP/bot"; fail "create bot -> $API_STATUS"; }
BID=$(py 'd["bot_id"]' "$TMP/bot")
ok "bot $BID"

lamp() {
  api GET /api/state > "$TMP/state"
  py '([b["lamp"] for p in d["projects"] for b in p["bots"] if b["id"]=="'"$BID"'"] or ["gone"])[0]' "$TMP/state"
}

wait_lamp() { # wait_lamp <seconds> <lamp>...
  _t=$1; shift
  _i=0
  while [ "$_i" -lt "$_t" ]; do
    _l=$(lamp)
    for _w in "$@"; do [ "$_l" = "$_w" ] && { echo "$_l"; return 0; }; done
    _i=$((_i+2)); sleep 2
  done
  echo "$_l"
  return 1
}

step "6. POST /api/bots/$BID/start"
api POST "/api/bots/$BID/start" > "$TMP/run"
[ "$API_STATUS" = 200 ] || { cat "$TMP/run"; fail "start -> $API_STATUS"; }
L=$(wait_lamp 90 idle blocked) || fail "bot never reached idle/blocked (lamp=$L)"
ok "started, lamp=$L"

step "7. remote hook material installed over ssh"
BOT_DIR="$HOME/.config/agents-manager/bots/$BID"
[ -x "$BOT_DIR/hook.sh" ] || fail "$BOT_DIR/hook.sh missing on the remote"
if [ "$KIND" = claude ]; then
  grep -q "$HOOK_PORT" "$BOT_DIR/claude-settings.json" || fail "claude-settings.json does not carry hook_port $HOOK_PORT"
fi
ok "hook.sh + settings present"

if [ "$L" = blocked ]; then
  step "8. answer the trust prompt (down, enter)"
  if [ "$KIND" = codex ]; then KEYS='["enter"]'; else KEYS='["down","enter"]'; fi
  api POST "/api/bots/$BID/keys" "{\"keys\":$KEYS}" >/dev/null
  L=$(wait_lamp 60 idle) || fail "still $L after answering the trust prompt"
  ok "trust prompt answered, lamp=idle"
else
  step "8. no trust prompt (folder already trusted)"
fi

# ---------------------------------------------------------------- 9. prompt (R3)

step "9. POST /api/bots/$BID/prompt"
api POST "/api/bots/$BID/prompt" '{"text":"Reply with exactly PONG","client_request_id":"loop-test-1"}' > "$TMP/p"
[ "$API_STATUS" = 200 ] || { cat "$TMP/p"; fail "prompt -> $API_STATUS"; }
py 'd["delivery"]' "$TMP/p" | grep -qx ok || { cat "$TMP/p"; fail "delivery was not ok"; }

i=0
SRC=""
while [ $i -lt 120 ]; do
  api GET "/api/bots/$BID/messages?limit=10" > "$TMP/msgs"
  SRC=$(py '([m["source"] for m in d["messages"] if m["role"]=="assistant"] or [""])[-1]' "$TMP/msgs")
  [ -n "$SRC" ] && break
  i=$((i+3)); sleep 3
done
[ -n "$SRC" ] || fail "no assistant reply within 120s"
BODY=$(py '([m["content"] for m in d["messages"] if m["role"]=="assistant"] or [""])[-1]' "$TMP/msgs")
printf 'reply source=%s body=%s\n' "$SRC" "$BODY"
[ "$SRC" = hook ] || fail "reply source was $SRC, expected hook (the reverse tunnel is not working)"
ok "reply arrived over the remote hook"

# ---------------------------------------------------------------- 10. stop + cleanup

step "10. POST /api/bots/$BID/stop"
api POST "/api/bots/$BID/stop" >/dev/null
L=$(wait_lamp 40 offline) || fail "bot did not go offline (lamp=$L)"
ok "stopped"

if [ "${AM_KEEP:-0}" = 1 ]; then
  printf '\nAM_KEEP=1: leaving host %s, project %s and the dev sshd in place.\n' "$HOST" "$PID"
else
  step "11. cleanup"
  api DELETE "/api/bots/$BID" >/dev/null
  api DELETE "/api/projects/$PID" >/dev/null
  api DELETE "/api/hosts/$HOST" > "$TMP/dh"
  [ "$API_STATUS" = 200 ] || { cat "$TMP/dh"; fail "DELETE /api/hosts/$HOST -> $API_STATUS"; }
  "$DEV_SSHD" stop >/dev/null
  case "$PROJECT_DIR" in /tmp/am-loop-test.*) rmdir "$PROJECT_DIR" 2>/dev/null || true;; esac
  ok "host, project, bot and dev sshd removed"
fi

printf '\nR1 (host up) / R2 (remote project+bot, trust prompt) / R3 (hook reply): PASS\n'
