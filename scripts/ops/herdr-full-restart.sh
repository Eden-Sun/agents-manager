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
# `scripts/ops/` 底下唯一沒有 set -u 的就是這支（issue #455 順帶）。這裡不加 -e：
# bootout／pkill 對「本來就沒在跑」回非零是正常的，那些 rc 自己記進 log。
set -u
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
# macOS **沒有** `setsid`（那是 util-linux 的東西，系統目錄與 Homebrew 的 bin、以及本檔第 13
# 行寫死的那組 PATH 底下都沒有），所以原本的 `nohup setsid herdr server` 是 `nohup` 找不到 `setsid`
# 直接失敗、herdr 一次都沒被執行（issue #455）。detach 改成跟 daemon 的 `ensure_session`
# （`daemon/src/state.rs`）同一款：直接起、不靠任何外部 detach 指令，`nohup` 擋 SIGHUP、
# 背景 + `disown` 讓它不掛在這個 shell 的 job table 上。
cd /Users/m4p || { log "FAIL: cd /Users/m4p 失敗，沒有起 default server"; exit 1; }
# 這一行**必須自己一行**：以前是 `cd … && nohup … &`，`&` 綁的是整個 `&&` 清單，
# 於是 `$!` 拿到的是那個 subshell 的 pid——不管裡面有沒有真的把 herdr 起來都有值，
# 失敗就這樣被記成 "started"（issue #455）。
nohup /opt/homebrew/bin/herdr server >/tmp/herdr-default.log 2>&1 < /dev/null &
DEFAULT_PID=$!
disown

# `$!` 只證明 fork 出來了，不證明 herdr 真的在服務。以 `herdr session list` 的 default 狀態為準，
# 起不來就非零退出——這是整支腳本唯一沒有自癒路徑的 session（daemon 的 `ensure_session` 明文
# 拒絕代起 `default`），靜默失敗等於機器就這樣留在半殘狀態。
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
    log "servers after: $(pgrep -fl 'herdr.*server' | tr '\n' ';')"
    log "== done (failed)"
    exit 1
fi
log "servers after: $(pgrep -fl 'herdr.*server' | tr '\n' ';')"
log "session list: $(herdr session list 2>&1 | tr '\n' ';')"
log "== done"
