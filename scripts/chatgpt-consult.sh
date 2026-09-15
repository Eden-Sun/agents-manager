#!/bin/bash
# 問 ChatGPT 決策顧問（ego lite）。每個專案一個固定對話，不重複開。說明：docs/CHATGPT-CONSULT.md
#
#   scripts/chatgpt-consult.sh [-p 專案名] "問題"
#   scripts/chatgpt-consult.sh [-p 專案名] -f question.md
#   scripts/chatgpt-consult.sh -p 專案名 --new "問題"     # 丟掉記住的對話重開（少用）
#
# 專案名預設是目前 git repo 的目錄名（agents-manager 裡就是 AG Man 的 project label）。
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
project=""
file=""
new=0
while [ $# -gt 0 ]; do
  case "$1" in
    -p|--project) project="$2"; shift 2 ;;
    -f|--file) file="$2"; shift 2 ;;
    --new) new=1; shift ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) break ;;
  esac
done
if [ -z "$project" ]; then
  top="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
  project="$(basename "$top")"
fi
tmp=""
if [ -z "$file" ]; then
  [ $# -gt 0 ] || { echo "用法：$0 [-p 專案名] \"問題\"  或  -f 問題檔" >&2; exit 2; }
  tmp="$(mktemp -t chatgpt-consult)"
  printf '%s\n' "$*" > "$tmp"
  file="$tmp"
fi
trap '[ -n "$tmp" ] && rm -f "$tmp"' EXIT
# ego 的 nodejs 不繼承環境變數：參數做成一行 JSON 接在腳本前面。
prelude="$(python3 -c 'import json,sys; print("globalThis.CONSULT_ARGS = " + json.dumps(dict(zip(["CONSULT_PROJECT","CONSULT_QUESTION_FILE","CONSULT_NEW","CONSULT_TIMEOUT_MS"], sys.argv[1:])), ensure_ascii=False) + ";")' \
  "$project" "$(cd "$(dirname "$file")" && pwd)/$(basename "$file")" "$new" "${CONSULT_TIMEOUT_MS:-600000}")"
{ printf '%s\n' "$prelude"; cat "$here/chatgpt-consult.mjs"; } | ego-browser nodejs
