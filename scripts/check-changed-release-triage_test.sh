#!/usr/bin/env bash
# scripts/check.sh changed 必須把 release triage 範例的契約測試結果傳回來（#1185）。
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
echo __am_base_release_triage_submission__
SH
cat > "$tmp/bin/cargo" <<'SH'
#!/usr/bin/env bash
if [ "$1" = check ]; then exit 0; fi
if [ "$1" = test ] && [ "$2" = -p ] && [ "$3" = am-base ]; then
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
    exit $?
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
if ! grep -Fq 'crate: cargo test -p am-base -- the_task_template_teaches_a_submission_the_daemon_accepts' "$output"; then
    echo "FAIL: check.sh changed did not select the am-base contract test" >&2
    cat "$output" >&2
    exit 1
fi
echo "check-changed-release-triage: OK"
