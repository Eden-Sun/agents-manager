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

PS="${PS_BIN:-ps}"
LSOF="${LSOF_BIN:-lsof}"
FALLBACK="${HERDR_FALLBACK_BIN:-/opt/homebrew/bin/herdr}"

# 不帶參數時驗「正在跑的 herdr server 的執行檔」：授權表記的是 server 那顆 binary 的 identifier。
# 以前是 PATH 上第一顆真 binary，但 PATH 上可能有另一份簽章不同的副本（2026-09-22 這台的 ~/.local/bin/herdr，
# identifier 不同、授權表沒有）→ 回報 FAIL，其實所有 server 跑的都是 Cellar 那顆、有授權。
# 認 server 用 argv：第一個字的 basename 是 herdr，且有一個參數剛好是 `server`（`pgrep -f` 會連 claude 的
# 長 argv 裡恰好提到 herdr／server 的字串一起抓進來）。實際路徑用 lsof 的 txt（解掉 symlink 與 `herdr` 這種相對名）。
server_binaries() { # 每行一個不重複的執行檔路徑
  "$PS" -axo pid=,args= 2>/dev/null | awk '
    { cmd=$2; n=split(cmd, parts, "/"); base=parts[n]
      if (base != "herdr") next
      for (i = 3; i <= NF; i++) if ($i == "server") { print $1; break } }' |
  while read -r pid; do
    "$LSOF" -a -p "$pid" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -1
  done | awk 'NF && !seen[$0]++'
}

BINARIES=()
if [ -n "$BINARY" ]; then
  BINARIES=("$BINARY")
else
  while IFS= read -r _b; do [ -n "$_b" ] && BINARIES+=("$_b"); done < <(server_binaries)
  if [ "${#BINARIES[@]}" -gt 0 ]; then
    echo "0/3 binary: 驗正在跑的 herdr server 執行檔（${#BINARIES[@]} 個）：${BINARIES[*]}"
  elif [ -e "$FALLBACK" ]; then
    _resolved=$(readlink -f "$FALLBACK" 2>/dev/null || echo "$FALLBACK")
    BINARIES=("$_resolved")
    echo "0/3 binary: 沒有在跑的 herdr server，退回 ${FALLBACK} → ${_resolved}"
  fi
fi

# 逐一驗：好幾個 server 跑不同路徑時，每一顆都要有授權，任何一顆沒有就 FAIL。
IDENTIFIERS=()
if [ "${#BINARIES[@]}" -eq 0 ]; then
  echo "1/3 identifier: FAIL 找不到在跑的 herdr server，${FALLBACK} 也不存在；請把新 binary 路徑當第一個參數"
  FAILED=1
else
  for BINARY in "${BINARIES[@]}"; do
    SIGNATURE=$("$CODESIGN" -dv "$BINARY" 2>&1)
    CODESIGN_RC=$?
    IDENTIFIER=$(printf '%s\n' "$SIGNATURE" | sed -n 's/^Identifier=//p' | head -1)
    if [ "$CODESIGN_RC" -eq 0 ] && [ -n "$IDENTIFIER" ]; then
      echo "1/3 identifier: ${IDENTIFIER}（${BINARY}）"
      IDENTIFIERS+=("$IDENTIFIER|$BINARY")
    else
      echo "1/3 identifier: FAIL codesign -dv 無法取出 identifier（${BINARY}）"
      [ -n "$SIGNATURE" ] && printf '%s\n' "$SIGNATURE" >&2
      FAILED=1
    fi
  done
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
  elif [ "${#IDENTIFIERS[@]}" -eq 0 ]; then
    echo "2/3 authorization: FAIL 沒有可比對的 identifier"
    FAILED=1
  else
    for _pair in "${IDENTIFIERS[@]}"; do
      IDENTIFIER=${_pair%%|*}
      BINARY=${_pair#*|}
      if printf '%s\n' "$AUTH_OUTPUT" | grep -F -- "$IDENTIFIER" >/dev/null 2>&1; then
        echo "2/3 authorization: found identifier $IDENTIFIER"
      elif printf '%s\n' "$AUTH_OUTPUT" | grep -F -- "$BINARY" >/dev/null 2>&1; then
        echo "2/3 authorization: FAIL 只找到 binary path，沒有新版 identifier ${IDENTIFIER}（${BINARY}）"
        FAILED=1
      else
        echo "2/3 authorization: FAIL 授權表沒有新版 identifier ${IDENTIFIER}（${BINARY}）"
        FAILED=1
      fi
    done
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
