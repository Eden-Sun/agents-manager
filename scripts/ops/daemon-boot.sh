#!/bin/bash
# 開機後正式 daemon 沒在跑就起一顆（issue #858）。
#
# 根因：Linux 上 daemon 只由 daemon-swap.sh 的 `systemd-run --user --collect` 臨時 unit 起；repo 沒有任何開機時
# 起 daemon 的東西，VM 重開之後 7788 沒人聽、bot 沒人接，要人手進去起。
# 由 systemd user unit `com.agm.daemon-boot.service`（oneshot，WantedBy=default.target）在開機時跑一次，不常駐。
#
# 做法跟 daemon-swap.sh 的 start() 一樣：交給 systemd user manager 起一個臨時 unit（Type=forking、KillMode=process），
# 讓 daemon 是 nice 0 的獨立行程，unit 結束不會連它一起殺。重複啟動沒有風險：已經有人聽 7788 就直接結束，
# 而且 daemon 自己有 daemon.lock 獨佔鎖，後來的一顆會在碰 DB 前退出。
#
# 可覆寫（測試用）：AGM_REPO、AM_DATA_DIR、DAEMON_LOG、AM_PORT_CHECK、AGM_START_PY、DAEMON_BOOT_WAIT_SECS、
# SYSTEMD_RUN_BIN、CURL_BIN、AGM_BIN。
set -u

AGM_REPO="${AGM_REPO:-$HOME/project/agents-manager}"
AM_DATA="${AM_DATA_DIR:-$HOME/.config/agents-manager}"
DLOG="${DAEMON_LOG:-$AM_DATA/daemon.log}"
PORT="${AM_PORT_CHECK:-7788}"
START_PY="${AGM_START_PY:-$HOME/.cache/agents-manager/deploy-checkout/scripts/ops/daemon-start.py}"
WAIT_SECS="${DAEMON_BOOT_WAIT_SECS:-60}"
SYSTEMD_RUN="${SYSTEMD_RUN_BIN:-systemd-run}"
CURL="${CURL_BIN:-curl}"
AGM="${AGM_BIN:-$HOME/.config/agents-manager/supervisor/AGM/bin/agm}"
LOG="${DAEMON_BOOT_LOG:-$HOME/.config/agents-manager/supervisor/AGM/daemon-boot.log}"
mkdir -p "$(dirname "$LOG")" 2>/dev/null

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

# 喊人：agm 找不到（或 daemon 本來就沒起來、推不出去）就只留 log。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  if [ -x "$AGM" ] || command -v "$AGM" >/dev/null 2>&1; then
    "$AGM" --compact ops-alert --source daemon-boot --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
      log "推 ops-alert 失敗（daemon 不在或 CLI 太舊），只留在這份 log"
  else
    log "找不到 agm（${AGM}），只留在這份 log"
  fi
}

# 活著＝/api/session 有回應；沒帶 token 的 401 也算（有人在聽就夠了，不必知道 token）。
alive() {
  local code
  code=$("$CURL" -sS -o /dev/null -m 3 -w '%{http_code}' "http://127.0.0.1:${PORT}/api/session" 2>/dev/null) || return 1
  case "$code" in 2??|401) return 0 ;; *) return 1 ;; esac
}

if alive; then
  log "daemon 已在 127.0.0.1:${PORT} 跑，不用起"
  exit 0
fi

BIN_PATH="$AGM_REPO/target/release/agents-managerd"
if [ ! -x "$BIN_PATH" ]; then
  alert binary_missing "找不到 ${BIN_PATH}；開機後 daemon 起不來。在 ${AGM_REPO} 跑 cargo build --release -p agents-managerd"
  exit 1
fi

if [ ! -r "$START_PY" ]; then
  FALLBACK="$AGM_REPO/scripts/ops/daemon-start.py"
  if [ -r "$FALLBACK" ]; then
    START_PY="$FALLBACK"
  else
    alert start_script_missing "找不到 daemon-start.py（${START_PY}、${FALLBACK}）；開機後 daemon 起不來"
    exit 1
  fi
fi

# 不是從登入 session 叫起來（沒有 pam_systemd）就沒有 XDG_RUNTIME_DIR，systemd-run --user 連不到 user bus。
[ -n "${XDG_RUNTIME_DIR:-}" ] || export XDG_RUNTIME_DIR="/run/user/$(id -u)"
PYTHON="$(command -v python3 || echo python3)"   # 直譯器寫絕對路徑：transient unit 的 PATH 是 user manager 的
log "daemon 沒在跑，用 systemd-run 起 ${BIN_PATH}（log ${DLOG}）"
"$SYSTEMD_RUN" --user --collect --unit="am-daemon-boot-$(date +%s)" -p Type=forking -p KillMode=process \
  -- "$PYTHON" "$START_PY" "$AGM_REPO" "$DLOG" >> "$LOG" 2>&1 || log "WARN: systemd-run rc=${?}（下面的 /api/session 驗證會接手判斷）"

waited=0
while [ "$waited" -lt "$WAIT_SECS" ]; do
  if alive; then
    log "daemon 已起來（等了 ${waited} 秒）"
    exit 0
  fi
  sleep 1
  waited=$((waited + 1))
done
alert boot_start_failed "開機起 daemon 後 ${WAIT_SECS} 秒內 127.0.0.1:${PORT}/api/session 都沒回應；看 ${DLOG} 與 ${LOG}"
exit 1
