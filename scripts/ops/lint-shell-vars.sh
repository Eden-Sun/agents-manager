#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

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
