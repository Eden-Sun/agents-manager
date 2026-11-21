#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/flaky-sweep.sh"
PASS=0
FAIL=0

check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  fi
}

check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "FAIL - $1"; echo "      不該出現 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else
    echo "ok   - $1"; PASS=$((PASS + 1))
  fi
}

check_eq() {
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1))
  fi
}

setup() {
  ROOT=$(mktemp -d)
  OUT="$ROOT/out" LOGF="$ROOT/run.log"
  mkdir -p "$ROOT/bin" "$ROOT/fake"
  export FAKE_MARK="$ROOT/truncated" FAKE_ROUNDS="$OUT/rounds.txt"
  { echo '#!/bin/bash'
    echo 'if [ -n "${FAKE_TRUNCATE_ROUNDS:-}" ] && [ ! -e "$FAKE_MARK" ] && [ -s "$FAKE_ROUNDS" ]; then'
    echo '  : > "$FAKE_ROUNDS"; touch "$FAKE_MARK"'
    echo 'fi'
    echo "if [ \"\${FAKE_ZERO_TESTS:-0}\" = 1 ]; then echo 'test result: ok. 0 passed; 0 failed;'; exit 0; fi"
    echo "if [ \"$1\" = 0 ]; then echo 'test result: ok. 1 passed; 0 failed;'; else echo 'test a_flaky_one ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed;'; fi"
    echo "exit $1"
  } > "$ROOT/fake/agents_managerd-deadbeef"
  chmod +x "$ROOT/fake/agents_managerd-deadbeef"
  { echo '#!/bin/bash'
    echo "printf '{\"reason\":\"compiler-artifact\",\"executable\":\"$ROOT/fake/agents_managerd-deadbeef\"}\\n'"
  } > "$ROOT/bin/cargo"
  chmod +x "$ROOT/bin/cargo"
  export PATH="$ROOT/bin:$PATH" FAKE_ZERO_TESTS=0
}
teardown() { rm -rf "$ROOT"; unset FAKE_TRUNCATE_ROUNDS FAKE_ZERO_TESTS; }

run() {
  bash "$SCRIPT" --i-know -k -o "$OUT" "$@" >"$LOGF" 2>&1
  echo $?
}

setup 0
rc=$(bash -c 'bash "$1" -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$OUT" "$LOGF")
check_eq "沒有 --i-know 就 rc=2" "2" "$rc"
check "講出為什麼拒絕" "拒絕：本機預設禁跑" "$LOGF"
check "檔頭寫明本機禁跑" "本機預設禁跑" "$SCRIPT"
check "檔頭指向遠端／CI" "請走遠端編譯主機或 CI" "$LOGF"
check "說明印到檔頭最後一行" "這支的用途就是拿數字當證據" "$LOGF"
check_no "沒印到程式碼" "set -euo pipefail" "$LOGF"
check "說明是整段檔頭印出來的" "usage() { awk" "$SCRIPT"
check_no "不是用寫死行號從 \$0 切出來的" "sed -n .*\"\$0\"" "$SCRIPT"
teardown

setup 0
rc=$(AM_ALLOW_LOCAL_FLAKY_SWEEP=1 bash -c 'bash "$1" -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$OUT" "$LOGF")
check_eq "環境變數繞不過去（rc=2）" "2" "$rc"
check_no "腳本裡沒有那個環境變數的繞法" "AM_ALLOW_LOCAL_FLAKY_SWEEP" "$SCRIPT"
teardown

setup 0
rc=$(run -n 0 -c 1); check_eq "-n 0 要紅（rc=2）" "2" "$rc"
check "講出是哪個旗標" "-n 要大於 0" "$LOGF"
rc=$(run -n 1 -c 0); check_eq "-c 0 要紅（rc=2）" "2" "$rc"
rc=$(run -n 1 -c 1 -t 0); check_eq "-t 0 要紅（rc=2）" "2" "$rc"
rc=$(run -n abc -c 1); check_eq "-n 非數字要紅（rc=2）" "2" "$rc"
check "講出拿到什麼" "要是正整數，拿到的是 'abc'" "$LOGF"
teardown

setup 0
rc=$(run -n 2 -c 2)
check_eq "全綠 rc=0" "0" "$rc"
check "輪數跑滿" "共 4 輪，紅了 0 輪" "$LOGF"
check_eq "rounds.txt 有 4 行" "4" "$(wc -l <"$OUT/rounds.txt" | tr -d ' ')"
check_no "沒有 unbound／pipefail 造成的意外中止" "unbound variable" "$LOGF"
teardown

setup 101
rc=$(run -n 2 -c 1)
check_eq "有紅 rc=1" "1" "$rc"
check "數出紅了幾輪" "共 2 輪，紅了 2 輪" "$LOGF"
check "列出紅過的測試" "a_flaky_one" "$LOGF"
teardown

setup 0
export FAKE_TRUNCATE_ROUNDS=1
rc=$(run -n 3 -c 1)
check_eq "輪次不滿 rc=2" "2" "$rc"
check "講出跑到幾輪／該幾輪" "輪（有份沒跑起來" "$LOGF"
teardown

setup 0
touch "$ROOT/blocked"
rc=$(bash -c 'bash "$1" --i-know -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$ROOT/blocked/out" "$LOGF")
check_eq "建不出輸出目錄 rc=2" "2" "$rc"
check "講出是哪個目錄" "建不出輸出目錄" "$LOGF"
teardown

setup 0
export FAKE_ZERO_TESTS=1
rc=$(run -n 2 -c 2 -f old_test_name)
check_eq "過濾到 0 條測試要 exit 2" "2" "$rc"
check "講明有輪次沒有跑到測試" "輪沒有跑到任何測試" "$LOGF"
check_eq "四輪都記下 no-tests" "4" "$(awk '$4 == "no-tests" {n++} END {print n+0}' "$OUT/rounds.txt")"
teardown

setup 0
rc=$(cd "$ROOT" && bash "$SCRIPT" --i-know -k -o relout -n 1 -c 1 >"$LOGF" 2>&1; echo $?)
check_eq "相對 -o 全綠 rc=0" "0" "$rc"
check "輪數跑滿" "共 1 輪，紅了 0 輪" "$LOGF"
check_eq "log 落在呼叫端 cwd 底下" "yes" "$([ -f "$ROOT/relout/1-1.log" ] && echo yes || echo no)"
check_eq "rounds.txt 也在那裡" "1" "$(wc -l <"$ROOT/relout/rounds.txt" 2>/dev/null | tr -d ' ')"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
