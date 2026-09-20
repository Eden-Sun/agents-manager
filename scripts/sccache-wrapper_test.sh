#!/bin/bash
# scripts/sccache-wrapper.sh 的隔離測試（issue #91）。
#
# 不碰這台機器實際有沒有裝 sccache：PATH 完全鎖進一個暫時目錄，裡面只放這個 case 需要的假
# rustc／sccache 執行檔，兩條路徑（有裝／沒裝）都能穩定測到，不受跑測試的機器現況影響。
#
#   bash scripts/sccache-wrapper_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WRAPPER="$HERE/sccache-wrapper.sh"
# 先用這支測試腳本自己的（未受限的）PATH 把 bash 解出絕對路徑：case 裡把 PATH 鎖進暫時目錄
# 之後，有些 shell 會用新的 PATH 去找要執行的命令本身（含 bash），鎖太乾淨反而連 bash 都
# 找不到——直接給絕對路徑就不受這個影響。
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

# 1. 退化路徑：PATH 上沒有 sccache（不管這台機器實際裝了沒有，這個 case 完全看不到），
#    直接照原樣呼叫 rustc，編譯不受影響（issue #91：不能是硬性依賴）。
setup
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" --edition 2021 foo.rs
check "沒有 sccache 時照樣呼叫 rustc" "rustc --edition 2021 foo.rs" "$CALLS"
teardown

# 1b. 退化路徑不濾掉 incremental 旗標：沒有 sccache 時，同一個 worktree 內的 incremental
#     compilation 要完全照舊，不能因為裝了這個 wrapper 就被拿掉。
setup
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C incremental=/wt-a/target/debug/incremental foo.rs
check "沒有 sccache 時 incremental 旗標原封不動" "incremental=/wt-a/target/debug/incremental" "$CALLS"
teardown

# 2. 有 sccache：轉呼叫 `sccache <rustc 本身> <參數...>`，不是繞過它直接呼叫 rustc。
setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" --edition 2021 foo.rs
check "裝了 sccache 就轉呼叫它，帶著原本的 rustc 呼叫" "sccache $FAKEBIN/rustc --edition 2021 foo.rs" "$CALLS"
check_no "不會繞過 sccache 直接呼叫 rustc" "^rustc " "$CALLS"
teardown

# 2b. `-C incremental=<path>` 兩個 token 的形式要被濾掉才交給 sccache，其他 `-C` 旗標留著
#     （issue #91 量測記錄：不濾掉的話，兩個 worktree 各自的 incremental 路徑不同，本來完全
#     沒改過的依賴也會被算成不同的快取鍵，實測命中率是 0%）。
setup
cat > "$FAKEBIN/sccache" <<'EOF'
#!/bin/bash
echo "sccache $*" >> "$CALLS"
exit 0
EOF
chmod +x "$FAKEBIN/sccache"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -C opt-level=0 -C incremental=/wt-a/target/debug/incremental -C debuginfo=2 foo.rs
check "留著跟 target dir 無關的 -C 旗標" "opt-level=0" "$CALLS"
check "留著 incremental 後面那個也是 -C 的旗標" "debuginfo=2" "$CALLS"
check_no "濾掉 -C incremental 這一組（兩個 token）" "incremental=" "$CALLS"

# 2c. 單一 token 的 `-Cincremental=<path>` 形式也要濾掉（rustc 兩種寫法都接受）。
: > "$CALLS"
PATH="$FAKEBIN" "$BASH_BIN" "$WRAPPER" "$FAKEBIN/rustc" -Cincremental=/wt-b/target/debug/incremental -C opt-level=0 foo.rs
check "留著其他旗標" "opt-level=0" "$CALLS"
check_no "濾掉單一 token 的 -Cincremental=" "incremental=" "$CALLS"
teardown

# 3. 沒人設過快取目錄／大小上限時，wrapper 自己補一個合理預設（issue #91：要有上限與清理策略，
#    sccache 本身的 SCCACHE_CACHE_SIZE 就是那個上限，交給它做 LRU 淘汰，不用自己另外寫）。
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

# 4. 呼叫端（例如某台機器的環境變數）自己設過快取目錄／大小：wrapper 不能蓋掉，
#    才能讓每台主機自己決定要放哪、放多大。
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

# 5. 結束碼要原封不動回傳：cargo 靠 rustc 的結束碼判斷編譯有沒有過，wrapper 用 `exec`
#    換掉行程本身，不能吞掉或改寫失敗的結束碼。
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
  echo "FAIL - 結束碼變成 $code，應該是 7"; FAIL=$((FAIL + 1))
fi
teardown

# 6. 有 sccache 時一樣要原封不動回傳它的結束碼（sccache 本身失敗、或它轉呼叫的 rustc 失敗）。
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
  echo "FAIL - 結束碼變成 $code，應該是 9"; FAIL=$((FAIL + 1))
fi
teardown

# 7. sccache 自己壞掉（連 --show-stats 都失敗）：退回直編 rustc，編譯照過（#376，檔頭承諾的行為）。
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

# 8. sccache 健康、真正的編譯錯誤：不能被吞成成功，也不能退回直編再編一次（#376）。
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
