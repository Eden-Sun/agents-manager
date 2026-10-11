#!/usr/bin/env bash
# scripts/check.sh changed 必須在 daemon target 執行 release triage 範例契約，且拒絕零測試（#1185/#1334）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/check-release-triage.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
repo="$tmp/repo"
mkdir -p "$repo/scripts/ops" "$repo/web" "$tmp/bin"
cp "$ROOT/scripts/check.sh" "$repo/scripts/check.sh"
cp "$ROOT/scripts/ops/release-triage-task.md" "$repo/scripts/ops/release-triage-task.md"

# Only the classifier result is stubbed here; its real path mapping is covered separately.
cat > "$repo/scripts/ci-changed-parts.sh" <<'SH'
#!/usr/bin/env bash
echo daemon
SH
cat > "$repo/scripts/ci-daemon-filters.sh" <<'SH'
#!/usr/bin/env bash
echo __daemon_release_triage_submission__
SH
cat > "$tmp/bin/cargo" <<'SH'
#!/usr/bin/env bash
if [ "$1" = check ]; then exit 0; fi
if [ "$1" = test ] && [ "$2" = -p ] && [ "$3" = agents-managerd ] && [[ " $* " == *the_task_template_teaches_a_submission_the_daemon_accepts* ]]; then
    python3 - "$PWD/scripts/ops/release-triage-task.md" <<'PY'
import json
import re
import sys
body = open(sys.argv[1], encoding="utf-8").read()
sample = re.search(r"```json\n(.*?)\n```", body, re.S)
assert sample, "missing JSON sample"
submission = json.loads(sample.group(1))
assert all(submission.get(key) is not None for key in ("kind", "version", "dispatch_gen")), "invalid Submission sample"
PY
    rc=$?
    [ "$rc" = 0 ] || exit "$rc"
fi
if [ "$1" = test ] && [ "$3" = agents-managerd ]; then
    if [ "${AM_TEST_CARGO_ZERO_TESTS:-0}" = 1 ]; then
        echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
    else
        echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
    fi
    exit 0
fi
echo "unexpected cargo command: $*" >&2
exit 2
SH
chmod +x "$tmp/bin/cargo"

git -C "$repo" init -q -b main
git -C "$repo" config user.name test
git -C "$repo" config user.email test@example.invalid
git -C "$repo" add scripts
git -C "$repo" commit -qm base
git -C "$repo" update-ref refs/remotes/origin/main HEAD
python3 - "$repo/scripts/ops/release-triage-task.md" <<'PY'
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
path.write_text("\n".join(line for line in path.read_text().splitlines() if '"dispatch_gen":' not in line) + "\n")
PY
git -C "$repo" add scripts/ops/release-triage-task.md
git -C "$repo" commit -qm invalid-template

output="$tmp/output"
if (cd "$repo" && PATH="$tmp/bin:$PATH" bash scripts/check.sh changed) >"$output" 2>&1; then
    echo "FAIL: check.sh changed passed an invalid release triage sample" >&2
    exit 1
fi
if ! grep -Fq 'daemon: cargo test -p agents-managerd -- runners::am_base_tests::release_triage::tests::the_task_template_teaches_a_submission_the_daemon_accepts' "$output"; then
    echo "FAIL: check.sh changed did not select the daemon contract test" >&2
    cat "$output" >&2
    exit 1
fi

# 指定合法的 daemon 過濾字串（CHECK_TESTS=cargo_shim::）時，發行契約照跑：蓋掉的只有 daemon 那一段（#1258）。
output2="$tmp/output2"
if (cd "$repo" && PATH="$tmp/bin:$PATH" CHECK_TESTS=cargo_shim:: bash scripts/check.sh changed) >"$output2" 2>&1; then
    echo "FAIL: CHECK_TESTS=cargo_shim:: 讓壞掉的發行模板過閘" >&2
    cat "$output2" >&2
    exit 1
fi
if ! grep -Fq 'daemon: cargo test -p agents-managerd -- runners::am_base_tests::release_triage::tests::the_task_template_teaches_a_submission_the_daemon_accepts' "$output2"; then
    echo "FAIL: CHECK_TESTS 指定字串時沒有執行發行契約" >&2
    cat "$output2" >&2
    exit 1
fi

# 模板是好的、同樣指定 daemon 過濾字串：契約通過，而且指定的 daemon 測試照跑。
git -C "$repo" show HEAD~1:scripts/ops/release-triage-task.md >"$repo/scripts/ops/release-triage-task.md"
git -C "$repo" commit -qam valid-template
output3="$tmp/output3"
if ! (cd "$repo" && PATH="$tmp/bin:$PATH" CHECK_TESTS=cargo_shim:: bash scripts/check.sh changed) >"$output3" 2>&1; then
    echo "FAIL: 好的模板＋CHECK_TESTS=cargo_shim:: 應該通過" >&2
    cat "$output3" >&2
    exit 1
fi
if ! grep -Fq 'daemon: cargo test cargo_shim::' "$output3"; then
    echo "FAIL: 指定的 daemon 過濾字串沒有照跑" >&2
    cat "$output3" >&2
    exit 1
fi
# CHECK_TESTS=none 才是明講的「不跑測試」：連發行契約一起略過，而且印出來。
git -C "$repo" show HEAD~1:scripts/ops/release-triage-task.md >"$repo/scripts/ops/release-triage-task.md"
output4="$tmp/output4"
if ! (cd "$repo" && PATH="$tmp/bin:$PATH" CHECK_TESTS=none bash scripts/check.sh changed) >"$output4" 2>&1; then
    echo "FAIL: CHECK_TESTS=none 應該整段略過（含發行契約）" >&2
    cat "$output4" >&2
    exit 1
fi
if ! grep -Fq 'daemon: CHECK_TESTS=none：不跑測試' "$output4"; then
    echo "FAIL: CHECK_TESTS=none 略過測試時沒有明講" >&2
    cat "$output4" >&2
    exit 1
fi

# The command may exit zero while libtest selected nothing; check.sh must reject it.
git -C "$repo" show HEAD:scripts/ops/release-triage-task.md >"$repo/scripts/ops/release-triage-task.md"
printf '\n<!-- zero-test probe -->\n' >>"$repo/scripts/ops/release-triage-task.md"
git -C "$repo" add scripts/ops/release-triage-task.md
git -C "$repo" commit -qam valid-template-for-zero-test-check
output5="$tmp/output5"
if (cd "$repo" && PATH="$tmp/bin:$PATH" AM_TEST_CARGO_ZERO_TESTS=1 bash scripts/check.sh changed) >"$output5" 2>&1; then
    echo "FAIL: check.sh changed passed a zero-test release contract" >&2
    cat "$output5" >&2
    exit 1
fi
if ! grep -Fq '沒有選到任何測試' "$output5"; then
    echo "FAIL: zero-test failure did not explain the rejected filter" >&2
    cat "$output5" >&2
    exit 1
fi
echo "check-changed-release-triage: OK"
