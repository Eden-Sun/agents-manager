#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B - <<'PY'
import runpy
from pathlib import Path
module = runpy.run_path('scripts/ci-crate-only-tests.py')
check = module['crate_only']
gate = '#[cfg(test)]\n'
assert check(gate + 'mod tests { #[test] fn pure() { let s = "}"; } }')
assert check(gate + 'mod tests;')
assert not check('#[test] fn app() {}')
assert not check(gate + 'mod pure {}\n#[test] fn app() {}')
assert not check(gate + 'mod pure {}\nmod unknown;')
assert not check('let fixture = r#"' + gate + 'mod tests;"#;')
assert not check('/* nested /* comment */ ' + gate + 'mod tests; */')
assert not check(gate + 'mod tests {')
assert check(Path('crates/am-lifecycle/src/ask_answers.rs').read_text())
assert not check(Path('crates/am-supervisor/src/supervisor/store.rs').read_text())
assert module['owned_path'](Path('crates/am-share/src/share/remote_fs_tests.rs'))
# #1862 遷移 B：budget_remote_tests 整檔 crate-only（mod.rs 的 gate 掛載）；portal_upload_tests 由 portal.rs 以 #[path] 掛上（同目錄兄弟檔）。
assert module['owned_path'](Path('crates/am-share/src/share/budget_remote_tests.rs'))
assert module['owned_path'](Path('crates/am-share/src/share/portal_upload_tests.rs'))
# #1862 C2：portal_remote_tests 整檔 crate-only（唯一要 daemon 的那條搬到 daemon/src/share_tests/remote_svg.rs），remote_cage_tests 搬到 daemon。
assert module['owned_path'](Path('crates/am-share/src/share/portal_remote_tests.rs'))
# store.rs 以 #[path] 掛 store_tests/：不含 App 的那批（非 harness）屬 crate 自己的測試，harness 那批不是。
assert module['owned_path'](Path('crates/am-supervisor/src/supervisor/store_tests/tests_crate_only.rs'))
assert module['owned_path'](Path('crates/am-supervisor/src/supervisor/store_tests/persona_migration_tests.rs'))
# #1862 S1：store_tests/tests.rs（daemon 整合測試）已搬到 daemon/src/supervisor_tests/store.rs，不再存在。
print('crate-only ownership: pure/mixed/unknown/fixture/malformed cases passed')
PY
