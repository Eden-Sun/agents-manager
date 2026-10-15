#!/usr/bin/env bash
# `.claude/skills/verify/SKILL.md`（Claude Code 2.1.286+ 會在 commit 前自動叫起的 project skill，issue #747）的契約：
#   1. 唯一的指令入口是 `scripts/check.sh changed`，skill 裡沒有另一套 web/daemon/ops/ob 判斷（不複製 CI matrix）。
# changed 的路由與失敗傳遞由 check_changed_daemon_fast_test.sh 負責，這裡只驗 skill 入口。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILL="$ROOT/.claude/skills/verify/SKILL.md"
fail=0
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

[ -f "$SKILL" ] || { echo "FAIL: 缺 .claude/skills/verify/SKILL.md" >&2; exit 1; }

# --- 1. 靜態契約 ---
[ "$(sed -n 1p "$SKILL")" = '---' ] || bad 'SKILL.md 第一行不是 frontmatter 的 ---'
grep -qx 'name: verify' "$SKILL" || bad 'frontmatter 缺 name: verify'
grep -Eq '^description: .{20,}' "$SKILL" || bad 'frontmatter 缺 description（要寫清楚什麼時候該叫起）'
cmd="$(grep -m1 -E '^scripts/check\.sh changed( |$)' "$SKILL" || true)"
[ -n "$cmd" ] || bad 'SKILL.md 沒有單獨一行的 `scripts/check.sh changed` 指令'
# fenced code 裡不准出現別的驗證指令：那就是在複製 check.sh 的判斷。
dup="$(awk '/^```/{f=!f; next} f && /^[[:space:]]*(cargo|bun|bunx|npx|python3|node|pytest)[[:space:]]/' "$SKILL")"
[ -z "$dup" ] || bad "SKILL.md 的指令區塊複製了 check.sh 的內容：$dup"
grep -q 'ubuntu-ci' "$SKILL" || bad 'SKILL.md 沒講 Ubuntu Full CI 是非同步的另一道'
if (cd "$ROOT" && git check-ignore -q .claude/skills/verify/SKILL.md); then
    bad 'SKILL.md 被 .gitignore 擋住，不會進 repo'
fi
[ "$fail" = 0 ] || exit 1

if [ "$fail" = 0 ]; then echo "verify skill: OK"; fi
exit "$fail"
