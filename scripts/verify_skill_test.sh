#!/usr/bin/env bash
# `.claude/skills/verify/SKILL.md`（Claude Code 2.1.286+ 會在 commit 前自動叫起的 project skill，issue #747）的契約：
#   1. 唯一的指令入口是 `scripts/check.sh changed`，skill 裡沒有另一套 web/daemon/ops/ob 判斷（不複製 CI matrix）。
#   2. 照 skill 寫的那一行去跑，web／daemon／ops／ob 各自走到 check.sh 既有的 target，而且不會多跑別的
#      （Rust 小改只 cargo check，不跑整套 daemon 測試）。
#   3. check.sh 失敗時那一行也失敗（skill 據此不得宣稱已驗證）。
# 假 git／bun／cargo／python3／node 與假 canary 都放在暫存的 fixture 裡，不碰正式環境。
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
grep -Eq '不可|不得|不要' "$SKILL" || bad 'SKILL.md 沒寫失敗時不可宣稱已驗證'
if (cd "$ROOT" && git check-ignore -q .claude/skills/verify/SKILL.md); then
    bad 'SKILL.md 被 .gitignore 擋住，不會進 repo'
fi
[ "$fail" = 0 ] || exit 1

# --- 2./3. fixture：照 skill 那一行跑 ---
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-verify-skill.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
fx="$tmp/repo"
mkdir -p "$fx/scripts/ops" "$fx/web" "$fx/bin" "$fx/.claude/skills/verify"
cp "$ROOT/scripts/check.sh" "$ROOT/scripts/ci-changed-parts.sh" "$ROOT/scripts/ci-daemon-filters.sh" "$fx/scripts/"
cp "$SKILL" "$fx/.claude/skills/verify/SKILL.md"

stub() { # <name> <body>：記錄「名稱 參數」到 $AM_TEST_LOG
    printf '#!/usr/bin/env bash\nprintf "%s %%s\\n" "$*" >>"$AM_TEST_LOG"\n%s\n' "$1" "${2:-}" >"$fx/bin/$1"
    chmod +x "$fx/bin/$1"
}
stub bun; stub bunx; stub python3; stub node
stub cargo 'if [ -n "${AM_TEST_CARGO_FAIL:-}" ]; then exit 1; fi'
cat >"$fx/bin/git" <<'SH'
#!/usr/bin/env bash
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf '%s\n' "$AM_TEST_CHANGED" ;;
    "ls-files --others --exclude-standard") ;;
    "ls-files -- *.pyc *__pycache__*") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH
chmod +x "$fx/bin/git"
printf '#!/usr/bin/env bash\ntrue\n' >"$fx/scripts/chatgpt-consult.sh"
printf '#!/usr/bin/env bash\necho "ops-test ran" >>"$AM_TEST_LOG"\n' >"$fx/scripts/fake_test.sh"
cp "$fx/scripts/fake_test.sh" "$fx/scripts/ops/fake_ops_test.sh"
printf '#!/usr/bin/env bash\necho "lint-shell-vars" >>"$AM_TEST_LOG"\n' >"$fx/scripts/ops/lint-shell-vars.sh"
cat >"$fx/scripts/ops/destructive-canary.sh" <<'SH'
#!/usr/bin/env bash
case "$1" in
    arm) mkdir -p "$2"; printf '#!/bin/sh\n' >"$2/am-canary-probe"; chmod +x "$2/am-canary-probe"; echo "export AM_CANARY_DIR='$2'" ;;
    cmds) echo stub ;;
esac
SH
chmod +x "$fx/scripts/chatgpt-consult.sh" "$fx/scripts/fake_test.sh" "$fx/scripts/ops/"*.sh

run_case() { # <name> <changed path> <want regex（行首）...> -- <forbidden regex（行首）...>
    local name="$1" changed="$2" out rc=0 pat want=1
    shift 2
    : >"$tmp/log"
    out="$(cd "$fx" && env -u CHECK_TESTS AM_TEST_LOG="$tmp/log" AM_TEST_CHANGED="$changed" PATH="$fx/bin:$PATH" bash -c "$cmd" 2>&1)" || rc=$?
    if [ "$rc" != 0 ]; then bad "${name}：skill 那一行 rc=${rc}\n${out}"; return; fi
    for pat in "$@"; do
        if [ "$pat" = -- ]; then want=0; continue; fi
        if [ "$want" = 1 ]; then
            grep -Eq "^$pat" "$tmp/log" || bad "${name}：沒走到 [${pat}]；log：$(tr '\n' '|' <"$tmp/log")"
        else
            ! grep -Eq "^$pat" "$tmp/log" || bad "${name}：不該跑 [${pat}]；log：$(tr '\n' '|' <"$tmp/log")"
        fi
    done
}

run_case web web/src/a.ts 'bunx tsc -p tsconfig.app.json' 'bun test' -- 'cargo' 'python3'
run_case daemon daemon/src/api.rs 'cargo check -p agents-managerd --all-targets' 'cargo test -p agents-managerd --locked -- api::' -- 'bun' 'python3'
run_case ops scripts/ops/x.sh 'lint-shell-vars' 'ops-test ran' 'python3 -B scripts/agm_test\.' -- 'cargo' 'bunx tsc'
run_case ob scripts/ob_x.txt 'python3 -B scripts/ob_test\.' 'node --test scripts/ob_browser_test\.' -- 'cargo' 'bun'

# 失敗要傳出來：cargo 掛了，skill 那一行就要非 0，否則 agent 會把紅燈當成已驗證。
: >"$tmp/log"
if (cd "$fx" && AM_TEST_CARGO_FAIL=1 AM_TEST_LOG="$tmp/log" AM_TEST_CHANGED=daemon/src/api.rs PATH="$fx/bin:$PATH" bash -c "$cmd") >/dev/null 2>&1; then
    bad 'check 失敗時 skill 那一行仍然回 0'
fi

if [ "$fail" = 0 ]; then echo "verify skill: OK"; fi
exit "$fail"
