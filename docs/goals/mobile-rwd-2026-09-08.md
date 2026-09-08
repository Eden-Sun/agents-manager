# 手機 RWD 優化（2026-09-08）

執行者：opus。每個區塊一個 commit，做完在此打勾。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。取捨寫進 `docs/UI-DECISIONS.md`，不要翻案已定案的。

截圖工具：`OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`（390×844、DPR 2、深色，走 5173 真 daemon；改完自己再跑一輪放 `docs/screenshots/mobile-rwd/after/`）。改前的六張在 `docs/screenshots/mobile-rwd/before/`。

## 看到的問題（390px）

全部做完（2026-09-08，opus）。逐項見下面的勾與 commit。

1. ✅ **抽屜不會自動收**：`m2/m4/m6`——點 bot、切「終端」、開設定之後側欄抽屜還蓋在上面。手機上選了東西就該收（`selectBot` / `selectProject` / `selectTeam` / `openSettings` / 開 shell 之後 `closeDrawer`）；點抽屜外的遮罩也要能關。
2. ✅ **底部「圖片暫存」長駐**：`m1`——收合狀態還佔兩行（標題列 + 提示字），永久吃掉約 120px。手機上：預設只留一條 36px 的 bar（圖示＋張數＋展開鍵），提示字只在展開時顯示；輸入框 focus（鍵盤彈出）時整條隱藏。
3. ✅ **頂部 git 工具列被裁**：`m1`——「commit / push / pull | co…」超出寬度且不能捲。改成可橫向捲動（`overflow-x:auto`、隱藏捲軸、右側漸層提示）或收進「⋯」選單；`+3 −2 ?4` 那組數字保留。
4. ✅ **頂部標題列**：bot 名被截成「C1-fa…」但右邊 chips（`cc1 F 47%`、對話／終端 tab）還很寬。手機上 tab 改成只有圖示或縮字距，額度 chip 只留百分比，名字至少留 10 個字。
5. ✅ **終端分頁**（`m4`）：`term-bar`（檢視、行數、壓縮空行、刷新）要能換行或橫捲；`<pre>` 維持橫向捲動但字級 11px；鍵盤列（ctrl+c / Esc / Tab…）在 390 要一列排得下或可橫捲。
6. ✅ **Bot 設定面板**：桌面是貼齒輪的浮窗；≤ 640px 改成全螢幕 sheet（`position:fixed; inset:0`），標題列固定、內容捲動、底部固定「儲存」。TeamPanel / TeamRoleEditor / TeamLaunchPanel / HostsPanel / IdentitiesPanel 這些對話框同樣規則。
7. ✅ **通用**：
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

## 做完了（2026-09-08，opus）

| 區塊 | commit | 怎麼驗的 |
| --- | --- | --- |
| 1 抽屜自動收 | `361ec78` | 導覽控制項在 capture 階段自己收抽屜（含「再點一次已經選著的那顆」）；`settingsBotId` 併進選取鍵。m3/m4 不再是抽屜蓋著。 |
| 7 通用 | `5c1fa16` | `100dvh`、`env(safe-area-inset-bottom)`、icon 鍵 40×40、`.tab` 40×40、11px 標籤提到 12px。 |
| 2 圖片暫存 | `9950ece` | 手機預設收成 36px 的一條；composer focus 後 `.shelf` 高度 0，blur 回 37px。 |
| 3 git 工具列 | `d72d113` | `.git-bar` 自己橫捲（`scrollWidth 216 / clientWidth 118`），scroll shadow 只在真的溢出時出現；統計數字留在最前面。 |
| 4 標題列 | `2fe19d9` | 額度 chip 只留圖示＋百分比，名字下限 10ch（63px → 88px）；十顆 bot 逐一量，齒輪溢出量全部是 0（原本 1–7px 被切）。 |
| 5 終端 | `ba62597` | `.term` 11px、內距 16→12（可見寬度 356 → 364）；`.term-bar` 一行（366 = 366）；七顆鍵一行（315 + 30 < 358）。 |
| 6 全螢幕 sheet | `3744a0f` | `.bot-settings` 與 `.modal` 在 ≤640 量到 `[0,0,390,844]`；桌機 1440 仍是 `anchored` 的 520×738 浮窗。 |
| 斷點統一 + 取捨 | `207df6a` | `MOBILE_QUERY` → `DRAWER_QUERY`；`docs/UI-DECISIONS.md` 補一節。 |
| 順手修的 | `d54b2dc` | 兩條全域額度樣式被誤夾進 ≤640 區塊（舊的合併意外），桌機完全套不到；還原成全域。 |

驗證：`bunx tsc --noEmit -p tsconfig.app.json` 乾淨（注意：不帶 `-p` 的那個是假綠燈）、
`bunx oxlint src` 無新增、`bun test src/components` 25 pass、`bun run build` 通過。
六張 after 在 `docs/screenshots/mobile-rwd/after/`，全部 `scrollWidth 390 = innerWidth 390`。
桌面 `OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 七張重跑，1440 沒有變化。

沒做到的：`m5-team` 拍不到 team 面板——這台機器目前沒有進行中的 team，`.team-node-btn`
數量是 0。截圖腳本改成偵測不到就收起抽屜並印一行說明，不再拍出 `m2` 的複本。
另外：`docs/UI-DECISIONS.md` 的取捨已寫，daemon 尚未重啟（見回報）。
