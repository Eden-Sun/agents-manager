#!/bin/bash
# herdr 全機重啟：bootout 兩個 herdr launchd job、收掉所有 herdr server、清 socket、
# 再 bootstrap 回來並補起預設 server。2026-09-22 使用者下令全機重啟時寫的一次性腳本，
# 留著給下次同樣狀況用；**沒有** launchd 排程，要用的人自己跑。
#
# 這份是**來源檔**：改行為改這裡再 install 到
# `~/.config/agents-manager/supervisor/AGM/bin/herdr-full-restart.sh`，不要只改安裝目錄那份
#（issue #418：這支在 2026-09-24 之前只存在於安裝目錄，沒有版控也沒有測試）。
#
# 路徑與 uid（gui/501）寫死成這台開發機的值，是原樣收進 repo 的既有行為，沒有改。
LOG=/Users/m4p/.config/agents-manager/supervisor/AGM/herdr-full-restart.log
log(){ echo "$(date '+%F %T') $*" >> "$LOG"; }
export PATH=/opt/homebrew/bin:/Users/m4p/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
sleep 3
log "== full restart start (user order 2026-09-22)"
log "servers before: $(pgrep -f 'herdr.*server' | tr '\n' ' ')"
launchctl bootout gui/501/dev.agents-manager.herdr-agents-manager 2>>"$LOG"; log "bootout agents-manager rc=$?"
launchctl bootout gui/501/dev.agents-manager.herdr-am-attach-remote 2>>"$LOG"; log "bootout am-attach-remote rc=$?"
pkill -TERM -f 'herdr.*server'; sleep 5
pkill -KILL -f 'herdr.*server' 2>/dev/null; sleep 2
log "servers after kill: $(pgrep -f 'herdr.*server' | tr '\n' ' ')"
for s in /Users/m4p/.config/herdr/herdr.sock /Users/m4p/.config/herdr/sessions/*/herdr.sock /Users/m4p/.config/herdr/sessions/*/herdr-client.sock /Users/m4p/.config/herdr/herdr-client.sock; do [ -S "$s" ] && rm -f "$s"; done
launchctl bootstrap gui/501 /Users/m4p/Library/LaunchAgents/dev.agents-manager.herdr-agents-manager.plist 2>>"$LOG"; log "bootstrap agents-manager rc=$?"
launchctl bootstrap gui/501 /Users/m4p/Library/LaunchAgents/dev.agents-manager.herdr-am-attach-remote.plist 2>>"$LOG"; log "bootstrap am-attach-remote rc=$?"
cd /Users/m4p && nohup setsid /opt/homebrew/bin/herdr server >/tmp/herdr-default.log 2>&1 < /dev/null & disown
log "default server started pid=$!"
sleep 6
log "servers after: $(pgrep -fl 'herdr.*server' | tr '\n' ';')"
log "session list: $(herdr session list 2>&1 | tr '\n' ';')"
log "== done"
