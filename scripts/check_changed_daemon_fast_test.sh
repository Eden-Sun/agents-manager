#!/usr/bin/env bash
# Clean-checkout daemon fast path: rust-embed needs an index.html, but should not trigger web CI.
set -euo pipefail
# 外層 `CHECK_TESTS=none scripts/check.sh changed` 會把開關漏進來，讓下面每一段假 changed 都不跑測試而誤判紅（issue #813）。
unset CHECK_TESTS CHECK_MAX_LOAD

# CHECK_TESTS belongs to the outer daemon check. This contract test sets its own overrides below;
# inheriting a caller's `CHECK_TESTS=none` would suppress the queue-module assertion.
unset CHECK_TESTS

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-changed-daemon.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT

fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/web" "$fixture/bin"
cp "$ROOT/scripts/check.sh" "$fixture/scripts/check.sh"
cp "$ROOT/scripts/ci-changed-parts.sh" "$fixture/scripts/ci-changed-parts.sh"
cp "$ROOT/scripts/ci-daemon-filters.sh" "$fixture/scripts/ci-daemon-filters.sh"
# SPEC 契約測試的替身：只記一筆，證明 changed 有叫它。
printf '#!/usr/bin/env bash\necho jev-ran >>"${AM_TEST_JEV_LOG:?}"\n' >"$fixture/scripts/jev-role_test.sh"
chmod +x "$fixture/scripts/jev-role_test.sh"
export AM_TEST_JEV_LOG="$tmp/jev.log"

cat >"$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
# 改動清單由 $AM_TEST_DIFF_HEAD 給（一行一個）；$AM_TEST_GIT_FAIL 設成某個子指令的整串參數，那一條就失敗（exit 128）。
if [ -n "${AM_TEST_GIT_FAIL:-}" ] && [ "$*" = "$AM_TEST_GIT_FAIL" ]; then
    echo "fatal: bad revision" >&2
    exit 128
fi
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf '%s\n' "${AM_TEST_DIFF_HEAD:-daemon/src/main.rs}" ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH

cat >"$fixture/bin/bun" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/bunx" <<'SH'
#!/usr/bin/env bash
printf 'bunx %s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_CARGO_LOG"
if [ "${1:-}" = test ] && [ -n "${AM_TEST_CARGO_FAIL_ON_TEST:-}" ]; then
    echo "test foo::bar ... FAILED" >&2
    exit 101
fi
if [ "${1:-}" = test ]; then
    if [ "${AM_TEST_CARGO_ZERO_TESTS:-0}" = 1 ]; then
        echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
    else
        echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
    fi
fi
if [ -f web/dist/index.html ]; then
    printf 'web-dist-stub-present\n' >>"$AM_TEST_CARGO_LOG"
else
    printf 'web-dist-stub-missing\n' >>"$AM_TEST_CARGO_LOG"
fi
SH

chmod +x "$fixture/bin/git" "$fixture/bin/bun" "$fixture/bin/bunx" "$fixture/bin/cargo"
export AM_TEST_BUN_LOG="$tmp/bun.log"
export AM_TEST_CARGO_LOG="$tmp/cargo.log"

changed() { (cd "$fixture" && PATH="$fixture/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1); }
if ! output="$(changed)"; then
    printf 'changed daemon fast path failed:\n%s\n' "$output" >&2
    exit 1
fi
if [ -s "$AM_TEST_BUN_LOG" ]; then
    printf 'daemon changed path invoked web checks:\n%s\n' "$(cat "$AM_TEST_BUN_LOG")" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
if ! grep -q '^web-dist-stub-present$' "$AM_TEST_CARGO_LOG"; then
    printf 'cargo check did not receive a temporary rust-embed stub:\n%s\n' "$(cat "$AM_TEST_CARGO_LOG" 2>/dev/null || true)" >&2
    exit 1
fi
if [ -e "$fixture/web/dist/index.html" ]; then
    echo 'changed daemon fast path left its temporary web/dist stub behind' >&2
    exit 1
fi

echo 'changed daemon fast path uses and cleans a temporary web/dist stub'

fail() { printf '%s\n' "$1" >&2; exit 1; }
cargo_tests() { grep -E '^test ' "$AM_TEST_CARGO_LOG" || true; }

# 改了某個模組：除了 cargo check，還要跑那個模組自己的測試（以前只有 check，壞掉的行為過閘）。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
output="$(changed)" || fail "改 queue.rs 的 changed 失敗：$output"
grep -qx 'test -p agents-managerd --locked -- lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "改 queue.rs 沒有跑 lifecycle::queue:: 的測試：$(cat "$AM_TEST_CARGO_LOG")"
[ -e "$fixture/web/dist/index.html" ] && fail 'stub 沒清掉'

# 兩個模組：兩個過濾字串都帶上。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=$'daemon/src/lifecycle/queue.rs\ndaemon/src/hookrecv.rs'
changed >/dev/null || fail "兩個模組的 changed 失敗"
grep -qx 'test -p agents-managerd --locked -- hookrecv:: lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "多模組的過濾字串不對：$(cat "$AM_TEST_CARGO_LOG")"

# 明講不要測試：只 check。明講要哪個：照它。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
CHECK_TESTS=none changed >/dev/null || fail "CHECK_TESTS=none 失敗"
[ -z "$(cargo_tests)" ] || fail "CHECK_TESTS=none 還是跑了測試：$(cargo_tests)"
CHECK_TESTS=prompt:: changed >/dev/null || fail "CHECK_TESTS=prompt:: 失敗"
grep -qx 'test -p agents-managerd --locked prompt::' "$AM_TEST_CARGO_LOG" || fail "CHECK_TESTS 沒照給的跑：$(cat "$AM_TEST_CARGO_LOG")"

# A typo or stale CHECK_TESTS override can make cargo test succeed after selecting zero tests.
export AM_TEST_CARGO_ZERO_TESTS=1
if output="$(CHECK_TESTS=definitely_not_a_test changed 2>&1)"; then
    fail "CHECK_TESTS 選到零個測試卻放行：$output"
fi
printf '%s' "$output" | grep -q '沒有選到任何測試' || fail "零測試失敗沒有說明原因：$output"
unset AM_TEST_CARGO_ZERO_TESTS

# Build scripts, manifests, common test helpers, and embedded fixture/data edits need the whole suite.
for path in daemon/build.rs Cargo.toml Cargo.lock .cargo/config.toml rust-toolchain.toml daemon/Cargo.toml \
    daemon/src/testing.rs daemon/src/lib.rs daemon/tests/fixtures/capture/claude/input.txt \
    crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt crates/am-base/src/release_triage/rules.toml; do
    : >"$AM_TEST_CARGO_LOG"
    export AM_TEST_DIFF_HEAD="$path"
    output="$(changed 2>&1)" || fail "$path 的 changed 失敗：$output"
    grep -qx 'test -p agents-managerd --locked' "$AM_TEST_CARGO_LOG" \
        || fail "$path 沒有跑完整 daemon 測試：$(cat "$AM_TEST_CARGO_LOG")"
done
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=$'daemon/build.rs\ndaemon/src/lifecycle/queue.rs'
output="$(changed 2>&1)" || fail "build.rs 與模組一起改時 changed 失敗：$output"
grep -qx 'test -p agents-managerd --locked' "$AM_TEST_CARGO_LOG" \
    || fail "global sentinel 和模組 filter 混合時沒有跑完整 suite：$(cat "$AM_TEST_CARGO_LOG")"

# 測試紅要讓 changed 紅（結束碼不能被 pipe 吞掉）。
export AM_TEST_CARGO_FAIL_ON_TEST=1
if changed >/dev/null; then fail "cargo test 失敗但 changed 回了 0"; fi
unset AM_TEST_CARGO_FAIL_ON_TEST

# base 比不出來（沒 fetch、淺 clone、沒有共同祖先）：不能當成「沒有改動」放行——以前 `{ git diff A; git diff B; } | …` 的結束碼只看最後一個，
# 第一個壞了會被吞掉，工作樹乾淨時印「只有文件類改動，不用跑」然後綠燈。
export AM_TEST_DIFF_HEAD=''
export AM_TEST_GIT_FAIL='diff --name-only origin/main...HEAD'
if out="$(changed)"; then fail "base 比不出來卻放行了：$out"; else rc=$?; fi
[ "$rc" = 2 ] || fail "base git diff 失敗應回 2，實際 ${rc}：${out}"
printf '%s' "$out" | grep -q 'origin/main' || fail "沒有說是 base 的問題：$out"
unset AM_TEST_GIT_FAIL
export AM_TEST_GIT_FAIL='diff --name-only HEAD'
if out="$(changed)"; then fail "工作樹 git diff 失敗卻放行了：$out"; else rc=$?; fi
[ "$rc" = 2 ] || fail "工作樹 git diff 失敗應回 2，實際 ${rc}：${out}"
unset AM_TEST_GIT_FAIL
export AM_TEST_GIT_FAIL='ls-files --others --exclude-standard'
if out="$(changed)"; then fail "ls-files 失敗卻放行了：$out"; else rc=$?; fi
[ "$rc" = 2 ] || fail "git ls-files 失敗應回 2，實際 ${rc}：${out}"
unset AM_TEST_GIT_FAIL

# 只改 docs/SPEC.md：跑 SPEC 契約測試（Jev 角色政策那幾句），但不跑 ops 整包、也不碰 cargo。
: >"$AM_TEST_CARGO_LOG"; : >"$AM_TEST_JEV_LOG"
export AM_TEST_DIFF_HEAD=docs/SPEC.md
output="$(changed)" || fail "只改 SPEC 的 changed 失敗：$output"
grep -q jev-ran "$AM_TEST_JEV_LOG" || fail "只改 SPEC 沒有跑 jev-role_test.sh：$output"
[ ! -s "$AM_TEST_CARGO_LOG" ] || fail "只改 SPEC 不該動 cargo"
# 其他文件不跑它。
: >"$AM_TEST_JEV_LOG"
export AM_TEST_DIFF_HEAD=docs/API.md
changed >/dev/null || fail "只改 API.md 的 changed 失敗"
[ ! -s "$AM_TEST_JEV_LOG" ] || fail "只改 API.md 不該跑 SPEC 契約測試"

# 平台敏感的 daemon 模組（macos-local 的三組測試所在）改了：Linux 上跑不了 macos-local，至少要明講請去 Mac 跑，
# 不能安靜地綠燈讓人以為都驗過了。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=crates/am-base/src/cargo_shim.rs
output="$(changed)" || fail "改 crates/am-base 的 cargo_shim.rs 的 changed 失敗：$output"
printf '%s' "$output" | grep -q 'scripts/check.sh macos-local' || fail "改 crates/am-base 的 cargo_shim.rs 沒提醒跑 macos-local：$output"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
output="$(changed)" || fail "改 queue.rs 的 changed 失敗"
printf '%s' "$output" | grep -q 'macos-local' && fail "改 queue.rs 不該提醒 macos-local：$output"

# The hint selector also covers module source below the first nested directory.
mkdir -p "$fixture/daemon/src/supervisor/deep"
printf 'fn macos_local_nested_probe() {}\n' >"$fixture/daemon/src/supervisor/deep/roles.rs"
export AM_TEST_DIFF_HEAD=daemon/src/supervisor/deep/roles.rs
output="$(changed)" || fail "改巢狀平台敏感模組的 changed 失敗：$output"
printf '%s' "$output" | grep -q 'scripts/check.sh macos-local' \
    || fail "巢狀模組有 macos_local_ 測試卻沒提醒：$output"

# crate 拆分後 macos_local_ 測試大多在 crates/ 底下（issue #1172），也要提醒；沒有 macos_local_ 的 crates 檔不提醒。
mkdir -p "$fixture/crates/am-base/src"
printf 'fn macos_local_probe() {}\n' >"$fixture/crates/am-base/src/outbox_remote.daemon-tests-tests.rs"
export AM_TEST_DIFF_HEAD=crates/am-base/src/outbox_remote.daemon-tests-tests.rs
output="$(changed)" || fail "改 crates 底下平台敏感測試檔的 changed 失敗：$output"
printf '%s' "$output" | grep -q 'scripts/check.sh macos-local' \
    || fail "crates/ 底下有 macos_local_ 測試卻沒提醒：$output"
printf 'pub fn plain() {}\n' >"$fixture/crates/am-base/src/plain.rs"
export AM_TEST_DIFF_HEAD=crates/am-base/src/plain.rs
output="$(changed)" || fail "改 crates 一般檔的 changed 失敗：$output"
printf '%s' "$output" | grep -q 'macos-local' && fail "crates 底下沒有 macos_local_ 的檔不該提醒：$output"

# web：改任何一個檔（含共用的 store／lib 與設定檔）都是整套——型別檢查、lint、**全部**測試（不是同目錄的）、build。
# 這支只是釘住現況：以後有人想為了快把 bun test 縮成只跑相關檔，這裡會紅。
for f in web/src/lib/shared.ts web/vite.config.ts web/package.json web/bun.lock; do
    : >"$AM_TEST_BUN_LOG"
    export AM_TEST_DIFF_HEAD="$f"
    output="$(changed)" || fail "$f 的 changed 失敗：$output"
    grep -qx 'test' "$AM_TEST_BUN_LOG" || fail "${f}：bun test 要不帶任何過濾字串跑全部：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -qx 'run build' "$AM_TEST_BUN_LOG" || fail "${f}：沒跑 build：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -q 'tsc -p tsconfig.app.json' "$AM_TEST_BUN_LOG" || fail "${f}：沒跑 tsc：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -q 'install --frozen-lockfile' "$AM_TEST_BUN_LOG" || fail "${f}：沒驗鎖檔：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
done

echo 'changed: 受影響模組的測試、CHECK_TESTS、測試紅與 git 失敗都不會被吞掉'
