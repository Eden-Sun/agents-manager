//! What a pane needs before an agent starts: hooks, shims, skills, persona and argv.

use crate::capabilities::Db;
use super::*;
use super::start::ports::ShareSessionRepo;
use super::s6_ports::SetupShareServices;

/// The hook / statusLine command line for a *local* bot. The token is deliberately **not**
/// on the argv (issue #43: `ps` shows every user the full command line, and the statusLine
/// runs on every redraw); the subcommands read `AM_HOOK_TOKEN` from the pane env instead.
fn hook_cmd_parts(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath + crate::capabilities::ListenPort), bot: &db::Bot, provider: &str) -> Vec<String> {
    hook_cmd_parts_for(&app.exe().to_string_lossy(), app.port(), &bot.id, provider, &app.data_dir().to_string_lossy())
}

/// `--data-dir` 寫死在 argv 裡：pane env 只保護這顆 daemon 新開的 pane，舊 pane 的 env 換不掉，
/// 但 hook.sh／設定檔每次啟動都重寫，spool 才不會跑去別顆 daemon 的目錄（sol 複審二輪）。
fn hook_cmd_parts_for(exe: &str, port: u16, bot_id: &str, provider: &str, data_dir: &str) -> Vec<String> {
    vec![
        exe.into(),
        "hook".into(),
        provider.into(),
        "--bot".into(),
        bot_id.into(),
        "--port".into(),
        port.to_string(),
        "--data-dir".into(),
        data_dir.into(),
    ]
}

/// `agents-managerd hook claude …` → `agents-managerd statusline …`（同一套旗標）。
fn statusline_parts(mut hook: Vec<String>) -> Vec<String> {
    hook[1] = "statusline".into();
    hook.remove(2);
    hook
}

/// What goes in `hook.sh`'s third argv slot. The script ignores it; the real `hook_token` is
/// never put on a remote command line (review 2026-09-12 #8).
pub const REMOTE_TOKEN_SLOT: &str = "-";

/// SPEC §11.4 — the POSIX sh hook for remote hosts. Payload goes to the bot's spool; state goes
/// to this machine's herdr (`pane report-agent`), whose event makes the daemon drain the spool (§11.4.3).
///
/// 根目錄是參數（`remote_hook_sh`）：隔離實例的事件要寫進自己的 `instances/<slug>`，否則會落進正式實例的
/// spool，而它自己的 scanner 永遠看不到（sol 三輪）。正式實例（`REMOTE_ROOT`）的 spool 路徑與行為不變。
pub fn remote_hook_sh(root: &str) -> String {
    REMOTE_HOOK_SH_TEMPLATE.replace("__AM_REMOTE_ROOT__", root)
}

const REMOTE_HOOK_SH_TEMPLATE: &str = r#"#!/bin/sh
# 子 agent 的 pane（帶 AM_CHILD_OF）繼承的是母 bot 的身份：不送、也不 spool（#1004 本機、#1031 遠端）。
[ -z "${AM_CHILD_OF:-}" ] || exit 0
PROVIDER="$1"; BOT="$2"; TOKEN="$3"; shift 3
LIMIT=1048576
DIR="$HOME/__AM_REMOTE_ROOT__/bots/$BOT"
# spool 裡是完整的 hook payload（prompt、工具輸入、回覆）：目錄 0700、檔案 0600，不交給那台機器的
# umask 決定（issue #494）。使用者自己的 statusLine 指令跑之前會還原，不改它建檔的權限。
AM_UMASK=$(umask)
umask 077
mkdir -p "$DIR" 2>/dev/null
# Argument 3 is the token slot. It is never used here (the spool file is already only ours to
# read), and the daemon passes `-` in it: a real token on codex's argv would be visible to
# every user of the host through `ps`.
: "$TOKEN"

# v4.0 statusLine mode: the rate limits arrive on every repaint, so they go to a single-slot
# file the daemon picks up with the next drain (§11.4.5) — never the spool, which is a queue.
# Then run the user's own statusLine command on the same input so the pane looks unchanged.
if [ "$PROVIDER" = "statusline" ]; then
  INPUT=$(head -c $LIMIT)
  case "$INPUT" in
    '{}'|'') ;;
    '{'*)
      printf '{"hook_event_name":"StatusLine",%s' "${INPUT#\{}" > "$DIR/hook-status.json.tmp" 2>/dev/null \
        && mv -f "$DIR/hook-status.json.tmp" "$DIR/hook-status.json" 2>/dev/null ;;
  esac
  CFG="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json"
  CMD=""
  if [ -f "$CFG" ]; then
    if command -v jq >/dev/null 2>&1; then
      CMD=$(jq -r 'if (.statusLine.type // "command") == "command" then (.statusLine.command // empty) else empty end' "$CFG" 2>/dev/null)
    elif command -v python3 >/dev/null 2>&1; then
      CMD=$(python3 -c 'import json,sys
s=(json.load(open(sys.argv[1])).get("statusLine") or {})
print(s.get("command","") if s.get("type","command")=="command" else "")' "$CFG" 2>/dev/null)
    fi
  fi
  case "$CMD" in *"hook.sh statusline"*|*"agents-managerd statusline"*) CMD="" ;; esac
  if [ -n "$CMD" ]; then umask "$AM_UMASK"; printf '%s' "$INPUT" | sh -c "$CMD" 2>/dev/null; fi
  exit 0
fi
if [ "$PROVIDER" = "codex" ] && [ "$#" -gt 0 ]; then PAYLOAD="$1"; else PAYLOAD=$(head -c $LIMIT); fi
LEN=$(printf '%s' "$PAYLOAD" | wc -c | tr -d ' ')
if { [ "$PROVIDER" != "codex" ] || [ "$#" -eq 0 ]; } && [ "$LEN" -ge "$LIMIT" ]; then TRUNC=true; else TRUNC=false; fi
# The payload is spliced into JSON verbatim, so it must BE valid JSON: empty stdin becomes
# null and anything that is not an object is wrapped as a string, otherwise the daemon would
# reject the body and the line would sit in the spool forever.
# A cut-off object still starts with `{`. Splicing it raw makes the whole spool line invalid
# JSON, and drain drops it — `truncated:true` never gets read (#653). Same wrap as a non-object.
am_raw() {
  ESC=$(printf '%s' "$1" | tr -d '\015' | tr '\011' ' ' | tr -d '\000-\010\013\014\016-\037' \
       | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{printf "%s\\n", $0}')
  printf '{"raw":"%s"}' "$ESC"
}
# A remote spool line must stay valid UTF-8, single-line JSON. Parse the complete input so leading
# JSON whitespace, malformed objects, and truncated objects cannot bypass the normalizer.
if command -v python3 >/dev/null 2>&1; then
  NORMALIZED=$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys
raw=sys.stdin.buffer.read()
if not raw:
    sys.stdout.write("null")
    raise SystemExit
text=raw.decode("utf-8","replace")
def reject_constant(value):
    raise ValueError("non-standard JSON constant")
def finite_float(value):
    import math
    result=float(value)
    if not math.isfinite(result):
        raise ValueError("non-finite JSON number")
    return result
if sys.argv[1]=="true":
    value={"raw":text}
else:
    try:
        value=json.loads(text,parse_constant=reject_constant,parse_float=finite_float)
        if not isinstance(value,dict):
            value={"raw":text}
    except Exception:
        value={"raw":text}
def clean(v):
    if isinstance(v,str):
        out=[]
        i=0
        while i<len(v):
            cp=ord(v[i])
            if 0xd800<=cp<=0xdbff:
                if i+1<len(v) and 0xdc00<=ord(v[i+1])<=0xdfff:
                    low=ord(v[i+1])
                    out.append(chr(0x10000+((cp-0xd800)<<10)+(low-0xdc00)))
                    i+=2
                    continue
                out.append("\ufffd")
            elif 0xdc00<=cp<=0xdfff:
                out.append("\ufffd")
            else:
                out.append(v[i])
            i+=1
        return "".join(out)
    if isinstance(v,list):
        return [clean(x) for x in v]
    if isinstance(v,dict):
        return {clean(k):clean(x) for k,x in v.items()}
    return v
safe=clean(value)
sys.stdout.buffer.write(json.dumps(safe,separators=(",",":"),ensure_ascii=False).encode("utf-8"))' "$TRUNC" 2>/dev/null)
  [ -n "$NORMALIZED" ] && PAYLOAD="$NORMALIZED"
else
  # Without Python, keep the prior POSIX path but recognize objects preceded by JSON whitespace.
  if [ -z "$PAYLOAD" ]; then
    PAYLOAD=null
  elif [ "$TRUNC" = true ]; then
    PAYLOAD=$(am_raw "$PAYLOAD")
  else
    LEADING=${PAYLOAD%%[![:space:]]*}
    REST=${PAYLOAD#"$LEADING"}
    case "$REST" in
      '{'*) ;;
      *) PAYLOAD=$(am_raw "$PAYLOAD") ;;
    esac
    # JSON whitespace may span physical lines; JSONL stores one complete event per line.
    PAYLOAD=$(printf '%s' "$PAYLOAD" | tr -d '\011\012\015')
  fi
  if command -v iconv >/dev/null 2>&1 && ! printf '%s' "$PAYLOAD" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1; then
    PAYLOAD='{"raw":"invalid UTF-8 hook payload"}'
  fi
fi
# issue #753: a Stop hook carries no user message, and the daemon cannot read this machine's
# transcript, so a Stop that arrives after the daemon's terminal fallback closed the turn could not
# be shown to belong to that turn. Read the last human message out of the LOCAL transcript here and
# carry it as `agm_user_text` (plus `agm_origin_kind`, issue #754: who started the turn, `origin.kind`
# of the newest entry that has one; the daemon stores the carried text only when it is `human`).
# 2026-10-02: the same read carries the finished `AskUserQuestion` answers of the last 512 KiB as
# `agm_asks` (Stop and StopFailure). issue #1759: claude compacting its conversation sends no Stop, only
# `SessionStart` with `source: "compact"`, so the newest `compact_boundary` goes out as `agm_compact`.
# All of it runs in ONE python3 and reads the transcript ONCE. Best effort: no python3, an unreadable
# transcript, or a stage that finds nothing leaves the payload as it was (the daemon then falls back as before).
if [ "$PROVIDER" = "claude" ] && [ "$TRUNC" = false ]; then
  case "$PAYLOAD" in
    *'"hook_event_name":"Stop"'*|*'"hook_event_name": "Stop"'*|*'"hook_event_name":"StopFailure"'*|*'"hook_event_name": "StopFailure"'*|*'"source":"compact"'*|*'"source": "compact"'*)
      if command -v python3 >/dev/null 2>&1; then
        WITH=$(printf '%s' "$PAYLOAD" | python3 -c 'import json,os,stat,sys
def _am_transcript(path):
    cfg=os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    proj=os.path.join(os.path.realpath(cfg),"projects")
    nofollow=getattr(os,"O_NOFOLLOW",None)
    directory=getattr(os,"O_DIRECTORY",0)
    if not isinstance(path,str) or not os.path.isabs(path) or nofollow is None:
        raise SystemExit
    project_stat=os.lstat(proj)
    if stat.S_ISLNK(project_stat.st_mode) or not stat.S_ISDIR(project_stat.st_mode) or os.path.islink(path):
        raise SystemExit
    dir_flags=os.O_RDONLY|directory|nofollow|os.O_NONBLOCK
    dirfd=os.open(proj,dir_flags)
    try:
        opened=os.fstat(dirfd)
        if not stat.S_ISDIR(opened.st_mode) or (opened.st_dev,opened.st_ino)!=(project_stat.st_dev,project_stat.st_ino):
            raise SystemExit
        absolute=os.path.abspath(path)
        ancestor=absolute
        rel=None
        while True:
            if os.path.realpath(ancestor)==proj:
                rel=os.path.relpath(absolute,ancestor)
                break
            parent=os.path.dirname(ancestor)
            if parent==ancestor:
                break
            ancestor=parent
        if rel in (None,"", "."):
            raise SystemExit
        parts=rel.split(os.sep)
        if not parts or any(part in ("",".","..") for part in parts):
            raise SystemExit
        for part in parts[:-1]:
            nextfd=os.open(part,dir_flags,dir_fd=dirfd)
            if not stat.S_ISDIR(os.fstat(nextfd).st_mode):
                os.close(nextfd)
                raise SystemExit
            os.close(dirfd)
            dirfd=nextfd
        fd=os.open(parts[-1],os.O_RDONLY|os.O_NONBLOCK|nofollow,dir_fd=dirfd)
    finally:
        os.close(dirfd)
    try:
        st=os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_nlink!=1:
            raise SystemExit
        with os.fdopen(fd,"rb") as f:
            fd=None
            f.seek(max(0,st.st_size-524288))
            return f.read(524288).decode("utf-8","replace")
    finally:
        if fd is not None:
            os.close(fd)
def _am_user(p,data):
    users=[]
    for line in reversed(data.splitlines()):
        try:
            v=json.loads(line)
        except Exception:
            continue
        if isinstance(v,dict) and v.get("type")=="user":
            users.append(v)
    kind=None
    kat=None
    for v in users:
        o=v.get("origin")
        if isinstance(o,dict) and isinstance(o.get("kind"),str):
            kind=o["kind"]
            t=v.get("timestamp")
            kat=t if isinstance(t,str) else None
            break
    echo=("<command-name>","<command-message>","<command-args>","<local-command-","<system-reminder>","Caveat: The messages below")
    for v in users:
        if v.get("isMeta") is True or v.get("isSidechain") is True or v.get("isCompactSummary") is True or v.get("isVisibleInTranscriptOnly") is True:
            continue
        c=(v.get("message") or {}).get("content")
        if isinstance(c,list):
            if any(not isinstance(x,dict) or x.get("type")!="text" for x in c):
                continue
            c="\n".join(x["text"] for x in c if isinstance(x.get("text"),str))
        if not isinstance(c,str) or not c.strip():
            continue
        if c.lstrip().startswith(echo):
            continue
        if len(c)<=65536:
            c.encode("utf-8")  # a lone surrogate would be written back out as an escape serde_json cannot read
            p["agm_user_text"]=c
            if kind is not None:
                p["agm_origin_kind"]=kind
                if kat is not None:
                    p["agm_origin_at"]=kat
            return True
        break
    return False
def _am_asks(p,data):
    asked={}
    out=[]
    def ans(a):
        if isinstance(a,str):
            return a
        if isinstance(a,list):
            xs=[x for x in a if isinstance(x,str)]
            return "、".join(xs) if xs else None
        return None
    for line in data.splitlines():
        try:
            v=json.loads(line)
        except Exception:
            continue
        c=(v.get("message") or {}).get("content") if isinstance(v,dict) else None
        if not isinstance(c,list):
            continue
        for b in c:
            if not isinstance(b,dict):
                continue
            if b.get("type")=="tool_use" and b.get("name")=="AskUserQuestion" and isinstance(b.get("id"),str):
                asked[b["id"]]=b.get("input") or {}
            elif b.get("type")=="tool_result" and b.get("tool_use_id") in asked:
                r=v.get("toolUseResult")
                inp=asked[b["tool_use_id"]]
                a=r.get("answers") if isinstance(r,dict) else None
                if isinstance(a,dict):
                    qs=r.get("questions") if isinstance(r.get("questions"),list) else inp.get("questions")
                    notes=r.get("annotations") if isinstance(r.get("annotations"),dict) else {}
                else:
                    t=b.get("content")
                    if isinstance(t,list):
                        t="\n".join(x.get("text","") for x in t if isinstance(x,dict))
                    t=t if isinstance(t,str) else ""
                    rejected=isinstance(r,str) and "reject" in r.lower()
                    if not (rejected or t.startswith("The user did not answer") or t.startswith("The user doesn"+chr(39)+"t want to proceed")):
                        continue
                    a={}
                    qs=inp.get("questions")
                    notes={}
                items=[]
                for q in (qs if isinstance(qs,list) else []):
                    if not isinstance(q,dict) or not isinstance(q.get("question"),str):
                        continue
                    it={"question":q["question"],"answer":ans(a.get(q["question"]))}
                    if isinstance(q.get("header"),str):
                        it["header"]=q["header"]
                    nt=(notes.get(q["question"]) or {}).get("notes") if isinstance(notes.get(q["question"]),dict) else None
                    if isinstance(nt,str) and nt.strip():
                        it["notes"]=nt.strip()
                    items.append(it)
                if items:
                    out.append({"id":b["tool_use_id"],"at":v.get("timestamp"),"items":items})
    out=out[-20:]
    if out and len(json.dumps(out,ensure_ascii=False).encode("utf-8"))<=262144:  # a lone surrogate raises here: carry nothing rather than a line serde_json cannot read
        p["agm_asks"]=out
        return True
    return False
def _am_compact(p,path):
    found=None
    for line in _am_transcript(path).splitlines():
        try:
            v=json.loads(line)
        except Exception:
            continue
        if not isinstance(v,dict) or v.get("type")!="system":
            continue
        if v.get("subtype")=="compact_boundary":
            m=v.get("compactMetadata") if isinstance(v.get("compactMetadata"),dict) else {}
            found={"idle":False}
            if isinstance(m.get("trigger"),str):
                found["trigger"]=m["trigger"]
            for k,o in (("preTokens","pre_tokens"),("postTokens","post_tokens")):
                if isinstance(m.get(k),int):
                    found[o]=m[k]
        elif found is not None and isinstance(v.get("content"),str) and v["content"].startswith("Compacted while idle"):
            found["idle"]=True
    if found is None:
        return False
    p["agm_compact"]=found
    return True
def _am_try(fn,*args):
    try:
        return fn(*args)
    except (Exception,SystemExit):
        return None
try:
    p=json.loads(sys.stdin.buffer.read().decode("utf-8","replace"))
    ev=p.get("hook_event_name")
    path=p.get("transcript_path") or p.get("transcriptPath")
    changed=False
    if path and ev in ("Stop","StopFailure"):
        data=_am_try(_am_transcript,path)
        if data is not None:
            if ev=="Stop" and _am_try(_am_user,p,data):
                changed=True
            if _am_try(_am_asks,p,data):
                changed=True
    if path and ev=="SessionStart" and p.get("source")=="compact":
        if _am_try(_am_compact,p,path):
            changed=True
    if changed:
        sys.stdout.buffer.write(json.dumps(p,separators=(",",":")).encode("ascii"))
except Exception:
    pass' 2>/dev/null)
        case "$WITH" in '{'*) PAYLOAD="$WITH" ;; esac
      fi ;;
  esac
fi
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# The run this CLI process was started for: `--resume` makes an old and a new process report the
# same session, so this is how the daemon tells their late hooks apart (issue #92). Only id-safe
# characters survive, so an odd value can never break the JSON line.
RUN=$(printf '%s' "${AM_RUN_ID:-}" | tr -cd 'A-Za-z0-9_-')
BODY=$(printf '{"bot_id":"%s","provider":"%s","payload":%s,"received_at":"%s","truncated":%s,"run_id":"%s"}' "$BOT" "$PROVIDER" "$PAYLOAD" "$NOW" "$TRUNC" "$RUN")
# Spool FIRST: the daemon reads this file the moment it sees the state change below, so the
# line has to be there before herdr is told anything.
# One file per event (#652). `printf >> jsonl` is several write()s once the line passes ~4 KiB
# (bash) or splits the newline off (dash). O_APPEND only makes a single write atomic, so two
# hooks interleave and drain drops both lines. A private temp plus rename is one event or none.
SD="$DIR/hook-spool.d"
mkdir -p "$SD" 2>/dev/null
TMP=$(mktemp "$SD/.tmp.XXXXXX" 2>/dev/null) || TMP=""
if [ -n "$TMP" ]; then
  if command -v python3 >/dev/null 2>&1; then
    ID=$(python3 -c 'import time,os; print("%d-%d" % (time.time_ns(), os.getpid()))' 2>/dev/null)
  else
    ID=""
  fi
  [ -n "$ID" ] || ID="$(date +%s)-$$-$(basename "$TMP")"
  if printf '%s\n' "$BODY" > "$TMP" 2>/dev/null; then
    mv -f "$TMP" "$SD/$ID.json" 2>/dev/null || rm -f "$TMP"
  else
    rm -f "$TMP"
  fi
fi

# ---- tell this host's herdr what the agent is doing (SPEC §11.4.2)
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
HERDR="${AM_REAL_HERDR:-}"
if [ -z "$HERDR" ] || [ ! -x "$HERDR" ]; then HERDR=$(command -v herdr 2>/dev/null); fi
if [ -z "$HERDR" ]; then printf '%s herdr not found; spooled only\n' "$NOW" >> "$DIR/hook.log"; exit 0; fi
am_herdr() {
  if [ -n "${HERDR_SESSION:-}" ]; then "$HERDR" --session "$HERDR_SESSION" "$@"; else "$HERDR" "$@"; fi
}
# `--seq` is monotonic per (pane, source) and anything <= the last one is dropped, so this has
# to keep climbing: nanoseconds where python3 exists (herdr's own integration does the same),
# else <epoch seconds><3-digit counter>, which is still seconds*1000 + n.
am_seq() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(time.time_ns())' 2>/dev/null && return 0
  fi
  _n=0
  [ -f "$DIR/hook-seq" ] && _n=$(cat "$DIR/hook-seq" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$(( (_n + 1) % 1000 ))
  printf '%s\n' "$_n" > "$DIR/hook-seq" 2>/dev/null
  printf '%s%03d\n' "$(date +%s)" "$_n"
}
# Read a top-level string field from the normalized JSON object. Missing, nested, or wrong-type
# fields produce no argument; user-controlled nested tool data cannot masquerade as event metadata.
am_str() {
  command -v python3 >/dev/null 2>&1 || return 0
  printf '%s' "$PAYLOAD" | python3 -c 'import json,sys
try:
    p=json.loads(sys.stdin.buffer.read().decode("utf-8"))
    v=p.get(sys.argv[1]) if isinstance(p,dict) else None
    if isinstance(v,str):
        sys.stdout.buffer.write(v.encode("utf-8"))
except Exception:
    pass' "$1" 2>/dev/null
}
# The authoritative classification lives in the daemon. This parsed top-level hint only wakes
# the host promptly; missing fields or invalid payloads leave the spool intact for normal polling.
STATE=""
START=0
if command -v python3 >/dev/null 2>&1; then
  EVENT=$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys
try:
    p=json.loads(sys.stdin.buffer.read().decode("utf-8"))
    if not isinstance(p,dict):
        raise SystemExit
    provider=sys.argv[1]
    if provider=="claude":
        event=p.get("hook_event_name")
        if event=="SessionStart":
            print("session")
        elif event in ("Stop","StopFailure") and p.get("stop_hook_active") is not True:
            print("idle")
    elif provider=="codex":
        if p.get("type")=="agent-turn-complete" or (p.get("hook_event_name")=="Stop" and p.get("stop_hook_active") is not True):
            print("idle")
    elif provider=="grok":
        event=p.get("hookEventName",p.get("hook_event_name"))
        if isinstance(event,str):
            event=event.lower()
            if event in ("session_start","sessionstart"):
                print("session")
            elif event=="stop" and p.get("reason","end_turn")=="end_turn" and p.get("stopHookActive",p.get("stop_hook_active")) is not True:
                print("idle")
    elif provider=="agy":
        # agy 的 payload 沒有事件名：遠端 dispatcher（`agy_remote`）先放進 `hookEventName`。
        event=p.get("hookEventName")
        if event in ("SessionStart","PreInvocation"):
            print("session")
        elif event=="Stop" and p.get("fullyIdle") is not False:
            print("idle")
except Exception:
    pass' "$PROVIDER" 2>/dev/null)
  case "$EVENT" in
    session) START=1 ;;
    idle) STATE=idle ;;
  esac
fi
[ "$START" = 1 ] || [ -n "$STATE" ] || exit 0
SID=$(am_str session_id)
[ -n "$SID" ] || SID=$(am_str sessionId)
[ -n "$SID" ] || SID=$(am_str thread-id)
[ -n "$SID" ] || SID=$(am_str conversationId)
TP=$(am_str transcript_path)
[ -n "$TP" ] || TP=$(am_str transcriptPath)
SEQ=$(am_seq)
if [ "$START" = 1 ]; then
  # herdr 0.8.2 emits no event for this and does not surface it; best effort, for herdr's own
  # bookkeeping. The daemon still learns the session id from the spooled payload.
  set -- pane report-agent-session "$HERDR_PANE_ID" --source "agents-manager:$BOT" --agent "$PROVIDER" --seq "$SEQ"
else
  # Only ever `idle`: there is no "the agent started" hook, and herdr's own terminal detection
  # already reports `working` / `blocked`. Reporting those here would just fight it.
  set -- pane report-agent "$HERDR_PANE_ID" --source "agents-manager:$BOT" --agent "$PROVIDER" --state "$STATE" --seq "$SEQ"
fi
[ -n "$SID" ] && set -- "$@" --agent-session-id "$SID"
[ -n "$TP" ] && set -- "$@" --agent-session-path "$TP"
# Herdr's client-side RPC deadline is 15 s, longer than provider hook budgets. The durable spool
# is already visible; let the report wake the drain in the background so a slow socket cannot
# hold Claude, Codex, or Grok open. The scanner and terminal poller cover a missed report.
am_herdr "$@" </dev/null >/dev/null 2>>"$DIR/hook.log" &
exit 0
"#;

pub const GROK_HOOKS_FILE: &str = "agents-manager.json";
pub const GROK_DISPATCH_SH: &str = "grok-hook.sh";

/// grok 會合併 `hooks/*.json` 全部檔案，所以每個實例一份檔、各自指向自己的 dispatcher，
/// 不會互相覆寫。正式實例沿用原檔名。
pub fn grok_hooks_file(instance: Option<&str>) -> String {
    match instance {
        Some(slug) => format!("agents-manager-{slug}.json"),
        None => GROK_HOOKS_FILE.to_string(),
    }
}

/// 每個實例的 dispatcher 都會被 grok 叫到；只處理自己實例的 pane（pane env 的 `AM_INSTANCE`）。
/// 正式實例的 pane 沒有這個變數，所以舊 pane 照舊歸正式實例。
fn instance_gate(instance: Option<&str>) -> String {
    match instance {
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n", sh_quote(slug)),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || exit 0\n".to_string(),
    }
}

/// Dispatcher installed on remote hosts: forwards to the per-bot `hook.sh` (SPEC §11.4).
/// 根目錄與實例閘門都跟著實例走：兩顆 daemon 管同一台遠端、bot id 又一樣時，不能共用同一份 spool。
///
/// 第三個參數是 [`REMOTE_TOKEN_SLOT`]，跟 claude／codex 一樣（issue #43、#495）：`hook.sh` 根本不讀它
/// （`: "$TOKEN"`），而真的 token 上了 argv 就等於印在那台機器的 `ps` 上。pane env 的 `AM_HOOK_TOKEN`
/// 仍然是身份的來源，上面那個 `-n` 檢查也還在——只是不再把它的值傳下去。
pub fn remote_grok_dispatch_sh(root: &str, instance: Option<&str>) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Installed by the daemon; no-op outside daemon panes.\n# 子 agent 的 pane 繼承母 bot 的身份，這裡就不送（#1031，同 hook.sh）。\n[ -z \"${{AM_CHILD_OF:-}}\" ] || exit 0\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n{gate}H=\"$HOME/{root}/bots/$AM_BOT_ID/hook.sh\"\n[ -x \"$H\" ] || exit 0\nexec \"$H\" grok \"$AM_BOT_ID\" {slot}\n",
        gate = instance_gate(instance),
        slot = REMOTE_TOKEN_SLOT,
    )
}

/// 沒有 `--token`（issue #43、#495）：`hook_cmd::hook_token()` 在旗標是空的時候本來就會讀
/// `AM_HOOK_TOKEN`，而上面那行已經確定它有值。claude／codex 的 `hook_cmd_parts` 也從來沒帶過它。
pub(super) fn local_grok_dispatch_sh(exe: &str, data_dir: &str, instance: Option<&str>) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Rewritten by the daemon on every grok bot start; no-op outside daemon panes.\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n{gate}exec {exe} hook grok --bot \"$AM_BOT_ID\" --port \"${{AM_PORT:-7788}}\" --data-dir {data_dir}\n",
        gate = instance_gate(instance),
        exe = sh_quote(exe),
        data_dir = sh_quote(data_dir)
    )
}

pub fn grok_home(env: &Value, home: &str) -> String {
    let configured = env.get("GROK_HOME")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(path) = configured {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() && path.starts_with('/') { "/".into() } else { trimmed.into() }
    } else {
        format!("{home}/.grok")
    }
}

fn install_local_grok_hook(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ExePath + crate::hosts::HostInstance), env: &Value) -> anyhow::Result<()> {
    super::grok_hook::install_local(app, env).map(|_| ())
}

/// Remote grok bot: the same two files, written over ssh after `install_remote_hook`.
/// 回傳 hooks 檔有沒有真的改寫（內容相同只換 dispatcher、不動 hooks 檔）。
async fn install_remote_grok_hook(conn: &HostConn, env: &Value, instance: Option<&str>) -> anyhow::Result<bool> {
    let home = conn.home().await?;
    let root = crate::hosts::remote_root_for(instance);
    let dispatcher = format!("{home}/{root}/{GROK_DISPATCH_SH}");
    let hooks_dir = format!("{}/hooks", grok_home(env, &home));
    let read = conn.ssh_exec(&remote_grok_read_script(&hooks_dir, &grok_hooks_file(instance))).await?;
    let existing = parse_remote_grok_file(&read)?;
    let guard = remote_kill_guard_cmd(&format!("{home}/{root}/{}", am_base::kill_guard::REMOTE_KILL_GUARD_SH), "grok");
    let merged = super::grok_hook::hooks_json_merged_remote(existing.as_ref().map(|(text, _)| text.as_str()), &dispatcher, &guard);
    let expected = existing.as_ref().map(|(_, hash)| hash.as_str());
    let same = existing.as_ref().is_some_and(|(text, _)| text.trim_end() == merged.trim_end());
    let script = format!(
        "set -e\numask 077\nW={w}\nmkdir -p \"$(dirname \"$W\")\"\nWT=$(mktemp \"$W.tmp.XXXXXX\")\ncat > \"$WT\" <<'AM_WRAP_EOF'\n{wrap}AM_WRAP_EOF\nchmod 700 \"$WT\"\nmv -f \"$WT\" \"$W\"\nG={g}\nmkdir -p \"$G\"\n[ ! -L \"$G\" ] && [ -d \"$G\" ] || {{ printf 'AM_GROK_UNTRUSTED\\n'; exit 0; }}\nchmod 700 \"$G\"\ncd \"$G\"\nF={file}\n{guard}\nT=$(mktemp .agents-manager.XXXXXX)\ntrap 'rm -f \"$T\"' EXIT HUP INT TERM\ncat > \"$T\" <<'AM_JSON_EOF'\n{json}\nAM_JSON_EOF\nchmod 600 \"$T\"\n{guard}\n{publish}\nprintf 'AM_GROK_INSTALLED\\n'\n",
        w = sh_quote(&dispatcher),
        wrap = remote_grok_dispatch_sh(&root, instance),
        g = sh_quote(&hooks_dir),
        file = sh_quote(&grok_hooks_file(instance)),
        guard = remote_grok_expect_script(&grok_hooks_file(instance), expected),
        publish = if same {
            "rm -f \"$T\"".to_string()
        } else if expected.is_some() {
            format!("mv -f \"$T\" \"$F\"")
        } else {
            "ln \"$T\" \"$F\" && rm -f \"$T\" || { printf 'AM_GROK_CHANGED\\n'; exit 0; }".into()
        },
        json = merged,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_GROK_INSTALLED") {
        anyhow::bail!("remote grok hook install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, hooks_dir, changed = !same, "remote grok hook installed");
    Ok(!same)
}

fn remote_grok_read_script(hooks_dir: &str, file: &str) -> String {
    format!(
        r#"G={g}
F="$G"/{f}
am_bad() {{ printf 'AM_GROK_UNTRUSTED\n'; exit 0; }}
[ -L "$G" ] && am_bad
if [ ! -e "$F" ] && [ ! -L "$F" ]; then printf 'AM_GROK_MISSING\n'; exit 0; fi
[ ! -L "$F" ] && [ -f "$F" ] || am_bad
{{
  exec 3< "$F" || am_bad
  if stat -c %Y "$F" >/dev/null 2>&1; then
    a=$(stat -L -c '%d %i %h %s' "$F") || am_bad
    b=$(stat -L -c '%d %i %h %s' /dev/fd/3) || am_bad
  else
    a=$(stat -L -f '%d %i %l %z' "$F") || am_bad
    # macOS 的 /dev/fd/3 是 devfs 節點，%d 回的是 devfs 而不是檔案所在的磁碟，跟 "$F" 永遠對不上；
    # BSD stat 不帶路徑時讀 stdin 的 fd，就是開著的那個檔。
    b=$(stat -L -f '%d %i %l %z' <&3) || am_bad
  fi
  [ "$a" = "$b" ] || am_bad
  set -- $b
  [ "$3" = 1 ] && [ "$4" -le 262144 ] || am_bad
  [ ! -L "$G" ] && [ ! -L "$F" ] || am_bad
  printf 'AM_GROK_FILE\n'
  head -c 262145 <&3 | base64
}} 2>/dev/null
"#,
        g = sh_quote(hooks_dir),
        f = sh_quote(file),
    )
}

fn parse_remote_grok_file(out: &str) -> anyhow::Result<Option<(String, String)>> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let (head, body) = out.split_once('\n').unwrap_or((out, ""));
    if head == "AM_GROK_MISSING" {
        return Ok(None);
    }
    if head != "AM_GROK_FILE" {
        anyhow::bail!("remote grok hooks file is not a safe regular file: {}", out.trim());
    }
    let encoded: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    if bytes.len() > 262_144 {
        anyhow::bail!("remote grok hooks file is too large");
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some((String::from_utf8(bytes)?, hash)))
}

fn remote_grok_expect_script(file: &str, expected: Option<&str>) -> String {
    match expected {
        Some(hash) => format!(
            "F={f}\n[ ! -L \"$F\" ] && [ -f \"$F\" ] || {{ printf 'AM_GROK_CHANGED\\n'; exit 0; }}\nif command -v sha256sum >/dev/null 2>&1; then NOW=$(sha256sum \"$F\" | awk '{{print $1}}'); else NOW=$(shasum -a 256 \"$F\" | awk '{{print $1}}'); fi\n[ \"$NOW\" = {hash} ] || {{ printf 'AM_GROK_CHANGED\\n'; exit 0; }}\n",
            f = sh_quote(file),
            hash = sh_quote(hash),
        ),
        None => format!(
            "F={f}\n[ ! -e \"$F\" ] && [ ! -L \"$F\" ] || {{ printf 'AM_GROK_CHANGED\\n'; exit 0; }}\n",
            f = sh_quote(file),
        ),
    }
}

#[path = "guard_refresh.rs"]
pub mod guard_refresh;

pub struct RemoteHookPaths {
    pub dir: String,
    pub hook_sh: String,
    pub settings: String,
    /// 遠端根目錄的 `kill-guard.sh`（所有 bot 共用，claude／grok／agy 的 PreToolUse 都指向它）。
    pub kill_guard: String,
}

pub async fn remote_bot_dir_for(conn: &HostConn, bot_id: &str, instance: Option<&str>) -> anyhow::Result<RemoteHookPaths> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let home = conn.home().await?;
    let root = format!("{home}/{}", crate::hosts::remote_root_for(instance));
    let dir = format!("{root}/bots/{bot_id}");
    let kill_guard = format!("{root}/{}", am_base::kill_guard::REMOTE_KILL_GUARD_SH);
    Ok(RemoteHookPaths { hook_sh: format!("{dir}/hook.sh"), settings: format!("{dir}/claude-settings.json"), kill_guard, dir })
}

/// SPEC §11.4 — push `hook.sh` (+ `claude-settings.json`) to the remote before `agent.start`.
/// 遠端 claude bot 的 settings：hook 指令指向遠端的 `hook.sh`。受限 bot 再由 `cage_settings` 蓋一次（見 `injected_args`）。
/// PreToolUse（Bash）只交給遠端的 `kill-guard.sh`，不進 `hook.sh` 的 spool。
fn remote_claude_settings(p: &RemoteHookPaths, bot: &db::Bot, time_zone: &str) -> Value {
    let cmd = shell_join(&[p.hook_sh.clone(), "claude".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    let statusline = shell_join(&[p.hook_sh.clone(), "statusline".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    let mut settings = claude_settings(&cmd, &statusline, bot.args().iter().any(|a| a == "--remote-control"), bot.auto_approve != 0, time_zone);
    with_kill_guard(&mut settings, &remote_kill_guard_cmd(&p.kill_guard, "claude"));
    settings
}

/// 遠端 PreToolUse 的指令：`'<遠端根>/kill-guard.sh' <provider>`。
pub fn remote_kill_guard_cmd(path: &str, provider: &str) -> String {
    format!("{} {provider}", sh_quote(path))
}

async fn install_remote_hook(conn: &HostConn, bot: &db::Bot, instance: Option<&str>, time_zone: &str) -> anyhow::Result<RemoteHookPaths> {
    let p = remote_bot_dir_for(conn, &bot.id, instance).await?;
    let settings = remote_claude_settings(&p, bot, time_zone);
    let settings_text = serde_json::to_string_pretty(&settings)?;
    let sig = if bot.kind == "claude" { guard_refresh::guard_sig_for("claude", &p, bot, time_zone, instance) } else { None };
    let sig_line = sig.map(|s| format!("printf '%s' {} > \"$D/{}\"\n", sh_quote(&s), guard_refresh::GUARD_SIG_FILE)).unwrap_or_default();
    let script = format!(
        "set -e\numask 077\nD={dir}\nmkdir -p \"$D\"\nchmod 700 \"$D\"\nchmod go-rwx \"$D\"/* 2>/dev/null || true\ncat > \"$D/hook.sh\" <<'AM_HOOK_EOF'\n{hook}AM_HOOK_EOF\nchmod 700 \"$D/hook.sh\"\ncat > \"$D/claude-settings.json\" <<'AM_SETTINGS_EOF'\n{settings}\nAM_SETTINGS_EOF\nchmod 600 \"$D/claude-settings.json\"\n{sig_line}KG={kg}\nKT=$(mktemp \"$KG.tmp.XXXXXX\")\ncat > \"$KT\" <<'AM_KILL_GUARD_EOF'\n{guard}AM_KILL_GUARD_EOF\nchmod 700 \"$KT\"\nmv -f \"$KT\" \"$KG\"\nprintf 'AM_INSTALLED\\n'\n",
        dir = sh_quote(&p.dir),
        kg = sh_quote(&p.kill_guard),
        guard = am_base::kill_guard::remote_kill_guard_sh(),
        hook = remote_hook_sh(&crate::hosts::remote_root_for(instance)),
        settings = settings_text,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_INSTALLED") {
        anyhow::bail!("remote hook install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, bot = %bot.name, dir = %p.dir, "remote hook installed");
    Ok(p)
}

/// Put the `herdr` shim (SPEC §6.5b) where this bot's pane can reach it; returns the PATH dir.
/// An unwritable host does not block the start: reconcile's descent match still tracks children.
///
/// The `cargo` build-scheduler shim (issue #90) rides along into the **same** dir — one PATH
/// prepend covers both. Its own install is best-effort and never blocks the bot on failure: a bot
/// that cannot get a scheduled `cargo` still gets a working `herdr`, which is what actually gates
/// starting at all.
pub async fn install_shim(app: &(impl crate::capabilities::DataDir + crate::hosts::HostInstance + crate::hosts::HostsAccess + crate::lifecycle::start::ports::ShimInstallPort), bot: &db::Bot, project: &db::Project) -> Option<String> {
    let installed = if project.host == LOCAL_HOST {
        app.bot_dir(&bot.id).and_then(|dir| {
            let bin = app.install_local_herdr_shim(&dir)?;
            if let Err(e) = app.install_local_cargo_shim(&dir) {
                tracing::warn!(bot = %bot.name, error = ?e, "could not install the cargo build-slot shim");
            }
            if let Err(e) = app.install_local_kill_shim(&dir) {
                tracing::warn!(bot = %bot.name, error = ?e, "could not install the pkill/killall guard shim");
            }
            Ok::<_, anyhow::Error>(bin.to_string_lossy().into_owned())
        })
    } else {
        match app.hosts().get(&project.host).await {
            Some(conn) => match remote_bot_dir_for(&conn, &bot.id, app.instance().as_deref()).await {
                Ok(p) => {
                    let dir = app.install_remote_herdr_shim(&conn, &p.dir).await;
                    if dir.is_ok() {
                        if let Err(e) = app.install_remote_cargo_shim(&conn, &p.dir).await {
                            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the remote cargo build-slot shim");
                        }
                        if let Err(e) = app.install_remote_kill_shim(&conn, &p.dir).await {
                            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the remote pkill/killall guard shim");
                        }
                    }
                    dir
                }
                Err(e) => Err(e),
            },
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    match installed {
        Ok(dir) => Some(dir),
        Err(e) => {
            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the herdr shim");
            None
        }
    }
}

/// 權限參數一律照 `auto_approve` 明講，開新對話、resume、fork、子 agent 重啟都走這一份。
///
/// codex 0.160.0 起 `resume` 會恢復這段對話上次存下的權限（openai/codex#49160：在 session 裡用
/// `/permissions` 切成 Full Access 之後，不帶權限參數的 `codex resume <id>` 回來就是 YOLO），除非啟動時
/// 明確覆寫（issue #778，0.160.0 拋棄式目錄實測：`-s`／`-c sandbox_mode=…` 都算覆寫，`/status` 回到
/// workspace）。所以沒開 auto_approve 的 codex 也要帶非 yolo 的 sandbox。用 `-c` 不用 `-s`：
/// 使用者在 bot 參數裡自己寫的 `-s` 比 `-c` 優先，也不會撞 clap 的「旗標重複」。approval policy 不碰，
/// 照使用者的 config.toml。
pub fn permission_args(kind: &str, auto_approve: bool) -> Vec<String> {
    match (kind, auto_approve) {
        ("claude", true) => vec!["--dangerously-skip-permissions".into()],
        ("codex", true) => vec!["--yolo".into()],
        ("grok", true) => vec!["--always-approve".into()],
        ("agy", true) => vec!["--dangerously-skip-permissions".into()],
        ("codex", false) => vec!["-c".into(), "sandbox_mode=\"workspace-write\"".into()],
        _ => Vec::new(),
    }
}

/// Daemon-injected CLI args that go *before* the bot's own; remote projects also upload the hook (§11.4).
pub async fn injected_args(app: &(impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ExePath + crate::capabilities::ListenPort + crate::hosts::HostInstance + crate::hosts::HostsAccess + ShareSessionRepo + SetupShareServices), bot: &db::Bot, project: &db::Project, env: &Value) -> anyhow::Result<Vec<String>> {
    if !matches!(bot.kind.as_str(), "claude" | "codex" | "grok" | "agy") {
        anyhow::bail!("unknown bot kind {}", bot.kind);
    }
    let restricted = app.caged_workspace(&bot.id).await?;
    let mut out = if restricted.is_some() { Vec::new() } else { permission_args(&bot.kind, bot.auto_approve != 0) };
    if bot.inject_hooks == 0 && restricted.is_none() {
        return Ok(out);
    }
    if restricted.is_some() && bot.kind != "claude" {
        anyhow::bail!("restricted share bot must be a claude bot");
    }

    let stop_hook_enabled = bot.kind == "codex" && app.cfg().get().await.codex.stop_hook;

    if project.host != LOCAL_HOST {
        let conn = app
            .hosts()
            .get(&project.host)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown host `{}`", project.host))?;
        let time_zone = app.cfg().agents_fresh().await.time_zone().to_string();
        let paths = install_remote_hook(&conn, bot, app.instance().as_deref(), &time_zone).await?;
        if let Some(ws) = &restricted {
            let fence = app.hosts().fence(&project.host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{}`", project.host))?;
            let mut settings = remote_claude_settings(&paths, bot, &time_zone);
            app.cage_settings(&mut settings, ws, env);
            let bytes = serde_json::to_vec_pretty(&settings)?;
            app.write_remote_private_file(&fence, &paths.dir, "claude-settings.json", &bytes).await?;
        }
        let hook_args: Vec<String> = match bot.kind.as_str() {
            "claude" => vec!["--settings".into(), paths.settings, "--verbose".into()],
            "codex" => {
                let parts = vec![paths.hook_sh, "codex".to_string(), bot.id.clone(), REMOTE_TOKEN_SLOT.to_string()];
                let legacy = vec!["-c".into(), format!("notify={}", serde_json::to_string(&parts)?)];
                let root = crate::hosts::remote_root_for(app.instance().as_deref());
                let paths = match tokio::time::timeout(REMOTE_STOP_HOOK_BUDGET, remote_codex_paths(env, &conn, &root)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("remote Codex home lookup timed out")),
                };
                if let Ok((home, _)) = &paths {
                    am_base::codex_kill_guard::observe_remote(&conn, &project.host, home, &root, REMOTE_STOP_HOOK_BUDGET).await;
                }
                if stop_hook_enabled {
                    let ready = match tokio::time::timeout(REMOTE_STOP_HOOK_BUDGET, async {
                        let (home, _) = paths.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                        am_base::codex_hook::install_remote(&conn, home, &root, app.instance().as_deref()).await
                    }).await {
                        Ok(r) => r,
                        Err(_) => Err(anyhow::anyhow!("remote Codex Stop hook check timed out")),
                    };
                    if !matches!(ready, Ok(true)) {
                        if let Ok((home, command)) = &paths {
                            reconcile_remote_stop_hook(&conn, &project.host, home, command, true).await;
                        }
                    }
                    codex_notification_args(legacy, ready)
                } else {
                    if let Ok((home, command)) = &paths {
                        reconcile_remote_stop_hook(&conn, &project.host, home, command, false).await;
                    }
                    legacy
                }
            }
            "grok" => {
                install_remote_grok_hook(&conn, env, app.instance().as_deref()).await?;
                guard_refresh::note_started(&conn, &paths, bot, &time_zone, app.instance().as_deref()).await;
                vec![]
            }
            "agy" => {
                match crate::agy_remote::install_remote(&conn, app.instance().as_deref(), env.get("HOME").and_then(|v| v.as_str())).await {
                    Ok(_) => guard_refresh::note_started(&conn, &paths, bot, &time_zone, app.instance().as_deref()).await,
                    Err(e) => tracing::warn!(bot = %bot.name, host = %project.host, error = %format!("{e:#}"), "could not install the remote agy hooks; the bot starts without them"),
                }
                vec![]
            }
            other => anyhow::bail!("unknown bot kind {other}"),
        };
        out.extend(hook_args);
        return Ok(out);
    }

    let dir = app.bot_dir(&bot.id)?;
    crate::private_files::create_private_dir(&dir)?;
    let hook_args: Vec<String> = match bot.kind.as_str() {
        "claude" => {
            let cmd = shell_join(&hook_cmd_parts(app, bot, "claude"));
            let statusline = shell_join(&statusline_parts(hook_cmd_parts(app, bot, "claude")));
            let wants_remote = bot.args().iter().any(|a| a == "--remote-control");
            let time_zone = app.cfg().agents_fresh().await.time_zone().to_string();
            let mut settings = claude_settings(&cmd, &statusline, wants_remote, bot.auto_approve != 0, &time_zone);
            with_kill_guard(&mut settings, &cmd);
            if let Some(ws) = &restricted {
                if bot.inject_hooks == 0 {
                    settings = json!({});
                }
                app.cage_settings(&mut settings, ws, env);
            }
            let path = dir.join("claude-settings.json");
            write_private(&path, &serde_json::to_vec_pretty(&settings)?)?;
            vec!["--settings".into(), path.to_string_lossy().to_string(), "--verbose".into()]
        }
        "codex" => {
            let parts = hook_cmd_parts(app, bot, "codex");
            let arr = serde_json::to_string(&parts)?;
            let legacy = vec!["-c".into(), format!("notify={arr}")];
            let home = crate::home::dir().ok_or_else(|| anyhow::anyhow!("Codex home unavailable"))
                .and_then(|h| codex_hook_home(env, h.to_string_lossy().into_owned()));
            let command = sh_quote(&app.data_dir().join(am_base::codex_hook::CODEX_HOOK_SH).to_string_lossy());
            let lock = home.as_deref().ok().map(|h| am_base::codex_hook::home_lock(am_config::LOCAL_HOST, h));
            let _guard = match &lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            if let Ok(h) = &home {
                am_base::codex_kill_guard::observe_local(LOCAL_HOST, h, app.data_dir());
            }
            if stop_hook_enabled {
                let ready = match &home {
                    Ok(h) => am_base::codex_hook::install_local(std::path::Path::new(h), app.data_dir(), app.instance().as_deref()),
                    Err(e) => Err(anyhow::anyhow!("{e:#}")),
                };
                if !matches!(ready, Ok(true)) {
                    if let Ok(h) = &home {
                        if let Err(e) = am_base::codex_hook::reconcile_local(std::path::Path::new(h), &command, true) {
                            tracing::warn!(error = %e, "could not reconcile the local Codex Stop hook; leaving it as is");
                        }
                    }
                }
                codex_notification_args(legacy, ready)
            } else {
                if let Ok(h) = &home {
                    if let Err(e) = am_base::codex_hook::reconcile_local(std::path::Path::new(h), &command, false) {
                        tracing::warn!(error = %e, "could not reconcile the local Codex Stop hook; leaving it as is");
                    }
                }
                legacy
            }
        }
        "grok" => {
            install_local_grok_hook(app, env)?;
            vec![]
        }
        "agy" => {
            if let Err(e) = super::agy_hook::install_local(app) {
                tracing::warn!(bot = %bot.name, error = %format!("{e:#}"), "could not install the agy hooks; the bot starts without them");
            }
            vec![]
        }
        other => anyhow::bail!("unknown bot kind {other}"),
    };
    out.extend(hook_args);
    Ok(out)
}
/// Upper bound for the remote Stop-hook install/trust check during a bot start (#1989).
const REMOTE_STOP_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
/// 殘留清理要多幾趟 ssh（備份、寫入、讀回）；平常沒有殘留時只有兩次讀取，遠低於這個上限。
const REMOTE_STOP_HOOK_RECONCILE_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

fn codex_hook_home(env: &Value, home: String) -> anyhow::Result<String> {
    let path = env.get("CODEX_HOME").and_then(Value::as_str).map(str::trim)
        .filter(|value| !value.is_empty()).map(str::to_owned)
        .unwrap_or_else(|| format!("{}/.codex", home.trim_end_matches('/')));
    if !std::path::Path::new(&path).is_absolute() {
        anyhow::bail!("relative CODEX_HOME cannot prove the Stop hook trust source");
    }
    Ok(path)
}

/// 遠端的 CODEX_HOME 與 dispatcher 的絕對路徑（與 `install_remote` 寫入的位置一致）。
async fn remote_codex_paths(env: &Value, conn: &HostConn, root: &str) -> anyhow::Result<(String, String)> {
    let host_home = conn.home().await?;
    let codex_home = codex_hook_home(env, host_home.clone())?;
    let command = sh_quote(&format!("{}/{}/{}", host_home.trim_end_matches('/'), root, am_base::codex_hook::CODEX_HOOK_SH));
    Ok((codex_home, command))
}

/// 遠端 CODEX_HOME 與設定對齊（#1989）：同一把 host＋CODEX_HOME 鎖底下，超時或失敗只記 warn、不擋啟動。
/// 回 `true` 表示我們的 handler 已信任並保留。
async fn reconcile_remote_stop_hook(conn: &HostConn, host: &str, codex_home: &str, command: &str, keep_trusted: bool) -> bool {
    let lock = am_base::codex_hook::home_lock(host, codex_home);
    let _guard = lock.lock().await;
    match tokio::time::timeout(REMOTE_STOP_HOOK_RECONCILE_BUDGET, am_base::codex_hook::reconcile_remote(conn, codex_home, command, keep_trusted)).await {
        Ok(Ok(kept)) => kept,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "could not reconcile the remote Codex Stop hook; leaving it as is");
            false
        }
        Err(_) => {
            tracing::warn!("remote Codex Stop hook reconcile timed out; leaving it as is");
            false
        }
    }
}

fn codex_notification_args(legacy: Vec<String>, trusted: anyhow::Result<bool>) -> Vec<String> {
    match trusted {
        Ok(true) => vec!["-c".into(), "notify=[]".into()],
        Ok(false) => {
            tracing::warn!("Codex Stop hook is not trusted yet; retaining legacy notify");
            legacy
        }
        Err(_) => {
            tracing::warn!("Codex Stop hook setup unavailable; retaining legacy notify");
            legacy
        }
    }
}

/// issue #1884：Bash 工具跑之前先過廣域 kill 守門。同一支 `agents-managerd hook claude`——它看到 PreToolUse 只判斷、不送 daemon、
/// 只有要擋時才印決策 JSON（`kill_guard`）。遠端 bot 的 `hook_cmd` 是遠端根目錄的 `kill-guard.sh claude`（POSIX sh＋awk 版，
/// 2026-11-27 m4p 事故後補上），不經 `hook.sh`，所以 PreToolUse 不會進 spool。
///
/// 守門失效（hook 起不來、逾時、退出碼不對）一律擋下：`onFailure: "block"`（Claude Code 2.1.295 起；舊版忽略這個欄位，
/// 外層的 `|| …` 仍會擋下並印原因）。擋下時 stderr 的訊息告訴 agent 這是暫時狀態、稍後重試，不要改寫成別的殺法。
fn with_kill_guard(settings: &mut Value, hook_cmd: &str) {
    let guarded = format!("{hook_cmd} || {{ echo {} >&2; exit 2; }}", sh_quote(KILL_GUARD_UNAVAILABLE_MSG));
    settings["hooks"]["PreToolUse"] = json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": guarded, "timeout": 5, "onFailure": "block"}]}]);
}

const KILL_GUARD_UNAVAILABLE_MSG: &str = am_base::kill_guard::KILL_GUARD_UNAVAILABLE_MSG;

/// 每顆 claude bot 自己的 `claude-settings.json`。
///
/// `remoteControlAtStartup` 明講而不是省略：使用者帳號的全域 `settings.json` 只要開了它，
/// **每一顆** bot 起來都會多開一個手機入口（SPEC §18.15 說入口只有巡檢一個，背景 worker 一律
/// rc off）。判準是這顆 bot 自己有沒有要求——argv 帶了 `--remote-control` 才是 true。
fn claude_settings(hook_cmd: &str, statusline: &str, wants_remote: bool, auto_approve: bool, time_zone: &str) -> Value {
    json!({
        "tui": "default",
        "idleCompaction": false,
        "permissions": {"defaultMode": if auto_approve { "bypassPermissions" } else { "default" }},
        "remoteControlAtStartup": wants_remote,
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "Notification": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "PreCompact": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "PostCompact": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "StopFailure": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "SubagentStart": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "SubagentStop": [{"hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}],
            "PostToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]},
                {"matcher": "AskUserQuestion", "hooks": [{"type": "command", "command": hook_cmd, "timeout": 5}]}
            ]
        },
        "statusLine": {"type": "command", "command": statusline},
        "outputStyle": "Concise",
        "skipDangerousModePermissionPrompt": true,
        "timeFormat": "24-hour",
        "timeZone": time_zone,
        "autoContinueAtUsageLimit": false,
        "syncClaudeAiSkills": false,
        "syncClaudeAiPlugins": false
    })
}

/// Write a file only its owner can read (0600): bot settings may carry secrets.
/// 先寫 0600 的暫存檔再 rename 蓋過去：新內容永遠不會進到既有（可能是 0644 的）inode，
/// 成功回傳就代表最終檔是 0600；任何一步失敗都回錯並刪掉暫存檔，舊檔維持原樣。
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let tmp = dir.join(format!(".{name}.{}.tmp", crate::db::ulid()));
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return written;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| if p.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) { p.clone() } else { format!("'{}'", p.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// claude 自己重試暫時性 API 錯誤（529／5xx／斷線，issue #2229）：互動 TUI 實測生效（畫面顯示 `attempt N/300`）。
/// 總等待上限 10 分鐘，之後才照舊收成 failed 交給 `api_error_nudge`；額度重置超過 1 分鐘就不等，照舊走額度暫停。
pub const CLAUDE_RETRY_WATCHDOG_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_RETRY_WATCHDOG", "1"),
    ("CLAUDE_CODE_RETRY_WATCHDOG_MAX_WAIT_MS", "600000"),
    ("CLAUDE_CODE_RETRY_WATCHDOG_MAX_USAGE_LIMIT_WAIT_MS", "60000"),
];

/// Pane env = daemon-injected ∪ identity.env ∪ bot.env (later wins); `$HOME` / `~` expand against that host's home.
pub async fn pane_env(
    app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ExePath + crate::capabilities::ListenPort + crate::hosts::HostInstance + crate::hosts::HostsAccess + crate::tools::ToolsEnv + 'static>,
    bot: &db::Bot,
    host: &str,
    run_id: &str,
    agent_name: &str,
    shim_dir: Option<&str>,
) -> anyhow::Result<Value> {
    let fence = app.hosts().fence(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    pane_env_for_fence(app, bot, host, run_id, agent_name, shim_dir, &fence).await
}

pub async fn pane_env_for_fence(
    app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::ExePath + crate::capabilities::ListenPort + crate::hosts::HostInstance + crate::hosts::HostsAccess + crate::tools::ToolsEnv + 'static>,
    bot: &db::Bot,
    host: &str,
    run_id: &str,
    agent_name: &str,
    shim_dir: Option<&str>,
    fence: &crate::hosts::HostFence,
) -> anyhow::Result<Value> {
    if fence.conn().name != host {
        anyhow::bail!("host fence `{}` does not match `{host}`", fence.conn().name);
    }
    let home = crate::hosts::home_for_fence(fence).await?;
    if !app.hosts().is_current(fence).await {
        anyhow::bail!("host `{host}` changed while resolving HOME");
    }
    let mut env = serde_json::Map::new();
    env.insert("AM_BOT_ID".into(), json!(bot.id));
    env.insert("AM_AGENT_NAME".into(), json!(agent_name));
    env.insert("AM_KIND".into(), json!(bot.kind));
    env.insert("AM_PROJECT_ID".into(), json!(bot.project_id));
    if let Ok(Some(p)) = crate::db::project(app.db(), &bot.project_id).await {
        if let Some(ws) = p.workspace_id.filter(|w| !w.trim().is_empty()) {
            env.insert("AM_WORKSPACE_ID".into(), json!(ws));
        }
    }
    if let Some(m) = bot.model.as_deref().filter(|m| !m.trim().is_empty()) {
        env.insert("AM_MODEL".into(), json!(m));
    }
    if let Some(e) = bot.effort.as_deref().filter(|e| !e.trim().is_empty()) {
        env.insert("AM_EFFORT".into(), json!(e));
    }
    if let Some(dir) = shim_dir {
        let path = match std::env::var("PATH") {
            Ok(p) if host == LOCAL_HOST => crate::shim_path::prepend_own_shim_dir(&p, dir),
            _ => {
                let rp = fence.conn().cfg.as_ref().map(|c| c.remote_path.clone()).unwrap_or_default();
                let mut dirs = vec![dir.to_string()];
                dirs.extend(crate::hosts::remote_path_dirs(&rp, &home));
                dirs.extend(["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(String::from));
                dirs.join(":")
            }
        };
        env.insert("PATH".into(), json!(path));
    }
    env.insert("AM_RUN_ID".into(), json!(run_id));
    if host == LOCAL_HOST {
        env.insert("AM_PORT".into(), json!(app.port().to_string()));
        env.insert("AM_DAEMON_EXE".into(), json!(app.exe().to_string_lossy()));
        env.insert("AM_CONFIG_PATH".into(), json!(app.cfg().path.to_string_lossy()));
    }
    env.insert("CLAUDE_CODE_CHILD_SESSION".into(), json!(""));
    env.insert("CLAUDECODE".into(), json!(""));
    if bot.kind == "agy" {
        env.insert("AGY_CLI_DISABLE_AUTO_UPDATE".into(), json!("true"));
    }
    if bot.kind == "claude" {
        for (k, v) in CLAUDE_RETRY_WATCHDOG_ENV {
            env.insert((*k).into(), json!(v));
        }
    }

    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = crate::tools::identity_for_host(app, host, idn).await {
            for (k, v) in &id.env {
                env.insert(k.clone(), json!(crate::config::expand_home(v, &home)));
            }
            if bot.kind == "agy" {
                if let Err(e) = crate::tools::ensure_agy_shadow_home(fence, &id.env, &home).await {
                    tracing::warn!(host, identity = idn, error = %e, "agy shadow HOME could not be prepared");
                }
            }
        }
    }
    let identities = crate::tools::identities_for_host(app, host).await;
    env.insert("AM_IDENTITIES".into(), json!(crate::lifecycle::pane_identities::identities_env(&identities, &home)));
    for (k, v) in bot.env() {
        env.insert(k, json!(crate::config::expand_home(&v, &home)));
    }
    for (k, v) in super::runtime_overlay::overlay_for(app.as_ref(), bot).await?.env {
        env.insert(k, json!(v));
    }
    let mode = crate::db::project(app.db(), &bot.project_id).await?.ok_or_else(|| anyhow::anyhow!("project missing while preparing pane environment"))?.mode;
    env.insert("AM_PROJECT_MODE".into(), json!(mode));
    env.remove("AM_DAEMON_URL");
    let daemon_url = if host == LOCAL_HOST { Some(format!("http://127.0.0.1:{}", app.port())) } else { fence.conn().daemon_url(app.port()).await };
    if daemon_url.is_none() && host != LOCAL_HOST {
        let expert_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE host=? AND mode='expert' AND deleted_at IS NULL)").bind(host).fetch_one(app.db()).await?;
        if expert_exists { anyhow::bail!("主機 {host} 有 expert 專案，SSH_CONNECTION 缺失或格式無效，無法推得 daemon 位址；請檢查 SSH 連線或設定 daemon_url 覆寫"); }
    }
    if let Some(url) = daemon_url { env.insert("AM_DAEMON_URL".into(), json!(url)); }
    reserve_bot_auth_env(&mut env, bot, bot.inject_hooks != 0);
    let local_data_dir = (host == LOCAL_HOST).then(|| app.data_dir().to_string_lossy().into_owned());
    reserve_instance_env(&mut env, app.instance().as_deref(), local_data_dir.as_deref());
    let outbox = if host == LOCAL_HOST {
        crate::outbox::ensure(app.data_dir(), &bot.id).map(|d| d.to_string_lossy().into_owned())
    } else {
        crate::outbox_remote::remote_dir(&home, app.instance().as_deref(), &bot.id)
    };
    match outbox {
        Some(dir) => env.insert("AM_OUTBOX".into(), json!(dir)),
        None => env.remove("AM_OUTBOX"),
    };
    Ok(Value::Object(env))
}

/// Bot identity credentials are daemon-owned even when custom identity/bot env contains the same
/// keys. Keep hook dispatch conditional while always restoring the API token.
fn reserve_bot_auth_env(env: &mut serde_json::Map<String, Value>, bot: &db::Bot, hooks_enabled: bool) {
    env.insert("AM_BOT_ID".into(), json!(bot.id));
    env.insert("AM_BOT_TOKEN".into(), json!(bot.hook_token));
    if hooks_enabled {
        env.insert("AM_HOOK_TOKEN".into(), json!(bot.hook_token));
    } else {
        env.remove("AM_HOOK_TOKEN");
    }
}

/// 決定「這個 pane 屬於哪顆 daemon」的兩個變數是保留的：identity.env、bot.env 合併**之後**才由 daemon
/// 蓋回去，自訂 env 寫了也不算（sol 四輪）。否則正式 bot 可以偽造隔離 slug、隔離 bot 可以清掉 slug，
/// grok dispatcher 就把事件送錯實例；`AM_DATA_DIR` 被改掉則 hook 的 spool 落到別顆 daemon 的目錄。
/// - `AM_INSTANCE`：隔離實例＝它的 slug；正式實例＝移除（dispatcher 以「沒有這個變數」認正式實例）。
/// - `AM_DATA_DIR`：本機＝這顆 daemon 的資料目錄；遠端＝移除（遠端 bot 目錄在遠端家目錄，§11.4）。
fn reserve_instance_env(
    env: &mut serde_json::Map<String, Value>,
    instance: Option<&str>,
    local_data_dir: Option<&str>,
) {
    match instance {
        Some(slug) => env.insert("AM_INSTANCE".into(), json!(slug)),
        None => env.remove("AM_INSTANCE"),
    };
    match local_data_dir {
        Some(dir) => env.insert("AM_DATA_DIR".into(), json!(dir)),
        None => env.remove("AM_DATA_DIR"),
    };
}

/// Quote `s` as a TOML basic string (for codex `-c key="…"`).
pub fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The one text about opening sub-agents, used by the persona (all kinds) and the claude herdr
/// skill so they never drift. Naming isn't load-bearing (§6.5a descent, §6.5b shim) but keeps
/// the agent's mental model matching the sidebar.
///
/// Every bot gets this exact text, so it must not contain anything that differs per bot (name, id,
/// date, pane): the bots share one prompt prefix in the provider's cache (#1941). The name is read
/// from `$AM_AGENT_NAME`, and the one concrete value goes at the end of the injected text in
/// [`agent_name_note`].
pub fn child_agent_rules() -> &'static str {
    "你在 agents-manager（AG Man）裡的 agent 名稱存在環境變數 `$AM_AGENT_NAME`（`echo \"$AM_AGENT_NAME\"`），下文的 `$AM_AGENT_NAME` 一律指它，用到時代入實際的值。對話歷史裡若出現別的 agent 名稱（例如你是從別的 bot fork 出來的），那是原 bot、不是你；原 bot 的子 agent 不歸你，開子 agent、改名一律用 `$AM_AGENT_NAME-` 前綴，派工 prompt 也寫你自己的名稱。\
以下是**硬規則，不是建議**：一律照做，不要自行判斷要不要遵守，也不要事後才補。違反就是錯誤。\n\
\n\
需要開子任務或平行工作時，一律用 herdr 開子 agent：\n\
\n\
- **只准用 herdr pane 派工**：把工作交給另一個 agent，**必須**是 herdr pane 裡的子 agent。\
**禁止**用 CLI 內建的子代理（Claude 的 `Agent`／`Task` 工具與 `Workflow`、codex 外掛的 `codex-rescue`／`/codex:rescue`、\
codex 的子代理、grok 的同類功能），也**禁止**在自己的 pane 裡另起 `claude`／`codex`／`grok` 行程（含丟背景、`codex exec`）分擔工作。\
那些 AG Man 看不到、追不到狀態、收不掉。要交給 codex 就開 `--kind codex` 的 herdr 子 agent。\
只在拋棄式目錄驗證 CLI 本身的行為（例如試新旗標）不算派工。\
開子 agent 一律 `herdr agent start <名稱> --kind … --pane <新 pane>`；**禁止** `herdr pane run <pane> \"claude …\"`／`\"codex …\"` \
這樣在新 pane 直接起 CLI（就算加了 `--name`）：那樣 agent 在 herdr 沒有名稱，AG Man 認領不到。\n\
- **先找閒置的 child**：開新的子 agent 之前，**必須先跑** `herdr agent list`，看自己底下有沒有 `idle` / `done` 的子 agent。\
有就用 `herdr agent prompt <名稱> \"…\"` 把下一份工作交下去。**禁止**每件事都開一顆新的；只有使用者明確要求新開時才可以。\
**先挑 prompt cache 還熱的**：`herdr agent list` 的 stderr 會附上每顆子 agent 的 cache 還剩幾分（claude 約 60 分；codex 不會因閒置冷掉）。\
有好幾顆閒置的，先交給 cache 還熱、剩最久的那顆（接著用省掉重讀整段 context 的 token；閒置的 codex 一律算熱）；cache 已冷的跟新開差不多貴，\
只有跟手上工作脈絡相關時才值得重用。\n\
- **命名**：`herdr agent start <名稱> …` 的名稱**必須**以 `$AM_AGENT_NAME-` 為前綴（例：`$AM_AGENT_NAME-review`、`$AM_AGENT_NAME-ui`）。\
PATH 上的 herdr 會幫你補，但你自己要寫對。\n\
- **開 pane**：一律 `herdr pane split --pane \"$HERDR_PANE_ID\"`（或 `--current`）。\
**禁止省略目標**——省略時 herdr 會去拆使用者正在看的那個 pane。\n\
- **開 codex 子 agent 的完整寫法**（照抄、換掉 `<>`，**禁止**為此去讀 `herdr_shim.rs` 或 API 文件）：先 `herdr pane split --pane \"$HERDR_PANE_ID\"` 拿新 pane id，\
全新題目 `herdr agent start <名稱> --kind codex --pane <新 pane>`；要接著你這段脈絡就 `herdr agent start <名稱> --kind codex --pane <新 pane> -- fork --last`。\
要指定模型才加 `--model <模型>`，codex 的旗標一律放在 `--` 之後、子命令（`fork`／`resume`）之前，例：`-- --model <模型> fork --last`。\
**啟動不帶派工 prompt**：先用 `herdr agent get <名稱>` 確認登錄的 pane 等於新 pane，並等到 idle／ready，再 `herdr agent prompt <名稱> \"<派工 prompt>\"`。\
start 逾時或名稱查不到時，child 可能已在執行；先用 `herdr agent list`／`get` 核對，**禁止直接重開或對未確認的名稱派工**。\
換身份就在 `--` 之前加 `--identity <身份>`（可用的身份看 `$AM_IDENTITIES`）。\
**agy 子 agent 的模型**：除非使用者指定，**禁止**帶 `--model`，PATH 上的 herdr 會補最低強度 `gemini-3.8-flash-low`；使用者點名模型或強度才帶 `-- --model <slug>`（例 `gemini-3.8-flash-high`）。\
PATH 上的 herdr 會補 `-p`、`--no-daemon`、`-c project_doc_max_bytes=0`：**禁止**自己補，也**禁止**用真 herdr 的絕對路徑繞過它。\n\
- **環境變數與身份**：帳號與 hook 會自動帶進子 pane（`CLAUDE_CONFIG_DIR`、`AM_*`）。**禁止**自己 export／`env -u`／`unset` 這些變數。\
要讓子 agent 用別的身份：`herdr agent start <名稱> --kind <kind> --pane <新 pane> --identity <身份> -- …`（`--identity` 寫在 `--` 之前）；\
這台可用的身份看 `$AM_IDENTITIES`（格式 `<kind>/<名稱>[:KEY=路徑]`，本機與遠端一樣）；預設帳號（agy 的 `ag0`，不在清單裡）直接用 `--identity ag0`，或不帶 `--identity` 也是預設帳號。**禁止**用 `herdr pane run <pane> \"claude …\"` 之類自己起 CLI 來換帳號。\n\
- **禁止 `git stash`、禁止 `--autostash`**：同一個工作樹上有別的 agent 還沒提交的改動，弄丟了算你的。\n\
- 你開的子 agent 會被 AG Man 掛在**你底下**追蹤（側欄縮排顯示）。做完**必須**自己把它的 pane 收掉，不准留著。\n\
- **派工 prompt 必須寫明**（寫進 prompt 時 `$AM_AGENT_NAME` 代入實際的值，不照抄變數名）：「你是 `$AM_AGENT_NAME` 的子 agent：禁止再開子 agent（含內建子代理、另起 agent CLI）；\
工作太大或要平行時，停下來在回報裡寫清楚要另派什麼，由 `$AM_AGENT_NAME` 決定」。子 agent 回報要人手時，由你決定重用閒置 child 或另開兄弟，\
**禁止**叫子 agent 自己開。\n\
\n\
**你是子 agent 時**（環境變數 `$AM_CHILD_OF` 有值＝派你的 parent；或派工 prompt 說你是子 agent），以下取代上面「開子 agent」那幾條：\n\
\n\
- **禁止再開子 agent**：`herdr agent start`、內建子代理、另起 agent CLI 一律禁止。PATH 上的 herdr 會直接拒絕，**禁止**繞過（包括動 `AM_CHILD_OF`）。\n\
- 工作太大、需要平行、或卡在範圍外的事時，**必須**停下來在回報裡寫清楚：要另派什麼、為什麼、跟你手上的工作怎麼切。\
要不要開兄弟 agent、交給誰，由 parent 決定。\n\
- 做完就回報 parent，由 parent 驗收並收掉你的 pane。\n\
\n\
終止行程（同樣是硬規則，issue #1884）：\n\
\n\
- **只准殺自己記下的 PID 或 port**：自己啟動的行程，啟動時就記下 PID（`$!`），收尾 `kill <PID>`；dev server 用 port：`lsof -ti :PORT | xargs kill`。\
**禁止** `pkill`／`pkill -f`／`killall` 比對寬鬆的字串（`node`、`next`、`1`、agent 名稱…），也**禁止** `kill -9 -1`／`kill 0`。\
同一台機器上有別的 bot 的 claude／codex／grok，字串比對錯一點就全部陪葬（2026-10-21 mac_builder 一條 `pkill -f \"next-server\" -P 1` 殺光整台機器的 agent）。\n\
- macOS／BSD 的 `pkill` 遇到第一個非選項參數就停止解析選項，之後的 `-P 1` 會被當成**要殺的字串**：選項**必須全部寫在比對字串之前**\
（`pkill -P 1 -f next-server`，不是 `pkill -f next-server -P 1`）。這台的 bot 有 hook 會擋下廣域的 kill 並告訴你原因，被擋就照訊息改用 PID 或 port，**禁止**換個寫法繞過。\n\
\n\
瀏覽器（同樣是硬規則）：\n\
\n\
- **一律用 agent-browser**（CLI，使用者 2026-10-13 裁示；用法先看 `agent-browser skills get core`）。\
**禁止**用 ego lite、**禁止**自己另起 Chrome、**禁止**用其他 headless 或內建的瀏覽器工具。\n\
- **一個 bot 最多一個 session**（你和你的子 agent 各自算一個）：每個指令都帶 `--session $AM_AGENT_NAME`，\
在同一個 session 裡用 `open` 換頁，**禁止**另開 session、**禁止**一個網址開一個新分頁。\n\
- **結束就關**：工作做完、或收掉子 agent 之前，**必須先** `agent-browser --session $AM_AGENT_NAME close`。\
瀏覽器留著不關，RAM 就是這樣被吃光的。\n\
- **登入態**（使用者 2026-10-15）：使用者要你登入時可以照做；使用者也可以在自己的環境用 agent-browser 登入，\
再把登入態（`--state` 檔、cookie、session）交給你的 session 接著執行。登入態檔算憑證：**禁止**放 scratchpad、`$AM_OUTBOX` 或 repo，用完就刪。\n\
\n\
背景工作（同樣是硬規則，使用者 2026-10-31，issue #2048 #2050）：\n\
\n\
- **背景等待一律限時**：`run_in_background` 的 until／while 迴圈、輪詢腳本都要有上限（外層 `timeout <秒>` 或最大次數）；逾時要印出原因並以非 0 結束。\n\
- **等待條件的查詢不准吞錯誤**：不要 `2>/dev/null` 之後再判空字串；查詢失敗就直接結束並印出錯誤。\n\
- **用完自己停**：不再需要的背景工作要自己停掉（Claude 用 TaskStop；其他 CLI 用自己記下的 PID `kill`）。\n\
- **常駐行程放 herdr shell pane**：dev server、watcher、長駐 tail／log 追蹤這類預期跑超過一個回合、或要給使用者／別的 bot 看的行程，用 `herdr pane split --pane \"$HERDR_PANE_ID\"` 開 pane、`herdr pane run <pane> \"<指令>\"` 執行，不放在 CLI 的背景 shell；做完**必須**關掉那個 pane。\n\
\n\
輸出檔案（同樣是硬規則，使用者 2026-09-16 裁示）：\n\
\n\
- **scratchpad 只放中間產物**（腳本、log、暫存資料）。scratchpad **不是**給使用者的地方，**禁止**把要交給使用者的檔案放在那裡。\n\
- **要交給使用者的檔案一律放 `$AM_OUTBOX`**（`~/.config/agents-manager/outbox/<AM_BOT_ID>/`）。寫之前**必須先** `mkdir -p \"$AM_OUTBOX\"`（空目錄會被清掉）。\
放進去 **1 小時後自動刪除**；要長期保留的放 repo 或 `reports/`。遠端主機的 bot 也有 `$AM_OUTBOX`（在那台機器上，網頁一樣列得到）。\
`$AM_OUTBOX` 沒有值時**禁止**改放 scratchpad，直接在對話裡講清楚檔案在哪台機器的哪個路徑。\n\
- **私鑰、憑證、DB 一律禁止放進 scratchpad 或 `$AM_OUTBOX`**（`.pem`、`.key`、`.p12`、`.env`、`*.sqlite*`、`*.db`、DB 複本、瀏覽器 profile）。\
驗證要用 DB 複本時，做完**必須當下刪掉**。\n\
\n\
CLI 登入／OAuth（同樣是硬規則，使用者 2026-10-03 裁示）：\n\
\n\
- **一律用不開瀏覽器的模式**：CLI 要做 OAuth 或登入時（gcloud、gh、az、aws sso、firebase…），**禁止**讓它在這台機器開瀏覽器。\n\
- **旗標**：`gcloud auth login --no-browser`（ADC 用 `gcloud auth application-default login --no-browser`）、`az login --use-device-code`、\
`aws sso login --use-device-code`、`firebase login --no-localhost`、gh 用 `GH_BROWSER=echo gh auth login --web`（印出網址與一次性碼）。\
使用者回覆手邊沒有裝 gcloud 的電腦時，gcloud 改用 `--no-launch-browser`（網址＋驗證碼，手機就能完成）。\n\
- **這類指令會停下來等輸入**：**禁止**在自己的 shell 工具裡直接跑（會卡到逾時）。**必須**開一個 shell pane 跑：\
`herdr pane split --pane \"$HERDR_PANE_ID\"` 開 pane、`herdr pane run <pane> \"<指令>\"` 執行、`herdr pane wait-output --match <字> <pane>`／`herdr pane read <pane>` \
讀出網址、一次性碼或 `--remote-bootstrap` 那一行指令。\n\
- **交給使用者**：把讀到的內容原樣放進這一回合的回覆（code block），寫清楚在哪裡做（手機或電腦打開網址；或在有瀏覽器、裝了 gcloud 的電腦跑那一行）、\
要貼回什麼，然後**結束這一回合**等使用者。**禁止**在回合裡輪詢等待。\n\
- **收尾**：使用者貼回結果後，用 `herdr pane send-text <pane> \"<結果>\"` 再 `herdr pane send-keys <pane> Enter` 送進去，確認登入成功（例如 `gcloud auth list`），\
**必須**關掉那個 pane。\n\
- 只會導回 `localhost`、沒有上述模式的 CLI：停下來在回覆裡說明，由使用者決定（例如在有瀏覽器的電腦跑 `ssh -L <埠>:localhost:<埠> <這台主機>` 再開網址）。"
}

/// Replaces `herdr --skill`'s description, which says 「只有使用者明確提到 Herdr 才用」 —
/// backwards for a bot inside AG Man.
const HERDR_SKILL_DESC: &str = "在 agents-manager（AG Man）裡控制 herdr 的 pane、tab、workspace 與子 agent。\
需要開子任務、平行工作、或把工作分給另一個 agent 時，一律用這個 skill，並**必須**照裡面的 AG Man 規則命名與開 pane。";

/// Turn `herdr --skill`'s output into the copy this bot should read: our own description in
/// the front matter, and `child_agent_rules` as the first section of the body.
///
/// Only the description line inside the front matter is touched; the rest of herdr's document
/// is the CLI's own authority on its commands and is passed through untouched, so a herdr
/// upgrade brings its new text along.
/// skill 檔一個身份一份，同身份的每顆 bot 與子 agent 都讀同一份：不能寫死某一顆的名字（#1121）。
fn herdr_skill_doc(raw: &str) -> String {
    let rules = format!("## AG Man 規則（硬規則，優先於本文件其餘內容）\n\n{}\n\n受管 prompt 可用同專案成員顯示名稱（含停止的成員）；同名時改用 `bot:<id>`。daemon 錯誤會顯示回應內容，依錯誤原因處理；投遞結果不明時沿用提示的 request-id 對帳或重試。\n", child_agent_rules());
    let lines: Vec<&str> = raw.lines().collect();
    let end = if lines.first().map(|l| l.trim_end()) == Some("---") {
        lines.iter().skip(1).position(|l| l.trim_end() == "---").map(|i| i + 1)
    } else {
        None
    };
    let Some(end) = end else {
        return format!("{rules}\n{raw}");
    };
    let mut out = String::new();
    let mut replaced = false;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && i < end && line.starts_with("description:") {
            out.push_str("description: \"");
            out.push_str(HERDR_SKILL_DESC);
            out.push_str("\"\n");
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if i == end {
            if !replaced {
                tracing::debug!("herdr --skill has no description line; leaving its front matter alone");
            }
            out.push('\n');
            out.push_str(&rules);
        }
    }
    out
}

/// 遠端寫 skill：`mktemp` 給每次的暫存檔獨一個名字（同身份兩顆 bot 同時啟動不互踩，#1121），trap 收尾；
/// 內容相同就不換（保留 mtime），不同才 `mv`。
fn remote_skill_script(dir: &str, doc: &str) -> String {
    format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\nN=$(mktemp \"$D/.SKILL.md.XXXXXX\")\ntrap 'rm -f \"$N\"' EXIT\ncat > \"$N\" <<'AM_SKILL_EOF'\n{doc}\nAM_SKILL_EOF\nif cmp -s \"$N\" \"$D/SKILL.md\" 2>/dev/null; then rm -f \"$N\"; else mv \"$N\" \"$D/SKILL.md\"; fi\nprintf 'AM_SKILL_OK\\n'\n",
        dir = sh_quote(dir),
        doc = doc.trim_end(),
    )
}

/// Install herdr's skill (with `child_agent_rules` on top) into `$CLAUDE_CONFIG_DIR/skills/`.
/// Per identity; idempotent because the file is in the user's own claude config (backup noise).
pub async fn install_herdr_skill(app: &impl crate::hosts::HostsAccess, bot: &db::Bot, project: &db::Project, env: &Value, _agent_name: &str) {
    if bot.kind != "claude" {
        return;
    }
    if cfg!(any(test, feature = "test-hooks")) {
        return;
    }
    let cfg_dir = env.get("CLAUDE_CONFIG_DIR").and_then(|v| v.as_str()).map(str::to_string);
    let result = if project.host == LOCAL_HOST {
        install_herdr_skill_local(cfg_dir).await
    } else {
        match app.hosts().get(&project.host).await {
            Some(conn) => install_herdr_skill_remote(&conn, cfg_dir).await,
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    if let Err(e) = result {
        tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the herdr skill");
    }
}

/// start_bot 在 per-bot 鎖裡等這一步；herdr 卡住不能讓整顆 bot 永遠起不來。
const HERDR_SKILL_TIMEOUT: Duration = Duration::from_secs(15);

async fn herdr_skill_output(bin: &str, timeout: Duration) -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(bin).arg("--skill").stdin(std::process::Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("`herdr --skill` 逾時（{}s）", timeout.as_secs_f32()))??;
    if !out.status.success() {
        anyhow::bail!("`herdr --skill` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn install_herdr_skill_local(cfg_dir: Option<String>) -> anyhow::Result<()> {
    let doc = herdr_skill_doc(&herdr_skill_output("herdr", HERDR_SKILL_TIMEOUT).await?);
    let base = match cfg_dir {
        Some(d) if !d.trim().is_empty() => std::path::PathBuf::from(d),
        _ => crate::home::dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?.join(".claude"),
    };
    let dir = base.join("skills").join("herdr");
    let path = dir.join("SKILL.md");
    if std::fs::read_to_string(&path).map(|c| c == doc).unwrap_or(false) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".SKILL.md.{}.tmp", db::ulid()));
    std::fs::write(&tmp, &doc)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    tracing::info!(path = %path.display(), "herdr skill installed for claude");
    Ok(())
}

async fn install_herdr_skill_remote(conn: &HostConn, cfg_dir: Option<String>) -> anyhow::Result<()> {
    let raw = conn.ssh_exec_path("herdr --skill").await?;
    if !raw.contains("name:") {
        anyhow::bail!("`herdr --skill` on `{}` did not look like a skill file", conn.name);
    }
    let doc = herdr_skill_doc(&raw);
    let base = match cfg_dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => format!("{}/.claude", conn.home().await?),
    };
    let dir = format!("{base}/skills/herdr");
    let out = conn.ssh_exec(&remote_skill_script(&dir, &doc)).await?;
    if !out.contains("AM_SKILL_OK") {
        anyhow::bail!("remote herdr skill install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, dir, "herdr skill installed for claude");
    Ok(())
}

/// The one line that carries this bot's concrete name. It goes last in every injected text, so all
/// the text above it is the same for every bot and the providers' prompt cache shares one prefix.
pub fn agent_name_note(agent_name: &str) -> String {
    format!("你在 AG Man 裡的 agent 名稱是 `{agent_name}`（上文的 `$AM_AGENT_NAME` 就是這個值；只有這一段寫的是實際名稱）。")
}

/// The name note for a sub-agent (#1952). The file it sits in belongs to the parent bot, so it must not
/// name the parent: a child's `$AM_AGENT_NAME` is its own full name, set by the herdr shim when it
/// starts the pane. Static text, no per-bot value, like #1941.
pub fn child_agent_name_note() -> &'static str {
    "你是子 agent：你在 AG Man 裡的 agent 名稱存在環境變數 `$AM_AGENT_NAME`（`echo \"$AM_AGENT_NAME\"`），那是 herdr 登錄你的全名，不是派你出來的母 bot 的名字。上文的 `$AM_AGENT_NAME` 一律指它，用到時代入實際的值。"
}

/// What a sub-agent reads (`instructions.md`, §6.5i): the AG Man rules, then the `[agents]` agent md,
/// then the child name note.
pub fn child_instructions(agent_md: &str) -> String {
    format!("{}\n\n{}", super::agent_md::compose(child_agent_rules(), agent_md), child_agent_name_note())
}

/// `bot.persona` appended to the system prompt per kind. Order: `child_agent_rules`, then the
/// `[agents]` agent md (§6.5i), then the bot's own persona, then [`agent_name_note`] last.
/// Grok receives a short `--rules` path pointer to a staged full-text file.
/// `agent_md` None＝`[agents]` 沒設定：CLI 照舊讀自己的指示檔。
pub fn persona_text(bot: &db::Bot, agent_name: &str, agent_md: Option<&str>) -> String {
    let base = super::agent_md::compose(child_agent_rules(), agent_md.unwrap_or(""));
    let user = bot.persona.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let text = match user {
        Some(u) => format!("{base}\n\n{u}"),
        None => base,
    };
    format!("{text}\n\n{}", agent_name_note(agent_name))
}

fn grok_rules_reference(path: &str) -> String {
    let path = serde_json::to_string(path).expect("a path string serializes as JSON");
    format!("AG Man 指示（硬規則，效力同系統指示）完整存放於 JSON 路徑 {path}。開始任何工作前先完整讀取該檔並照做。")
}

/// `staged_persona` 是母 bot 完整指示已經放在 argv 之外的位置（herdr 把整條指令壓在 900 bytes 內，整份放 argv 會被砍）：
/// claude／grok 是檔案路徑，codex 是 profile 名（`<CODEX_HOME>/<名>.config.toml`）。None＝沒有 staged，退回把整份放進 argv
/// （只剩單元測試與沒有 bot 目錄的路徑；正式啟動沒 staged 成功會直接拒絕，見 `start::stage_persona`）。
pub fn persona_args(bot: &db::Bot, agent_name: &str, agent_md: Option<&str>, staged_persona: Option<&str>) -> Vec<String> {
    persona_args_cli_docs(bot, agent_name, agent_md, staged_persona, agent_md.is_some())
}

/// 同 [`persona_args`]，但「關掉 CLI 自己的指示檔」（codex 的 `project_doc_max_bytes=0`）獨立給：agent md 有一份讀不到時
/// 仍要注入讀得到的那幾份，卻不能關掉 AGENTS.md（#769）。
pub fn persona_args_cli_docs(bot: &db::Bot, agent_name: &str, agent_md: Option<&str>, staged_persona: Option<&str>, disable_cli_docs: bool) -> Vec<String> {
    let p = persona_text(bot, agent_name, agent_md);
    let p = p.as_str();
    match bot.kind.as_str() {
        "claude" => match staged_persona {
            Some(path) => vec!["--append-system-prompt-file".into(), path.to_string()],
            None => vec!["--append-system-prompt".into(), p.to_string()],
        },
        "grok" => vec!["--rules".into(), staged_persona.map(grok_rules_reference).unwrap_or_else(|| p.to_string())],
        "codex" => {
            let mut v = match staged_persona {
                Some(profile) => vec!["-p".into(), profile.to_string()],
                None => vec!["-c".into(), format!("developer_instructions={}", toml_basic_string(p))],
            };
            if disable_cli_docs {
                v.extend(["-c".into(), "project_doc_max_bytes=0".into()]);
            }
            v
        }
        _ => vec![],
    }
}

/// `bot.model` / `bot.effort` as CLI args. Efforts are per model: a stale one gets
/// `400 unsupported_value` on every turn (codex `max` on `gpt-5.5`), so it is dropped — but only
/// when the model's list is readable and lacks it; uncertain paths keep it (dropping a valid one
/// silently downgrades the agent).
pub async fn effort_checked(app: &impl crate::lifecycle::start::ports::SessionProviderPort, bot: &db::Bot, host: &str) -> db::Bot {
    let Some(effort) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    let Some(model) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    let Ok(list) = app.models_list(host, &bot.kind, None, false).await else { return bot.clone() };
    let Some(entry) = list
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|a| a.iter().find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model)))
    else {
        return bot.clone();
    };
    let Some(efforts) = entry.get("efforts").and_then(|e| e.as_array()).filter(|a| !a.is_empty()) else { return bot.clone() };
    if efforts.iter().any(|e| e.as_str() == Some(effort)) {
        return bot.clone();
    }
    tracing::warn!(bot = %bot.name, model, effort, "model does not accept this reasoning effort; starting without it");
    let mut out = bot.clone();
    out.effort = None;
    out
}

pub fn model_args(bot: &db::Bot) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(m) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let m = crate::models::canonical_model(&bot.kind, m);
        match bot.kind.as_str() {
            "claude" => out.extend(["--model".to_string(), m.to_string()]),
            "codex" | "grok" => out.extend(["-m".to_string(), m.to_string()]),
            "agy" => out.extend(["--model".to_string(), m.to_string()]),
            _ => {}
        }
    }
    if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match bot.kind.as_str() {
            "claude" => out.extend(["--effort".to_string(), e.to_lowercase()]),
            "grok" => out.extend(["--reasoning-effort".to_string(), e.to_lowercase()]),
            "codex" => out.extend(["-c".to_string(), format!("model_reasoning_effort=\"{}\"", e.to_lowercase())]),
            _ => {}
        }
    }
    if bot.kind == "codex" {
        let tier = if bot.fast != 0 { "priority" } else { "" };
        out.extend(["-c".to_string(), format!("service_tier=\"{tier}\"")]);
    }
    out
}

#[cfg(test)]
mod hook_cmd_parts_tests {
    use super::{claude_settings, hook_cmd_parts_for, write_private};
    #[test]
    fn kill_guard_fails_closed_with_a_retry_hint() {
        use super::{with_kill_guard, KILL_GUARD_UNAVAILABLE_MSG};
        let mut v = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        with_kill_guard(&mut v, "/no/such/agents-managerd hook claude");
        let hook = &v["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(hook["onFailure"], "block");
        let cmd = hook["command"].as_str().expect("command");
        let run = |c: &str| std::process::Command::new("sh").arg("-c").arg(c).output().expect("sh");
        let broken = run(cmd);
        assert_eq!(broken.status.code(), Some(2), "hook 起不來要擋");
        assert!(String::from_utf8_lossy(&broken.stderr).contains(KILL_GUARD_UNAVAILABLE_MSG));
        let mut ok = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        with_kill_guard(&mut ok, "true");
        let ok_cmd = ok["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().expect("command").to_string();
        assert_eq!(run(&ok_cmd).status.code(), Some(0), "hook 正常時不改變退出碼");
    }
    #[test]
    fn every_claude_command_hook_has_a_five_second_timeout() {
        let v = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        for (event, groups) in v["hooks"].as_object().expect("hooks") {
            for group in groups.as_array().expect("hook groups") {
                for hook in group["hooks"].as_array().expect("hooks in group") {
                    assert_eq!(hook["timeout"], 5, "{event}: {hook}");
                }
            }
        }
    }
    /// Remote event parsing keeps the failure end event as an idle hint too.
    #[test]
    fn the_remote_dispatcher_reports_idle_for_a_failed_turn_too() {
        assert!(
            super::REMOTE_HOOK_SH_TEMPLATE.contains(r#"event in ("Stop","StopFailure")"#),
            "hook.sh 少了 StopFailure 那一條",
        );
    }
    /// Issue #43: the hook / statusLine command line must not carry the token.
    #[test]
    fn hook_cmd_parts_has_no_token() {
        let parts = hook_cmd_parts_for("/usr/bin/agents-managerd", 7788, "b1", "claude", "/tmp/am-iso");
        assert_eq!(
            parts,
            vec!["/usr/bin/agents-managerd", "hook", "claude", "--bot", "b1", "--port", "7788", "--data-dir", "/tmp/am-iso"]
        );
        assert!(!parts.iter().any(|p| p == "--token"));
    }
    #[cfg(unix)]
    #[test]
    fn write_private_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = crate::test_dirs::track(std::env::temp_dir().join(format!("am-write-private-{}.json", std::process::id())));
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod remote_kill_guard_tests {
    use super::{remote_claude_settings, RemoteHookPaths, KILL_GUARD_UNAVAILABLE_MSG};

    /// 2026-11-27 m4p `pkill -P 1` 事故：遠端 claude bot 的 Bash 也要先過守門，指向遠端根目錄的 `kill-guard.sh`，不經 `hook.sh` 的 spool。
    #[test]
    fn a_remote_claude_bot_gets_a_bash_pre_tool_use_kill_guard_that_fails_closed() {
        let p = RemoteHookPaths {
            dir: "/h/.config/agents-manager/bots/b".into(),
            hook_sh: "/h/.config/agents-manager/bots/b/hook.sh".into(),
            settings: "/h/.config/agents-manager/bots/b/claude-settings.json".into(),
            kill_guard: "/no/such/.config/agents-manager/kill-guard.sh".into(),
        };
        let v = remote_claude_settings(&p, &super::model_args_tests::bot("claude", None, None, false), "Asia/Taipei");
        let groups = v["hooks"]["PreToolUse"].as_array().expect("PreToolUse");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], "Bash");
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["onFailure"], "block");
        assert_eq!(hook["timeout"], 5);
        let cmd = hook["command"].as_str().unwrap();
        assert!(cmd.starts_with("'/no/such/.config/agents-manager/kill-guard.sh' claude || "), "{cmd}");
        assert!(!cmd.contains("hook.sh"), "PreToolUse 不進 spool：{cmd}");
        assert!(v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook.sh"));
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "腳本不在就擋");
        assert!(String::from_utf8_lossy(&out.stderr).contains(KILL_GUARD_UNAVAILABLE_MSG));
    }
}

#[cfg(test)]
mod model_args_tests {
    use super::model_args;
    use crate::db::Bot;

    pub(super) fn bot(kind: &str, model: Option<&str>, effort: Option<&str>, fast: bool) -> Bot {
        Bot { role: None, manager_use: 1, track: None, runtime_override_json: None,
            id: "b".into(),
            project_id: "p".into(),
            name: "n".into(),
            kind: kind.into(),
            model: model.map(String::from),
            effort: effort.map(String::from),
            fast: fast as i64,
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn codex_previous_sol_start_args_use_current_sol() {
        assert_eq!(model_args(&bot("codex", Some("gpt-6-sol"), None, false)),
            vec!["-m", "gpt-6.1-sol", "-c", "service_tier=\"\""]);
        for model in ["gpt-6.1-sol", "gpt-6-sol-preview"] {
            assert_eq!(model_args(&bot("codex", Some(model), None, false)),
                vec!["-m", model, "-c", "service_tier=\"\""]);
        }
    }

    #[test]
    fn codex_effort_and_fast() {
        let a = model_args(&bot("codex", Some("gpt-5.6-sol"), Some("high"), true));
        assert_eq!(
            a,
            vec!["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=\"high\"", "-c", "service_tier=\"priority\""]
        );
        let a = model_args(&bot("codex", None, None, false));
        assert_eq!(a, vec!["-c", "service_tier=\"\""]);
    }

    /// SPEC §4.4a: `model_args` and its inverse must agree — the run stamp is read back through it.
    #[test]
    fn the_runtime_stamp_reads_back_what_we_passed() {
        let b = bot("codex", Some("gpt-5.6-luna"), Some("xhigh"), true);
        let args = model_args(&b);
        let (model, effort) = crate::models::model_effort_from_argv("codex", &args);
        assert_eq!(model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(effort.as_deref(), Some("xhigh"));
        assert!(args.iter().any(|a| a.contains("service_tier=\"priority\"")), "fast is a flag we can read back");
        let bare = model_args(&bot("codex", None, None, false));
        assert_eq!(crate::models::model_effort_from_argv("codex", &bare), (None, None));
        assert!(!bare.iter().any(|a| a.contains("service_tier=\"priority\"")));
    }

    #[test]
    fn full_claude_model_ids_and_codex_catalog_models_are_passed_verbatim() {
        assert_eq!(model_args(&bot("claude", Some("claude-opus-5-5"), None, false)), vec!["--model", "claude-opus-5-5"]);
        assert_eq!(
            model_args(&bot("codex", Some("gpt-6-luna"), None, false)),
            vec!["-m", "gpt-6-luna", "-c", "service_tier=\"\""]
        );
    }

    #[test]
    fn retired_models_are_rewritten_before_start_args_but_other_versions_stay_verbatim() {
        assert_eq!(model_args(&bot("codex", Some("gpt-5.6-sol"), None, false)), vec!["-m", "gpt-6.1-sol", "-c", "service_tier=\"\""]);
        assert_eq!(model_args(&bot("codex", Some("gpt-5.6-terra"), None, false)), vec!["-m", "gpt-6.1-sol", "-c", "service_tier=\"\""]);
        assert_eq!(model_args(&bot("codex", Some("gpt-5.6-luna"), None, false)), vec!["-m", "gpt-6-luna", "-c", "service_tier=\"\""]);
        assert_eq!(model_args(&bot("codex", Some("gpt-5.6-sol-preview"), None, false)), vec!["-m", "gpt-5.6-sol-preview", "-c", "service_tier=\"\""]);
        assert_eq!(model_args(&bot("claude", Some("opus"), None, false)), vec!["--model", "claude-opus-5-5"]);
        assert_eq!(model_args(&bot("claude", Some("claude-opus-4-1"), None, false)), vec!["--model", "claude-opus-4-1"]);
    }

    #[test]
    fn agy_takes_its_permission_flag_and_a_model_slug_but_no_effort_or_fast() {
        assert_eq!(super::permission_args("agy", true), vec!["--dangerously-skip-permissions"]);
        assert!(super::permission_args("agy", false).is_empty(), "沒開 auto_approve 就不帶，agy 自己的 toolPermission 照預設");
        assert_eq!(model_args(&bot("agy", Some("gemini-3.8-flash-high"), None, true)), vec!["--model", "gemini-3.8-flash-high"]);
        for retired in ["gemini-3.1-pro-high", "gemini-3.7-flash-low", "claude-sonnet-4-6", "gpt-oss-120b-medium"] {
            assert_eq!(model_args(&bot("agy", Some(retired), None, true)), vec!["--model", "gemini-3.8-flash-medium"], "{retired}");
        }
        assert!(model_args(&bot("agy", None, None, false)).is_empty());
        assert!(super::persona_args(&bot("agy", None, None, false), "x", None, None).is_empty(), "agy 沒有 argv 的 persona 管道（第二階段）");
    }

    #[test]
    fn grok_keeps_reasoning_effort_and_ignores_fast() {
        let a = model_args(&bot("grok", Some("grok-4.6"), Some("low"), true));
        assert_eq!(a, vec!["-m", "grok-4.6", "--reasoning-effort", "low"]);
    }

    /// claude takes `--effort` (2.1+) but never the codex Fast tier.
    #[test]
    fn claude_gets_effort_but_not_fast() {
        let a = model_args(&bot("claude", Some("opus"), Some("high"), true));
        assert_eq!(a, vec!["--model", "claude-opus-5-5", "--effort", "high"]);
        let a = model_args(&bot("claude", None, Some("MAX"), false));
        assert_eq!(a, vec!["--effort", "max"], "the level goes out lowercase");
        assert!(model_args(&bot("claude", None, None, true)).is_empty());
    }

    /// Injected herdr skill: our description replaces herdr's, our rules go first, herdr's CLI text survives.
    #[test]
    fn the_herdr_skill_is_rewritten_for_ag_man() {
        let raw = "---\nname: herdr\ndescription: \"Control Herdr… Use only when the user explicitly mentions Herdr.\"\n---\n\n# Herdr\n\nherdr organizes terminals.\n";
        let doc = super::herdr_skill_doc(raw);
        assert!(doc.starts_with("---\nname: herdr\ndescription: \""), "front matter is kept, description replaced:\n{doc}");
        assert!(!doc.contains("Use only when the user explicitly mentions Herdr"), "herdr's own description is gone");
        assert!(doc.contains("需要開子任務"), "ours says to use it for sub-tasks");
        assert!(doc.contains("## AG Man 規則"), "the rules lead the body");
        assert!(doc.contains("`$AM_AGENT_NAME-`"), "the naming rule quotes the env var, not one bot's name");
        assert!(doc.contains("git stash"), "the no-stash rule is carried");
        assert!(doc.contains("herdr agent list"), "reuse an idle child before opening a new one");
        assert!(doc.contains("$HERDR_PANE_ID"), "how to split its own pane");
        assert!(doc.contains("herdr organizes terminals."), "herdr's own body survives");
        assert!(doc.find("AG Man 規則").unwrap() < doc.find("herdr organizes").unwrap());
    }

    /// 一個身份一份檔、多顆 bot 共用：規則裡不能寫死任何一顆的名字（#1121）。
    #[test]
    fn the_herdr_skill_names_no_particular_bot() {
        let doc = super::herdr_skill_doc("---\nname: herdr\ndescription: x\n---\n# herdr\n");
        assert!(doc.contains("`--session $AM_AGENT_NAME`"), "{doc}");
        assert!(doc.contains("echo \"$AM_AGENT_NAME\""), "說明這是環境變數");
        assert!(!doc.contains("proj-abc123"), "{doc}");
    }

    /// 遠端寫檔用獨一的暫存檔名，不再是固定的 `.SKILL.md.new`（兩顆同身份的 bot 同時啟動會互相截斷）。
    #[test]
    fn the_remote_skill_script_uses_a_private_temp_file() {
        let script = super::remote_skill_script("/home/u/.claude/skills/herdr", "x");
        assert!(script.contains("mktemp") && !script.contains(".SKILL.md.new"), "{script}");
        assert!(script.contains("trap 'rm -f"), "{script}");
    }

    /// A document without front matter is not edited — our rules simply go in front of it.
    #[test]
    fn a_skill_without_front_matter_is_not_rewritten() {
        let doc = super::herdr_skill_doc("# Herdr\n\nbody\n");
        assert!(doc.starts_with("## AG Man 規則"));
        assert!(doc.ends_with("# Herdr\n\nbody\n"));
    }

    /// 同角色的兩顆 codex bot：注入的指示（含 developer_instructions／staged profile 的內容）除了名稱段，逐字相同。
    /// 名稱段是最後一行 [`agent_name_note`]；替換成佔位後再比對，所以 prompt cache 的前綴只差這一段。
    #[test]
    fn codex_prefix_is_identical_across_same_role_bots_except_the_name_note() {
        use super::{agent_name_note, persona_args};
        let mut a = bot("codex", None, None, false);
        a.role = Some("executor".into());
        a.persona = Some("同一個角色的人設：先讀 README，再動手。".into());
        let mut b = a.clone();
        b.name = "other-name".into();
        let md = Some("## 專案規則\n所有成員共用的內容。");
        let (na, nb) = ("proj-aaaaaa", "proj-bbbbbb");
        let strip = |args: Vec<String>, name: &str| -> Vec<String> { args.into_iter().map(|x| x.replace(&agent_name_note(name), "<NAME-NOTE>")).collect() };
        let xa = strip(persona_args(&a, na, md, None), na);
        let xb = strip(persona_args(&b, nb, md, None), nb);
        assert_eq!(xa, xb, "名稱段以外的注入指示要逐字相同");
        assert!(xa.iter().any(|x| x.contains("<NAME-NOTE>")), "名稱段確實在裡面（替換有作用）");
        assert_ne!(persona_args(&a, na, md, None), persona_args(&b, nb, md, None), "沒替換前名稱不同");
    }

    #[test]
    fn persona_per_kind() {
        use super::{agent_name_note, child_agent_rules, persona_args, toml_basic_string};
        let mut b = bot("claude", None, None, false);
        let rule = child_agent_rules();
        let note = agent_name_note("proj-abc123");
        b.persona = Some("回覆結尾一律加上 [PERSONA-OK]".into());
        let want = format!("{rule}\n\n回覆結尾一律加上 [PERSONA-OK]\n\n{note}");
        assert_eq!(persona_args(&b, "proj-abc123", None, None), vec!["--append-system-prompt".to_string(), want.clone()]);
        b.kind = "grok".into();
        assert_eq!(persona_args(&b, "proj-abc123", None, None), vec!["--rules".to_string(), want.clone()]);
        b.kind = "codex".into();
        b.persona = Some("line1\nsay \"hi\" \\ done".into());
        let want = format!("{rule}\n\nline1\nsay \"hi\" \\ done\n\n{note}");
        assert_eq!(
            persona_args(&b, "proj-abc123", None, None),
            vec!["-c".to_string(), format!("developer_instructions={}", toml_basic_string(&want))],
            "沒設 [agents]：codex 照舊讀 AGENTS.md"
        );
        let args = persona_args(&b, "proj-abc123", Some("AGENT-MD"), None);
        assert_eq!(&args[2..], ["-c", "project_doc_max_bytes=0"], "§6.5i：設了 [agents] 就不讀 AGENTS.md");
        assert_eq!(args[1], format!("developer_instructions={}", toml_basic_string(&format!("{rule}\n\nAGENT-MD\n\nline1\nsay \"hi\" \\ done\n\n{note}"))));
        b.kind = "claude".into();
        b.persona = None;
        assert_eq!(persona_args(&b, "proj-abc123", None, None), vec!["--append-system-prompt".to_string(), format!("{rule}\n\n{note}")]);
        assert!(!rule.contains("proj-abc123"), "共用的規則本文不寫任何一顆的名字");
        assert!(rule.contains("**一律用 agent-browser**"));
        assert!(rule.contains("**禁止**用 ego lite"));
        assert!(rule.contains("`--session $AM_AGENT_NAME`"));
        assert!(rule.contains("`agent-browser --session $AM_AGENT_NAME close`"));
    }

    #[test]
    fn claude_persona_args_point_at_the_staged_file() {
        let mut b = bot("claude", None, None, false);
        b.persona = Some("PERSONA".into());
        assert_eq!(
            super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), Some("/data/bots/b1/persona.md")),
            vec!["--append-system-prompt-file".to_string(), "/data/bots/b1/persona.md".to_string()],
            "argv 只帶檔案路徑，不帶任何指示內容"
        );
    }

    #[test]
    fn codex_persona_args_point_at_the_staged_profile() {
        let mut b = bot("codex", None, None, false);
        b.persona = Some("PERSONA".into());
        assert_eq!(super::persona_args(&b, "proj-abc123", None, Some("am-bot-b1")), vec!["-p", "am-bot-b1"]);
        assert_eq!(
            super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), Some("am-bot-b1")),
            vec!["-p", "am-bot-b1", "-c", "project_doc_max_bytes=0"],
            "設了 [agents] 照舊關掉 AGENTS.md"
        );
    }

    /// #901：頂層 bot 的啟動參數經 `fit_command_line`（900 bytes 預算）之後，claude／codex 的指示不能被砍成「…（後略）」——
    /// persona 再長，argv 裡也只有短路徑／profile 名。
    #[test]
    fn staged_persona_launch_args_survive_the_command_line_budget_untouched() {
        let long = "完整的人設內容".repeat(400);
        for (kind, staged) in [("claude", "/data/bots/01HZZZZZZZZZZZZZZZZZZZZZZZ/persona.md"), ("codex", "am-bot-01HZZZZZZZZZZZZZZZZZZZZZZZ")] {
            let mut b = bot(kind, Some("some-model"), None, true);
            b.persona = Some(long.clone());
            let mut args = vec!["--dangerously-skip-permissions".to_string()];
            args.extend(super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), Some(staged)));
            args.extend(["--model".to_string(), "some-model".to_string()]);
            let fitted = crate::herdr::fit_command_line(args.clone());
            assert_eq!(fitted, args, "{kind}: nothing needed trimming");
            assert!(!fitted.iter().any(|a| a.contains("後略")), "{kind}: {fitted:?}");
            assert!(fitted.iter().all(|a| !a.contains("AG Man 規則") && !a.contains(&long)), "{kind}: the text itself stays out of argv");
        }
        let mut b = bot("claude", None, None, false);
        b.persona = Some(long.clone());
        let inline = crate::herdr::fit_command_line(super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), None));
        assert!(inline.iter().any(|a| a.ends_with("後略）")), "{inline:?}");
    }

    #[test]
    fn codex_keeps_project_docs_when_the_cli_docs_are_not_disabled() {
        let b = bot("codex", None, None, false);
        let on = super::persona_args_cli_docs(&b, "proj-abc123", Some("AGENT-MD"), None, true);
        assert_eq!(&on[2..], ["-c", "project_doc_max_bytes=0"]);
        let off = super::persona_args_cli_docs(&b, "proj-abc123", Some("AGENT-MD"), None, false);
        assert_eq!(off.len(), 2, "有指示檔讀不到：讀得到的 agent md 照注入，但不關 AGENTS.md：{off:?}");
        assert!(off[1].contains("AGENT-MD"));
        assert_eq!(super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), None), on, "既有簽名：有 agent md 就關");
    }

    /// 2026-09-13 使用者要求：人設與注入提示一律用命令語氣。客氣的寫法（「請…」「不要…比較好」）
    /// agent 會當成建議，實際上不照做；硬規則要寫成命令才會被執行。
    #[test]
    fn every_bot_is_told_to_kill_only_its_own_pid_or_port_and_to_put_pkill_options_first() {
        let rule = super::child_agent_rules();
        assert!(rule.contains("只准殺自己記下的 PID 或 port"), "{rule}");
        assert!(rule.contains("lsof -ti :PORT | xargs kill"));
        assert!(rule.contains("`pkill -f next-server -P 1`") && rule.contains("`pkill -P 1 -f next-server`"), "附對／錯寫法");
        assert!(rule.contains("kill -9 -1") && rule.contains("killall"));
        assert!(rule.contains("選項**必須全部寫在比對字串之前**"));
    }

    #[test]
    fn the_rules_read_as_orders_not_suggestions() {
        let rule = super::child_agent_rules();
        assert!(rule.contains("硬規則，不是建議"), "the register is stated up front");
        assert!(rule.contains("必須"));
        assert!(rule.contains("禁止"));
        assert!(!rule.contains("請"), "no polite softeners in an injected rule");
    }

    /// #1941：每顆 bot 的注入文字共用同一段前綴（provider 的 prompt cache 才能共用）。
    /// 除了最後一段（這顆 bot 的名字）以外，兩顆不同名字的 bot 注入的文字逐字相同。
    #[test]
    fn bots_with_different_names_share_every_injected_byte_but_the_last_paragraph() {
        use super::{child_agent_rules, persona_text};
        let a = bot("claude", None, None, false);
        let b = bot("claude", None, None, false);
        let (ta, tb) = (persona_text(&a, "proj-abc123", Some("AGENT-MD")), persona_text(&b, "hub-kytpg9", Some("AGENT-MD")));
        let (head_a, note_a) = ta.rsplit_once("\n\n").expect("名字在最後一段");
        let (head_b, note_b) = tb.rsplit_once("\n\n").expect("名字在最後一段");
        assert_eq!(head_a, head_b, "除最後一段外逐字相同");
        assert_ne!(note_a, note_b);
        assert!(note_a.contains("`proj-abc123`") && note_b.contains("`hub-kytpg9`"), "{note_a} / {note_b}");
        assert!(!head_a.contains("proj-abc123") && !head_a.contains("hub-kytpg9"), "前段不寫任何一顆的名字");
        assert!(!child_agent_rules().contains("proj-abc123"));
    }

    /// #1952：子 agent 的指示檔寫在母 bot 的目錄裡，最後一段只能是靜態文字、不寫母 bot 的名字。
    /// 子 agent 的名字是 shim 開 pane 時設好的 `$AM_AGENT_NAME`（全名），不是這裡拿到的任何值。
    #[test]
    fn the_child_name_note_is_static_and_never_names_the_parent() {
        use super::{child_agent_name_note, child_instructions};
        let md = child_instructions("AGENT-MD");
        assert!(md.ends_with(child_agent_name_note()), "{md}");
        assert!(md.contains("AGENT-MD") && !md.contains("proj-abc123"), "{md}");
        assert!(child_agent_name_note().contains("`$AM_AGENT_NAME`"), "{}", child_agent_name_note());
        assert!(!child_agent_name_note().contains('{') && !child_agent_name_note().contains('}'), "沒有格式化的插值");
    }

    /// #1952：頂層 bot 的 pane 環境不動，`$AM_AGENT_NAME` 還是 bot 自己的名字（子 agent 全名只在 shim 開 child 時換）。
    /// 解析原始碼（同 `every_env_key_the_daemon_injects…`）：pattern 用 concat 拆開，免得比到測試自己。
    #[test]
    fn the_top_level_pane_keeps_its_own_agent_name() {
        let src = include_str!("setup.rs");
        let needle = concat!("env.insert(\"AM_AGENT_NAME\"", ".into(), json!(agent_name));");
        assert_eq!(src.matches(needle).count(), 1, "pane_env_for_fence 應該仍只注入一次 bot 自己的名字");
    }

    /// 使用者 2026-09-30：派工只走 herdr pane（內建子代理追不到），子 agent 不再開子 agent、要人手找 parent。
    #[test]
    fn children_go_through_herdr_panes_and_never_spawn_grandchildren() {
        let rule = super::child_agent_rules();
        for want in ["`Agent`／`Task`", "`Workflow`", "`codex-rescue`", "codex 的子代理", "只准用 herdr pane 派工", "`--kind codex`"] {
            assert!(rule.contains(want), "要點名禁止內建子代理：{want}");
        }
        assert!(rule.contains("`herdr agent start <名稱> --kind … --pane <新 pane>`"), "要說明唯一的開法");
        assert!(rule.contains("**禁止** `herdr pane run <pane> \"claude …\"`"), "要點名禁止 pane run 直接起 CLI");
        assert!(rule.contains("沒有名稱，AG Man 認領不到"), "要講原因");
        assert!(rule.contains("$AM_CHILD_OF"), "子 agent 要知道怎麼認出自己");
        assert!(rule.contains("禁止再開子 agent"));
        assert!(rule.contains("由 parent 決定"));
        assert!(rule.contains("你是 `$AM_AGENT_NAME` 的子 agent"), "派工 prompt 要帶上的那句話，名字用環境變數");
    }

    /// issue #1789：fork 出來的 bot 會把原 bot 的名字當成自己的（對話歷史裡有），規則要說清楚那不是自己，改名也一律用自己的前綴。
    #[test]
    fn a_name_in_forked_history_is_not_ours() {
        let rule = super::child_agent_rules();
        for want in ["對話歷史裡若出現別的 agent 名稱", "那是原 bot、不是你", "原 bot 的子 agent 不歸你", "改名一律用 `$AM_AGENT_NAME-` 前綴"] {
            assert!(rule.contains(want), "要說清楚 fork 來的名字不是自己：{want}\n{rule}");
        }
    }

    /// issue #1675：codex bot 花了一整個回合讀 `herdr_shim.rs` 與 API 文件才弄懂怎麼開 codex 子 agent。
    /// 規則要直接給可照抄的指令：全新題目、`fork --last`、旗標位置，以及不准自己補 shim 會補的參數。
    #[test]
    fn the_rules_give_a_copy_paste_way_to_start_a_codex_child() {
        let rule = super::child_agent_rules();
        for want in [
            "herdr agent start <名稱> --kind codex --pane <新 pane>",
            "herdr agent start <名稱> --kind codex --pane <新 pane> -- fork --last",
            "-- --model <模型> fork --last",
            "**啟動不帶派工 prompt**",
            "herdr agent get <名稱>",
            "herdr agent prompt <名稱>",
            "**禁止直接重開或對未確認的名稱派工**",
            "`--` 之後、子命令",
            "`--no-daemon`",
            "**禁止**自己補，也**禁止**用真 herdr 的絕對路徑繞過它",
        ] {
            assert!(rule.contains(want), "要給 codex 子 agent 的照抄寫法：{want}\n{rule}");
        }
    }

    /// 使用者 2026-10-03：CLI 要 OAuth 時一律走不開瀏覽器的模式，互動式指令放進 shell pane，網址／碼交給使用者後結束回合。
    #[test]
    fn oauth_logins_never_open_a_browser_and_wait_in_a_shell_pane() {
        let rule = super::child_agent_rules();
        for want in ["gcloud auth login --no-browser", "--use-device-code", "firebase login --no-localhost", "GH_BROWSER=echo", "--no-launch-browser"] {
            assert!(rule.contains(want), "要點名旗標：{want}");
        }
        assert!(!rule.contains("替使用者登入"), "「禁止替使用者登入」已拿掉");
        assert!(!rule.contains("搬瀏覽器的 cookie"), "舊規則要拿掉");
        assert!(rule.contains("**登入態**（使用者 2026-10-15）") && rule.contains("交給你的 session 接著執行"));
        assert!(rule.contains("herdr pane split --pane \"$HERDR_PANE_ID\""), "互動式指令放 pane，不在 shell 工具裡卡住");
        assert!(rule.contains("herdr pane send-text"));
        assert!(rule.contains("**結束這一回合**"));
    }

    /// §6.5f（使用者 2026-09-16 裁示）：每顆 bot 都讀得到輸出規則——claude 的 skill 與三種 kind 的 persona
    /// 都用這份文字，所以不在 agents-manager 專案裡的 bot 也拿得到。
    #[test]
    fn every_bot_is_told_where_user_files_go_and_what_never_goes_there() {
        let rule = super::child_agent_rules();
        assert!(rule.contains("scratchpad 只放中間產物"), "{rule}");
        assert!(rule.contains("$AM_OUTBOX"));
        assert!(rule.contains("mkdir -p \"$AM_OUTBOX\""), "空目錄會被清掉，寫之前要自己建");
        assert!(rule.contains("1 小時"));
        assert!(rule.contains("reports/"));
        for banned in [".pem", ".key", ".sqlite", ".db", "DB 複本"] {
            assert!(rule.contains(banned), "禁放清單要列出 {banned}");
        }
        let mut b = bot("claude", None, None, false);
        for kind in ["claude", "grok", "codex"] {
            b.kind = kind.into();
            let args = super::persona_args(&b, "proj-abc123", None, None).join(" ");
            assert!(args.contains("$AM_OUTBOX"), "{kind}: {args}");
        }
        let doc = super::herdr_skill_doc("---\nname: herdr\ndescription: x\n---\n# herdr\n");
        assert!(doc.contains("$AM_OUTBOX"), "{doc}");
    }

    #[test]
    fn grok_can_read_the_full_rules_from_a_json_quoted_path_without_inline_markdown_injection() {
        let mut b = bot("grok", None, None, false);
        b.persona = Some("persona stays in the staged file".into());
        let path = "/tmp/AG Man `stop` \"quoted\" \\ path/grok-rules.md";
        let args = super::persona_args(&b, "proj-abc123", Some("AGENT-MD"), Some(path));
        assert_eq!(args[0], "--rules");
        assert!(args[1].len() < 300, "the reference stays below Herdr's shell limit: {}", args[1].len());
        assert!(args[1].contains(&serde_json::to_string(path).unwrap()), "path is encoded as one JSON string: {}", args[1]);
        assert!(!args[1].contains(&format!("`{path}`")), "backticks in a path cannot break out of inline code: {}", args[1]);
        assert!(!args[1].contains("persona stays in the staged file"), "full persona text is staged, not truncated in argv");
    }

    #[test]
    fn grok_home_keeps_a_root_directory_override_absolute() {
        assert_eq!(super::grok_home(&serde_json::json!({"GROK_HOME": "/"}), "/home/tester"), "/");
    }
}

#[cfg(test)]
mod remote_hook_tests {
    //! SPEC §11.4.2 — runs `REMOTE_HOOK_SH` with `/bin/sh` against a fake `$HOME` and `herdr`.
    //! The shell parses top-level event hints; authoritative classification is `hookrecv::classify`.
    use std::io::Write as _;
    use crate::test_dirs as tt;
    use std::process::{Command, Stdio};

    struct Sandbox {
        dir: std::path::PathBuf,
        bot: String,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// 遠端 hook 一則一檔（`hook-spool.d/*.json`）。舊的 jsonl 還在就接在後面，升級當下沒掃完的不會不見。
    fn spool_events(bot_dir: &std::path::Path) -> String {
        let mut out = String::new();
        let mut files = Vec::new();
        if let Ok(rd) = std::fs::read_dir(bot_dir.join("hook-spool.d")) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) == Some("json") {
                    files.push(p);
                }
            }
        }
        files.sort();
        for p in files {
            let mut s = std::fs::read_to_string(&p).unwrap_or_default();
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            out.push_str(&s);
        }
        if let Ok(legacy) = std::fs::read_to_string(bot_dir.join("hook-spool.jsonl")) {
            out.push_str(&legacy);
        }
        out
    }

    fn write_exec(path: &std::path::Path, body: &str) {
        tt::write_exec(path, body);
    }

    impl Sandbox {
        /// `with_herdr = false` is the §11.4.2 "no herdr on this host" path (acceptance H5).
        fn new(with_herdr: bool) -> Self {
            Self::with_root(with_herdr, crate::hosts::REMOTE_ROOT)
        }

        /// 用哪個遠端根產生腳本（正式實例＝`REMOTE_ROOT`，隔離實例＝`instances/<slug>`）。
        fn with_root(with_herdr: bool, root: &str) -> Self {
            let dir = tt::track(std::env::temp_dir().join(format!("am-hook-{}", crate::db::ulid())));
            std::fs::create_dir_all(&dir).unwrap();
            write_exec(&dir.join("hook.sh"), &super::remote_hook_sh(root));
            if with_herdr {
                write_exec(
                    &dir.join("herdr"),
                    "#!/bin/sh\n{ for a in \"$@\"; do printf '%s ' \"$a\"; done; printf '\\n'; } >> \"$AM_TEST_LOG\"\n",
                );
            }
            Sandbox { dir, bot: "b-test".into() }
        }

        fn bot_dir(&self) -> std::path::PathBuf {
            self.dir.join(".config/agents-manager/bots").join(&self.bot)
        }

        fn read(&self, name: &str) -> String {
            if name == "hook-spool.jsonl" {
                return spool_events(&self.bot_dir());
            }
            std::fs::read_to_string(self.bot_dir().join(name)).unwrap_or_default()
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("herdr.log"))
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        }

        fn wait_calls(&self, expected: usize) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let calls = self.calls();
                if calls.len() >= expected || std::time::Instant::now() >= deadline {
                    return calls;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        /// Runs `hook.sh <argv…>` with `stdin`, answering `(stdout, exit_ok)`.
        fn run(&self, argv: &[&str], stdin: &str) -> (String, bool) {
            self.run_with_env(argv, stdin, &[])
        }

        /// Same, with extra pane env on top of the fixed test environment.
        fn run_with_env(&self, argv: &[&str], stdin: &str, extra: &[(&str, &str)]) -> (String, bool) {
            self.run_with_env_bytes(argv, stdin.as_bytes(), extra)
        }

        /// Byte input verifies that invalid provider UTF-8 still produces a valid spool JSON line.
        fn run_with_env_bytes(&self, argv: &[&str], stdin: &[u8], extra: &[(&str, &str)]) -> (String, bool) {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(self.dir.join("hook.sh"));
            cmd.args(argv);
            cmd.env_clear();
            cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            cmd.env("HOME", &self.dir);
            cmd.env("AM_TEST_LOG", self.dir.join("herdr.log"));
            cmd.env("HERDR_PANE_ID", "p1");
            cmd.env("HERDR_SESSION", "am-test");
            let herdr = self.dir.join("herdr");
            if herdr.exists() {
                cmd.env("AM_REAL_HERDR", &herdr);
            }
            for (k, v) in extra {
                cmd.env(k, v);
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut ch = cmd.spawn().unwrap();
            match ch.stdin.take().unwrap().write_all(stdin) {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                r => r.unwrap(),
            }
            let out = ch.wait_with_output().unwrap();
            (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
        }
    }

    const STOP: &str = r#"{"hook_event_name":"Stop","session_id":"s-1","transcript_path":"/tmp/t.jsonl","stop_hook_active":false}"#;

    #[test]
    fn macos_local_codex_stop_reads_large_stdin_and_reports_the_native_session() {
        let sb = Sandbox::new(true);
        let payload = serde_json::json!({
            "hook_event_name": "Stop", "session_id": "codex-stdin", "turn_id": "native-turn",
            "stop_hook_active": false, "last_assistant_message": "x".repeat(256 * 1024),
        });
        let text = payload.to_string();
        assert!(text.len() > 128 * 1024);
        let (stdout, ok) = sb.run(&["codex", &sb.bot, "-"], &text);
        assert!(ok);
        assert!(stdout.is_empty());
        let spool = sb.read("hook-spool.jsonl");
        let events: Vec<serde_json::Value> = spool.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["payload"], payload);
        assert_eq!(events[0]["truncated"], false);
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("--state idle "));
        assert!(calls[0].contains("--agent-session-id codex-stdin"));
    }

    #[test]
    fn macos_local_codex_legacy_argv_and_active_stop_stdin_keep_their_semantics() {
        let sb = Sandbox::new(false);
        let legacy = r#"{"type":"agent-turn-complete","thread-id":"legacy","turn-id":"one","last-assistant-message":"reply"}"#;
        assert!(sb.run(&["codex", &sb.bot, "-", legacy], "").1);
        let body: serde_json::Value = serde_json::from_str(sb.read("hook-spool.jsonl").trim()).unwrap();
        assert_eq!(body["payload"], serde_json::from_str::<serde_json::Value>(legacy).unwrap());
        let active = serde_json::json!({
            "hook_event_name": "Stop", "session_id": "active", "turn_id": "two",
            "stop_hook_active": true, "last_assistant_message": "intermediate",
        });
        assert!(sb.run(&["codex", &sb.bot, "-"], &active.to_string()).1);
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 2);
        assert!(sb.calls().is_empty());
    }

    /// issue #753：Stop 的 payload 不帶使用者訊息，agm-host 又讀不到遠端的 transcript，遲到的 Stop 就證明不了「是同一回合」。
    /// `hook.sh` 在 spool 前從**本機** transcript 讀出最後一則使用者訊息，放進 payload 的 `agm_user_text`：
    /// 跳過 tool_result 與 meta 那種不是人打的條目；讀不到、沒有 python3、不是 Stop 就原樣不動（沒有證據，daemon 照舊不蓋）。
    #[test]
    fn a_remote_stop_carries_the_last_user_message_from_the_local_transcript() {
        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        let line = |v: serde_json::Value| format!("{v}\n");
        std::fs::write(
            &transcript,
            [
                line(serde_json::json!({"type": "user", "message": {"role": "user", "content": "舊的一句"}})),
                line(serde_json::json!({"type": "user", "message": {"role": "user", "content": "現在部 \"demo\"\n第二行"}})),
                line(serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "好"}]}})),
                line(serde_json::json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}})),
                line(serde_json::json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "meta"}})),
            ]
            .concat(),
        )
        .unwrap();
        let stop = |path: &str, event: &str| {
            serde_json::json!({"hook_event_name": event, "session_id": "s-1", "transcript_path": path, "stop_hook_active": false, "last_assistant_message": "回覆 ✓"}).to_string()
        };
        let spooled = |sb: &Sandbox| -> serde_json::Value {
            let events = sb.read("hook-spool.jsonl");
            let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone()
        };

        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &stop(&transcript.to_string_lossy(), "Stop"));
        assert!(ok);
        let p = spooled(&sb);
        assert_eq!(p["agm_user_text"], "現在部 \"demo\"\n第二行", "{p}");
        assert_eq!(p["last_assistant_message"], "回覆 ✓", "原本的欄位不動：{p}");
        assert_eq!(p["session_id"], "s-1");

        let sb2 = Sandbox::new(false);
        sb2.run(&["claude", &sb2.bot, "-"], &stop("/Users/nobody/none.jsonl", "Stop"));
        let p = spooled(&sb2);
        assert!(p.get("agm_user_text").is_none(), "{p}");
        assert_eq!(p["session_id"], "s-1");

        let sb3 = Sandbox::new(false);
        sb3.run(&["claude", &sb3.bot, "-"], &stop(&transcript.to_string_lossy(), "SessionStart"));
        assert!(spooled(&sb3).get("agm_user_text").is_none());
    }

    /// #1283：auto-compact 的摘要（`isCompactSummary`）、slash 指令的回音、sidechain 都不是人打的一問：跳過，
    /// 回到真正那句 `跑完整測試`（與本機 `user_prompt_raw` 同一套規則）。
    #[test]
    fn a_remote_stop_skips_compact_summaries_and_command_echoes() {
        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        let line = |v: serde_json::Value| format!("{v}\n");
        std::fs::write(
            &transcript,
            [
                line(serde_json::json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "跑完整測試"}})),
                line(serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "好"}]}})),
                line(serde_json::json!({"type": "user", "isCompactSummary": true, "isVisibleInTranscriptOnly": true, "message": {"role": "user", "content": "This session is being continued from a previous conversation"}})),
                line(serde_json::json!({"type": "user", "message": {"role": "user", "content": "<command-name>/usage</command-name>"}})),
                line(serde_json::json!({"type": "user", "isSidechain": true, "message": {"role": "user", "content": "子代理的提示"}})),
                line(serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "完成"}]}})),
            ]
            .concat(),
        )
        .unwrap();
        let stop = serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": transcript.to_string_lossy(), "stop_hook_active": false, "last_assistant_message": "完成"}).to_string();

        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &stop);
        assert!(ok);
        let events = sb.read("hook-spool.jsonl");
        let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
        let p = serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone();
        assert_eq!(p["agm_user_text"], "跑完整測試", "{p}");
        assert_eq!(p["agm_origin_kind"], "human", "{p}");
    }

    /// #1759：遠端壓縮做完的 `SessionStart`（source=compact）帶上那台 transcript 最後一個 compact_boundary（含閒置提示）。
    #[test]
    fn a_remote_compact_session_start_carries_the_last_boundary() {
        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        let lines = [
            serde_json::json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "manual", "preTokens": 5000}}),
            serde_json::json!({"type": "system", "subtype": "informational", "content": "Compacted while idle, before the prompt cache expired"}),
            serde_json::json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "auto", "preTokens": 286047, "postTokens": 11362}}),
            serde_json::json!({"type": "user", "isCompactSummary": true, "message": {"content": "summary"}}),
            serde_json::json!({"type": "system", "subtype": "informational", "content": "Compacted while idle, before the prompt cache expired"}),
        ];
        std::fs::write(&transcript, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        let payload = |source: &str| {
            serde_json::json!({"hook_event_name": "SessionStart", "source": source, "session_id": "s-1", "transcript_path": transcript.to_string_lossy()}).to_string()
        };
        let spooled = |sb: &Sandbox| -> serde_json::Value {
            let events = sb.read("hook-spool.jsonl");
            let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone()
        };
        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &payload("compact"));
        assert!(ok);
        assert_eq!(
            spooled(&sb)["agm_compact"],
            serde_json::json!({"trigger": "auto", "idle": true, "pre_tokens": 286047, "post_tokens": 11362})
        );
        let sb = Sandbox::new(false);
        sb.run(&["claude", &sb.bot, "-"], &payload("resume"));
        assert!(spooled(&sb).get("agm_compact").is_none());
    }

    /// 2026-10-02：遠端 transcript 在那台機器上，`hook.sh` 在 Stop／StopFailure 前把已答完（或取消）的 `AskUserQuestion`
    /// 讀出來放進 `agm_asks`；沒答完的、別的工具的結果不帶；孤立 surrogate 不能弄壞 spool 那一行。
    #[test]
    fn a_remote_stop_carries_the_finished_ask_user_question_records() {
        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        let qs = serde_json::json!([{"question": "拋單怎麼處理？", "header": "拋單倉庫", "options": []}, {"question": "go API 放哪？", "header": "go API", "options": []}]);
        let ask = |id: &str| serde_json::json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": qs}}]}});
        let lines = [
            ask("t1"),
            serde_json::json!({"type": "user", "timestamp": "2026-10-02T08:34:54.635Z", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "Your questions have been answered"}]},
                "toolUseResult": {"questions": qs, "answers": {"拋單怎麼處理？": "拿掉\"拋單\"\n第二行", "go API 放哪？": "自訂：gateway"}, "annotations": {"go API 放哪？": {"notes": "備註"}}}}),
            ask("t2"),
            serde_json::json!({"type": "user", "timestamp": "2026-10-02T08:40:00.000Z", "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "content": "The user did not answer the questions."}]},
                "toolUseResult": {"questions": qs}}),
            ask("t3"),
        ];
        std::fs::write(&transcript, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        let payload = |event: &str| serde_json::json!({"hook_event_name": event, "session_id": "s-1", "transcript_path": transcript.to_string_lossy(), "stop_hook_active": false}).to_string();
        let spooled = |sb: &Sandbox| -> serde_json::Value {
            let events = sb.read("hook-spool.jsonl");
            let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone()
        };

        for event in ["Stop", "StopFailure"] {
            let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &payload(event));
            assert!(ok);
            let p = spooled(&sb);
            let asks = p["agm_asks"].as_array().unwrap_or_else(|| panic!("{event}: {p}"));
            assert_eq!(asks.len(), 2, "t3 還在等人，不帶：{p}");
            assert_eq!(asks[0]["id"], "t1");
            assert_eq!(asks[0]["at"], "2026-10-02T08:34:54.635Z");
            assert_eq!(asks[0]["items"][0], serde_json::json!({"question": "拋單怎麼處理？", "header": "拋單倉庫", "answer": "拿掉\"拋單\"\n第二行"}));
            assert_eq!(asks[0]["items"][1]["notes"], "備註");
            assert_eq!(asks[1]["id"], "t2");
            assert!(asks[1]["items"].as_array().unwrap().iter().all(|i| i["answer"].is_null()), "取消＝沒有回答：{p}");
        }

        let sb = Sandbox::new(false);
        sb.run(&["claude", &sb.bot, "-"], &payload("SessionStart"));
        assert!(spooled(&sb).get("agm_asks").is_none());
        let sb = Sandbox::new(false);
        sb.run(&["claude", &sb.bot, "-"], &serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": "/Users/nobody/none.jsonl"}).to_string());
        assert!(spooled(&sb).get("agm_asks").is_none());

        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                ask("t1"),
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"x\"}]},\"toolUseResult\":{\"answers\":{\"拋單怎麼處理？\":\"切斷 \\ud83d\"}}}"
            ),
        )
        .unwrap();
        let lone_surrogate = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "s-1",
            "transcript_path": transcript.to_string_lossy(),
            "stop_hook_active": false,
        })
        .to_string();
        sb.run(&["claude", &sb.bot, "-"], &lone_surrogate);
        let p = spooled(&sb);
        assert!(p.get("agm_asks").is_none(), "{p}");
        assert_eq!(p["session_id"], "s-1");
    }

    /// transcript 裡的使用者訊息帶孤立的 surrogate 跳脫（`\\ud83d`，貼上的字被 UTF-16 切在 emoji 中間）：Python 的
    /// `json.dumps` 會原樣寫回去，但 serde_json 讀不了孤立 surrogate——整行 spool 解析失敗、Stop 事件整筆被丟，
    /// 比沒帶證據更糟。帶不進去就不帶，事件本身一定要是合法 JSON。
    #[test]
    fn a_lone_surrogate_in_the_transcript_never_breaks_the_spool_line() {
        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join("t.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"貼上的字 \\ud83d 被切斷\"}}\n").unwrap();
        let stop = serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": transcript.to_string_lossy(), "stop_hook_active": false, "last_assistant_message": "回覆"}).to_string();
        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &stop);
        assert!(ok);
        let events = sb.read("hook-spool.jsonl");
        let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行");
        let v: serde_json::Value = serde_json::from_str(last).unwrap_or_else(|e| panic!("spool 行不是 serde_json 讀得了的 JSON（{e}）：{last}"));
        assert_eq!(v["payload"]["session_id"], "s-1");
        assert_eq!(v["payload"]["last_assistant_message"], "回覆");
    }

    /// issue #754：背景工作完成喚醒的那一輪沒有新 prompt，transcript 最後一則使用者訊息是上一句。
    /// `hook.sh` 另外帶 `agm_origin_kind`（最新一筆有 `origin.kind` 的使用者條目），daemon 才分得出來；工具結果沒有 origin，不算起點。
    #[test]
    fn a_remote_stop_carries_who_started_the_turn() {
        let line = |v: serde_json::Value| format!("{v}\n");
        let stop = |path: &std::path::Path| {
            serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": path, "stop_hook_active": false}).to_string()
        };
        let spooled = |sb: &Sandbox| -> serde_json::Value {
            let events = sb.read("hook-spool.jsonl");
            let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone()
        };
        let human = |t: &str| line(serde_json::json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": t}}));
        let tool_result = line(serde_json::json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}}));
        let wake = line(serde_json::json!({"type": "user", "origin": {"kind": "task-notification"}, "message": {"role": "user", "content": "<task-notification>done</task-notification>"}}));
        let reply = line(serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "好"}]}}));

        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let t = project_dir.join("human.jsonl");
        std::fs::write(&t, [human("現在部 demo"), reply.clone(), tool_result.clone()].concat()).unwrap();
        sb.run(&["claude", &sb.bot, "-"], &stop(&t));
        let p = spooled(&sb);
        assert_eq!((p["agm_user_text"].as_str(), p["agm_origin_kind"].as_str()), (Some("現在部 demo"), Some("human")), "{p}");

        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let t = project_dir.join("wake.jsonl");
        std::fs::write(&t, [human("現在部 demo"), reply, wake, tool_result].concat()).unwrap();
        sb.run(&["claude", &sb.bot, "-"], &stop(&t));
        let p = spooled(&sb);
        assert_eq!(p["agm_origin_kind"].as_str(), Some("task-notification"), "{p}");

        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let t = project_dir.join("old.jsonl");
        std::fs::write(&t, line(serde_json::json!({"type": "user", "message": {"role": "user", "content": "舊版"}}))).unwrap();
        sb.run(&["claude", &sb.bot, "-"], &stop(&t));
        let p = spooled(&sb);
        assert_eq!(p["agm_user_text"].as_str(), Some("舊版"), "{p}");
        assert!(p.get("agm_origin_kind").is_none(), "{p}");
    }

    /// 起點那筆條目的時間跟著 `agm_origin_kind` 一起帶來：喚醒那一輪的使用者條目沒有 origin，起點要是它前面那筆 `human`，時間也是那筆的。
    #[test]
    fn a_remote_stop_carries_when_the_starter_was_typed() {
        let line = |v: serde_json::Value| format!("{v}\n");
        let stop = |path: &std::path::Path| {
            serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": path, "stop_hook_active": false}).to_string()
        };
        let spooled = |sb: &Sandbox| -> serde_json::Value {
            let events = sb.read("hook-spool.jsonl");
            let last = events.lines().rev().find(|l| !l.trim().is_empty()).expect("spool 有一行").to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("spool 行是合法 JSON")["payload"].clone()
        };
        let human = line(serde_json::json!({"type": "user", "origin": {"kind": "human"}, "timestamp": "2026-11-22T01:21:00.000Z", "message": {"role": "user", "content": "現在部 demo"}}));
        let reply = line(serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "好"}]}}));
        let wake_without_origin = line(serde_json::json!({"type": "user", "timestamp": "2026-11-22T01:30:09.000Z", "message": {"role": "user", "content": "定時檢查"}}));

        let sb = Sandbox::new(false);
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let t = project_dir.join("wake-no-origin.jsonl");
        std::fs::write(&t, [human, reply, wake_without_origin].concat()).unwrap();
        sb.run(&["claude", &sb.bot, "-"], &stop(&t));
        let p = spooled(&sb);
        assert_eq!(p["agm_origin_kind"].as_str(), Some("human"), "{p}");
        assert_eq!(p["agm_origin_at"].as_str(), Some("2026-11-22T01:21:00.000Z"), "{p}");
    }

    /// 去重 B：Stop 的 transcript 只讀一次、`_am_transcript` 只由一顆 python3 執行（使用者文字與 AskUserQuestion 共用）。
    #[cfg(unix)]
    #[test]
    fn a_stop_reads_the_transcript_in_one_python_run() {
        let sb = Sandbox::new(false);
        let shim = sb.dir.join("shim");
        std::fs::create_dir_all(&shim).unwrap();
        let count = sb.dir.join("py-transcript.count");
        write_exec(
            &shim.join("python3"),
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in *_am_transcript*) echo x >> '{}';; esac; done\nexec /usr/bin/python3 \"$@\"\n",
                count.display()
            ),
        );
        let project_dir = sb.dir.join(".claude/projects/project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let t = project_dir.join("once.jsonl");
        let human = serde_json::json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "只讀一次"}});
        std::fs::write(&t, format!("{human}\n")).unwrap();
        let stop = serde_json::json!({"hook_event_name": "Stop", "session_id": "s-1", "transcript_path": t.to_string_lossy(), "stop_hook_active": false}).to_string();
        let path = format!("{}:/usr/bin:/bin", shim.display());
        sb.run_with_env(&["claude", &sb.bot, "-"], &stop, &[("PATH", &path)]);
        let runs = std::fs::read_to_string(&count).unwrap_or_default().lines().count();
        assert_eq!(runs, 1, "Stop 應只啟動一次讀 transcript 的 python3");
        let spool = spool_events(&sb.bot_dir());
        let p: serde_json::Value = serde_json::from_str(spool.lines().next().unwrap_or_default()).unwrap_or_default();
        assert_eq!(p["payload"]["agm_user_text"].as_str(), Some("只讀一次"), "{spool}");
    }

    /// 真的執行產生出來的腳本：spool 要落在**這個實例**的根底下（sol 三輪）。正式實例的既有路徑不變。
    #[test]
    fn the_generated_hook_spools_under_its_own_instance_root() {
        let default = super::remote_hook_sh(crate::hosts::REMOTE_ROOT);
        assert!(default.contains("DIR=\"$HOME/.config/agents-manager/bots/$BOT\""), "正式實例的腳本路徑不能變");
        assert!(!default.contains("__AM_REMOTE_ROOT__"));

        for slug in [None, Some("a1b2c3d4")] {
            let root = crate::hosts::remote_root_for(slug);
            let sb = Sandbox::with_root(false, &root);
            let (_, ok) = sb.run(&["claude", &sb.bot, "-"], STOP);
            assert!(ok);
            let bot_dir = sb.dir.join(&root).join("bots").join(&sb.bot);
            let line = spool_events(&bot_dir);
            assert!(line.contains("\"Stop\""), "{slug:?}: 沒寫到 {}: {line}", bot_dir.display());
            if slug.is_some() {
                let prod = sb.dir.join(".config/agents-manager/bots").join(&sb.bot);
                assert!(spool_events(&prod).is_empty(), "隔離實例的事件跑進了正式 spool");
            }
        }
    }

    /// #1031：子 agent 的 pane 帶 `AM_CHILD_OF`，它繼承的是母 bot 的身份：遠端 hook 不 spool、也不叫 herdr；沒帶就照舊。
    #[test]
    fn a_child_pane_never_spools_a_remote_hook_as_its_parent() {
        let sb = Sandbox::new(true);
        let (_, ok) = sb.run_with_env(&["claude", &sb.bot, "-"], STOP, &[("AM_CHILD_OF", "parent-bot")]);
        assert!(ok);
        assert!(spool_events(&sb.bot_dir()).is_empty(), "子 agent 的事件不准寫進母 bot 的 spool");
        assert!(sb.calls().is_empty(), "子 agent 的 pane 不叫 herdr：{:?}", sb.calls());

        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], STOP);
        assert!(ok);
        assert!(!spool_events(&sb.bot_dir()).is_empty(), "沒帶 AM_CHILD_OF 的 pane 照舊送");
    }

    /// #1031：遠端 grok 的 dispatcher 也一樣：子 agent 的 pane 不轉給母 bot 的 `hook.sh`。
    #[test]
    fn a_child_pane_never_reaches_the_remote_grok_hook() {
        let home = tt::track(std::env::temp_dir().join(format!("am-grok-child-{}", crate::db::ulid())));
        let root = crate::hosts::remote_root_for(None);
        let bot_dir = home.join(&root).join("bots/b1");
        std::fs::create_dir_all(&bot_dir).unwrap();
        let marker = bot_dir.join("called");
        write_exec(&bot_dir.join("hook.sh"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
        let disp = home.join("dispatch.sh");
        write_exec(&disp, &super::remote_grok_dispatch_sh(&root, None));
        let run = |child: Option<&str>| -> bool {
            let _ = std::fs::remove_file(&marker);
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(&disp).env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &home);
            cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "t");
            if let Some(c) = child {
                cmd.env("AM_CHILD_OF", c);
            }
            assert!(cmd.status().unwrap().success());
            marker.exists()
        };
        assert!(!run(Some("parent-bot")), "子 agent 的 pane 不轉給母 bot 的 hook");
        assert!(run(None), "沒有 AM_CHILD_OF 照舊轉");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// issue #43／#495：token 不上命令列。dispatcher 仍然用 `AM_HOOK_TOKEN` 判斷「這是不是 daemon 開的
    /// pane」（`inject_hooks = false` 就是靠它擋掉），但不把值傳下去：遠端那支 `hook.sh` 根本不讀第三個
    /// 參數（`: "$TOKEN"`），本機的 `hook_cmd::hook_token()` 旗標空著時會自己去讀 env。
    #[test]
    fn neither_grok_dispatcher_puts_the_token_on_the_command_line() {
        for slug in [None, Some("a1b2")] {
            let root = crate::hosts::remote_root_for(slug);
            let remote = super::remote_grok_dispatch_sh(&root, slug);
            let local = super::local_grok_dispatch_sh("/x/agents-managerd", "/x/data", slug);
            for s in [&remote, &local] {
                assert!(s.contains("[ -n \"$AM_HOOK_TOKEN\" ]"), "身份判斷要留著：{s}");
                let after_exec = s.split_once("exec").expect("dispatcher 以 exec 收尾").1;
                assert!(!after_exec.contains("$AM_HOOK_TOKEN"), "exec 之後不准再出現 token：{s}");
            }
            assert!(
                remote.contains(&format!("grok \"$AM_BOT_ID\" {}", super::REMOTE_TOKEN_SLOT)),
                "遠端用跟 claude／codex 同一個佔位：{remote}"
            );
            assert!(!local.contains("--token"), "本機走 hook_cmd::hook_token 的 env 回退：{local}");
        }
    }

    /// grok 會把每個實例的 dispatcher 都叫一次：各自只接自己實例的 pane（`AM_INSTANCE`），
    /// 同 id 的 bot 才不會被兩邊各記一次。正式實例的舊 pane 沒有這個變數，照舊歸正式。
    #[test]
    fn each_grok_dispatcher_only_serves_its_own_instance() {
        let home = tt::track(std::env::temp_dir().join(format!("am-grok-{}", crate::db::ulid())));
        let run = |slug: Option<&str>, pane_instance: Option<&str>| -> bool {
            let root = crate::hosts::remote_root_for(slug);
            let bot_dir = home.join(&root).join("bots/b1");
            std::fs::create_dir_all(&bot_dir).unwrap();
            let marker = bot_dir.join("called");
            let _ = std::fs::remove_file(&marker);
            write_exec(&bot_dir.join("hook.sh"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
            let disp = home.join(format!("{}-dispatch.sh", slug.unwrap_or("default")));
            write_exec(&disp, &super::remote_grok_dispatch_sh(&root, slug));
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(&disp).env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &home);
            cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "t");
            if let Some(i) = pane_instance {
                cmd.env("AM_INSTANCE", i);
            }
            assert!(cmd.status().unwrap().success());
            marker.exists()
        };
        assert!(run(None, None), "正式實例接沒有 AM_INSTANCE 的 pane（含升級前開的舊 pane）");
        assert!(!run(None, Some("a1b2")), "正式實例不碰隔離實例的 pane");
        assert!(run(Some("a1b2"), Some("a1b2")));
        assert!(!run(Some("a1b2"), None), "隔離實例不碰正式實例的 pane");
        assert!(!run(Some("a1b2"), Some("zzzz")));

        assert_eq!(super::grok_hooks_file(None), super::GROK_HOOKS_FILE, "正式實例檔名不變");
        assert_eq!(super::grok_hooks_file(Some("a1b2")), "agents-manager-a1b2.json");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// issue #92：遠端的 hook 也要帶這個行程自己的 run id（pane env 的 `AM_RUN_ID`），daemon 才分得出
    /// `--resume` 前後兩個行程送的同一個 session。怪字元一律濾掉：spool 裡一行壞掉的 JSON 會永遠卡在那裡。
    #[test]
    fn the_remote_hook_names_the_run_its_process_was_started_for() {
        let parse = |line: &str| -> crate::hookrecv::HookBody { serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")) };
        let sb = Sandbox::new(false);
        let (_, ok) = sb.run_with_env(&["claude", &sb.bot, "tok"], STOP, &[("AM_RUN_ID", "01RUNREMOTE")]);
        assert!(ok);
        let (_, ok) = sb.run_with_env(&["claude", &sb.bot, "tok"], STOP, &[("AM_RUN_ID", "01RUN\"$(touch pwned)\\x")]);
        assert!(ok);
        let (_, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok);
        let spool = sb.read("hook-spool.jsonl");
        let lines: Vec<&str> = spool.lines().collect();
        assert_eq!(lines.len(), 3, "{spool}");
        assert_eq!(parse(lines[0]).run_id.as_deref(), Some("01RUNREMOTE"));
        assert_eq!(parse(lines[1]).run_id.as_deref(), Some("01RUNtouchpwnedx"), "引號、反斜線、括號都被濾掉，JSON 還是好的");
        assert_eq!(parse(lines[2]).run_id.as_deref().map(str::trim), Some(""), "沒有 AM_RUN_ID：空字串，圍籬當成沒帶");
    }

    #[test]
    fn claude_stop_spools_then_reports_idle() {
        let sb = Sandbox::new(true);
        let (out, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok, "the hook must always exit 0");
        assert_eq!(out, "", "the hook must always keep stdout empty (§4.4)");
        let spool = sb.read("hook-spool.jsonl");
        assert_eq!(spool.lines().count(), 1);
        assert!(spool.contains(r#""bot_id":"b-test""#) && spool.contains(r#""provider":"claude""#));
        assert!(spool.contains(r#""hook_event_name":"Stop""#));
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1, "one report per turn end: {calls:?}");
        let c = &calls[0];
        assert!(c.contains("--session am-test "), "{c}");
        assert!(c.contains("pane report-agent p1 "), "{c}");
        assert!(c.contains("--source agents-manager:b-test "), "{c}");
        assert!(c.contains("--agent claude "), "{c}");
        assert!(c.contains("--state idle "), "{c}");
        assert!(c.contains("--agent-session-id s-1"), "{c}");
        assert!(c.contains("--agent-session-path /tmp/t.jsonl"), "{c}");
        assert!(!c.contains("--message"), "{c}");
    }

    #[test]
    fn claude_stop_failure_spools_then_reports_idle() {
        let sb = Sandbox::new(true);
        let payload = r#"{"hook_event_name":"StopFailure","session_id":"sf-1","reason":"API Error: 500"}"#;
        let (_, ok) = sb.run(&["claude", &sb.bot, "tok"], payload);
        assert!(ok);
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("--state idle ") && calls[0].contains("--agent-session-id sf-1"), "{calls:?}");
        let text = sb.read("hook-spool.jsonl");
        assert!(text.contains("StopFailure"), "failure event is spooled too: {text}");
    }

    #[test]
    fn macos_local_remote_hook_returns_before_a_stalled_herdr_report() {
        let sb = Sandbox::new(true);
        write_exec(
            &sb.dir.join("herdr"),
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$HOME/herdr.pid\"\nexec sleep 120\n",
        );
        let payload = r#"{"type":"agent-turn-complete","thread-id":"slow"}"#;
        let mut command = Command::new("/bin/sh");
        command.env_remove("AM_CHILD_OF");
        command
            .arg(sb.dir.join("hook.sh"))
            .args(["codex", sb.bot.as_str(), "-", payload])
            .env("HOME", &sb.dir)
            .env("HERDR_PANE_ID", "p1")
            .env("AM_TEST_LOG", sb.dir.join("herdr.log"))
            .env("AM_REAL_HERDR", sb.dir.join("herdr"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = std::time::Instant::now();
        let mut child = command.spawn().unwrap();
        let deadline = started + std::time::Duration::from_secs(8);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().ok();
                break child.wait().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let pid_path = sb.dir.join("herdr.pid");
        let pid_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !pid_path.exists() && std::time::Instant::now() < pid_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if let Ok(pid) = std::fs::read_to_string(&pid_path) {
            let _ = Command::new("/bin/kill").args(["-KILL", pid.trim()]).status();
        }
        let diag = format!(
            "{status}; elapsed={:?}; herdr_started={}; hook.log={:?}; spool_d={:?}",
            started.elapsed(),
            pid_path.exists(),
            std::fs::read_to_string(sb.dir.join("hook.log")).unwrap_or_default(),
            std::fs::read_dir(sb.dir.join("hook-spool.d")).map(|d| d.count()).ok()
        );
        assert!(status.success(), "hook must exit successfully without waiting on herdr: {diag}");
        assert!(started.elapsed() < std::time::Duration::from_secs(8), "hook waited for a blocked herdr report: {diag}");
        assert!(sb.read("hook-spool.jsonl").contains("agent-turn-complete"), "spool commits before the report");
        assert!(pid_path.exists(), "the report still starts in the background");
    }

    #[test]
    fn nested_stop_and_grok_shutdown_spool_without_reporting() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"Stop","stop_hook_active":true}"#);
        sb.run(&["grok", &sb.bot, "tok"], r#"{"hookEventName":"stop","reason":"shutdown"}"#);
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 2, "both still spool");
        assert!(sb.calls().is_empty(), "neither is a turn ending: {:?}", sb.calls());
    }

    #[test]
    fn grok_end_turn_reports_idle() {
        let sb = Sandbox::new(true);
        sb.run(&["grok", &sb.bot, "tok"], r#"{"hookEventName":"stop","reason":"end_turn","sessionId":"g-9"}"#);
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("--agent grok ") && calls[0].contains("--state idle "), "{:?}", calls);
        assert!(calls[0].contains("--agent-session-id g-9"), "{:?}", calls);
    }

    /// Token slot `-` (review 2026-09-12 #8) is treated like a real token: spool, then report.
    #[test]
    fn a_placeholder_token_slot_changes_nothing() {
        let sb = Sandbox::new(true);
        let (out, ok) = sb.run(&["claude", &sb.bot, super::REMOTE_TOKEN_SLOT], STOP);
        assert!(ok && out.is_empty());
        let spool = sb.read("hook-spool.jsonl");
        assert_eq!(spool.lines().count(), 1);
        assert!(spool.contains(r#""bot_id":"b-test""#), "{spool}");
        assert!(!spool.contains("tok"), "no token, real or placeholder, is written anywhere: {spool}");
        assert_eq!(sb.wait_calls(1).len(), 1);
        let payload = r#"{"type":"agent-turn-complete","thread-id":"c-4"}"#;
        sb.run(&["codex", &sb.bot, super::REMOTE_TOKEN_SLOT, payload], "");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 2);
        let calls = sb.wait_calls(2);
        assert!(calls[1].contains("--agent-session-id c-4"), "{calls:?}");
    }

    #[test]
    fn codex_takes_the_payload_from_argv() {
        let sb = Sandbox::new(true);
        let payload = r#"{"type":"agent-turn-complete","thread-id":"c-3"}"#;
        sb.run(&["codex", &sb.bot, "tok", payload], "");
        assert!(sb.read("hook-spool.jsonl").contains("agent-turn-complete"));
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("--state idle ") && calls[0].contains("--agent-session-id c-3"), "{calls:?}");
    }

    #[test]
    fn session_start_only_reports_the_session() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"SessionStart","session_id":"s-2"}"#);
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("pane report-agent-session p1 "), "{calls:?}");
        assert!(!calls[0].contains("--state"), "a session start says nothing about the state: {calls:?}");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
    }

    #[test]
    fn remote_event_hints_ignore_nested_fields_and_tolerate_missing_metadata() {
        let sb = Sandbox::new(true);
        sb.run(
            &["claude", &sb.bot, "tok"],
            r#"{"metadata":{"hook_event_name":"Stop","session_id":"nested"},"hook_event_name":"PostToolUse"}"#,
        );
        assert!(sb.calls().is_empty(), "nested data must not impersonate the event: {:?}", sb.calls());

        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"Stop""#);
        assert!(sb.calls().is_empty(), "incomplete JSON is not a Stop event: {:?}", sb.calls());

        sb.run(
            &["claude", &sb.bot, "tok"],
            " \n {\"hook_event_name\":\"SessionStart\",\"session_id\":\"s-start\"} \n",
        );
        let calls = sb.wait_calls(1);
        assert_eq!(calls.len(), 1, "leading whitespace must not turn an object into raw text: {calls:?}");
        assert!(calls[0].contains("pane report-agent-session p1 ") && calls[0].contains("--agent-session-id s-start"), "{calls:?}");

        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"Stop"}"#);
        let calls = sb.wait_calls(2);
        assert_eq!(calls.len(), 2, "missing optional fields must not break event handling: {calls:?}");
        assert!(calls[1].contains("--state idle "), "{calls:?}");
        assert!(!calls[1].contains("--agent-session-id"), "missing session id stays absent: {calls:?}");
        let text = sb.read("hook-spool.jsonl");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in lines {
            let _: crate::hookrecv::HookBody = serde_json::from_str(line).unwrap_or_else(|e| panic!("spool event remains valid JSON ({e}): {line}"));
        }
    }

    #[test]
    fn remote_hook_repairs_invalid_utf8_before_spooling_json() {
        let sb = Sandbox::new(false);
        let mut payload = br#"{"hook_event_name":"PostToolUse","bad":"before"#.to_vec();
        payload.push(0xff);
        payload.extend_from_slice(br#"after"}"#);
        let (_, ok) = sb.run_with_env_bytes(&["claude", &sb.bot, "-"], &payload, &[]);
        assert!(ok);
        let text = sb.read("hook-spool.jsonl");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let body: crate::hookrecv::HookBody = serde_json::from_str(lines[0]).expect("invalid input bytes must become valid JSON");
        assert_eq!(body.payload["bad"], "before\u{fffd}after");
        assert!(!body.truncated, "UTF-8 repair is separate from payload-size truncation");
    }

    #[test]
    fn remote_hook_preserves_escaped_surrogate_pairs_and_repairs_lone_surrogates() {
        let sb = Sandbox::new(false);
        let payload = r#"{"hook_event_name":"PostToolUse","pair":"\ud83d\ude00","lone":"\ud83d"}"#;
        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], payload);
        assert!(ok);
        let text = sb.read("hook-spool.jsonl");
        let line = text.lines().next().expect("one spool line");
        let body: crate::hookrecv::HookBody = serde_json::from_str(line).expect("surrogates must not invalidate spool JSON");
        assert_eq!(body.payload["pair"], "😀", "a valid escaped pair is one Unicode scalar");
        assert_eq!(body.payload["lone"], "\u{fffd}", "a lone surrogate is replaced");
    }

    #[test]
    fn empty_remote_payload_is_spooled_as_json_null() {
        let sb = Sandbox::new(false);
        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], "");
        assert!(ok);
        let text = sb.read("hook-spool.jsonl");
        let line = text.lines().next().expect("empty payload still leaves a spool event");
        let body: crate::hookrecv::HookBody = serde_json::from_str(line).expect("spool line remains valid JSON");
        assert!(body.payload.is_null(), "{body:?}");
    }

    #[test]
    fn without_herdr_it_spools_and_says_so() {
        let sb = Sandbox::new(false);
        let (out, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok);
        assert_eq!(out, "");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
        assert!(sb.read("hook.log").contains("herdr not found; spooled only"));
    }

    /// #652：大行並行寫進同一個 jsonl 會交錯，drain 兩則都丟。一則一檔之後每一則都是完整 JSON。
    #[test]
    fn macos_local_parallel_large_hooks_stay_one_json_each() {
        let sb = Sandbox::new(false);
        let blob = "y".repeat(20_000);
        let payload = format!(r#"{{"hook_event_name":"PostToolUse","blob":"{blob}"}}"#);
        let mut kids = Vec::new();
        for i in 0..24 {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(sb.dir.join("hook.sh"));
            cmd.args(["claude", &sb.bot, "-"]);
            cmd.env_clear();
            cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            cmd.env("HOME", &sb.dir);
            cmd.env("AM_RUN_ID", format!("r{i}"));
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
            let mut ch = cmd.spawn().unwrap();
            ch.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
            kids.push(ch);
        }
        for ch in kids {
            assert!(ch.wait_with_output().unwrap().status.success());
        }
        let text = sb.read("hook-spool.jsonl");
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 24, "24 支 hook 要留下 24 則，實際 {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let body: crate::hookrecv::HookBody = serde_json::from_str(line).unwrap_or_else(|e| panic!("第 {i} 則壞了：{e}"));
            assert_eq!(body.payload["hook_event_name"], "PostToolUse");
            assert_eq!(body.payload["blob"].as_str().map(str::len), Some(20_000));
            assert!(!body.truncated);
        }
        let live = sb.bot_dir().join("hook-spool.jsonl");
        assert!(!live.exists(), "新的 hook 不再追加同一個 jsonl");
    }

    /// #653：截斷後仍以 `{` 開頭。原樣嵌進 JSON 整行無效，`truncated` 沒人讀得到。
    #[test]
    fn macos_local_truncated_object_is_wrapped_so_the_line_parses() {
        let sb = Sandbox::new(false);
        let mut payload = String::from(r#"{"hook_event_name":"Stop","blob":""#);
        payload.push_str(&"a".repeat(1_100_000));
        let (_, ok) = sb.run(&["claude", &sb.bot, "-"], &payload);
        assert!(ok);
        let text = sb.read("hook-spool.jsonl");
        let line = text.lines().next().expect("要有一則");
        let body: crate::hookrecv::HookBody = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: 行長 {}", line.len()));
        assert!(body.truncated, "超過 1 MiB 要標 truncated");
        let raw = body.payload["raw"].as_str().unwrap_or_else(|| panic!("截斷的物件要包成 raw：{body:?}"));
        assert!(raw.starts_with('{'), "原文還在");
        assert!(raw.contains("hook_event_name"));
        assert!(raw.len() <= 1_048_576 + 8, "head -c 之後不該比上限長一截：{}", raw.len());
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// #494：spool 裡是完整的 hook payload（prompt、工具輸入、回覆）。目錄 0700、檔案 0600，
    /// 不交給那台機器的 umask 決定——Linux 預設 022 的話以前是 0644，同機任何使用者都讀得到。
    #[test]
    fn the_generated_hook_keeps_the_spool_private() {
        let sb = Sandbox::new(false);
        let script = format!(
            "umask 022; exec {} claude {} -",
            crate::hosts::sh_quote(&sb.dir.join("hook.sh").to_string_lossy()),
            crate::hosts::sh_quote(&sb.bot),
        );
        let mut ch = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &sb.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(STOP.as_bytes()).unwrap();
        let ok = ch.wait_with_output().unwrap().status.success();
        assert!(ok);
        assert_eq!(mode_of(&sb.bot_dir()), 0o700, "bot 目錄只給自己");
        let spool_dir = sb.bot_dir().join("hook-spool.d");
        assert_eq!(mode_of(&spool_dir), 0o700, "一則一檔的目錄只給自己");
        let file = std::fs::read_dir(&spool_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .expect("spool 檔要在");
        assert_eq!(mode_of(&file), 0o600, "spool 只給自己");
        assert_eq!(mode_of(&sb.dir.join(".config/agents-manager/bots")), 0o700, "中間層也是");
    }

    /// 收緊只管我們自己寫的東西：使用者的 statusLine 指令要在**原本的** umask 底下跑，
    /// 不然它建出來的檔案會莫名其妙變成 0600。
    #[test]
    fn the_users_own_statusline_command_keeps_its_own_umask() {
        let sb = Sandbox::new(true);
        let cfg = sb.dir.join(".claude");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("settings.json"), r#"{"statusLine":{"type":"command","command":"umask"}}"#).unwrap();
        let (out, ok) = sb.run(&["statusline", &sb.bot, "-"], r#"{"cost":{"a":1}}"#);
        assert!(ok);
        let baseline = Command::new("/bin/sh").arg("-c").arg("umask").output().unwrap();
        assert_eq!(out.trim(), String::from_utf8_lossy(&baseline.stdout).trim(), "還原成這個行程原本的 umask");
        assert_eq!(mode_of(&sb.bot_dir().join("hook-status.json")), 0o600, "我們自己的單槽檔還是 0600");
    }

    #[test]
    fn statusline_overwrites_a_single_slot_and_runs_the_user_command() {
        let sb = Sandbox::new(true);
        let cfg = sb.dir.join(".claude");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("settings.json"),
            r#"{"statusLine":{"type":"command","command":"printf USERLINE"}}"#,
        )
        .unwrap();
        sb.run(&["statusline", &sb.bot, "tok"], r#"{"cost":{"a":1}}"#);
        let (out, ok) = sb.run(&["statusline", &sb.bot, "tok"], r#"{"cost":{"b":2}}"#);
        assert!(ok);
        assert_eq!(out, "USERLINE", "stdout is the user's own status line, nothing else");
        let slot = sb.read("hook-status.json");
        assert_eq!(slot.lines().count(), 1, "single slot, not a queue: {slot}");
        assert!(slot.starts_with(r#"{"hook_event_name":"StatusLine","cost":{"b":2}}"#), "{slot}");
        assert_eq!(sb.read("hook-spool.jsonl"), "", "the status line never enters the spool (§11.4.5)");
        assert!(sb.calls().is_empty(), "the status line reports no state");
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], STOP);
        sb.run(&["claude", &sb.bot, "tok"], STOP);
        let calls = sb.wait_calls(2);
        let seqs: Vec<u128> = calls
            .iter()
            .map(|c| {
                c.split(" --seq ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<u128>().unwrap()
            })
            .collect();
        assert_eq!(seqs.len(), 2, "{calls:?}");
        assert!(seqs[1] > seqs[0], "herdr drops a seq that did not grow: {seqs:?}");
    }

    #[test]
    fn without_a_pane_id_it_only_spools() {
        let sb = Sandbox::new(true);
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(sb.dir.join("hook.sh")).args(["claude", &sb.bot, "tok"]);
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("HOME", &sb.dir);
        cmd.env("AM_TEST_LOG", sb.dir.join("herdr.log"));
        cmd.env("AM_REAL_HERDR", sb.dir.join("herdr"));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut ch = cmd.spawn().unwrap();
        ch.stdin.take().unwrap().write_all(STOP.as_bytes()).unwrap();
        assert!(ch.wait().unwrap().success());
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
        assert!(sb.calls().is_empty(), "no pane to report against");
    }
}

#[cfg(test)]
mod pane_env_tests {
    use super::*;
    /// 正式／隔離 × 本機／遠端 × 自訂 env 帶了偽造值、清空值、完全沒帶：結果只看 daemon 自己。
    #[test]
    fn reserved_instance_keys_ignore_whatever_custom_env_says() {
        for instance in [None, Some("a1b2")] {
            for local in [Some("/data/iso"), None] {
                for custom in [Some("forged"), Some(""), None] {
                    let mut env = serde_json::Map::new();
                    if let Some(v) = custom {
                        env.insert("AM_INSTANCE".into(), json!(v));
                        env.insert("AM_DATA_DIR".into(), json!(v));
                    }
                    reserve_instance_env(&mut env, instance, local);
                    let case = format!("instance={instance:?} local={local:?} custom={custom:?}");
                    assert_eq!(env.get("AM_INSTANCE"), instance.map(|s| json!(s)).as_ref(), "{case}");
                    assert_eq!(env.get("AM_DATA_DIR"), local.map(|s| json!(s)).as_ref(), "{case}");
                }
            }
        }
    }
}

#[cfg(test)]
mod claude_settings_tests {
    use super::*;

    #[test]
    fn observation_hooks_use_the_same_dispatcher() {
        let settings = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        for event in ["UserPromptSubmit", "Notification"] {
            assert_eq!(settings["hooks"][event][0]["hooks"][0]["command"], "hook");
        }
        assert!(settings["hooks"].get("PermissionRequest").is_none());
    }

    /// 2026-09-14 review：協調者的 rc off 只靠 `args=[]`，但帳號的全域設定可以把它打開。
    /// 現在每顆 bot 的設定檔都明講，且只有自己 argv 要求過才是 true。
    #[test]
    fn remote_control_is_stated_per_bot_not_inherited_from_the_account() {
        let off = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        assert_eq!(off["remoteControlAtStartup"], json!(false));
        let on = claude_settings("hook", "sl", true, true, "Asia/Taipei");
        assert_eq!(on["remoteControlAtStartup"], json!(true));
        for v in [&off, &on] {
            assert_eq!(v["statusLine"]["command"], "sl");
            assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "hook");
            assert_eq!(v["outputStyle"], "Concise");
            assert_eq!(v["skipDangerousModePermissionPrompt"], json!(true));
            assert_eq!(v["timeFormat"], "24-hour");
            assert_eq!(v["timeZone"], "Asia/Taipei");
        }
    }

    /// issue #78：managed pane 不讓 claude 自己排「撞到用量上限就自動續跑」——那條線改由 daemon 的
    /// resend／排隊機制接手（`stuck_turns.rs`）。這一個鍵是 `/config` 裡「Continue automatically at
    /// usage limit」的設定檔對應（`autoContinueAtUsageLimit`，claude 2.1.234 起存在），不是
    /// `CLAUDE_CODE_RESUME_INTERRUPTED_TURN`（那是不相干的 cloud worker 續傳環境變數）。
    #[test]
    fn managed_panes_do_not_let_claude_auto_continue_past_a_usage_limit() {
        for wants_remote in [false, true] {
            let v = claude_settings("hook", "sl", wants_remote, true, "Asia/Taipei");
            assert_eq!(v["autoContinueAtUsageLimit"], json!(false), "wants_remote={wants_remote}");
        }
    }

    /// issue #102：claude 2.1.275 把 claude.ai 帳號上啟用的 skills／plugins 同步進終端 session。managed
    /// pane 的工具集由 daemon 決定，不讓 CLI 接一條我們看不到的線——帳號是共用的，一個人在網站上開一個
    /// skill 會同時改掉所有用那個帳號的 bot，而且同步進來的 skills 會吃掉 §4.4a 在算的 context。
    #[test]
    fn managed_panes_do_not_sync_skills_or_plugins_from_the_claude_ai_account() {
        for wants_remote in [false, true] {
            let v = claude_settings("hook", "sl", wants_remote, true, "Asia/Taipei");
            assert_eq!(v["syncClaudeAiSkills"], json!(false), "wants_remote={wants_remote}");
            assert_eq!(v["syncClaudeAiPlugins"], json!(false), "wants_remote={wants_remote}");
        }
    }

    /// 2026-10-02 使用者決定：新版 claude 自己吃 AGENTS.md，daemon 不再釘 `agents-md` plugin 的 `instructionFiles`（issue #206／#213 撤回），
    /// `--settings` 不帶 `pluginConfigs`，讓 CLI 用它自己的預設。
    #[test]
    fn managed_panes_do_not_pin_the_agents_md_plugin() {
        for wants_remote in [false, true] {
            let v = claude_settings("hook", "sl", wants_remote, true, "Asia/Taipei");
            assert!(v.get("pluginConfigs").is_none(), "wants_remote={wants_remote}");
        }
    }

    /// issue #82：native `SubagentStart`／`SubagentStop` 走同一支 hook 指令，跟 `SessionStart`／`Stop`
    /// 一樣——`hook_cmd.rs` 是通用轉發，不分事件名字（`payload comes from stdin`），不需要另外的旗標。
    #[test]
    fn subagent_lifecycle_hooks_are_registered_on_the_same_command() {
        let v = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        for event in ["SubagentStart", "SubagentStop"] {
            assert_eq!(v["hooks"][event][0]["hooks"][0]["command"], "hook", "{event}");
        }
    }

    /// issue #94：`PostToolUse` 只在 Bash 工具觸發（`matcher`），不是每個工具呼叫都送一次——那樣會把
    /// Read／Edit／Grep 這些跟子 pane 完全無關的呼叫也送進 daemon，白白增加流量。
    #[test]
    fn post_tool_use_only_matches_the_bash_tool_and_ask_user_question() {
        let v = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], json!("Bash"));
        assert_eq!(v["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "hook");
        let matchers: Vec<&str> = v["hooks"]["PostToolUse"].as_array().unwrap().iter().filter_map(|h| h["matcher"].as_str()).collect();
        assert_eq!(matchers, ["Bash", "AskUserQuestion"]);
        assert_eq!(v["hooks"]["PostToolUse"][1]["hooks"][0]["command"], "hook");
    }

    /// issue #1884：本機 claude bot 的 Bash 呼叫先過廣域 kill 守門（同一支 hook 指令，它自己分辨 PreToolUse）；
    /// `claude_settings` 本身不帶，本機與遠端各自用 `with_kill_guard` 接上（遠端指向 `kill-guard.sh`）。
    #[test]
    fn a_local_claude_bot_gets_a_bash_pre_tool_use_kill_guard_on_the_same_hook_command() {
        let mut v = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        assert!(v["hooks"].get("PreToolUse").is_none(), "遠端那份不帶");
        with_kill_guard(&mut v, "hook");
        let groups = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], json!("Bash"), "只在 shell 指令跑之前");
        assert!(groups[0]["hooks"][0]["command"].as_str().unwrap().starts_with("hook || "), "同一支 hook 指令，失敗時擋下");
        assert_eq!(groups[0]["hooks"][0]["onFailure"], "block");
        assert_eq!(groups[0]["hooks"][0]["timeout"], json!(5));
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "hook", "別的 hook 原樣");
    }

    /// issue #722：2.1.283 的 fullscreen 選單與 auto mode 選單都靠「較高層設定沒寫」才跳；
    /// bot 一律釘一般渲染，權限模式照 auto_approve 寫、不放大。
    #[test]
    fn bots_pin_the_default_renderer_and_state_their_permission_mode() {
        let yolo = claude_settings("hook", "sl", false, true, "Asia/Taipei");
        assert_eq!(yolo["tui"], json!("default"));
        assert_eq!(yolo["permissions"]["defaultMode"], json!("bypassPermissions"));
        let asks = claude_settings("hook", "sl", false, false, "Asia/Taipei");
        assert_eq!(asks["tui"], json!("default"));
        assert_eq!(asks["permissions"]["defaultMode"], json!("default"));
    }

    /// issue #1756：閒置壓縮交給 daemon 的保溫／熱壓，不讓 2.1.299 的 idle compaction 自己跑。
    #[test]
    fn bots_turn_off_upstream_idle_compaction() {
        for auto in [false, true] {
            for remote in [false, true] {
                assert_eq!(claude_settings("hook", "sl", remote, auto, "Asia/Taipei")["idleCompaction"], json!(false));
            }
        }
    }
}

#[cfg(test)]
mod herdr_skill_timeout_tests {
    use super::*;
    fn fake_herdr(body: &str) -> std::path::PathBuf {
        let f = crate::test_dirs::scratch_dir("am-skill").join("herdr");
        crate::test_dirs::write_exec(&f, format!("#!/bin/sh\n{body}\n"));
        f
    }
    /// herdr 卡住時 start_bot（持 per-bot 鎖）不能跟著永遠卡住；正常與失敗照舊。
    ///
    /// 「不卡住」的判準是**子行程還在睡的時候呼叫就回來了**（睡 120 秒、要在 100 秒內回），不是「多快回來」：
    /// 整樹在高負載下（load average 60～150）spawn 一個 `sh` 腳本就可能慢過好幾秒，正常／失敗那兩條的 `timeout` 只是放棄的上限
    /// （給足 10 分鐘），不是成功的條件——以前用 10 秒，負載一高 `bad` 那條回的是「逾時」而不是腳本的 stderr（部署 a43f6846 時整樹 1 敗）。
    #[tokio::test]
    async fn a_hung_herdr_skill_command_times_out_instead_of_blocking_start() {
        const GIVE_UP: Duration = Duration::from_secs(600);
        let hung = fake_herdr("sleep 120");
        let t = std::time::Instant::now();
        let err = herdr_skill_output(hung.to_str().unwrap(), Duration::from_millis(300)).await.unwrap_err().to_string();
        assert!(t.elapsed() < Duration::from_secs(100), "沒有在子行程睡完之前放棄（子行程 sleep 120）");
        assert!(err.contains("逾時"), "{err}");

        let ok = fake_herdr("echo 'name: herdr'");
        assert_eq!(herdr_skill_output(ok.to_str().unwrap(), GIVE_UP).await.unwrap().trim(), "name: herdr");
        let bad = fake_herdr("echo boom >&2; exit 3");
        let err = herdr_skill_output(bad.to_str().unwrap(), GIVE_UP).await.unwrap_err().to_string();
        assert!(err.contains("boom"), "{err}");
    }
}

#[cfg(test)]
mod codex_stop_args_tests {
    use super::*;
    #[test]
    fn legacy_notify_is_dropped_only_for_a_trusted_hook() {
        let legacy = vec!["-c".into(), "notify=[\"legacy-hook\"]".into()];
        assert_eq!(codex_notification_args(legacy.clone(), Ok(true)), vec!["-c".to_string(), "notify=[]".to_string()]);
        assert_eq!(codex_notification_args(legacy.clone(), Ok(false)), legacy);
        assert_eq!(codex_notification_args(legacy.clone(), Err(anyhow::anyhow!("unavailable"))), legacy);
    }
    #[test]
    fn codex_home_uses_the_effective_identity_and_rejects_ambiguous_relative_paths() {
        assert_eq!(codex_hook_home(&json!({"CODEX_HOME":"/identity/codex"}), "/home/default".into()).unwrap(), "/identity/codex");
        assert_eq!(codex_hook_home(&json!({}), "/home/default".into()).unwrap(), "/home/default/.codex");
        assert!(codex_hook_home(&json!({"CODEX_HOME":"relative"}), "/home/default".into()).is_err());
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    pub fn local_grok_dispatch_sh(exe: &str, data_dir: &str, instance: Option<&str>) -> String { super::local_grok_dispatch_sh(exe, data_dir, instance) }
    use super::*;
    pub async fn install_remote_grok_hook(conn: &HostConn, env: &Value, instance: Option<&str>) -> anyhow::Result<bool> { super::install_remote_grok_hook(conn, env, instance).await }
    pub fn hook_cmd_parts(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath + crate::capabilities::ListenPort), bot: &db::Bot, provider: &str) -> Vec<String> { super::hook_cmd_parts(app, bot, provider) }
    pub async fn remote_codex_paths(env: &Value, conn: &HostConn, root: &str) -> anyhow::Result<(String, String)> { super::remote_codex_paths(env, conn, root).await }
    pub async fn reconcile_remote_stop_hook(conn: &HostConn, host: &str, codex_home: &str, command: &str, keep_trusted: bool) -> bool { super::reconcile_remote_stop_hook(conn, host, codex_home, command, keep_trusted).await }
    pub fn remote_grok_read_script(hooks_dir: &str, file: &str) -> String { super::remote_grok_read_script(hooks_dir, file) }
    pub fn claude_settings(hook_cmd: &str, statusline: &str, wants_remote: bool, auto_approve: bool, time_zone: &str) -> Value { super::claude_settings(hook_cmd, statusline, wants_remote, auto_approve, time_zone) }
    pub fn hook_cmd_parts_for(exe: &str, port: u16, bot_id: &str, provider: &str, data_dir: &str) -> Vec<String> { super::hook_cmd_parts_for(exe, port, bot_id, provider, data_dir) }
    pub fn statusline_parts(mut hook: Vec<String>) -> Vec<String> { super::statusline_parts(hook) }
    pub async fn install_remote_hook(conn: &HostConn, bot: &db::Bot, instance: Option<&str>, time_zone: &str) -> anyhow::Result<RemoteHookPaths> { super::install_remote_hook(conn, bot, instance, time_zone).await }
}
