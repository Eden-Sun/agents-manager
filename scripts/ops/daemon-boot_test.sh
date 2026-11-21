#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-boot.sh"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}

setup() {
  ROOT="$(mktemp -d)"
  export HOME="$ROOT/home"
  FIX="$ROOT/fix"; BIN="$ROOT/fakebin"
  REPO="$ROOT/repo"; STARTPY="$ROOT/daemon-start.py"
  mkdir -p "$HOME/.config/agents-manager/supervisor/AGM" "$FIX" "$BIN" "$REPO/target/release"
  : > "$FIX/sr.log"; : > "$FIX/agm.log"
  echo "#!/bin/sh" > "$REPO/target/release/agents-managerd"; chmod 755 "$REPO/target/release/agents-managerd"
  : > "$STARTPY"
  cat > "$BIN/curl" <<STUB
#!/bin/bash
if [ -s "$FIX/code" ]; then cat "$FIX/code"; exit 0; fi
if [ -e "$FIX/up-after-start" ] && [ -s "$FIX/sr.log" ]; then printf 200; exit 0; fi
exit 7
STUB
  cat > "$BIN/systemd-run" <<STUB
#!/bin/bash
echo "systemd-run \$*" >> "$FIX/sr.log"
exit 0
STUB
  cat > "$BIN/agm" <<STUB
#!/bin/bash
echo "agm \$*" >> "$FIX/agm.log"
exit 0
STUB
  chmod 755 "$BIN"/*
  DLOG="$ROOT/daemon.log"
  BLOG="$ROOT/boot.log"
}
teardown() { rm -rf "$ROOT"; }
run() {
  PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO" AM_DATA_DIR="$HOME/.config/agents-manager" DAEMON_LOG="$DLOG" \
    AGM_START_PY="$STARTPY" SYSTEMD_RUN_BIN="$BIN/systemd-run" CURL_BIN="$BIN/curl" AGM_BIN="$BIN/agm" \
    DAEMON_BOOT_LOG="$BLOG" DAEMON_BOOT_WAIT_SECS="${WAIT:-2}" bash "$SCRIPT" >/dev/null 2>&1
  echo $?
}

setup
printf 200 > "$FIX/code"
equals "已在聽時 exit 0" "$(run)" "0"
equals "已在聽時不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
check "log 說已在跑" "已在" "$BLOG"
teardown

setup
printf 401 > "$FIX/code"
equals "401 算活著、exit 0" "$(run)" "0"
equals "401 不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
teardown

setup
: > "$FIX/up-after-start"
equals "起得來時 exit 0" "$(run)" "0"
equals "systemd-run 只叫一次" "$(wc -l < "$FIX/sr.log" | tr -d ' ')" "1"
check "帶 --user --collect" "systemd-run --user --collect --unit=am-daemon-boot-" "$FIX/sr.log"
check "帶 Type=forking" "\-p Type=forking" "$FIX/sr.log"
check "帶 KillMode=process" "\-p KillMode=process" "$FIX/sr.log"
check "跑 daemon-start.py <repo> <log>" "${STARTPY} ${REPO} ${DLOG}" "$FIX/sr.log"
equals "起得來時不喊人" "$(wc -c < "$FIX/agm.log" | tr -d ' ')" "0"
teardown

setup
: > "$FIX/up-after-start"
rm -f "$STARTPY"; mkdir -p "$REPO/scripts/ops"; : > "$REPO/scripts/ops/daemon-start.py"
equals "退回 repo 的 daemon-start.py：exit 0" "$(run)" "0"
check "用的是 repo 裡那份" "$REPO/scripts/ops/daemon-start.py ${REPO} ${DLOG}" "$FIX/sr.log"
teardown

setup
rm -f "$STARTPY"
equals "沒有 daemon-start.py：exit 1" "$(run)" "1"
check "喊 start_script_missing" "ops-alert --source daemon-boot --reason start_script_missing" "$FIX/agm.log"
equals "不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
teardown

setup
rm -f "$REPO/target/release/agents-managerd"
equals "binary 不在：exit 1" "$(run)" "1"
equals "binary 不在：不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
check "喊 binary_missing" "ops-alert --source daemon-boot --reason binary_missing" "$FIX/agm.log"
teardown

setup
rm -f "$REPO/target/release/agents-managerd" "$BIN/agm"
equals "agm 不在也只是 exit 1" "$(run)" "1"
check "log 記下 binary_missing" "binary_missing" "$BLOG"
teardown

setup
equals "起不來：exit 1" "$(WAIT=2 run)" "1"
equals "起不來：systemd-run 叫了一次" "$(wc -l < "$FIX/sr.log" | tr -d ' ')" "1"
check "喊 boot_start_failed" "ops-alert --source daemon-boot --reason boot_start_failed" "$FIX/agm.log"
teardown

setup
CDIR="$ROOT/bin"; mkdir -p "$CDIR"; cp "$SCRIPT" "$CDIR/daemon-boot.sh"
equals "缺 kick-common：exit 1" "$(PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO" DAEMON_LOG="$DLOG" AGM_START_PY="$STARTPY" SYSTEMD_RUN_BIN="$BIN/systemd-run" CURL_BIN="$BIN/curl" AGM_BIN="$BIN/agm" DAEMON_BOOT_LOG="$BLOG" bash "$CDIR/daemon-boot.sh" >/dev/null 2>&1; echo $?)" "1"
equals "缺 kick-common：不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
check "缺 kick-common：log 記下" "ALERT kick_common_missing" "$BLOG"
check "缺 kick-common：推 kick_common_missing" "ops-alert --source daemon-boot --reason kick_common_missing" "$FIX/agm.log"
teardown

setup
rm -f "$REPO/target/release/agents-managerd"
equals "binary 不在：exit 1" "$(run)" "1"
check "共用 alert 的 --source 是 daemon-boot" "ops-alert --source daemon-boot --reason binary_missing" "$FIX/agm.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
