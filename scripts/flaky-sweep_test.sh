#!/bin/bash
# flaky-sweep.sh 的隔離測試（issue #479）。
#
# 不編譯、不跑真的測試：`cargo` 與「測試 binary」都是 stub，只看這支腳本自己的決定——
#   1. 沒有同意閘門就不准在本機跑（協調者 2026-09-24 裁示）。
#   2. 全綠那一輪必須 exit 0：`set -euo pipefail` 加上去之後，彙總那段的 `grep` 找不到東西會回 1，
#      沒有 `|| true` 的話會在**最該回 0 的那一刻**把腳本殺掉。
#   3. 少跑輪次（某一份中途死掉、進不去目錄）不准當成全綠：這支的用途就是拿數字當證據。
#
#   bash scripts/flaky-sweep_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/flaky-sweep.sh"
PASS=0
FAIL=0

check() { # check <描述> <要出現的字串> <檔案>
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

check_eq() { # check_eq <描述> <期望> <實際>
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1))
  fi
}

setup() { # setup <假測試 binary 的結束碼>
  # 刻意**不 export ROOT／OUT**：flaky-sweep.sh 自己也用這兩個名字（ROOT＝repo 根），
  # 這裡 export 的話它的賦值會沿用 export 屬性，假 binary 就會拿到 repo 根當 ROOT 並在裡面寫檔。
  ROOT=$(mktemp -d)
  OUT="$ROOT/out" LOGF="$ROOT/run.log"
  mkdir -p "$ROOT/bin" "$ROOT/fake"
  export FAKE_MARK="$ROOT/truncated" FAKE_ROUNDS="$OUT/rounds.txt"
  # 假的測試 binary：結束碼可控，紅的時候印出 cargo test 那種 FAILED 行（彙總那段要吃得到）。
  { echo '#!/bin/bash'
    # 等 rounds.txt 已經有東西才清（第一輪就清等於沒少掉任何一行）。
    echo 'if [ -n "${FAKE_TRUNCATE_ROUNDS:-}" ] && [ ! -e "$FAKE_MARK" ] && [ -s "$FAKE_ROUNDS" ]; then'
    echo '  : > "$FAKE_ROUNDS"; touch "$FAKE_MARK"'   # 模擬某一份中途死掉、輪次少了
    echo 'fi'
    echo "[ \"$1\" = 0 ] || echo \"test a_flaky_one ... FAILED\""
    echo "exit $1"
  } > "$ROOT/fake/agents_managerd-deadbeef"
  chmod +x "$ROOT/fake/agents_managerd-deadbeef"
  # 假 cargo：只回 --message-format=json 那一行，讓腳本找得到「上次編好的」binary。
  { echo '#!/bin/bash'
    echo "printf '{\"reason\":\"compiler-artifact\",\"executable\":\"$ROOT/fake/agents_managerd-deadbeef\"}\\n'"
  } > "$ROOT/bin/cargo"
  chmod +x "$ROOT/bin/cargo"
  export PATH="$ROOT/bin:$PATH"
}
teardown() { rm -rf "$ROOT"; unset FAKE_TRUNCATE_ROUNDS; }

run() { # run <額外參數…>；閘門預設開著，測閘門的 case 自己 unset
  AM_ALLOW_LOCAL_FLAKY_SWEEP=${AM_ALLOW_LOCAL_FLAKY_SWEEP:-1} \
    bash "$SCRIPT" -k -o "$OUT" "$@" >"$LOGF" 2>&1
  echo $?
}

# 1. 沒有同意閘門：直接拒絕，不准在本機起高並行測試。
setup 0
rc=$(env -u AM_ALLOW_LOCAL_FLAKY_SWEEP bash -c 'bash "$1" -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$OUT" "$LOGF")
check_eq "沒有 --i-know 就 rc=2" "2" "$rc"
check "講出為什麼拒絕" "拒絕：本機預設禁跑" "$LOGF"
check "檔頭寫明本機禁跑" "本機預設禁跑" "$SCRIPT"
check "檔頭指向遠端／CI" "請走遠端編譯主機或 CI" "$LOGF"
teardown

# 2. --i-know 當作同意（環境變數以外的那條路）。
setup 0
rc=$(env -u AM_ALLOW_LOCAL_FLAKY_SWEEP bash -c 'bash "$1" --i-know -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$OUT" "$LOGF")
check_eq "--i-know 放行且全綠 rc=0" "0" "$rc"
teardown

# 3. 全綠：rc=0，而且輪數要跑滿。彙總那段的 grep 找不到東西不准把腳本殺掉（set -e ＋ pipefail）。
setup 0
rc=$(run -n 2 -c 2)
check_eq "全綠 rc=0" "0" "$rc"
check "輪數跑滿" "共 4 輪，紅了 0 輪" "$LOGF"
check_eq "rounds.txt 有 4 行" "4" "$(wc -l <"$OUT/rounds.txt" | tr -d ' ')"
check_no "沒有 unbound／pipefail 造成的意外中止" "unbound variable" "$LOGF"
teardown

# 4. 有輪次紅：rc=1，並列出紅過的測試。
setup 101
rc=$(run -n 2 -c 1)
check_eq "有紅 rc=1" "1" "$rc"
check "數出紅了幾輪" "共 2 輪，紅了 2 輪" "$LOGF"
check "列出紅過的測試" "a_flaky_one" "$LOGF"
teardown

# 5. 少跑輪次（某一份中途死掉）：不准回 0，也不准回 1——要用 rc=2 說「這輪不算數」。
#    以前 TOTAL=0 時 BAD 也是 0，最後那行 `[ "$BAD" = 0 ]` 成立，跟「跑滿全綠」同一個結束碼。
setup 0
export FAKE_TRUNCATE_ROUNDS=1
rc=$(run -n 3 -c 1)
check_eq "輪次不滿 rc=2" "2" "$rc"
check "講出跑到幾輪／該幾輪" "輪（有份沒跑起來" "$LOGF"
teardown

# 6. 輸出目錄建不出來：rc=2，不是默默用別的地方。
setup 0
touch "$ROOT/blocked"
rc=$(AM_ALLOW_LOCAL_FLAKY_SWEEP=1 bash -c 'bash "$1" -k -o "$2" -n 1 -c 1 >"$3" 2>&1; echo $?' _ "$SCRIPT" "$ROOT/blocked/out" "$LOGF")
check_eq "建不出輸出目錄 rc=2" "2" "$rc"
check "講出是哪個目錄" "建不出輸出目錄" "$LOGF"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
