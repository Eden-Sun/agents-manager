#!/bin/bash
# lint-shell-vars.sh 的隔離測試（issue #412）：在暫存目錄裡放假的 .sh，驗「該中的中、不該中的不中」。
#
#   bash scripts/ops/lint-shell-vars_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
LINT="${HERE}/lint-shell-vars.sh"
PASS=0
FAIL=0
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}
has() {
  case "$2" in
    *"$3"*) echo "ok   - $1"; PASS=$((PASS + 1)) ;;
    *) echo "FAIL - $1"; echo "      找不到 '$3'，實際輸出："; printf '%s\n' "$2" | sed 's/^/      /'; FAIL=$((FAIL + 1)) ;;
  esac
}
has_no() {
  case "$2" in
    *"$3"*) echo "FAIL - $1"; echo "      不該出現 '$3'，實際輸出："; printf '%s\n' "$2" | sed 's/^/      /'; FAIL=$((FAIL + 1)) ;;
    *) echo "ok   - $1"; PASS=$((PASS + 1)) ;;
  esac
}

ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
mkdir -p "${ROOT}/dirty" "${ROOT}/clean/nested"

# 壞寫法的字面值直接寫在這裡會被這支 lint 自己掃到（這個檔也是 scripts/**/*.sh），
# 所以錢字號用 ${D} 拼出來，heredoc 不加引號讓它展開，其他的錢字號用反斜線跳脫。
D='$'

# 會中的：第 2 行全形括號、第 6 行全形斜線。
cat > "${ROOT}/dirty/bad.sh" <<EOF
#!/bin/bash
echo "keep pid ${D}pid（父程序還活著）"
echo "\${ok}（大括號不算）"
echo "\$plain 後面有空白"
echo "\$ascii, ASCII 標點不算"
echo "收掉 ${D}reaped／保留 \${kept}"
EOF

# 不會中的：全部都是 ${VAR} 或後面接 ASCII。
cat > "${ROOT}/dirty/good.sh" <<'EOF'
#!/bin/bash
echo "${pid}（好寫法）"
echo "$plain 有空白就沒事"
V="$HOME/x"
echo "$V"
EOF

# 同樣的壞寫法但不是 .sh，不該被掃到。
cp "${ROOT}/dirty/bad.sh" "${ROOT}/dirty/bad.txt"
cp "${ROOT}/dirty/good.sh" "${ROOT}/clean/nested/good.sh"

out="$(bash "${LINT}" "${ROOT}/dirty" 2>&1)"; rc=$?
equals "有壞寫法時 exit 1" "${rc}" "1"
has    "列出壞的檔名與行號（全形括號）" "${out}" "bad.sh:2:"
has    "同一個檔的第二處也列出（全形斜線）" "${out}" "bad.sh:6:"
has_no "乾淨的檔不列" "${out}" "good.sh"
has_no "非 .sh 的檔不掃" "${out}" "bad.txt"
has_no "大括號形式不算" "${out}" "bad.sh:3:"
has_no "後面接空白不算" "${out}" "bad.sh:4:"
has_no "後面接 ASCII 標點不算" "${out}" "bad.sh:5:"
has    "有提示怎麼修" "${out}" "改成大括號形式"

out="$(bash "${LINT}" "${ROOT}/clean" 2>&1)"; rc=$?
equals "全乾淨時 exit 0" "${rc}" "0"
has    "全乾淨時回報掃了幾個檔" "${out}" "1 個 .sh 檔乾淨"

# 也接受直接指定單一檔案。
out="$(bash "${LINT}" "${ROOT}/dirty/bad.sh" 2>&1)"; rc=$?
equals "單一壞檔 exit 1" "${rc}" "1"
out="$(bash "${LINT}" "${ROOT}/dirty/good.sh" 2>&1)"; rc=$?
equals "單一好檔 exit 0" "${rc}" "0"

# 沒帶參數就掃 repo 自己的 scripts/**/*.sh，現況必須是乾淨的。
out="$(bash "${LINT}" 2>&1)"; rc=$?
equals "repo 現況乾淨（預設掃 scripts/）" "${rc}" "0"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
