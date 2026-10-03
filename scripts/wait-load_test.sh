#!/bin/bash
# scripts/wait-load.sh 與 check.sh 負載閘的隔離測試（issue #813）：負載從假檔案讀（WAIT_LOAD_FILE），不看真機器。
# 重點是「逾時一定回非 0、`&&` 後面的指令不會跑」——#789 的手寫迴圈逾時卻回成功，在 load≈152 時照跑。
#
#   bash scripts/wait-load_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WAIT="${HERE}/wait-load.sh"
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

ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
LOAD="${ROOT}/loadavg"

echo "12.50 30.00 40.00 3/900 1234" > "${LOAD}"
out="$(WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 5 2>&1)"; rc=$?
equals "負載在門檻以下：立刻放行" "${rc}" 0

echo "40.00 1 1" > "${LOAD}"
WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 0 >/dev/null 2>&1; rc=$?
equals "剛好等於門檻也放行" "${rc}" 0

echo "152.30 120.00 100.00" > "${LOAD}"
out="$(WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 0 2>&1)"; rc=$?
equals "負載太高、--timeout 0：只看一次就失敗" "${rc}" 124
has "失敗時講明逾時與當下負載" "${out}" "152.30"

start="$(date +%s)"
out="$(WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40.5 --timeout 2 --interval 1 2>&1)"; rc=$?
took=$(($(date +%s) - start))
equals "等到逾時負載還是高：回 124，不是 0" "${rc}" 124
if [ "${took}" -ge 2 ] && [ "${took}" -le 6 ]; then equals "真的等滿 --timeout 才放棄（${took} 秒）" ok ok; else equals "真的等滿 --timeout 才放棄" "${took}" "2～6"; fi

ran=no
WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 1 --interval 1 >/dev/null 2>&1 && ran=yes
equals "逾時時 \`wait-load.sh && 指令\` 的指令不會跑" "${ran}" no

# 等的途中負載降下來：放行。
echo "90.00 1 1" > "${LOAD}"
( sleep 1; echo "10.00 1 1" > "${LOAD}" ) &
out="$(WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 20 --interval 1 2>&1)"; rc=$?
wait
equals "等的途中負載降下來就放行" "${rc}" 0
has "放行時講一聲" "${out}" "放行"

WAIT_LOAD_FILE="${ROOT}/nope" bash "${WAIT}" --max 40 --timeout 0 >/dev/null 2>&1; rc=$?
equals "讀不到負載：不放行（3）" "${rc}" 3
echo "garbage" > "${LOAD}"
WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" --max 40 --timeout 0 >/dev/null 2>&1; rc=$?
equals "負載看不懂：不放行（3）" "${rc}" 3

for bad in "--max abc" "--timeout -1" "--interval 0" "--bogus"; do
  # shellcheck disable=SC2086
  WAIT_LOAD_FILE="${LOAD}" bash "${WAIT}" ${bad} >/dev/null 2>&1; rc=$?
  equals "參數錯（${bad}）回 2" "${rc}" 2
done

# check.sh 的負載閘：在任何一步之前就擋下（這裡用一個不存在的模式，閘放行才會走到「用法」的 exit 2）。
echo "152.30 1 1" > "${LOAD}"
out="$(cd "${HERE}/.." && WAIT_LOAD_FILE="${LOAD}" CHECK_MAX_LOAD=40 CHECK_LOAD_TIMEOUT=0 bash scripts/check.sh no-such-mode 2>&1)"; rc=$?
equals "check.sh：負載太高、逾時就整輪失敗，不往下跑" "${rc}" 124
has "check.sh：講明是負載閘擋下" "${out}" "負載閘"
echo "1.00 1 1" > "${LOAD}"
out="$(cd "${HERE}/.." && WAIT_LOAD_FILE="${LOAD}" CHECK_MAX_LOAD=40 CHECK_LOAD_TIMEOUT=0 bash scripts/check.sh no-such-mode 2>&1)"; rc=$?
equals "check.sh：負載夠低就照常往下（走到用法錯誤）" "${rc}" 2
# 只在最外層等：閘放行後子行程看不到 CHECK_MAX_LOAD（ops 測試會在沙盒裡再叫 check.sh，那裡沒有 wait-load.sh）。
fake="${ROOT}/fake-check"
mkdir -p "${fake}/scripts"
cp "${HERE}/check.sh" "${HERE}/wait-load.sh" "${fake}/scripts/"
printf '#!/bin/bash\necho "nested CHECK_MAX_LOAD=${CHECK_MAX_LOAD:-unset}"\n' > "${ROOT}/probe.sh"
sed -i.bak 's|^case "${1:-default}" in|bash "${PROBE}"\ncase "${1:-default}" in|' "${fake}/scripts/check.sh"
out="$(cd "${fake}" && PROBE="${ROOT}/probe.sh" WAIT_LOAD_FILE="${LOAD}" CHECK_MAX_LOAD=40 CHECK_LOAD_TIMEOUT=0 bash scripts/check.sh no-such-mode 2>&1)"
has "check.sh：閘放行後子行程不再看到 CHECK_MAX_LOAD" "${out}" "nested CHECK_MAX_LOAD=unset"
out="$(cd "${HERE}/.." && env -u CHECK_MAX_LOAD WAIT_LOAD_FILE="${ROOT}/nope" bash scripts/check.sh no-such-mode 2>&1)"; rc=$?
equals "check.sh：沒設 CHECK_MAX_LOAD 就不看負載" "${rc}" 2

echo "${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]
