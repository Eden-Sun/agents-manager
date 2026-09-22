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
printf '%s\n' '#!/bin/bash' 'echo "Identifier=herdr-1672acdeb8e5ac40" >&2' > "$ROOT/bin/codesign"
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

echo "2 passed, 0 failed"
