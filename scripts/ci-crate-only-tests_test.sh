#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B - <<'PY'
import runpy
from pathlib import Path
module = runpy.run_path('scripts/ci-crate-only-tests.py')
check = module['crate_only']
gate = '#[cfg(all(test, not(feature = "daemon-test-harness")))]\n'
assert check(gate + 'mod tests { #[test] fn pure() { let s = "}"; } }')
assert check(gate + 'mod tests;')
assert not check('#[cfg(test)] mod tests { #[test] fn app() {} }')
assert not check(gate + 'mod pure {}\n#[test] fn app() {}')
assert not check(gate + 'mod pure {}\nmod unknown;')
assert not check('let fixture = r#"' + gate + 'mod tests;"#;')
assert not check('/* nested /* comment */ ' + gate + 'mod tests; */')
assert not check(gate + 'mod tests {')
assert check(Path('crates/am-lifecycle/src/ask_answers.rs').read_text())
assert not check(Path('crates/am-supervisor/src/supervisor/store.rs').read_text())
assert module['owned_path'](Path('crates/am-share/src/share/remote_fs_tests.rs'))
assert not module['owned_path'](Path('crates/am-share/src/share/remote_cage_tests.rs'))
assert not module['owned_path'](Path('crates/am-share/src/share/budget_remote_tests.rs'))
print('crate-only ownership: pure/mixed/unknown/fixture/malformed cases passed')
PY
