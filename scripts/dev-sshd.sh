#!/bin/sh
# dev-sshd.sh — a throwaway sshd on 127.0.0.1:2222, run with the current user's
# privileges (SPEC §11.8 R5). Nothing outside ~/.config/agents-manager/dev-sshd is
# touched: no sudo, no changes to the system sshd, no new system users.
#
#   scripts/dev-sshd.sh start     start it (idempotent) and print the host config
#   scripts/dev-sshd.sh stop      stop it
#   scripts/dev-sshd.sh status    is it up?
#   scripts/dev-sshd.sh ssh-opts  print the ssh_opts JSON array for POST /api/hosts
#
# The generated key pair lets *this* user ssh back into their own account, so a
# `[[hosts]]` entry pointing at 127.0.0.1:2222 behaves exactly like a real remote.

set -eu

DIR="${AM_DEV_SSHD_DIR:-$HOME/.config/agents-manager/dev-sshd}"
PORT="${AM_DEV_SSHD_PORT:-2222}"
CFG="$DIR/sshd_config"
PIDFILE="$DIR/sshd.pid"
LOG="$DIR/sshd.log"
HOSTKEY="$DIR/hostkey"
CLIENTKEY="$DIR/clientkey"
AUTHKEYS="$DIR/authorized_keys"
KNOWN="$DIR/known_hosts"
SSHD=/usr/sbin/sshd

is_running() {
  [ -f "$PIDFILE" ] || return 1
  pid=$(cat "$PIDFILE" 2>/dev/null || echo "")
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null
}

ensure_material() {
  mkdir -p "$DIR"
  chmod 700 "$DIR"
  [ -f "$HOSTKEY" ] || ssh-keygen -q -t ed25519 -N '' -C am-dev-sshd-host -f "$HOSTKEY"
  [ -f "$CLIENTKEY" ] || ssh-keygen -q -t ed25519 -N '' -C am-dev-sshd-client -f "$CLIENTKEY"
  cp "$CLIENTKEY.pub" "$AUTHKEYS"
  chmod 600 "$AUTHKEYS" "$HOSTKEY" "$CLIENTKEY"
  cat > "$CFG" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $HOSTKEY
AuthorizedKeysFile $AUTHKEYS
PidFile $PIDFILE
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
StrictModes no
AllowStreamLocalForwarding yes
StreamLocalBindUnlink yes
AllowTcpForwarding yes
GatewayPorts no
UsePAM no
Subsystem sftp /usr/libexec/sftp-server
EOF
  # Pin the host key so BatchMode ssh never has to ask.
  : > "$KNOWN"
  sed "s|^|[127.0.0.1]:$PORT |" "$HOSTKEY.pub" > "$KNOWN"
}

ssh_opts_json() {
  printf '["-i","%s","-o","UserKnownHostsFile=%s","-o","StrictHostKeyChecking=yes"]\n' "$CLIENTKEY" "$KNOWN"
}

case "${1:-}" in
  start)
    ensure_material
    if is_running; then
      echo "dev sshd already running on 127.0.0.1:$PORT (pid $(cat "$PIDFILE"))"
    else
      "$SSHD" -f "$CFG" -E "$LOG"
      # sshd daemonizes; give it a moment to write the pid file.
      i=0
      while [ $i -lt 20 ] && ! is_running; do i=$((i+1)); sleep 0.2; done
      is_running || { echo "dev sshd failed to start; see $LOG" >&2; tail -5 "$LOG" >&2 || true; exit 1; }
      echo "dev sshd started on 127.0.0.1:$PORT (pid $(cat "$PIDFILE"))"
    fi
    if ! ssh -i "$CLIENTKEY" -p "$PORT" -o BatchMode=yes -o StrictHostKeyChecking=yes \
         -o UserKnownHostsFile="$KNOWN" 127.0.0.1 true 2>/dev/null; then
      echo "warning: login check failed; see $LOG" >&2
    fi
    echo
    echo "POST /api/hosts body:"
    # hook_port must differ from the daemon's own port: the reverse forward binds
    # 127.0.0.1:<hook_port> on the "remote", which here is this very machine.
    printf '{"name":"loop","ssh":"%s@127.0.0.1","ssh_port":%s,"herdr_session":"am-loop","remote_path":"%s","hook_port":17788,"ssh_opts":%s}\n' \
      "$(id -un)" "$PORT" "/opt/homebrew/bin:\$HOME/.local/bin" "$(ssh_opts_json)"
    ;;
  stop)
    if is_running; then
      pid=$(cat "$PIDFILE")
      kill "$pid" 2>/dev/null || true
      i=0
      while [ $i -lt 25 ] && kill -0 "$pid" 2>/dev/null; do i=$((i+1)); sleep 0.2; done
      kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
      rm -f "$PIDFILE"
      echo "dev sshd stopped"
    else
      echo "dev sshd is not running"
    fi
    ;;
  status)
    if is_running; then echo "running (pid $(cat "$PIDFILE")) on 127.0.0.1:$PORT"; else echo "stopped"; exit 1; fi
    ;;
  ssh-opts)
    ensure_material >/dev/null 2>&1 || true
    ssh_opts_json
    ;;
  key)
    printf '%s\n' "$CLIENTKEY"
    ;;
  *)
    echo "usage: $0 start|stop|status|ssh-opts|key" >&2
    exit 2
    ;;
esac
