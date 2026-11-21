check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1)); fi
}

check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}

equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}
