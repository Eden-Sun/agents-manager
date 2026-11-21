#!/bin/bash
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

OWNER=daemon-boot
. "$(dirname "$0")/kick-common.sh" 2>/dev/null || {
  _m="找不到 $(dirname "$0")/kick-common.sh，daemon-boot.sh 停擺（它要和 kick 一起裝進 bin/）"
  log "ALERT kick_common_missing：${_m}"
  [ -x "$AGM" ] && "$AGM" --compact ops-alert --source daemon-boot --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

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

[ -n "${XDG_RUNTIME_DIR:-}" ] || export XDG_RUNTIME_DIR="/run/user/$(id -u)"
PYTHON="$(command -v python3 || echo python3)"
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
