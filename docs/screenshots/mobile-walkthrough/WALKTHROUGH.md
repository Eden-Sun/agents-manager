# 用 ego 模擬手機把每一頁操作一遍（2026-09-08）

goal：`docs/goals/mobile-ego-walkthrough-2026-09-08.md`。390×844、`deviceScaleFactor 2`、
touch emulation、iPhone UA、深色。操作一律用 `Input.dispatchTouchEvent` 的真 touch，
不是 mouse hover。

**用的是 headless Chrome 的 CDP，不是 ego 的 task space**（goal 允許的備援）：ego-lite
的 task space tab 沒有 web contents（`Browser.getWindowForTarget` 回
`No web contents in the target`），`Page.captureScreenshot` 每次都逾時；互動可以，截圖不行，
而這個任務每一頁都要留圖。CDP 的模擬參數與腳本行為跟 `scripts/ui-mobile-shots.mjs` 同一套。

大部分驗證跑在 `127.0.0.1:5173`（vite dev，直接吃 `web/src`）而不是 7788——邊改邊看。
`web/` 開工時是乾淨的，dev 上看到的就是 HEAD＋自己的改動。

測試用的 project `mwalk-test` 與 bot `mwalk-bot`（haiku/cc1，cwd 在 scratchpad）都是自己建的，
會改狀態的操作（啟動、送訊息、中斷、強制中止、改設定、刪除）只對它做；現成的 team #47
只看不按會改變它狀態的鍵。

## 逐項結果

| # | 項目 | 結果 |
|---|---|---|
| 1 | 抽屜：☰ 開、遮罩關、專案收合／展開、點 bot 進對話後自動收、搜尋過濾、新增 Project／Bot 對話框開得了關得掉 | 🔧 對話框只有高度全螢幕、寬度縮成 262px（`ab18841`）；🔧 抽屜最底下的「環境設定」被圖片暫存 bar 蓋住點不到（`3a864ec`）；其餘 ✅（抽屜標題 `AG Man` 被 clip 掉是刻意的視覺隱藏，a11y 樹還在） |
| 2 | 對話頁：啟動、打字、`enterKeyHint`、軟鍵盤 Enter 兩條路、附件鍵、中斷回覆／強制中止、回底部、鍵盤彈出時 header、⋯ 選單 | 🔧 軟鍵盤 Enter 只換行不送出（`4c6fc2c`）；🔧 鍵盤彈出時第一下按不到送出（`31cce2f`）；🔧 標題列齒輪被切成 17px（`99f4c41`）；🔧 通知落在輸入框上又被暫存 bar 切掉（`7f7bf04`）；`enterKeyHint=send` ✅、附件鍵 ✅、中斷回覆／強制中止 ✅、回底部 ✅、視窗高度改成 500 時 header 仍在 y=0 ✅。手機的對話頁沒有 `⋯` 選單（那是側欄每列與 team 標題列的），改測那兩處 |
| 3 | 終端分頁：切換、預設折行、換行開關、URL 點一下「已複製」、行數選單、刷新 | ✅ 全過（關掉換行後 `white-space: pre`、`overflow-x: auto`、scrollWidth 640 > clientWidth 364、`scrollLeft` 有效；頁面沒有橫向溢出） |
| 4 | Bot 設定 sheet：開、捲、改名／改 model 儲存、Esc／✕ 關、髒表單確認 | ✅ 全過（全螢幕 sheet、`.bs-body` 一頁放得下；改 model→儲存生效；改名後 ✕ 與 Esc 都跳確認，取消回得去、放棄關得掉） |
| 5 | 群組聊天：點專案標題進群組、@ mention 選單、送一則給測試 bot | ✅ 全過（mention 選單 240×69、每列 230×30 點得到；選完插入 `@mwalk-bot `，送出成功） |
| 6 | Team 面板：phase chip、⋯ 選單、角色摘要展開／收合、成員列橫捲、統計換行、插話輸入框 | 🔧 角色摘要展開之後收不回去（`4bec1c3`）；其餘 ✅（成員列 `overflow-x: auto`，scrollWidth 464 > 366，捲到底 `rev` 完整露出；⋯ 三個項目都在畫面內） |
| 7 | 主機／身分／環境設定 popover 與面板、開 shell、`echo hi`、URL 可點、結束 shell | ✅ 全過（環境設定全螢幕 sheet、`.modal-body` 捲得動 441px；shell 開得起來、`echo hi-from-mwalk` 有輸出、`echo <url>` 的連結點一下顯示「已複製」、結束 shell 有確認框） |
| 8 | 圖片暫存：展開／收合、`+` 選檔對話框 | ✅ 全過（`+` 觸發 `input[type=file]` 的 click，沒有真的傳檔） |
| 9 | 額度 popover | ✅（底部 sheet 390×520、`overflow-y: auto` 捲得動；`停用` 那幾顆沒按——會改到別人帳號的額度設定） |
| 10 | 通用：`scrollWidth === innerWidth`、可點元素最小 40×40、橫向捲區真的捲得動 | ✅ 每一頁 `scrollWidth` 都等於 390；🔧 觸控目標補到 40（`2f93db7`）；🔧 表單欄位補到 16px 免得 iOS 聚焦放大（`9ec7d34`） |

## 修了什麼（一個問題一個 commit）

1. `ab18841` 手機的全螢幕對話框只有高度全螢幕，寬度縮成 262px
2. `4c6fc2c` 手機軟鍵盤的 Enter 還是只換行——React 的 `onBeforeInput` 收不到 `inputType`
3. `99f4c41` 390px 標題列的設定齒輪被切成 17px 的細縫
4. `31cce2f` 手機鍵盤彈出時第一下按不到「送出」——圖片暫存那條 bar 回來把它推走
5. `7f7bf04` 手機的通知落在輸入框上，下半截還被圖片暫存那條 bar 切掉
6. `4bec1c3` 手機上 Team 的角色摘要展開之後收不回去
7. `3a864ec` 抽屜最底下的「環境設定」被圖片暫存那條 bar 蓋住，點不到
8. `2f93db7` 390px 逐頁掃觸控目標，把 11×18、18×14、26×26 的鍵補到 40
9. `9ec7d34` iOS 一點輸入框就把整頁放大——手機的表單欄位補到 16px

## 沒修的

- **`.project-fold`（專案摺疊三角）停在 27×40**，不是 40×40。它夾在列首，右邊 x=37 就是
  `.project-label-btn`（進群組聊天的入口）；再寬就會吃掉那顆的點擊範圍。高度已經補滿 40。
- **`.attach-pick` 的提示泡泡（`附加圖片 · 拖放 / 貼上`）在 390px 會從左邊界漏出去**。
  `.icon-tip[data-tip]::after` 是 `left: 50%` + `translateX(-50%)`，而這顆鍵貼著畫面左緣
  （中心 x=45、泡泡寬 107 → 左緣 −8.6）。只有觸控裝置上 `:hover` 黏住時看得到，是純視覺問題，
  這一輪沒動——修法跟 `.bot-actions .icon-tip` 那條一樣（改成靠一邊展開）。
- **額度明細 sheet 裡的「停用」核取方塊 20×20**。它們會改到別人帳號的額度設定，這一輪
  只做了 `elementFromPoint` 命中測試、沒有按，也就沒有跟著改尺寸。
- **目錄選擇器底下的鍵盤提示（`↩ 進入 · ⌘↩ 直接選擇 · ← 上一層 · ↑↓ 移動`）在手機沒有意義**，
  白佔一行。屬於文案取捨，不在這次「操作測試」的範圍。

## 附帶記錄

- 送出鍵有幾次「點了沒反應」是測試腳本自己的競態（`Input.insertText` 之後 React 還沒把
  `nothingToSend` 更新掉就送 touch），不是產品問題；真正的產品問題是第 4 項那個，已修。
- 走查途中另一個 agent 推了 `d037f0c`（額度 chip 改寫成 `F 剩 26%`），齒輪被切那一項就是它
  連帶造成的，順手一起修掉並更正了 `docs/UI-DECISIONS.md` 的「名字下限 10ch」。
