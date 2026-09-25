#!/usr/bin/env bash
set -euo pipefail

spec="${1:-docs/SPEC.md}"

require_literal() {
    local literal="$1"
    if ! rg --fixed-strings --quiet -- "$literal" "$spec"; then
        printf 'SPEC 缺少 Jev 角色政策：%s\n' "$literal" >&2
        return 1
    fi
}

require_literal '#### Jev 的角色與界線（2026-09-25，#240）'
require_literal 'Jev 的角色是旁路判斷層，不是 worker；實際工作仍由 claude／codex／grok 執行。'
require_literal 'Jev 不產生自由文字、程式碼或工具呼叫，也沒有可啟動的 pane、session 或 herdr agent kind。'
require_literal 'worker、model、identity、ownership、額度處理與 issue 優先順序仍由現有確定性規則或使用者決定。'
require_literal 'Jev 不負責回合收尾、額度記帳或競態判斷；這些依 run、turn、身分與時鐘等結構資料處理。'
require_literal 'Jev 的答案只能成為 shadow 記錄或人可檢視的提示；不可直接按鍵、核准、停派、改認領或關票'
require_literal '#262 的 `touches_agm` Noul 準確率是 0.88'
require_literal '等 release-triage 有真實 verdict 標籤後再重跑，'
require_literal '#266 與 #267 不接進工作流程。'

printf 'Jev role contract is present in %s\n' "$spec"
