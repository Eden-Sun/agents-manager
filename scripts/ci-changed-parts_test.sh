#!/usr/bin/env bash
# scripts/ci-changed-parts.sh 的路徑對應（issue #716）。純函式：stdin 檔案清單 → 要跑的部分。
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
check() {
    local want="$1" got
    shift
    got="$(printf '%s\n' "$@" | bash scripts/ci-changed-parts.sh | tr '\n' ' ' | sed 's/ $//')"
    if [ "$got" != "$want" ]; then
        printf 'FAIL: %s → got [%s] want [%s]\n' "$*" "$got" "$want" >&2
        fail=1
    fi
}

check "" "docs/FRONTEND.md" "README.md"
# docs/API.md 不是「只有文件」：daemon 的 api_doc_parity 與 ws_event_docs_tests 在測試時讀它（#1170）。
check "daemon" "docs/API.md"
check "web" "web/src/store/store.ts"
check "daemon" "daemon/src/api.rs"
check "daemon" "Cargo.lock"
check "ops" "scripts/ops/daemon-swap.sh"
check "daemon ops" "scripts/agm.py"
check "ob" "scripts/ob.py"
check "ops" ".claude/skills/verify/SKILL.md"
check "web daemon ops ob" "web/src/a.ts" "daemon/src/b.rs" "scripts/check.sh"
check "full" "web/src/a.ts" "somewhere/unknown.txt"
check "full" "xtask/src/main.rs"
check "" ""
# 被 daemon include_str! 編進去的 .md／docs（supervisor::persona::BUILD_INPUTS）不是「只有文件」：改名、刪掉＝編不過。
check "daemon" "docs/goals/agm-supervisor-persona.md"
check "daemon" "docs/goals/agm-responder-persona.md"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
# crates 底下的 .md 也可能是測試 fixture：release_triage 的 claude_2.1.276-278.md 由 daemon 的測試（runners/release_triage.rs、bot_scope_matrix.rs）include_str! 編進去，不是「只有文件」。
check "daemon" "crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"
check "" "docs/goals/other-goal.md"
# chatgpt-consult.sh 是 scripts/ 底下的 shell：lint-shell-vars 與 canary 在 ops，不能只跑 ob。
check "ops ob" "scripts/chatgpt-consult.sh"
check "ob" "scripts/chatgpt-consult.mjs"

# 2026-10-02 閘門審查：不是 daemon/ 底下、但 daemon 的測試會在執行期讀的檔案，改了就要跑 daemon 的測試。
#   cargo_shim 的測試讀 scripts/check.sh、herdr_shim 的讀 scripts/ops/lint-shell-vars.sh、claude_review 的讀 scripts/ops/*-release-task.md、
#   supervisor::setup 的讀 scripts/ops/fixtures/patrol-runtime.json。
check "web daemon ops ob" "scripts/check.sh"
check "daemon ops" "scripts/ops/lint-shell-vars.sh"
check "daemon ops" "scripts/ops/codex-release-task.md"
check "daemon ops" "scripts/ops/release-triage-task.md"
check "daemon ops" "scripts/ops/fixtures/patrol-runtime.json"
# scripts/ 底下的 .md 不是「只有文件」：任務檔被 install-manifest／ops 測試／daemon 讀（browser-gc-task.md 等），README 也由 ops 測試釘住內容。
check "ops" "scripts/ops/README.md"
check "ops" "scripts/ops/browser-gc-task.md"
# 反方向：web 的測試讀 am-lifecycle 的 fixtures；ops 的 project-transfer 測試從 am-base schema 與 supervisor store 抽欄位。
check "web daemon" "crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
for f in crates/am-base/src/db.rs crates/am-supervisor/src/supervisor/store.rs crates/am-supervisor/src/supervisor/roles.rs crates/am-supervisor/src/mission/store.rs; do
    check "daemon ops" "$f"
done
check "daemon" "daemon/src/api.rs"

# #2072：am-base 讀的 web／ops／scripts 檔。baseline.toml 由 include_str! 編進 binary；outboxList.ts 是前後端預覽白名單比對；
# codex-async-* 是 codex_async_question 的共用 fixture；project-cargo.sh、host-baseline-sync-mac-verify.sh 由 cargo_shim／host_baseline 測試讀。
check "daemon ops" "ops/host-baseline/baseline.toml"
check "daemon ops" "scripts/project-cargo.sh"
check "daemon ops" "scripts/ops/host-baseline-sync-mac-verify.sh"
check "web daemon" "web/src/lib/outboxList.ts"
check "web daemon" "web/src/lib/__fixtures__/codex-async-screen-cases.json"
check "web daemon" "web/src/lib/__fixtures__/codex-async-ordinary-menu.txt"
# 同目錄的其他 fixture 仍只歸 web。
check "web" "web/src/lib/__fixtures__/other.json"

# 防再漏（#2072）：Rust 原始碼裡 include_str!("…")／env!("CARGO_MANIFEST_DIR")).join("…") 指到 web／ops／scripts／docs 的檔，
# 改了都要跑 daemon。include_str! 相對於原始檔；CARGO_MANIFEST_DIR 是編它的 crate，#[path] 掛進 daemon 的檔要連 daemon/ 一起試。
# 指到目錄（再 .join 檔名）的跳過：那種要逐條明列在上面。
# 解析路徑用 python3（macOS 的 realpath 沒有 -m／--relative-to）。
scanned=0
while IFS=$'\t' read -r file p; do
    scanned=$((scanned + 1))
    got="$(printf '%s\n' "$p" | bash scripts/ci-changed-parts.sh)"
    if ! grep -qx -e daemon -e full <<<"$got"; then
        printf 'FAIL: %s 讀 %s，但只分到 [%s]（要含 daemon；補進 ci-changed-parts.sh 的跨界清單）\n' "$file" "$p" "$(echo $got)" >&2
        fail=1
    fi
done < <(python3 -B - <<'PY'
import os, re
pat = re.compile(r'include_str!\("([^"]+)"\)|CARGO_MANIFEST_DIR"\)\)\.join\("([^"]+)"\)')
for top in ("daemon", "crates"):
    for d, _, names in os.walk(top):
        for n in names:
            if not n.endswith(".rs"):
                continue
            f = os.path.join(d, n)
            crate = d
            while not os.path.isfile(os.path.join(crate, "Cargo.toml")):
                crate = os.path.dirname(crate)
            for inc, joined in pat.findall(open(f, encoding="utf-8").read()):
                for base in ([d] if inc else [crate, "daemon"]):
                    p = os.path.relpath(os.path.normpath(os.path.join(base, inc or joined)))
                    if os.path.isfile(p) and p.split(os.sep)[0] in ("web", "ops", "scripts", "docs"):
                        print(f"{f}\t{p}")
PY
)
if [ "$scanned" -lt 5 ]; then
    echo "FAIL: 只掃到 ${scanned} 個跨界讀取，掃描規則大概壞了" >&2
    fail=1
fi

# SPEC 的敘述不需要執行測試；API 等執行期契約文件仍按原分類。
check "" "docs/SPEC.md"
check "daemon" "docs/SPEC.md" "docs/API.md"
check "daemon" "docs/SPEC.md" "daemon/src/api.rs"
check "ops" "docs/SPEC.md" "scripts/ops/README.md"
# 有測試釘住內容的根目錄文件：AGENTS.md（scripts/lint_scope_test.sh 要求 clippy 指令涵蓋 workspace，#1077）。
# 只改它也要跑 ops，不能當成「只有文件」（#1252）。
check "ops" "AGENTS.md"
check "ops" "AGENTS.md" "docs/SPEC.md"
# 其他根目錄文件仍是「只有文件」。
check "" "README.md"

# web 的設定檔改了＝web 整套（build 與全部測試）：vite／tsconfig／package.json／鎖檔／bunfig／測試墊片／靜態資源。
for f in web/vite.config.ts web/tsconfig.app.json web/tsconfig.json web/tsconfig.node.json web/package.json web/bun.lock web/bunfig.toml web/test/node-test-shim.ts web/index.html web/public/x.svg; do
    check "web" "$f"
done

if [ "$fail" = 0 ]; then echo "ci-changed-parts: OK"; fi
exit "$fail"
