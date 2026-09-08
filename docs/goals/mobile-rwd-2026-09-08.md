# 手機 RWD 優化（2026-09-08）

執行者：opus。每個區塊一個 commit，做完在此打勾。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。取捨寫進 `docs/UI-DECISIONS.md`，不要翻案已定案的。

截圖工具：`OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`（390×844、DPR 2、深色，走 5173 真 daemon；改完自己再跑一輪放 `docs/screenshots/mobile-rwd/after/`）。改前的六張在 `docs/screenshots/mobile-rwd/before/`。

## 看到的問題（390px）
1. **抽屜不會自動收**：`m2/m4/m6`——點 bot、切「終端」、開設定之後側欄抽屜還蓋在上面。手機上選了東西就該收（`selectBot` / `selectProject` / `selectTeam` / `openSettings` / 開 shell 之後 `closeDrawer`）；點抽屜外的遮罩也要能關。
2. **底部「圖片暫存」長駐**：`m1`——收合狀態還佔兩行（標題列 + 提示字），永久吃掉約 120px。手機上：預設只留一條 36px 的 bar（圖示＋張數＋展開鍵），提示字只在展開時顯示；輸入框 focus（鍵盤彈出）時整條隱藏。
3. **頂部 git 工具列被裁**：`m1`——「commit / push / pull | co…」超出寬度且不能捲。改成可橫向捲動（`overflow-x:auto`、隱藏捲軸、右側漸層提示）或收進「⋯」選單；`+3 −2 ?4` 那組數字保留。
4. **頂部標題列**：bot 名被截成「C1-fa…」但右邊 chips（`cc1 F 47%`、對話／終端 tab）還很寬。手機上 tab 改成只有圖示或縮字距，額度 chip 只留百分比，名字至少留 10 個字。
5. **終端分頁**（`m4`）：`term-bar`（檢視、行數、壓縮空行、刷新）要能換行或橫捲；`<pre>` 維持橫向捲動但字級 11px；鍵盤列（ctrl+c / Esc / Tab…）在 390 要一列排得下或可橫捲。
6. **Bot 設定面板**：桌面是貼齒輪的浮窗；≤ 640px 改成全螢幕 sheet（`position:fixed; inset:0`），標題列固定、內容捲動、底部固定「儲存」。TeamPanel / TeamRoleEditor / TeamLaunchPanel / HostsPanel / IdentitiesPanel 這些對話框同樣規則。
7. **通用**：
   - 用 `100dvh`（iOS 鍵盤／網址列）取代 `100vh`；`padding-bottom: env(safe-area-inset-bottom)` 加在輸入區與底部 bar。
   - 點擊目標 ≥ 40×40（側欄 `⋯`、齒輪、附件鍵、tab）。
   - 字級：任何 < 12px 的文字在 ≤ 640px 提到 12px。
   - 不能出現整頁橫向捲動：截圖腳本會印 `scrollWidth x innerWidth`，兩者必須相等。
   - 斷點統一：現有 `PHONE_QUERY`（`hooks/useMediaQuery`）與 CSS 的 `(width <= 1024px)` / `640px` 各自為政，整理成兩個：`≤ 640` 手機、`≤ 1024` 抽屜；寫進 UI-DECISIONS。

## 做法
- 先跑截圖腳本再自己看一遍六張，補上我沒列到的問題。
- CSS 改動集中在 `styles.css` 尾端一個新的 `/* ---- mobile (≤ 640px) ---- */` 區塊，不要散落。
- 行為改動（抽屜自動收、shelf 隱藏）放 `App.tsx` / `Sidebar.tsx` / `ImageShelf.tsx`，hunk 要小。
- 每個區塊一個 commit：`fix(web): 手機…`。

## 驗證
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun test src/components && bun run build`
- `OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`：六張都沒有橫向溢出，放 `docs/screenshots/mobile-rwd/after/` 一起 commit。
- 桌面不能退化：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 跑一次確認 1440 寬沒變。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon（要 `bun run build` 後 `cargo build --release -p agents-managerd`，可以先做好不要重啟）、沒做到的與原因。
