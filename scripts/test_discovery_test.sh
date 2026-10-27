#!/usr/bin/env bash
# 每一支測試檔都要有人會跑它：check.sh ops 只撈 scripts/*_test.sh 與 scripts/ops/*_test.sh，另外直接點名 agm_test.py／ob_test.py／ob_browser_test.mjs；
# 其他測試檔（例如 .py）必須被某支會跑的腳本點名。放錯位置或命名不合（`foo-test.sh` 少了底線、放在子目錄）的測試檔
# 會變成「看起來有測試、其實從來沒跑過」——這支把它擋下來。
#
# 例外只有兩支需要真的在跑的 daemon 才能執行的手動端到端腳本，在下面明列。
set -euo pipefail
cd "$(dirname "$0")/.."

# 需要運作中的 daemon／sshd 的手動端到端腳本，不進 check.sh。
MANUAL="scripts/hook-timing-test.sh scripts/remote-loop-test.sh"

fail=0
runs_directly() {
    case "$1" in
        scripts/agm_test.py | scripts/ob_test.py | scripts/ob_browser_test.mjs) return 0 ;;
        scripts/*_test.sh) [ "$(dirname "$1")" = scripts ] && return 0 ;;
    esac
    case "$1" in scripts/ops/*_test.sh) [ "$(dirname "$1")" = scripts/ops ] && return 0 ;; esac
    return 1
}

# 非直接入口的測試只能由明列的 wrapper 執行；新增 wrapper 時要在這裡登記。
wrapper_for() {
    case "$1" in
        scripts/ops/browser_gc_linux_test.py) echo scripts/ops/browser-gc-linux_test.sh ;;
        scripts/ops/claude_binary_diff_test.py) echo scripts/ops/claude_binary_diff_test.sh ;;
        scripts/ops/cutover-helper_test.py) echo scripts/ops/cutover-helper_test.sh ;;
        scripts/ops/daemon-swap-network_test.py) echo scripts/ops/daemon-swap-network_test.sh ;;
        scripts/ops/host-state-transfer_test.py) echo scripts/ops/cutover-to-host_test.sh ;;
        scripts/ops/project-transfer_test.py) echo scripts/ops/project-transfer_test.sh ;;
        scripts/ops/transcript-transfer_test.py) echo scripts/ops/transcript-transfer_test.sh ;;
        scripts/claude-hook-debug_test.py) echo scripts/claude-hook-debug_test.sh ;;
        scripts/wait-log_test.py) echo scripts/wait-log_test.sh ;;
        *) return 1 ;;
    esac
}

wrapper_calls_test() {
    PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 - "$1" "$(basename "$2")" <<'PY'
import os
import shlex
import sys

runner, target = sys.argv[1:]
interpreters = {"python", "python2", "python3"}
for line in open(runner, encoding="utf-8"):
    try:
        words = shlex.split(line, comments=True)
    except ValueError:
        continue
    while words and words[0] == "exec":
        words.pop(0)
    while words and "=" in words[0] and not words[0].startswith(("-", "$")):
        words.pop(0)
    if not words or os.path.basename(words[0]) not in interpreters:
        continue
    arguments = words[1:]
    while arguments and arguments[0] in {"-B", "-E", "-I", "-s", "-S", "-u", "-v"}:
        arguments.pop(0)
    if arguments and os.path.basename(arguments[0]) == target:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

while IFS= read -r t; do
    case " $MANUAL " in *" $t "*) continue ;; esac
    runs_directly "$t" && continue
    runner="$(wrapper_for "$t" || true)"
    if [ -n "$runner" ] && runs_directly "$runner" && [ -f "$runner" ] && wrapper_calls_test "$runner" "$t"; then
        continue
    fi
    printf 'FAIL: %s 沒有可達的 check.sh 測試入口（不在直接執行清單，也沒有已登記且實際呼叫它的 wrapper）\n' "$t" >&2
    fail=1
done < <(find scripts -type f \( -name '*_test.*' -o -name '*-test.*' -o -name 'test_*' \) ! -name '*.md' ! -path '*/fixtures/*' ! -path '*/__pycache__/*' | sort)

if [ "${TEST_DISCOVERY_SKIP_SELFTEST:-0}" != 1 ]; then
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/test-discovery.XXXXXX")"
    mkdir -p "${fixture}/scripts/hidden" "${fixture}/scripts/ops"
    cp scripts/test_discovery_test.sh "${fixture}/scripts/test_discovery_test.sh"
    cat > "${fixture}/scripts/hidden/a_test.py" <<'PY'
# b_test.py
raise RuntimeError("must not run")
PY
    cat > "${fixture}/scripts/hidden/b_test.py" <<'PY'
# a_test.py
raise RuntimeError("must not run")
PY
    if output="$(cd "${fixture}" && TEST_DISCOVERY_SKIP_SELFTEST=1 bash scripts/test_discovery_test.sh 2>&1)"; then
        echo "FAIL - discovery 拒絕互相提名的不可達測試" >&2
        fail=1
    elif printf '%s\n' "${output}" | grep -Fq 'scripts/hidden/a_test.py 沒有可達的 check.sh 測試入口' \
        && printf '%s\n' "${output}" | grep -Fq 'scripts/hidden/b_test.py 沒有可達的 check.sh 測試入口'; then
        echo "ok   - discovery 拒絕互相提名的不可達測試"
    else
        echo "FAIL - discovery 沒有點名兩支互相提名的測試" >&2
        fail=1
    fi

    cat > "${fixture}/scripts/ops/project-transfer_test.py" <<'PY'
raise RuntimeError("must not run")
PY
    cat > "${fixture}/scripts/ops/project-transfer_test.sh" <<'SH'
#!/bin/bash
# python3 -B "$HERE/project-transfer_test.py"
SH
    if output="$(cd "${fixture}" && TEST_DISCOVERY_SKIP_SELFTEST=1 bash scripts/test_discovery_test.sh 2>&1)"; then
        echo "FAIL - discovery 把註解當成 wrapper 呼叫" >&2
        fail=1
    elif printf '%s\n' "${output}" | grep -Fq 'scripts/ops/project-transfer_test.py 沒有可達的 check.sh 測試入口'; then
        echo "ok   - discovery 不把註解當成 wrapper 呼叫"
    else
        echo "FAIL - 註解 wrapper 的錯誤訊息不完整" >&2
        fail=1
    fi

    /bin/rm "${fixture}/scripts/hidden/a_test.py" "${fixture}/scripts/hidden/b_test.py"
    cat > "${fixture}/scripts/ops/project-transfer_test.sh" <<'SH'
#!/bin/bash
    exec PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$HERE/project-transfer_test.py"
SH
    if output="$(cd "${fixture}" && TEST_DISCOVERY_SKIP_SELFTEST=1 bash scripts/test_discovery_test.sh 2>&1)"; then
        echo "ok   - discovery 接受實際呼叫的可達 wrapper"
    else
        echo "FAIL - discovery 拒絕實際呼叫的可達 wrapper" >&2
        printf '%s\n' "${output}" >&2
        fail=1
    fi
    /bin/rm -rf "${fixture}"
fi

if [ "$fail" = 0 ]; then echo "test discovery: OK"; fi
exit "$fail"
