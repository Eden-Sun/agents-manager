#!/usr/bin/env bash
# Clean-checkout daemon fast path: rust-embed needs an index.html, but should not trigger web CI.
set -euo pipefail
# 外層 `CHECK_TESTS=none scripts/check.sh changed` 會把開關漏進來，讓下面每一段假 changed 都不跑測試而誤判紅（issue #813）。
unset CHECK_TESTS CHECK_MAX_LOAD

# CHECK_TESTS belongs to the outer daemon check. This contract test sets its own overrides below;
# inheriting a caller's `CHECK_TESTS=none` would suppress the queue-module assertion.
unset CHECK_TESTS

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Every workspace member must have a test route or a documented exclusion in check_daemon:
# daemon + xtask are explicit, crates/* are looped, and desktop needs the package sidecar.
workspace_members="$(awk '
    /^[[:space:]]*members[[:space:]]*=/ {
        in_members = 1
        sub(/^[[:space:]]*members[[:space:]]*=[[:space:]]*/, "")
    }
    in_members {
        block = block " " $0
        if (index($0, "]")) exit
    }
    END { print block }
' "$ROOT/Cargo.toml" | grep -Eo '"[^"]+"' | tr -d '"' | LC_ALL=C sort)"
expected_members="$({
    printf '%s\n' daemon desktop xtask
    for manifest in "$ROOT"/crates/*/Cargo.toml; do
        [ -f "$manifest" ] || continue
        printf '%s\n' "${manifest#"$ROOT"/}" | sed 's#/Cargo.toml$##'
    done
} | LC_ALL=C sort)"
if [ "$workspace_members" != "$expected_members" ]; then
    printf 'workspace member coverage needs an explicit update in check_daemon:\nactual:\n%s\nexpected policy:\n%s\n' \
        "$workspace_members" "$expected_members" >&2
    exit 1
fi
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-changed-daemon.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT

fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/web" "$fixture/bin"
for crate in am-base am-config am-core am-lifecycle am-ports am-share am-supervisor; do
    mkdir -p "$fixture/crates/$crate"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$crate" >"$fixture/crates/$crate/Cargo.toml"
done
cp "$ROOT/scripts/check.sh" "$fixture/scripts/check.sh"
cp "$ROOT/scripts/ci-changed-parts.sh" "$fixture/scripts/ci-changed-parts.sh"
cp "$ROOT/scripts/ci-daemon-filters.sh" "$fixture/scripts/ci-daemon-filters.sh"
cp "$ROOT/scripts/web-test-shards.sh" "$fixture/scripts/web-test-shards.sh"
# Rust test mount check stand-in: record ordering alongside cargo and support a deterministic failure.
cat >"$fixture/scripts/rust_test_mount_test.sh" <<'SH'
#!/usr/bin/env bash
echo mount-ran >>"${AM_TEST_MOUNT_LOG:?}"
echo mount >>"${AM_TEST_CARGO_LOG:?}"
[ "${AM_TEST_MOUNT_FAIL:-0}" != 1 ]
SH
chmod +x "$fixture/scripts/rust_test_mount_test.sh"
export AM_TEST_MOUNT_LOG="$tmp/mount.log"

cat >"$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
# 改動清單由 $AM_TEST_DIFF_HEAD 給（一行一個）；$AM_TEST_GIT_FAIL 設成某個子指令的整串參數，那一條就失敗（exit 128）。
if [ -n "${AM_TEST_GIT_FAIL:-}" ] && [ "$*" = "$AM_TEST_GIT_FAIL" ]; then
    echo "fatal: bad revision" >&2
    exit 128
fi
# changed 的 git diff 要關掉改名偵測（issue #1173）：沒帶 diff.renames=false 就拒絕，讓測試能抓到。
if [ -n "${AM_TEST_REQUIRE_NO_RENAMES:-}" ] && [ "${1:-}" = diff ]; then
    if [ "${GIT_CONFIG_COUNT:-}" != 1 ] || [ "${GIT_CONFIG_KEY_0:-}" != diff.renames ] || [ "${GIT_CONFIG_VALUE_0:-}" != false ]; then
        echo "git diff 沒有關掉改名偵測" >&2
        exit 91
    fi
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
fail_test=0
if [ "${1:-}" = test ]; then
    [ -z "${AM_TEST_CARGO_FAIL_ON_TEST:-}" ] || fail_test=1
    if [ "${2:-}" = -p ] && [ -n "${AM_TEST_CARGO_FAIL_PACKAGE:-}" ] && [ "${3:-}" = "$AM_TEST_CARGO_FAIL_PACKAGE" ]; then
        fail_test=1
    fi
fi
if [ "$fail_test" = 1 ]; then
    echo "test foo::bar ... FAILED" >&2
    exit 101
fi
if [ "${1:-}" = test ] && [ -n "${AM_TEST_CARGO_FAIL_PACKAGE:-}" ] && [ "${3:-}" = "$AM_TEST_CARGO_FAIL_PACKAGE" ]; then
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

fail() { printf '%s\n' "$1" >&2; exit 1; }
echo 'changed daemon fast path uses and cleans a temporary web/dist stub'

# Daemon/crate changes must run the pure-text Rust test mount check before cargo.
: >"$AM_TEST_CARGO_LOG"
: >"$AM_TEST_MOUNT_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/queue.rs
changed >/dev/null || fail "daemon changed path with mount check failed: $(cat "$AM_TEST_CARGO_LOG")"
[ "$(cat "$AM_TEST_MOUNT_LOG")" = mount-ran ] || fail "daemon change did not run the mount check"
[ "$(head -1 "$AM_TEST_CARGO_LOG")" = mount ] || fail "mount check did not run before cargo: $(cat "$AM_TEST_CARGO_LOG")"

# A mount-check failure blocks compilation and fails the changed run.
: >"$AM_TEST_CARGO_LOG"
AM_TEST_MOUNT_FAIL=1 changed >/dev/null 2>&1 && fail 'mount check failure was ignored'
! grep -Eq '^(check|test)( |$)' "$AM_TEST_CARGO_LOG" || fail "cargo ran after mount check failed: $(cat "$AM_TEST_CARGO_LOG")"
unset AM_TEST_MOUNT_FAIL

# none, web-only changes, and fixture repos without the checker do not invoke it.
: >"$AM_TEST_CARGO_LOG"
: >"$AM_TEST_MOUNT_LOG"
CHECK_TESTS=none changed >/dev/null || fail 'CHECK_TESTS=none changed failed'
[ ! -s "$AM_TEST_MOUNT_LOG" ] || fail 'CHECK_TESTS=none ran the mount checker'
: >"$AM_TEST_MOUNT_LOG"
export AM_TEST_DIFF_HEAD=web/src/App.tsx
changed >/dev/null || fail 'web-only changed failed'
[ ! -s "$AM_TEST_MOUNT_LOG" ] || fail 'web-only changed ran the mount checker'
: >"$AM_TEST_MOUNT_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/queue.rs
rm "$fixture/scripts/rust_test_mount_test.sh"
changed >/dev/null || fail 'daemon changed failed when the optional mount checker was absent'
[ ! -s "$AM_TEST_MOUNT_LOG" ] || fail 'absent mount checker was unexpectedly invoked'

fail() { printf '%s\n' "$1" >&2; exit 1; }
cargo_tests() { grep -E '^test ' "$AM_TEST_CARGO_LOG" || true; }

# The shared daemon/full gate must execute xtask and fail if its tests fail.
mkdir -p "$fixture/web/dist" "$fixture/xtask" "$fixture/desktop" "$fixture/crates/am-core" "$fixture/crates/am-share"
: >"$fixture/web/dist/index.html"
: >"$AM_TEST_CARGO_LOG"
(cd "$fixture" && PATH="$fixture/bin:$PATH" bash scripts/check.sh daemon) \
    || fail "daemon gate failed with successful fake cargo: $(cat "$AM_TEST_CARGO_LOG")"
for package in agents-managerd xtask am-core am-share; do
    grep -qx "test -p ${package} --locked" "$AM_TEST_CARGO_LOG" \
        || fail "daemon gate omitted workspace test package ${package}: $(cat "$AM_TEST_CARGO_LOG")"
done
: >"$AM_TEST_CARGO_LOG"
if (cd "$fixture" && AM_TEST_CARGO_FAIL_PACKAGE=xtask PATH="$fixture/bin:$PATH" bash scripts/check.sh daemon) >"$tmp/xtask-fail.log" 2>&1; then
    fail "daemon gate ignored a failing xtask test"
fi
grep -qx 'test -p xtask --locked' "$AM_TEST_CARGO_LOG" \
    || fail "xtask failure fixture never ran xtask: $(cat "$AM_TEST_CARGO_LOG")"
! grep -Eq '^test -p (am-core|am-share) --locked$' "$AM_TEST_CARGO_LOG" \
    || fail "daemon gate continued to crates after xtask failed: $(cat "$AM_TEST_CARGO_LOG")"
rm "$fixture/web/dist/index.html"
rmdir "$fixture/web/dist"
echo 'daemon/full workspace gate covers xtask and propagates its failures'

# A daemon module test already compiles the same daemon targets as check --all-targets.
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
changed >/dev/null || fail "module changed failed: $(cat "$AM_TEST_CARGO_LOG")"
! grep -qx 'check -p agents-managerd --all-targets --locked' "$AM_TEST_CARGO_LOG" \
    || fail "module test redundantly ran cargo check: $(cat "$AM_TEST_CARGO_LOG")"
[ "$(grep -c '^test -p agents-managerd --locked -- lifecycle::queue::$' "$AM_TEST_CARGO_LOG")" = 1 ] \
    || fail "module change did not run exactly one daemon test: $(cat "$AM_TEST_CARGO_LOG")"
[ "$(sed -n '1,2p' "$AM_TEST_CARGO_LOG")" = $'test -p agents-managerd --locked -- lifecycle::queue::\nweb-dist-stub-present' ] \
    || fail "the first cargo test did not see the temporary web/dist stub: $(cat "$AM_TEST_CARGO_LOG")"

# Explicit filters, all-suite sentinels, and cases with no daemon tests choose the matching path.
: >"$AM_TEST_CARGO_LOG"
CHECK_TESTS=none changed >/dev/null || fail 'CHECK_TESTS=none changed failed'
grep -qx 'check -p agents-managerd --all-targets --locked' "$AM_TEST_CARGO_LOG" \
    || fail "CHECK_TESTS=none skipped check: $(cat "$AM_TEST_CARGO_LOG")"
[ -z "$(cargo_tests)" ] || fail "CHECK_TESTS=none ran a test: $(cargo_tests)"

: >"$AM_TEST_CARGO_LOG"
CHECK_TESTS=foo:: changed >/dev/null || fail 'CHECK_TESTS=foo:: changed failed'
! grep -qx 'check -p agents-managerd --all-targets --locked' "$AM_TEST_CARGO_LOG" \
    || fail "explicit test filter redundantly ran cargo check: $(cat "$AM_TEST_CARGO_LOG")"
grep -qx 'test -p agents-managerd --locked foo::' "$AM_TEST_CARGO_LOG" \
    || fail "explicit test filter was not used: $(cat "$AM_TEST_CARGO_LOG")"

: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/main.rs
changed >/dev/null || fail 'main.rs changed failed'
! grep -qx 'check -p agents-managerd --all-targets --locked' "$AM_TEST_CARGO_LOG" \
    || fail "main.rs all-suite tests redundantly ran cargo check: $(cat "$AM_TEST_CARGO_LOG")"
[ "$(grep -c '^test -p agents-managerd --locked$' "$AM_TEST_CARGO_LOG")" = 1 ] \
    || fail "main.rs all-suite test did not run once: $(cat "$AM_TEST_CARGO_LOG")"

: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/build.rs
changed >/dev/null || fail 'build.rs changed failed'
! grep -qx 'check -p agents-managerd --all-targets --locked' "$AM_TEST_CARGO_LOG" \
    || fail "all-suite daemon test redundantly ran cargo check: $(cat "$AM_TEST_CARGO_LOG")"
[ "$(grep -c '^test -p agents-managerd --locked$' "$AM_TEST_CARGO_LOG")" = 1 ] \
    || fail "all-suite change did not run exactly one unfiltered daemon test: $(cat "$AM_TEST_CARGO_LOG")"

# 改了某個模組：除了 cargo check，還要跑那個模組自己的測試（以前只有 check，壞掉的行為過閘）。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
output="$(changed)" || fail "改 queue.rs 的 changed 失敗：$output"
grep -qx 'test -p agents-managerd --locked -- lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "改 queue.rs 沒有跑 lifecycle::queue:: 的測試：$(cat "$AM_TEST_CARGO_LOG")"
[ -e "$fixture/web/dist/index.html" ] && fail 'stub 沒清掉'

# 兩個模組：過濾字串一起交給同一條 cargo test（libtest 收 [FILTERS...] 取聯集；#1332：#1263 的「每個各跑一次」前提是錯的，
# 每個各跑一次會把整包測試重跑 N 遍）。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=$'daemon/src/lifecycle/queue.rs\ndaemon/src/hookrecv.rs'
changed >/dev/null || fail "兩個模組的 changed 失敗：$(cat "$AM_TEST_CARGO_LOG")"
grep -qx 'test -p agents-managerd --locked -- hookrecv:: lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "兩個模組沒有一起交給同一條 cargo test：$(cat "$AM_TEST_CARGO_LOG")"
[ "$(grep -c '^test -p agents-managerd --locked -- ' "$AM_TEST_CARGO_LOG")" = 1 ] || fail "兩個模組起了不只一次 cargo test：$(cat "$AM_TEST_CARGO_LOG")"
# 兩個都紅：整輪要紅，而且那一條 cargo test 跑完（紅了不能提前收手，也不能只跑其中一個）。
: >"$AM_TEST_CARGO_LOG"
AM_TEST_CARGO_FAIL_ON_TEST=1 changed >/dev/null 2>&1 && fail "兩個模組的測試都紅，changed 卻過了"
[ "$(grep -c '^test -p agents-managerd --locked -- ' "$AM_TEST_CARGO_LOG")" = 1 ] || fail "測試紅時 cargo test 沒有跑完（應只起一次）：$(cat "$AM_TEST_CARGO_LOG")"

# 明講不要測試：只 check。明講要哪個：照它。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
CHECK_TESTS=none changed >/dev/null || fail "CHECK_TESTS=none 失敗"
[ -z "$(cargo_tests)" ] || fail "CHECK_TESTS=none 還是跑了測試：$(cargo_tests)"
# crates/ 的改動也一樣：none 就一條 cargo test 都不跑（crate 自己的測試也不跑；#1251）。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=crates/am-core/src/lib.rs
CHECK_TESTS=none changed >/dev/null || fail "crates 改動的 CHECK_TESTS=none 失敗"
[ -z "$(cargo_tests)" ] || fail "CHECK_TESTS=none 還是跑了 crate 測試：$(cargo_tests)"
# 沒設 CHECK_TESTS 時 crate 測試照跑（不能把預設行為一起關掉）。
: >"$AM_TEST_CARGO_LOG"
changed >/dev/null || fail "crates 改動的 changed 失敗"
grep -qx 'test -p am-core --locked' "$AM_TEST_CARGO_LOG" || fail "沒設 CHECK_TESTS 時 crate 測試沒跑：$(cat "$AM_TEST_CARGO_LOG")"
[ "$(cargo_tests | wc -l | tr -d ' ')" = 1 ] || fail "單一 crate 改動不該跑其他 crate 測試：$(cargo_tests)"
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

# Workspace-wide manifests／lockfile／toolchain 設定讓所有獨立 crate 測試失效；CHECK_TESTS=none 仍明確略過。
workspace_crates='am-base am-config am-core am-lifecycle am-ports am-share am-supervisor'
for path in Cargo.toml Cargo.lock rust-toolchain.toml .cargo/config.toml; do
    : >"$AM_TEST_CARGO_LOG"
    export AM_TEST_DIFF_HEAD="$path"
    output="$(changed 2>&1)" || fail "$path 的 changed 失敗：$output"
    for crate in $workspace_crates; do
        grep -qx "test -p $crate --locked" "$AM_TEST_CARGO_LOG" \
            || fail "$path 沒有跑獨立 crate ${crate}：$(cat "$AM_TEST_CARGO_LOG")"
    done
    [ "$(cargo_tests | wc -l | tr -d ' ')" = 8 ] \
        || fail "$path 沒有恰好跑 daemon＋七個 crate 測試：$(cargo_tests)"
done

: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=Cargo.lock
CHECK_TESTS=none changed >/dev/null || fail 'workspace Cargo.lock 搭配 CHECK_TESTS=none 失敗'
[ -z "$(cargo_tests)" ] || fail "workspace Cargo.lock 的 CHECK_TESTS=none 還是跑了測試：$(cargo_tests)"

: >"$AM_TEST_CARGO_LOG"
if output="$(AM_TEST_CARGO_FAIL_PACKAGE=am-core changed 2>&1)"; then
    fail "workspace am-core 測試失敗但 changed 放行：$output"
fi
grep -qx 'test -p am-core --locked' "$AM_TEST_CARGO_LOG" \
    || fail "workspace 測試失敗前沒有執行 am-core：$(cat "$AM_TEST_CARGO_LOG")"

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

export AM_TEST_DIFF_HEAD=docs/API.md
changed >/dev/null || fail "只改 API.md 的 changed 失敗"

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

# changed 的 git diff 要關掉改名偵測（issue #1173）：改名只列新路徑的話，把會進 binary 的檔搬到 docs/ 會被當成「只有文件」。
export AM_TEST_REQUIRE_NO_RENAMES=1 AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
output="$(changed)" || fail "check.sh changed 的 git diff 沒帶 diff.renames=false：$output"
unset AM_TEST_REQUIRE_NO_RENAMES

# web：改任何一個檔（含共用的 store／lib 與設定檔）都是整套——型別檢查、lint、**全部**測試（不是同目錄的）、build。
# 這支只是釘住現況：以後有人想為了快把 bun test 縮成只跑相關檔，這裡會紅。
for f in web/src/lib/shared.ts web/vite.config.ts web/package.json web/bun.lock; do
    : >"$AM_TEST_BUN_LOG"
    export AM_TEST_DIFF_HEAD="$f"
    output="$(changed)" || fail "$f 的 changed 失敗：$output"
    # 允許 web-test-shards.sh 的 `--shard=i/N`（只是切分，不是過濾）；其他任何參數都算縮小範圍。
    grep -qE '^test( --shard=[0-9]+/[0-9]+)?$' "$AM_TEST_BUN_LOG" || fail "${f}：bun test 要不帶任何過濾字串跑全部：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    ! grep -E '^test ' "$AM_TEST_BUN_LOG" | grep -qvE '^test --shard=[0-9]+/[0-9]+$' || fail "${f}：bun test 帶了 shard 以外的參數：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -qx 'bunx vite build' "$AM_TEST_BUN_LOG" || fail "${f}：沒跑 build：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -q 'tsc -b' "$AM_TEST_BUN_LOG" || fail "${f}：沒跑 tsc：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
    grep -q 'install --frozen-lockfile' "$AM_TEST_BUN_LOG" || fail "${f}：沒驗鎖檔：$(tr '\n' '|' <"$AM_TEST_BUN_LOG")"
done

echo 'changed: 受影響模組的測試、CHECK_TESTS、測試紅與 git 失敗都不會被吞掉'
