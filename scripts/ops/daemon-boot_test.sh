#!/bin/bash
# daemon-boot.sh 的隔離測試（issue #858）：HOME 指到暫存目錄；假 systemd-run／curl／agm 放進暫存 PATH 記錄呼叫，
# 不會真的起 daemon、不碰真的 7788、不讀寫真的 ~/.config。
#
#   bash scripts/ops/daemon-boot_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-boot.sh"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
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
  # 假 curl：預設「沒人聽」（exit 7）；FIX/code 有內容就回那個狀態碼；FIX/up-after-start 存在時，systemd-run 被叫過之後才回 200。
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
run() { # 印 exit code；PATH 只留假指令與系統目錄
  PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO" AM_DATA_DIR="$HOME/.config/agents-manager" DAEMON_LOG="$DLOG" \
    AGM_START_PY="$STARTPY" SYSTEMD_RUN_BIN="$BIN/systemd-run" CURL_BIN="$BIN/curl" AGM_BIN="$BIN/agm" \
    DAEMON_BOOT_LOG="$BLOG" DAEMON_BOOT_WAIT_SECS="${WAIT:-2}" bash "$SCRIPT" >/dev/null 2>&1
  echo $?
}

# 1. 已在聽（200）→ 不呼叫 systemd-run、exit 0。
setup
printf 200 > "$FIX/code"
equals "已在聽時 exit 0" "$(run)" "0"
equals "已在聽時不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
check "log 說已在跑" "已在" "$BLOG"
teardown

# 1b. 401 也算活著（沒帶 token）。
setup
printf 401 > "$FIX/code"
equals "401 算活著、exit 0" "$(run)" "0"
equals "401 不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
teardown

# 2. 沒在聽、binary 在 → systemd-run 呼叫一次，帶 Type=forking／KillMode=process 與 daemon-start.py <repo> <log>；之後起來了 → exit 0。
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

# 2b. 專用 checkout 的 daemon-start.py 不在 → 退回 repo 裡那份。
setup
: > "$FIX/up-after-start"
rm -f "$STARTPY"; mkdir -p "$REPO/scripts/ops"; : > "$REPO/scripts/ops/daemon-start.py"
equals "退回 repo 的 daemon-start.py：exit 0" "$(run)" "0"
check "用的是 repo 裡那份" "$REPO/scripts/ops/daemon-start.py ${REPO} ${DLOG}" "$FIX/sr.log"
teardown

# 2c. 兩份 daemon-start.py 都沒有 → start_script_missing、exit 1、不呼叫 systemd-run。
setup
rm -f "$STARTPY"
equals "沒有 daemon-start.py：exit 1" "$(run)" "1"
check "喊 start_script_missing" "ops-alert --source daemon-boot --reason start_script_missing" "$FIX/agm.log"
equals "不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
teardown

# 3. binary 不在 → 不呼叫 systemd-run、喊 binary_missing、exit 1。
setup
rm -f "$REPO/target/release/agents-managerd"
equals "binary 不在：exit 1" "$(run)" "1"
equals "binary 不在：不呼叫 systemd-run" "$(wc -c < "$FIX/sr.log" | tr -d ' ')" "0"
check "喊 binary_missing" "ops-alert --source daemon-boot --reason binary_missing" "$FIX/agm.log"
teardown

# 3b. binary 不在、agm 也不在 → 只 log、仍 exit 1，不爆。
setup
rm -f "$REPO/target/release/agents-managerd" "$BIN/agm"
equals "agm 不在也只是 exit 1" "$(run)" "1"
check "log 記下 binary_missing" "binary_missing" "$BLOG"
teardown

# 4. 起了但等待秒數內都不回 → boot_start_failed、exit 1。
setup
equals "起不來：exit 1" "$(WAIT=2 run)" "1"
equals "起不來：systemd-run 叫了一次" "$(wc -l < "$FIX/sr.log" | tr -d ' ')" "1"
check "喊 boot_start_failed" "ops-alert --source daemon-boot --reason boot_start_failed" "$FIX/agm.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
