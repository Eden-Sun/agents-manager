#!/usr/bin/env bash
# End-to-end smoke test for the injected hooks (SPEC §4.1, appendix B).
#
# Runs the *real* Claude Code and Codex CLIs once each with our hook / notify wiring
# and checks that `agents-managerd hook ...` actually fired with a usable payload.
#
# Two modes:
#   spool (default)  point the hook at a closed port; the payload lands in
#                    <data dir>/bots/<bot>/hook-spool.jsonl. Needs no daemon.
#   live  (--live)   point the hook at a running daemon with a real bot id + token;
#                    the daemon's own capture is checked via GET /api/bots/:id/messages.
#
# Usage:
#   scripts/hook-smoke.sh [--claude-only|--codex-only] [--binary PATH] [--keep]
#   scripts/hook-smoke.sh --live --bot BOT_ID --bot-token T [--ui-token T] [--port N]
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REAL_DATA_DIR="${AM_DATA_DIR:-$HOME/.config/agents-manager}"
MODE=spool; RUN_CLAUDE=1; RUN_CODEX=1; BINARY=""; KEEP=0
BOT_ID=""; BOT_TOKEN=""; UI_TOKEN=""; PORT=""

while [ $# -gt 0 ]; do
  case "$1" in
    --live)        MODE=live; shift ;;
    --claude-only) RUN_CODEX=0; shift ;;
    --codex-only)  RUN_CLAUDE=0; shift ;;
    --binary)      BINARY="$2"; shift 2 ;;
    --bot)         BOT_ID="$2"; shift 2 ;;
    --bot-token)   BOT_TOKEN="$2"; shift 2 ;;
    --ui-token)    UI_TOKEN="$2"; shift 2 ;;
    --port)        PORT="$2"; shift 2 ;;
    --keep)        KEEP=1; shift ;;
    -h|--help)     sed -n '2,18p' "$0"; exit 0 ;;
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

if [ -z "$BINARY" ]; then
  for c in "$REPO_ROOT/target/debug/agents-managerd" "$REPO_ROOT/target/release/agents-managerd" "$(command -v agents-managerd 2>/dev/null)"; do
    [ -n "$c" ] && [ -x "$c" ] && { BINARY="$c"; break; }
  done
fi
[ -n "$BINARY" ] || die "agents-managerd not built; run 'cargo build' or pass --binary"

if [ "$MODE" = "live" ]; then
  [ -n "$BOT_ID" ] && [ -n "$BOT_TOKEN" ] || die "--live needs --bot and --bot-token"
  [ -n "$PORT" ] || PORT=7788
  if [ -z "$UI_TOKEN" ] && [ -r "$REAL_DATA_DIR/ui-token" ]; then
    UI_TOKEN="$(tr -d '[:space:]' < "$REAL_DATA_DIR/ui-token")"
  fi
  [ -n "$UI_TOKEN" ] || die "--live needs a UI token to read back messages"
  DATA_DIR="$REAL_DATA_DIR"
else
  # A throwaway data dir keeps the real bots' spools untouched. The hook child reads
  # AM_DATA_DIR, and the agent CLIs pass their environment straight through.
  DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/am-hook-smoke.XXXXXX")"
  BOT_ID="${BOT_ID:-smoke-$(python3 -c 'import uuid;print(uuid.uuid4().hex[:12])')}"
  BOT_TOKEN="${BOT_TOKEN:-smoke-token}"
  # Nothing listens here, so every hook call must fall through to the spool.
  PORT=45999
  for p in 45999 45998 45997; do
    nc -z 127.0.0.1 "$p" >/dev/null 2>&1 || { PORT="$p"; break; }
  done
fi
export AM_DATA_DIR="$DATA_DIR"

SPOOL="$DATA_DIR/bots/$BOT_ID/hook-spool.jsonl"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/am-hook-smoke-work.XXXXXX")"

printf '\033[1mhook smoke test\033[0m\n'
info "mode      $MODE"
info "binary    $BINARY"
info "bot       $BOT_ID  port $PORT"
info "data dir  $DATA_DIR"
info "workdir   $WORK"

cleanup() {
  if [ "$KEEP" = "1" ]; then
    info "kept $WORK and $DATA_DIR"
  else
    rm -rf "$WORK"
    [ "$MODE" = "spool" ] && rm -rf "$DATA_DIR"
  fi
}
trap cleanup EXIT

pyq() { python3 -c 'import sys,json;d=json.load(sys.stdin);a=sys.argv[2:];print(eval(sys.argv[1],{"d":d,"a":a,"json":json,"len":len,"set":set}))' "$@"; }

spool_lines() { [ -s "$SPOOL" ] && wc -l < "$SPOOL" | tr -d ' ' || echo 0; }

live_assistant_count() {
  curl -s --max-time 20 -H "X-AM-Token: $UI_TOKEN" \
    "http://127.0.0.1:$PORT/api/bots/$BOT_ID/messages?limit=500" \
    | pyq 'len([m for m in d["messages"] if m["role"]=="assistant"])' 2>/dev/null || echo 0
}

before_count() { if [ "$MODE" = "live" ]; then live_assistant_count; else spool_lines; fi; }

# $1 = label, $2 = count before, $3 = python predicate over one spool payload dict
check_after() {
  local label="$1" before="$2" pred="${3:-True}" after
  if [ "$MODE" = "live" ]; then
    local deadline=$(( $(date +%s) + 20 ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
      after="$(live_assistant_count)"
      [ "$after" -gt "$before" ] && break
      sleep 1
    done
    if [ "${after:-0}" -gt "$before" ]; then
      pass "$label daemon recorded an assistant message ($before -> $after)"
    else
      fail "$label daemon recorded an assistant message" "still $before; check the daemon log"
    fi
  else
    after="$(spool_lines)"
    if [ "$after" -le "$before" ]; then
      fail "$label spool grew" "no new line in $SPOOL (hook never ran?)"
      return
    fi
    pass "$label spool grew ($before -> $after)"
    local ok
    ok="$(python3 -c '
import json,sys
lines=[json.loads(l) for l in open(sys.argv[1]) if l.strip()]
new=lines[int(sys.argv[2]):]
pred=sys.argv[3]
hits=[b for b in new if eval(pred, {"b":b,"p":b.get("payload",{})})]
print("OK" if hits else "NONE")
print(json.dumps(new[-1], ensure_ascii=False)[:400])
' "$SPOOL" "$before" "$pred")"
    if [ "$(printf '%s' "$ok" | head -1)" = "OK" ]; then
      pass "$label payload shape"
    else
      fail "$label payload shape" "no spooled line matched: $pred"
    fi
    info "last line: $(printf '%s' "$ok" | tail -1)"
  fi
}

# $1 = label, $2 = before count, $3 = predicate, $4 = CLI exit code, $5 = CLI stderr file
# A non-zero CLI exit with no hook activity is the CLI's problem (auth, quota,
# network), not the hook's — report it as SKIP so it cannot mask a real failure.
check_or_skip() {
  if [ "$4" != "0" ] && [ "$(before_count)" = "$2" ]; then
    skip "$1" "the $1 CLI exited $4 without firing a hook: $(tail -c 200 "$5" | tr '\n' ' ')"
  else
    check_after "$1" "$2" "$3"
  fi
}

# ---------------------------------------------------------------- claude

head1 "claude"
if [ "$RUN_CLAUDE" = "0" ]; then
  skip "claude" "--codex-only"
elif ! command -v claude >/dev/null; then
  skip "claude" "claude CLI not on PATH"
else
  SETTINGS="$WORK/claude-settings.json"
  python3 -c '
import json, sys
out, binary, bot, token, port = sys.argv[1:6]
cmd = " ".join([binary, "hook", "claude", "--bot", bot, "--token", token, "--port", port])
entry = [{"hooks": [{"type": "command", "command": cmd}]}]
json.dump({"hooks": {"SessionStart": entry, "Stop": entry}}, open(out, "w"), indent=2)
' "$SETTINGS" "$BINARY" "$BOT_ID" "$BOT_TOKEN" "$PORT"
  info "settings  $SETTINGS"

  BEFORE="$(before_count)"
  # CLAUDE_CODE_CHILD_SESSION / CLAUDECODE leak in when this runs inside Claude Code
  # and suppress the transcript (appendix A); drop them.
  ( cd "$WORK" && env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE \
      claude -p "Reply with exactly PONG" --settings "$SETTINGS" ) \
    > "$WORK/claude.out" 2> "$WORK/claude.err"
  EC=$?
  info "claude exit $EC, stdout: $(head -c 120 "$WORK/claude.out" | tr '\n' ' ')"
  [ -s "$WORK/claude.err" ] && info "stderr: $(head -c 200 "$WORK/claude.err" | tr '\n' ' ')"
  check_or_skip claude "$BEFORE" 'b.get("provider")=="claude" and ("prompt_id" in p or "session_id" in p)' "$EC" "$WORK/claude.err"
fi

# ---------------------------------------------------------------- codex

head1 "codex"
if [ "$RUN_CODEX" = "0" ]; then
  skip "codex" "--claude-only"
elif ! command -v codex >/dev/null; then
  skip "codex" "codex CLI not on PATH"
else
  NOTIFY="$(python3 -c '
import json,sys
print("notify=" + json.dumps([sys.argv[1],"hook","codex","--bot",sys.argv[2],"--token",sys.argv[3],"--port",sys.argv[4]]))
' "$BINARY" "$BOT_ID" "$BOT_TOKEN" "$PORT")"
  info "notify    $NOTIFY"

  BEFORE="$(before_count)"
  ( cd "$WORK" && env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE \
      codex exec -c "$NOTIFY" --skip-git-repo-check "Reply with exactly PONG" ) \
    > "$WORK/codex.out" 2> "$WORK/codex.err"
  EC=$?
  info "codex exit $EC, stdout tail: $(tail -c 160 "$WORK/codex.out" | tr '\n' ' ')"
  [ -s "$WORK/codex.err" ] && info "stderr: $(tail -c 200 "$WORK/codex.err" | tr '\n' ' ')"
  check_or_skip codex "$BEFORE" 'b.get("provider")=="codex" and p.get("type")=="agent-turn-complete"' "$EC" "$WORK/codex.err"
fi

# ---------------------------------------------------------------- summary

head1 "summary"
printf '  %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" = "0" ] || exit 1
exit 0
