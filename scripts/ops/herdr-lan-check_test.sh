#!/bin/bash
# herdr-lan-check.sh 的隔離測試：假的 codesign／plutil／node，完全不碰真 binary、
# 真授權表或網路。測 identifier、授權表與 node TCP 三步都有回報。
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/herdr-lan-check.sh"
ROOT=$(mktemp -d)
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

# 不帶參數＋PATH 最前面是一支沒簽章的 shim（跟 daemon 起的 bot pane 一樣）：要跳過 shim 選到真 binary。
mkdir -p "$ROOT/.config/agents-manager/bots/b1/bin" "$ROOT/real/bin" "$ROOT/cellar"
printf '%s\n' '#!/bin/sh' 'exec /nowhere "$@"' > "$ROOT/.config/agents-manager/bots/b1/bin/herdr"
printf '%s\n' 'fake real binary' > "$ROOT/cellar/herdr"
ln -s "$ROOT/cellar/herdr" "$ROOT/real/bin/herdr"
chmod +x "$ROOT/.config/agents-manager/bots/b1/bin/herdr" "$ROOT/cellar/herdr"
NOARG_OUTPUT=$(PATH="$ROOT/.config/agents-manager/bots/b1/bin:$ROOT/real/bin:$ROOT/bin:/usr/bin:/bin" HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" HERDR_NODE_LOG="$ROOT/node.log" \
  bash "$SCRIPT" "" 192.168.1.1 80 2>&1) || fail "不帶參數時應跳過 shim、選到真 binary 並 PASS：$NOARG_OUTPUT"
printf '%s\n' "$NOARG_OUTPUT" | grep -F '1/3 identifier: herdr-1672acdeb8e5ac40' >/dev/null || fail "不帶參數沒選到真 binary：$NOARG_OUTPUT"
ok "不帶參數時跳過 per-bot shim，解 symlink 選到真 binary"

# PATH 上只有 shim：明確 FAIL、rc≠0，不可 crash。
set +e
ONLY_SHIM=$(PATH="$ROOT/.config/agents-manager/bots/b1/bin:$ROOT/bin:/usr/bin:/bin" HERDR_NETWORK_AUTH_PLIST="$ROOT/network.plist" HERDR_NODE_LOG="$ROOT/node.log" \
  bash "$SCRIPT" "" 192.168.1.1 80 2>&1)
ONLY_SHIM_RC=$?
set -u
[ "$ONLY_SHIM_RC" -eq 1 ] || fail "只有 shim 時應 FAIL（rc=1），實際 rc=${ONLY_SHIM_RC}：$ONLY_SHIM"
printf '%s\n' "$ONLY_SHIM" | grep -F '1/3 identifier: FAIL' >/dev/null || fail "只有 shim 時沒有明確 FAIL：$ONLY_SHIM"
ok "PATH 上只有 shim：明確 FAIL、rc=1"

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

echo "5 passed, 0 failed"
