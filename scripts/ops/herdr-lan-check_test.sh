#!/bin/bash
# herdr-lan-check.sh 的隔離測試：假的 codesign／plutil／node，完全不碰真 binary、
# 真授權表或網路。測 identifier、授權表與 node TCP 三步都有回報。
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/herdr-lan-check.sh"
# 解成實際路徑：macOS 的 /var 是 /private/var 的連結，腳本會 readlink -f，假 codesign 比的是完整路徑。
ROOT=$(cd "$(mktemp -d)" && pwd -P)
trap 'rm -rf "$ROOT"' EXIT

fail() { echo "FAIL - $1" >&2; exit 1; }
ok() { echo "ok   - $1"; }

mkdir -p "$ROOT/bin"
printf '%s\n' '#!/bin/bash' 'case "$2" in *cellar/herdr|*/herdr) [ -f "$2" ] && ! head -c 2 "$2" | grep -q "^#!" && { echo "Identifier=herdr-1672acdeb8e5ac40" >&2; exit 0; } ;; esac' 'echo "$2: code object is not signed at all" >&2; exit 1' > "$ROOT/bin/codesign"
printf '%s\n' '#!/bin/bash' 'shift' 'cat "$1"' > "$ROOT/bin/plutil"
printf '%s\n' '#!/bin/bash' 'echo "$*" > "$HERDR_NODE_LOG"' 'echo "node connected"' > "$ROOT/bin/node"
chmod +x "$ROOT/bin/codesign" "$ROOT/bin/plutil" "$ROOT/bin/node"
printf '%s\n' '"herdr-1672acdeb8e5ac40" => 1' > "$ROOT/network.plist"
printf '%s\n' 'fake binary' > "$ROOT/herdr"
chmod +x "$ROOT/herdr"

OUTPUT=$(PATH="$ROOT/bin:/usr/bin:/bin" HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" HERDR_NODE_LOG="$ROOT/node.log" \
  bash "$SCRIPT" "$ROOT/herdr" 192.168.1.1 80 2>&1) || fail "三步驗證應成功"

printf '%s\n' "$OUTPUT" | grep -F '1/3 identifier: herdr-1672acdeb8e5ac40' >/dev/null || fail "沒有回報 identifier"
printf '%s\n' "$OUTPUT" | grep -F '2/3 authorization: found identifier' >/dev/null || fail "沒有回報授權表"
printf '%s\n' "$OUTPUT" | grep -F '3/3 LAN: node connected' >/dev/null || fail "沒有回報 node 區網連線"
grep -F -- '192.168.1.1 80' "$ROOT/node.log" >/dev/null || fail "node 沒收到目標主機與 port"
ok "假的 codesign/plutil/node 完成三步，未連真網路"

set +e
SKIP_OUTPUT=$(PATH="$ROOT/bin:/usr/bin:/bin" NODE_BIN=missing-node HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" \
  bash "$SCRIPT" "$ROOT/herdr" 192.168.1.1 80 2>&1)
SKIP_RC=$?
set -u
[ "$SKIP_RC" -eq 2 ] || fail "node 缺少時應以 skip 狀態結束"
printf '%s\n' "$SKIP_OUTPUT" | grep -F '3/3 LAN: SKIP 找不到 node' >/dev/null || fail "node 缺少時沒有明確 skip 原因"
ok "node 缺少時明確 skip，沒有嘗試真網路"

# 不帶參數：驗「正在跑的 herdr server 的執行檔」。ps／lsof 換成假的，列出指定的 server 行程與它們的 txt。
# codesign 依路徑回不同 identifier：cellar＝有授權那顆、local＝PATH 上另一份簽章不同的副本（授權表沒有）。
mkdir -p "$ROOT/cellar" "$ROOT/local/bin" "$ROOT/.config/agents-manager/bots/b1/bin"
printf '%s\n' 'fake cellar binary' > "$ROOT/cellar/herdr"
printf '%s\n' 'fake local copy' > "$ROOT/local/bin/herdr"
printf '%s\n' '#!/bin/sh' 'exec /nowhere "$@"' > "$ROOT/.config/agents-manager/bots/b1/bin/herdr"
chmod +x "$ROOT/cellar/herdr" "$ROOT/local/bin/herdr" "$ROOT/.config/agents-manager/bots/b1/bin/herdr"
cat > "$ROOT/bin/codesign" <<EOF
#!/bin/bash
case "\$2" in
  "$ROOT/cellar/herdr") echo "Identifier=herdr-1672acdeb8e5ac40" >&2; exit 0 ;;
  "$ROOT/local/bin/herdr") echo "Identifier=herdr-2ae2bd8e9c54788e" >&2; exit 0 ;;
esac
echo "\$2: code object is not signed at all" >&2; exit 1
EOF
chmod +x "$ROOT/bin/codesign"
# 假 ps：$HERDR_FAKE_PS 的內容就是 `ps -axo pid=,args=` 的輸出。假 lsof：-p <pid> 查 $HERDR_FAKE_TXT 裡 `<pid> <path>`。
printf '%s\n' '#!/bin/bash' 'cat "$HERDR_FAKE_PS"' > "$ROOT/bin/fake-ps"
printf '%s\n' '#!/bin/bash' 'pid=""; while [ $# -gt 0 ]; do [ "$1" = "-p" ] && pid="$2"; shift; done' \
  'awk -v p="$pid" '"'"'$1==p { print "n" $2 }'"'"' "$HERDR_FAKE_TXT"' > "$ROOT/bin/fake-lsof"
chmod +x "$ROOT/bin/fake-ps" "$ROOT/bin/fake-lsof"

run_noarg() { # run_noarg <ps 內容> <txt 對照> [fallback]
  printf '%s\n' "$1" > "$ROOT/ps.txt"
  printf '%s\n' "$2" > "$ROOT/txt.txt"
  PATH="$ROOT/.config/agents-manager/bots/b1/bin:$ROOT/local/bin:$ROOT/bin:/usr/bin:/bin" \
    PS_BIN="$ROOT/bin/fake-ps" LSOF_BIN="$ROOT/bin/fake-lsof" HERDR_FAKE_PS="$ROOT/ps.txt" HERDR_FAKE_TXT="$ROOT/txt.txt" \
    HERDR_FALLBACK_BIN="${3:-$ROOT/nowhere/herdr}" HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" HERDR_NODE_LOG="$ROOT/node.log" \
    LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 bash "$SCRIPT" "" 192.168.1.1 80 2>&1
}

# 2026-09-22 這台的實況：PATH 上最前面的真 binary 是另一份簽章不同的副本（授權表沒有），但 server 跑的是
# cellar 那顆。要選 server 那顆 → PASS；以前選到 PATH 那顆 → FAIL。另夾一支 claude，argv 裡提到 herdr 與 server，不能被當成 server。
set +e
SERVER_OUT=$(run_noarg "  101 /opt/homebrew/bin/herdr server
  102 herdr --session am-quota server
  103 claude --append-system-prompt 用 herdr agent list … herdr server 也在
  104 /usr/bin/vim server.txt" "101 $ROOT/cellar/herdr
102 $ROOT/cellar/herdr
103 $ROOT/claude
104 /usr/bin/vim")
SERVER_RC=$?
set -u
[ "$SERVER_RC" -eq 0 ] || fail "PATH 上有簽章不同的副本時，應選 server 用的那顆並 PASS（rc=${SERVER_RC}）：$SERVER_OUT"
printf '%s\n' "$SERVER_OUT" | grep -F "1/3 identifier: herdr-1672acdeb8e5ac40（$ROOT/cellar/herdr）" >/dev/null || fail "沒選到 server 的執行檔：$SERVER_OUT"
printf '%s\n' "$SERVER_OUT" | grep -F 'herdr-2ae2bd8e9c54788e' >/dev/null && fail "選到了 PATH 上那份副本：$SERVER_OUT"
printf '%s\n' "$SERVER_OUT" | grep -F '（1 個）' >/dev/null || fail "兩顆 server 同一個執行檔要去重成 1 個、claude／vim 不能算：$SERVER_OUT"
ok "不帶參數時驗 server 的執行檔，不管 PATH 上另一份簽章不同的副本；去重、不誤認 claude"

# 好幾顆 server 跑不同路徑：逐一驗，任何一顆沒授權就 FAIL，而且指名是哪一顆。
set +e
MULTI_OUT=$(run_noarg "  201 /opt/homebrew/bin/herdr server
  202 $ROOT/local/bin/herdr --session x server" "201 $ROOT/cellar/herdr
202 $ROOT/local/bin/herdr")
MULTI_RC=$?
set -u
[ "$MULTI_RC" -eq 1 ] || fail "兩顆 server 其中一顆沒授權應 FAIL（rc=1），實際 ${MULTI_RC}：$MULTI_OUT"
printf '%s\n' "$MULTI_OUT" | grep -F '（2 個）' >/dev/null || fail "應逐一驗兩個執行檔：$MULTI_OUT"
printf '%s\n' "$MULTI_OUT" | grep -F '2/3 authorization: found identifier herdr-1672acdeb8e5ac40' >/dev/null || fail "有授權那顆沒被驗到：$MULTI_OUT"
printf '%s\n' "$MULTI_OUT" | grep -F "2/3 authorization: FAIL 授權表沒有新版 identifier herdr-2ae2bd8e9c54788e（$ROOT/local/bin/herdr）" >/dev/null || fail "沒授權那顆沒指名：$MULTI_OUT"
ok "好幾顆 server 跑不同路徑：逐一驗，指名沒授權的那顆並 FAIL"

# 沒有在跑的 server：退回 fallback 並解 symlink（/opt/homebrew/bin/herdr → Cellar）。
mkdir -p "$ROOT/brew/bin"
ln -sf "$ROOT/cellar/herdr" "$ROOT/brew/bin/herdr"
set +e
FB_OUT=$(run_noarg "  301 /usr/bin/vim notes" "301 /usr/bin/vim" "$ROOT/brew/bin/herdr")
FB_RC=$?
set -u
[ "$FB_RC" -eq 0 ] || fail "沒有 server 時退回 fallback 應 PASS（rc=${FB_RC}）：$FB_OUT"
printf '%s\n' "$FB_OUT" | grep -F "退回 $ROOT/brew/bin/herdr → $ROOT/cellar/herdr" >/dev/null || fail "fallback 沒解 symlink：$FB_OUT"
ok "沒有在跑的 server：退回 fallback 並解 symlink"

# 沒有 server、fallback 也不存在：明確 FAIL、rc=1，不可 crash（PATH 上的 shim／副本都不能拿來湊數）。
set +e
NONE_OUT=$(run_noarg "  401 /usr/bin/vim notes" "401 /usr/bin/vim")
NONE_RC=$?
set -u
[ "$NONE_RC" -eq 1 ] || fail "沒有 server 也沒有 fallback 應 FAIL（rc=1），實際 ${NONE_RC}：$NONE_OUT"
printf '%s\n' "$NONE_OUT" | grep -F '1/3 identifier: FAIL 找不到在跑的 herdr server' >/dev/null || fail "沒有明確 FAIL：$NONE_OUT"
printf '%s\n' "$NONE_OUT" | grep -F 'unbound variable' >/dev/null && fail "crash 在 unbound variable：$NONE_OUT"
ok "沒有 server、沒有 fallback：明確 FAIL、rc=1"

# codesign 對指定路徑失敗（沒簽章）：在 UTF-8 locale 下要印得出 FAIL 那一行、rc=1，不能死在 unbound variable
# （bash 3.2＋set -u 會把 `${BINARY}（` 的全形括號併進變數名）。
printf '%s\n' '#!/bin/bash' 'echo "code object is not signed at all" >&2' 'exit 1' > "$ROOT/bin/codesign"
set +e
BAD_OUTPUT=$(LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 PATH="$ROOT/bin:/usr/bin:/bin" HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" HERDR_NODE_LOG="$ROOT/node.log" \
  bash "$SCRIPT" "$ROOT/herdr" 192.168.1.1 80 2>&1)
BAD_RC=$?
set -u
[ "$BAD_RC" -eq 1 ] || fail "codesign 失敗應 rc=1，實際 ${BAD_RC}：$BAD_OUTPUT"
printf '%s\n' "$BAD_OUTPUT" | grep -F "1/3 identifier: FAIL codesign -dv 無法取出 identifier（$ROOT/herdr）" >/dev/null || fail "FAIL 那一行沒印出來（crash？）：$BAD_OUTPUT"
printf '%s\n' "$BAD_OUTPUT" | grep -F 'unbound variable' >/dev/null && fail "還是 crash 在 unbound variable：$BAD_OUTPUT"
printf '%s\n' "$BAD_OUTPUT" | grep -F '結果：FAIL' >/dev/null || fail "沒走到結尾的 FAIL 結論：$BAD_OUTPUT"
ok "codesign 失敗：UTF-8 locale 下印出 FAIL、rc=1，沒 crash"

echo "7 passed, 0 failed"
