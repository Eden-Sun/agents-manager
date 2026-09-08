# 手機畫面優化第二輪（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。取捨寫進 `docs/UI-DECISIONS.md`，第一輪的決定（`docs/goals/mobile-rwd-2026-09-08.md`、斷點 640 / 1024、`DRAWER_QUERY`）不要翻案。手機 CSS 一律放進 styles.css 既有的 `/* ---- mobile (≤ 640px) ---- */` 區塊。

截圖：`OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`；這輪的現況在 `docs/screenshots/mobile-rwd/round2-before/`，改完放 `round2-after/`。

## 看到的問題（390px）
1. **終端分頁**（`m4`）：`<pre>` 是 185 欄的 pane 內容，手機上每行都被右邊裁掉，只能橫捲。改成 ≤640 預設 `white-space: pre-wrap; word-break: break-all`，term-bar 加一顆「換行／不換行」切換（記在 localStorage）；URL 連結（TermLinks）在 pre-wrap 下要還能點。BlockedPanel 與 HostShellPanel 的 `<pre>` 同樣處理。
2. **標題列第二行**（`m1`、`m4`）：`rt 0 open | +3 −2 ?4 | commit | context 21% · 209k/1M | 花費 $12.07` 一列橫捲時「commit」被切一半、看不出可以捲。改成兩行：第一行 repo 狀態與 git 動作（可橫捲，右側加漸層），第二行 context / 花費（不捲，字級 12px）；或把 context / 花費收進 ⋯ 選單。選一種寫進 UI-DECISIONS。
3. **Team 面板**（`m6`）：
   - 標題列 `Team · #4… [進…] 暫停 中止 關閉 ⋯`：phase chip 被截成「進…」。手機上只留 phase chip 與一顆主要動作（paused → 繼續／加碼；working → 暫停），其餘（中止、關閉、修改角色）收進 ⋯。
   - 三列角色（PM / 執行者 / Reviewer 各一顆「修改」）佔掉 200px。手機上收成一列摘要「PM fable · 執行者 ×2 gpt-5.6 · Rev opus」，點了展開成現在的三列（或直接開 TeamRoleEditor）。
   - 成員節點列 `pm — dev-1 預設 t15 — dev-2 預設 t1…` 被右邊裁掉：改成可橫捲並隱藏捲軸，或在 ≤640 換行成兩列。
   - 統計列 `issue 佇列 10 個 · 已交付 6 · 待處理 3` 與 `Task (16) 併行 2/2 · 進行中 2 · 完成 14`：右邊被裁，改成允許換行（`flex-wrap`），數字用 `tabular-nums`。
   - 插話列的 `@pm @dev-1 @dev-2 @rev` chips 一列排不下時可橫捲。
4. **抽屜標題列**（`m2`）：`AG Man` 加兩列徽章把標題列撐到 100px。手機上 h1 縮成 icon 或藏起來，徽章排一列（pane / RAM / 分頁 各縮短：`20` `5.4G` `🌐8 ◎4`），連線燈保留。
5. **截圖腳本**：`m5-team` / `m6-settings` 會因為當下選的是 team 而拍錯畫面。改成先點一顆 bot 再拍 settings；team 用 `.team-node-btn` 找不到就試 sidebar 的 `[aria-label^="開啟 Team"]`；都沒有就印一行跳過。
6. **順手檢查**（用 mock `VITE_MOCK=1` 開得到的）：BotSettingsPanel sheet、TeamLaunchPanel、TeamRoleEditor、HostsPanel、IdentitiesPanel、ConfirmDialog 在 390 寬有沒有橫向溢出或按鈕被裁；有就修。

## 驗證
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun test src/components && bun run build`
- `OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`：每張印出的 `scrollWidth x innerWidth` 必須相等；六張放 `docs/screenshots/mobile-rwd/round2-after/`。
- `OUT=/tmp/shots node scripts/ui-goal-shots.mjs`：桌面 1440 沒走樣。
- 每個區塊一個 commit（`fix(web): 手機…`）並 push origin main。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon（先 `bun run build` 與 `cargo build --release -p agents-managerd`，不要重啟）、沒做到的與原因。
