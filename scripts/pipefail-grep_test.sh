#!/usr/bin/env bash
set -euo pipefail

big="needle"$'\n'"$(head -c 8388608 /dev/zero | tr '\0' x)"
rounds=20
old_bad=0
new_bad=0
for ((i = 0; i < rounds; i++)); do
    if ! printf '%s' "$big" | grep -q needle; then old_bad=$((old_bad + 1)); fi
    if ! grep -q needle <<<"$big"; then new_bad=$((new_bad + 1)); fi
done
echo "pipefail-grep: old-style fails ${old_bad}/${rounds}, here-string fails ${new_bad}/${rounds}"
[ "$new_bad" = 0 ] || { echo "FAIL: here-string 式在大輸出下誤判 ${new_bad} 次"; exit 1; }
[ "$old_bad" -ge 1 ] || { echo "FAIL: 舊式 ${rounds} 輪都沒重現 SIGPIPE，無法證明新式的必要（斷言不放寬）"; exit 1; }

small=$'alpha beta\nfoo.bar\nabc\n'
grep -q 'alpha' <<<"$small" || { echo 'FAIL: 命中要 rc 0'; exit 1; }
if grep -q 'gamma' <<<"$small"; then echo 'FAIL: 不命中卻 rc 0'; exit 1; fi
grep -qx 'abc' <<<"$small" || { echo 'FAIL: -x 整行命中要 rc 0'; exit 1; }
if grep -qx 'ab' <<<"$small"; then echo 'FAIL: -x 只給部分行卻命中'; exit 1; fi
grep -qF 'foo.bar' <<<"$small" || { echo 'FAIL: -F 字面命中要 rc 0'; exit 1; }
if grep -qF 'f.o' <<<"$small"; then echo 'FAIL: -F 把 . 當成萬用字元'; exit 1; fi
grep -qE 'alp(ha|x) beta' <<<"$small" || { echo 'FAIL: -E 交替命中要 rc 0'; exit 1; }
if grep -qE 'gam|del' <<<"$small"; then echo 'FAIL: -E 誤命中'; exit 1; fi
if grep -q 'gamma' <<<"$small" && false; then echo 'FAIL: && 短路錯誤'; exit 1; fi
grep -q 'abc' <<<"$small" && ok=1 || ok=0
[ "$ok" = 1 ] || { echo 'FAIL: && 命中分支沒走到'; exit 1; }
if grep -q 'zzz' <<<"$small" || false; then echo 'FAIL: || 不命中分支錯誤'; exit 1; fi

echo 'pipefail-grep: here-string 大輸出穩定，旗標與 && / || / 否定語意通過'
