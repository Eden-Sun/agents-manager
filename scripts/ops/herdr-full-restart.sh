#!/bin/bash
set -u

server_pids() {
  ps -axo pid=,args= 2>/dev/null | awk '
    { n=split($2, parts, "/"); if (parts[n] != "herdr") next
      for (i = 3; i <= NF; i++) {
        if ($i == "--session" || $i == "--machine") { i++; continue }
        if ($i ~ /^-/) continue
        if ($i == "server") print $1
        break
      } }'
}
kill_servers() {
  local p
  for p in $(server_pids); do kill -TERM "$p" 2>/dev/null; done
  sleep 5
  for p in $(server_pids); do kill -KILL "$p" 2>/dev/null; done
  sleep 2
}

if [ "$(uname -s)" = Linux ]; then
export PATH="$HOME/.local/bin:$HOME/.bun/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
AGM_LOG_DIR="$HOME/.config/agents-manager/supervisor/AGM"
LOG="${AGM_LOG_DIR}/herdr-full-restart.log"
mkdir -p "$AGM_LOG_DIR"
log(){ echo "$(date '+%F %T') $*" >> "$LOG"; }
: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"; export XDG_RUNTIME_DIR
default_status() { herdr session list 2>/dev/null | awk '$1 == "default" { print $2; exit }'; }
sleep 3
log "== full restart start (linux)"
UNITS="$(systemctl --user list-units 'herdr@*.service' --state=active --plain --no-legend 2>>"$LOG" | awk '{print $1}' | tr '\n' ' ')"
HAD_DEFAULT="$(default_status)"
log "units before: ${UNITS}"
log "servers before: $(server_pids | tr '\n' ' ')"
for u in $UNITS; do systemctl --user stop "$u" 2>>"$LOG"; log "stop ${u} rc=$?"; done
kill_servers
log "servers after kill: $(server_pids | tr '\n' ' ')"
for s in "$HOME/.config/herdr/herdr.sock" "$HOME"/.config/herdr/sessions/*/herdr.sock "$HOME"/.config/herdr/sessions/*/herdr-client.sock "$HOME/.config/herdr/herdr-client.sock"; do [ -S "$s" ] && rm -f "$s"; done
for u in $UNITS; do systemctl --user start "$u" 2>>"$LOG"; log "start ${u} rc=$?"; done
FAILED=0
if [ "$HAD_DEFAULT" = running ]; then
    cd "$HOME" || { log "FAIL: cd HOME 失敗，沒有起 default server"; exit 1; }
    nohup herdr server >/tmp/herdr-default.log 2>&1 < /dev/null &
    DEFAULT_PID=$!
    disown
    i=0
    while [ "$i" -lt 15 ]; do
        [ "$(default_status)" = running ] && break
        sleep 1
        i=$((i + 1))
    done
    if [ "$(default_status)" = running ]; then
        log "default server up pid=${DEFAULT_PID}"
    else
        log "FAIL: default server 沒起來（pid=${DEFAULT_PID} 只代表 fork 成功）"
        FAILED=1
    fi
else
    log "default 原本沒在跑（${HAD_DEFAULT:-none}），不補起"
fi
for u in $UNITS; do
    systemctl --user is-active --quiet "$u" || { log "FAIL: ${u} 沒回到 active"; FAILED=1; }
done
log "servers after: $(server_pids | tr '\n' ';')"
log "session list: $(herdr session list 2>&1 | tr '\n' ';')"
if [ "$FAILED" = 0 ]; then log "== done"; exit 0; fi
log "== done (failed)"
exit 1
fi

LOG=/Users/me/.config/agents-manager/supervisor/AGM/herdr-full-restart.log
log(){ echo "$(date '+%F %T') $*" >> "$LOG"; }
export PATH=/opt/homebrew/bin:/Users/me/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
sleep 3
log "== full restart start (user order 2026-09-22)"
log "servers before: $(server_pids | tr '\n' ' ')"
launchctl bootout gui/501/dev.agents-manager.herdr-agents-manager 2>>"$LOG"; log "bootout agents-manager rc=$?"
launchctl bootout gui/501/dev.agents-manager.herdr-am-attach-remote 2>>"$LOG"; log "bootout am-attach-remote rc=$?"
kill_servers
log "servers after kill: $(server_pids | tr '\n' ' ')"
for s in /Users/me/.config/herdr/herdr.sock /Users/me/.config/herdr/sessions/*/herdr.sock /Users/me/.config/herdr/sessions/*/herdr-client.sock /Users/me/.config/herdr/herdr-client.sock; do [ -S "$s" ] && rm -f "$s"; done
launchctl bootstrap gui/501 /Users/me/Library/LaunchAgents/dev.agents-manager.herdr-agents-manager.plist 2>>"$LOG"; log "bootstrap agents-manager rc=$?"
launchctl bootstrap gui/501 /Users/me/Library/LaunchAgents/dev.agents-manager.herdr-am-attach-remote.plist 2>>"$LOG"; log "bootstrap am-attach-remote rc=$?"
cd /Users/me || { log "FAIL: cd /Users/me 失敗，沒有起 default server"; exit 1; }
nohup /opt/homebrew/bin/herdr server >/tmp/herdr-default.log 2>&1 < /dev/null &
DEFAULT_PID=$!
disown

default_status() { herdr session list 2>/dev/null | awk '$1 == "default" { print $2; exit }'; }
i=0
while [ "$i" -lt 15 ]; do
    [ "$(default_status)" = running ] && break
    sleep 1
    i=$((i + 1))
done
if [ "$(default_status)" = running ]; then
    log "default server up pid=$DEFAULT_PID"
else
    log "FAIL: default server 沒起來（pid=$DEFAULT_PID 只代表 fork 成功，不代表 herdr 在跑）"
    log "      /tmp/herdr-default.log 末幾行：$(tail -3 /tmp/herdr-default.log 2>/dev/null | tr '\n' ' ')"
    log "servers after: $(server_pids | tr '\n' ';')"
    log "== done (failed)"
    exit 1
fi
log "servers after: $(server_pids | tr '\n' ';')"
log "session list: $(herdr session list 2>&1 | tr '\n' ';')"
log "== done"
