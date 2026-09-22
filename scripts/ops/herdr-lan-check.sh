#!/bin/bash
# 驗證 herdr 升級後 macOS「本機網路」授權與區網 TCP 連線。
# 這支腳本必須從 herdr pane 執行；Apple 內建 nc/python3/curl 不可作為驗證工具。
set -u

BINARY="${1:-}"
HOST="${2:-${HERDR_LAN_HOST:-192.168.1.1}}"
PORT="${3:-${HERDR_LAN_PORT:-80}}"
AUTH_PLIST="${HERDR_NETWORK_AUTH_PLIST:-/Library/Preferences/com.apple.networkextension.plist}"
CODESIGN="${CODESIGN_BIN:-codesign}"
PLUTIL="${PLUTIL_BIN:-plutil}"
NODE="${NODE_BIN:-node}"
FAILED=0
SKIPPED=0

# 不帶參數時找真的 binary，不是 PATH 最前面那個：daemon 起的 bot pane 裡 `command -v herdr` 抓到的是
# per-bot shim（~/.config/agents-manager/bots/<id>/bin/herdr，一支沒簽章的 sh 腳本），codesign 會回
# "not signed at all" 而誤判 FAIL。跳過所有 shim（AM 的 bots/*/bin、或檔頭是 shell 腳本的），並解開 symlink
# （/opt/homebrew/bin/herdr → Cellar），授權表記的是真 binary 的 identifier。
is_shim() { # is_shim <path> → 0＝這是 shim／腳本，不是要驗的 binary
  case "$1" in */.config/agents-manager/bots/*/bin/herdr) return 0 ;; esac
  head -c 2 "$1" 2>/dev/null | grep -q '^#!'
}
if [ -z "$BINARY" ]; then
  _saved_ifs=$IFS; IFS=:
  for _dir in $PATH; do
    _cand="${_dir:-.}/herdr"
    [ -x "$_cand" ] || continue
    is_shim "$_cand" && continue
    BINARY=$(readlink -f "$_cand" 2>/dev/null || echo "$_cand")
    break
  done
  IFS=$_saved_ifs
fi

if [ -z "$BINARY" ]; then
  echo "1/3 identifier: FAIL PATH 上找不到真的 herdr binary（只有 shim 或沒有）；請把新 binary 路徑當第一個參數，例如 /opt/homebrew/bin/herdr"
  FAILED=1
else
  SIGNATURE=$("$CODESIGN" -dv "$BINARY" 2>&1)
  CODESIGN_RC=$?
  IDENTIFIER=$(printf '%s\n' "$SIGNATURE" | sed -n 's/^Identifier=//p' | head -1)
  if [ "$CODESIGN_RC" -eq 0 ] && [ -n "$IDENTIFIER" ]; then
    echo "1/3 identifier: $IDENTIFIER"
  else
    echo "1/3 identifier: FAIL codesign -dv 無法取出 identifier（${BINARY}）"
    [ -n "$SIGNATURE" ] && printf '%s\n' "$SIGNATURE" >&2
    IDENTIFIER=""
    FAILED=1
  fi
fi

if ! command -v "$PLUTIL" >/dev/null 2>&1; then
  echo "2/3 authorization: FAIL 找不到 plutil（不使用 sudo；授權表應可唯讀讀取）"
  FAILED=1
elif [ ! -r "$AUTH_PLIST" ]; then
  echo "2/3 authorization: FAIL 讀不到 ${AUTH_PLIST}（不使用 sudo）"
  FAILED=1
else
  AUTH_OUTPUT=$("$PLUTIL" -p "$AUTH_PLIST" 2>&1)
  PLUTIL_RC=$?
  if [ "$PLUTIL_RC" -ne 0 ]; then
    echo "2/3 authorization: FAIL plutil -p 讀取 ${AUTH_PLIST} 失敗"
    printf '%s\n' "$AUTH_OUTPUT" >&2
    FAILED=1
  elif [ -n "$IDENTIFIER" ] && printf '%s\n' "$AUTH_OUTPUT" | grep -F -- "$IDENTIFIER" >/dev/null 2>&1; then
    echo "2/3 authorization: found identifier $IDENTIFIER"
  elif [ -n "$BINARY" ] && printf '%s\n' "$AUTH_OUTPUT" | grep -F -- "$BINARY" >/dev/null 2>&1; then
    echo "2/3 authorization: FAIL 只找到 binary path，沒有新版 identifier $IDENTIFIER"
    FAILED=1
  else
    echo "2/3 authorization: FAIL 授權表沒有新版 identifier $IDENTIFIER"
    FAILED=1
  fi
fi

NODE_PATH=$(command -v "$NODE" 2>/dev/null || true)
if [ -z "$NODE_PATH" ]; then
  echo "3/3 LAN: SKIP 找不到 node；請在 herdr pane 安裝／使用 Node.js 後重跑，不可改用 Apple 內建 nc、python3 或 curl"
  SKIPPED=1
else
  NODE_OUTPUT=$($NODE_PATH -e '
const net = require("net");
const host = process.argv[1];
const port = Number(process.argv[2]);
const socket = net.createConnection({host, port});
let finished = false;
function finish(code, message) {
  if (finished) return;
  finished = true;
  console.log(message);
  socket.destroy();
  process.exit(code);
}
socket.setTimeout(5000);
socket.once("connect", () => finish(0, "node connected"));
socket.once("timeout", () => finish(1, "node connection timeout"));
socket.once("error", (error) => finish(1, `node ${error.code || error.message}`));
' "$HOST" "$PORT" 2>&1)
  NODE_RC=$?
  NODE_DETAIL=$(printf '%s\n' "$NODE_OUTPUT" | tr '\n' ' ' | sed 's/[[:space:]]*$//')
  if [ "$NODE_RC" -eq 0 ]; then
    echo "3/3 LAN: ${NODE_DETAIL} (${HOST}:${PORT})"
  else
    echo "3/3 LAN: FAIL ${NODE_DETAIL} (${HOST}:${PORT})"
    FAILED=1
  fi
fi

if [ "$FAILED" -ne 0 ]; then
  echo "結果：FAIL；需要使用者在 系統設定 → 隱私權與安全性 → 本機網路 允許新版 herdr。不要靠重啟硬試。" >&2
  exit 1
fi
if [ "$SKIPPED" -ne 0 ]; then
  echo "結果：SKIP；區網連線尚未驗證，升級流程不可繼續。" >&2
  exit 2
fi
echo "結果：PASS；新版 herdr 已取得授權且 node 區網連線成功。"
