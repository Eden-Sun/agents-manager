#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WRAPPER="$HERE/sccache-wrapper.sh"
BASH_BIN="$(command -v bash)"
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  FAKEBIN="$ROOT/bin"
  mkdir -p "$FAKEBIN"
  CALLS="$ROOT/calls.log"
  : > "$CALLS"
  export CALLS
  cat > "$FAKEBIN/rustc" <<'EOF'
#!/bin/bash
echo "rustc $*" >> "$CALLS"
exit 0
EOF
  chmod +x "$FAKEBIN/rustc"
}

teardown() { rm -rf "$ROOT"; unset SCCACHE_DIR SCCACHE_CACHE_SIZE CALLS; }

check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1))
  fi
}

check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "FAIL - $1"; echo "      不該出現 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else
    echo "ok   - $1"; PASS=$((PASS + 1))
  fi
}

setup
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" --edition 2021 foo.rs
check "沒有 sccache 時照樣呼叫 rustc" "rustc --edition 2021 foo.rs" "$CALLS"
teardown

setup
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C incremental=/wt-a/target/debug/incremental foo.rs
check "沒有 sccache 時 incremental 旗標原封不動" "incremental=/wt-a/target/debug/incremental" "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C opt-level=0 --edition 2021 foo.rs
check "裝了 sccache 就轉呼叫它，帶著原本的 rustc 與全部旗標（registry 依賴：不濾掉任何 -C）" "sccache $FAKEBIN/rustc -C opt-level=0 --edition 2021 foo.rs" "$CALLS"
check_no "不會繞過 sccache 直接呼叫 rustc" "^rustc " "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C opt-level=0 -C incremental=/wt-a/target/debug/incremental -C debuginfo=2 foo.rs
check "workspace crate 直接呼叫 rustc、旗標原樣" "^rustc -C opt-level=0 -C incremental=/wt-a/target/debug/incremental -C debuginfo=2 foo.rs" "$CALLS"
check_no "workspace crate 不經過 sccache（連 --show-stats 都沒有）" "^sccache" "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -Cincremental=/wt-b/target/debug/incremental -C opt-level=0 foo.rs
check "單一 token 的 incremental：直接呼叫 rustc、旗標原樣" "^rustc -Cincremental=/wt-b/target/debug/incremental -C opt-level=0 foo.rs" "$CALLS"
check_no "單一 token 的 incremental：不經過 sccache" "^sccache" "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "SCCACHE_DIR=$SCCACHE_DIR" >> "$CALLS"
echo "SCCACHE_CACHE_SIZE=$SCCACHE_CACHE_SIZE" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
unset SCCACHE_DIR SCCACHE_CACHE_SIZE
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc"
check "沒設快取目錄時給預設" "SCCACHE_DIR=.*agents-manager-sccache" "$CALLS"
check "沒設大小上限時給預設" "SCCACHE_CACHE_SIZE=10G" "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "SCCACHE_DIR=$SCCACHE_DIR" >> "$CALLS"
echo "SCCACHE_CACHE_SIZE=$SCCACHE_CACHE_SIZE" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
export SCCACHE_DIR="/custom/cache" SCCACHE_CACHE_SIZE="2G"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc"
check "不蓋掉呼叫端自己設的快取目錄" "SCCACHE_DIR=/custom/cache" "$CALLS"
check "不蓋掉呼叫端自己設的大小上限" "SCCACHE_CACHE_SIZE=2G" "$CALLS"
teardown

setup
cat > "$FAKEBIN/rustc" <<'EOF'
#!/bin/bash
exit 7
EOF
chmod +x "$FAKEBIN/rustc"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc"
code=$?
if [ "$code" -eq 7 ]; then
  echo "ok   - 結束碼原樣回傳（沒裝 sccache 這條路徑）"; PASS=$((PASS + 1))
else
  echo "FAIL - 結束碼變成 ${code}，應該是 7"; FAIL=$((FAIL + 1))
fi
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
[ "${1:-}" = "--show-stats" ] && exit 0
exit 9
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc"
code=$?
if [ "$code" -eq 9 ]; then
  echo "ok   - 結束碼原樣回傳（有 sccache 這條路徑）"; PASS=$((PASS + 1))
else
  echo "FAIL - 結束碼變成 ${code}，應該是 9"; FAIL=$((FAIL + 1))
fi
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 2
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C incremental=/wt-a/inc --edition 2021 foo.rs
code=$?
[ "$code" -eq 0 ] && { echo "ok   - sccache 壞掉：exit 0"; PASS=$((PASS + 1)); } || { echo "FAIL - sccache 壞掉：exit ${code}，應該退回直編"; FAIL=$((FAIL + 1)); }
check "sccache 壞掉：直編 rustc" "^rustc -C incremental=/wt-a/inc --edition 2021 foo.rs" "$CALLS"
teardown

setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
[ "${1:-}" = "--show-stats" ] && exit 0
exit 1
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" foo.rs
code=$?
[ "$code" -eq 1 ] && { echo "ok   - 編譯失敗：結束碼 1 原樣回傳"; PASS=$((PASS + 1)); } || { echo "FAIL - 編譯失敗：結束碼 ${code}，應該是 1"; FAIL=$((FAIL + 1)); }
check_no "編譯失敗：不退回直編" "^rustc " "$CALLS"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
