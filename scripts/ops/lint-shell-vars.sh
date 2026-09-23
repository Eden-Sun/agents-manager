#!/usr/bin/env bash
# 擋「變數展開後面緊接非 ASCII 字元」的寫法（issue #412）。
#
#   scripts/ops/lint-shell-vars.sh              # 掃 repo 的 scripts/**/*.sh
#   scripts/ops/lint-shell-vars.sh 路徑 [路徑…]  # 掃指定的檔或目錄（測試用）
#
# 為什麼：macOS 的 /bin/bash 是 3.2。變數名後面直接接「（」「：」「／」這種全形標點時，
# bash 會把標點的位元組一起當成變數名的一部分，set -u 下就是 `BINARY?: unbound variable`，
# 腳本在印出該印的訊息之前就死掉，rc 還可能被 tail／pipe 吃掉而看不出來。三天內踩到三次
# （e0004c90 的 herdr-lan-check.sh、0ffa7612 的 daemon-update-kick.sh 與兩者的測試）。
#
# 修法一律是加大括號：${BINARY}（…）。規則刻意很機械——註解裡、單引號裡的也算，
# 因為 ${VAR} 在任何位置都是對的，不必記「這裡安不安全」。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# LC_ALL=C 下 [:print:] 是 0x20-0x7E、[:cntrl:] 是 0x00-0x1F 與 0x7F，
# 兩者的補集剛好是 0x80-0xFF，也就是任何非 ASCII 位元組（UTF-8 字元的第一個 byte）。
PATTERN='\$[A-Za-z_][A-Za-z0-9_]*[^[:print:][:cntrl:]]'

if [ "$#" -gt 0 ]; then
    targets=("$@")
else
    targets=("$ROOT/scripts")
fi

files="$(find "${targets[@]}" -type f -name '*.sh' | sort)"

report=""
scanned=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    scanned=$((scanned + 1))
    hits="$(LC_ALL=C grep -nE "$PATTERN" "$f" || true)"
    [ -n "$hits" ] || continue
    rel="${f#"$ROOT"/}"
    report="${report}$(printf '%s\n' "$hits" | sed "s|^|${rel}:|")
"
done <<INPUT
${files}
INPUT

if [ -n "$report" ]; then
    printf '%s' "$report" >&2
    cat >&2 <<'HINT'

上面這些位置的變數名後面直接接了非 ASCII 字元（多半是全形的「（」「：」「／」「，」）。
macOS 的 bash 3.2 會把標點併進變數名，set -u 下直接 unbound variable 而中止。
改成大括號形式即可，例如 ${pid}（…）、${rc}：…、${a}／${b}。
HINT
    exit 1
fi

printf 'lint-shell-vars: %s 個 .sh 檔乾淨\n' "${scanned}"
