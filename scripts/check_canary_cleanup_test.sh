#!/usr/bin/env bash
# check.sh ops 中途紅（set -e 提早結束）時，護欄目錄 am-canary-<pid> 不能留在 TMPDIR；
# 被護欄擋下的破壞性指令紀錄要先印出來再清（#1250）。假 fixture：真的 check.sh＋護欄、假 bun、一支自己紅的測試。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-canary.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
fx="$tmp/repo"
canary_tmp="$tmp/t"
mkdir -p "$fx/scripts/ops" "$fx/bin" "$canary_tmp"
cp "$ROOT/scripts/check.sh" "$fx/scripts/"
cp "$ROOT/scripts/ops/lint-shell-vars.sh" "$ROOT/scripts/ops/destructive-canary.sh" "$fx/scripts/ops/"
printf '# empty\n' >"$fx/scripts/ops/canary-baseline.tsv"
printf '#!/usr/bin/env bash\nexit 0\n' >"$fx/bin/bun"
chmod +x "$fx/bin/bun"
fail=0
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

# 前綴而不是覆寫 PATH：canary lint 才不會多出一處。
run_with_failing_test() {
    printf '%b' "$1" >"$fx/scripts/zz_fail_test.sh"
    (cd "$fx" && PATH="$fx/bin:$PATH" TMPDIR="$canary_tmp" bash scripts/check.sh ops 2>&1)
}

# 1. 測試紅：整輪非零，護欄目錄清掉。
rc=0; out="$(run_with_failing_test 'exit 1\n')" || rc=$?
[ "$rc" != 0 ] || bad "測試紅時 check.sh ops 要非零"
leftover="$(find "$canary_tmp" -maxdepth 1 -name 'am-canary-*' | wc -l | tr -d ' ')"
[ "$leftover" = 0 ] || bad "測試紅後護欄目錄還留在 TMPDIR（$leftover 個）：$out"

# 2. 被擋下的指令：紅的同時要把紀錄印出來，目錄一樣不留。
rc=0; out="$(run_with_failing_test 'am-canary-probe\nexit 1\n')" || rc=$?
[ "$rc" != 0 ] || bad "叫到破壞性指令時 check.sh ops 要非零"
case "$out" in *"中斷前有測試真的叫到破壞性指令"*) ;; *) bad "中斷時沒印出被擋下的指令紀錄：$out" ;; esac
case "$out" in *"am-canary-probe"*) ;; *) bad "紀錄裡沒有被擋下的指令名：$out" ;; esac
leftover="$(find "$canary_tmp" -maxdepth 1 -name 'am-canary-*' | wc -l | tr -d ' ')"
[ "$leftover" = 0 ] || bad "叫到破壞性指令後護欄目錄還留在 TMPDIR（$leftover 個）"

[ "$fail" = 0 ] && echo "check.sh canary cleanup: OK"
exit "$fail"
