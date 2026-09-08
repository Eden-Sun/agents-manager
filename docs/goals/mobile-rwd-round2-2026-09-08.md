# 手機畫面優化第二輪（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。取捨寫進 `docs/UI-DECISIONS.md`，第一輪的決定（`docs/goals/mobile-rwd-2026-09-08.md`、斷點 640 / 1024、`DRAWER_QUERY`）不要翻案。手機 CSS 一律放進 styles.css 既有的 `/* ---- mobile (≤ 640px) ---- */` 區塊。

截圖：`OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`；這輪的現況在 `docs/screenshots/mobile-rwd/round2-before/`，改完放 `round2-after/`。

## 看到的問題（390px）
1. ✅ **終端分頁**（`m4`）：`<pre>` 是 185 欄的 pane 內容，手機上每行都被右邊裁掉，只能橫捲。改成 ≤640 預設 `white-space: pre-wrap; word-break: break-all`，term-bar 加一顆「換行／不換行」切換（記在 localStorage）；URL 連結（TermLinks）在 pre-wrap 下要還能點。BlockedPanel 與 HostShellPanel 的 `<pre>` 同樣處理。
2. ✅ **標題列第二行**（`m1`、`m4`）：`rt 0 open | +3 −2 ?4 | commit | context 21% · 209k/1M | 花費 $12.07` 一列橫捲時「commit」被切一半、看不出可以捲。改成兩行：第一行 repo 狀態與 git 動作（可橫捲，右側加漸層），第二行 context / 花費（不捲，字級 12px）；或把 context / 花費收進 ⋯ 選單。選一種寫進 UI-DECISIONS。
3. ✅ **Team 面板**（`m6`）：
   - 標題列 `Team · #4… [進…] 暫停 中止 關閉 ⋯`：phase chip 被截成「進…」。手機上只留 phase chip 與一顆主要動作（paused → 繼續／加碼；working → 暫停），其餘（中止、關閉、修改角色）收進 ⋯。
   - 三列角色（PM / 執行者 / Reviewer 各一顆「修改」）佔掉 200px。手機上收成一列摘要「PM fable · 執行者 ×2 gpt-5.6 · Rev opus」，點了展開成現在的三列（或直接開 TeamRoleEditor）。
   - 成員節點列 `pm — dev-1 預設 t15 — dev-2 預設 t1…` 被右邊裁掉：改成可橫捲並隱藏捲軸，或在 ≤640 換行成兩列。
   - 統計列 `issue 佇列 10 個 · 已交付 6 · 待處理 3` 與 `Task (16) 併行 2/2 · 進行中 2 · 完成 14`：右邊被裁，改成允許換行（`flex-wrap`），數字用 `tabular-nums`。
   - 插話列的 `@pm @dev-1 @dev-2 @rev` chips 一列排不下時可橫捲。
4. ✅ **抽屜標題列**（`m2`）：`AG Man` 加兩列徽章把標題列撐到 100px。手機上 h1 縮成 icon 或藏起來，徽章排一列（pane / RAM / 分頁 各縮短：`20` `5.4G` `🌐8 ◎4`），連線燈保留。
5. ✅ **截圖腳本**：`m5-team` / `m6-settings` 會因為當下選的是 team 而拍錯畫面。改成先點一顆 bot 再拍 settings；team 用 `.team-node-btn` 找不到就試 sidebar 的 `[aria-label^="開啟 Team"]`；都沒有就印一行跳過。
6. ✅ **順手檢查**（用 mock `VITE_MOCK=1` 開得到的）：BotSettingsPanel sheet、TeamLaunchPanel、TeamRoleEditor、HostsPanel、IdentitiesPanel、ConfirmDialog 在 390 寬有沒有橫向溢出或按鈕被裁；有就修。

## 驗證
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun test src/components && bun run build`
- `OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`：每張印出的 `scrollWidth x innerWidth` 必須相等；六張放 `docs/screenshots/mobile-rwd/round2-after/`。
- `OUT=/tmp/shots node scripts/ui-goal-shots.mjs`：桌面 1440 沒走樣。
- 每個區塊一個 commit（`fix(web): 手機…`）並 push origin main。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon（先 `bun run build` 與 `cargo build --release -p agents-managerd`，不要重啟）、沒做到的與原因。

## 收工（2026-09-08，opus）

全部六項做完，每項一個 commit 並 push。取捨寫進 `docs/UI-DECISIONS.md`
（「手機第二輪：終端折行，與標題列第二行拆成兩列」）。

| # | commit | 做了什麼 |
| --- | --- | --- |
| 1 | `4854d74` | `.term-wrap`（`pre-wrap` + `break-all`），預設跟著 `PHONE_QUERY`；term-bar 一顆「換行」開關，記在 `am.term.wrap`；`TerminalTab` / `BlockedPanel` / `HostShellPanel` 共用 `components/termWrap.ts` |
| 2 | `c65c682` | `.context-bar` 拆兩列：第一列 repo + git（照舊橫捲），第二列 statusline（`flex: 1 0 100%`、12px、換行不橫捲、`tabular-nums`）。**選的是拆兩列，不是收進 `⋯`**——理由寫在 UI-DECISIONS |
| 3 | `db39aa9`（標題列，隨別人的 commit 進 main）+ `db9498d` | 標題列只留 phase chip 與一顆主要動作，中止／關閉進 `⋯`；角色三列收成一行摘要（點了展開）；統計列換行 + `tabular-nums`；成員列與 `@chips` 補 `flex: none` 與隱藏捲軸的 scroll shadow |
| 4 | `4b5f61d` | 抽屜標題列 100px → 46px：`h1` 只在視覺上收掉（留在 a11y 樹）、徽章排一列、`pane` / `RAM` 標籤與瀏覽器的位元組數讓位 |
| 5 | `7f7b6c6` | `m6-settings` 先點回一顆 bot 再點齒輪；`m5-team` 找不到 team 就整張跳過並印原因，選擇器多兩條後路 |
| 6 | `a996313` | mock 逐面掃 390 寬，只找到一處：組隊畫面 issue 佇列的「先做這個」被擠成兩行 |

驗證：`bunx tsc --noEmit -p tsconfig.app.json` 乾淨、`bunx oxlint src` 沒有新 warning、
`bun test src/components` 25 pass、`bun run build` 成功。
`OUT=… node scripts/ui-mobile-shots.mjs` 六張都是 `390x390`（`scrollWidth == innerWidth`），
放在 `docs/screenshots/mobile-rwd/round2-after/`；`ui-goal-shots.mjs` 桌面 1440 七張沒走樣。

沒做的：`TeamRoleEditor` 的表單在 390px 是全螢幕 sheet，但內容只佔上半頁、取消／儲存那一列
停在畫面中間（`RoleForm` 用的是 `Modal` 的 `sheet-form`，不是 `.modal-foot`）。沒有溢出也沒有
按鈕被裁，屬於版面美觀而不是這一輪要修的問題，留給下一輪。
