# 用 ego 模擬手機，把每一頁實際操作一遍（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。這是「操作測試 + 修」：找到的問題當場修、一個問題一個 commit；修不了的寫進本檔「未修」段。

## 環境
- 用 `ego-browser` skill（task space 名 `am mobile walkthrough`）。開 `http://127.0.0.1:7788/?token=$(cat ~/.config/agents-manager/ui-token)`（正式 UI，daemon 已含最新前端；要驗自己剛改的東西改開 5173）。
- 用 CDP 模擬手機：`Emulation.setDeviceMetricsOverride {width:390,height:844,deviceScaleFactor:3,mobile:true}`、`Emulation.setTouchEmulationEnabled {enabled:true,maxTouchPoints:1}`、`Emulation.setUserAgentOverride`（iPhone Safari UA）、`Emulation.setEmulatedMedia`（深色）。操作一律用 **touch**（`Input.dispatchTouchEvent` 或 ego 的 `click`，確認它在 touch 模擬下有效）而不是 mouse hover。
- 若 ego 回「user has taken control」，等 30 秒 `takeOverTaskSpace` 一次；仍不行就改用 headless Chrome（`scripts/ui-mobile-shots.mjs` 那套 CDP）做同樣的事，並在回報裡註明。
- 每一頁做完存一張 `captureScreenshot` 到 `docs/screenshots/mobile-walkthrough/<nn>-<page>.png`，並把「做了什麼、結果」寫進同目錄的 `WALKTHROUGH.md`。
- 不要對別人正在用的 bot 送會改變狀態的東西：**送訊息、中斷、重啟、刪除只對你自己開的測試 bot 做**。先用 API 建一個測試 project（cwd 用 scratchpad）與 claude bot（`model: haiku`、identity `cc1`），測完刪掉。

## 要走過的頁與操作（每項都要真的點、真的打字、真的看結果）
1. **抽屜**：☰ 開、點遮罩關、點 project 標題收合／展開、點 bot 進對話後抽屜自動收、搜尋框打字過濾、「新增 Project」「新增 Bot」對話框開得了也關得掉（不要真的建）。
2. **對話頁**（測試 bot）：啟動 bot；輸入框打字、`enterKeyHint` 是 send、模擬軟鍵盤 Enter（`Input.dispatchKeyEvent` key=Enter + `Input.insertText` 的 beforeinput 路徑兩種都試）真的送出；圖片附件鍵開得了；「中斷回覆」「強制中止」按得到；長對話往上捲再回底部的按鈕；header 在鍵盤彈出（`Emulation.setDeviceMetricsOverride` 把 height 改成 500 模擬）時仍在最上面；`⋯` 選單每一項都點得到。
3. **終端分頁**：切換、預設折行、「換行」開關切換後 `<pre>` 真的變橫捲、URL 連結點一下有「已複製」、行數選單、刷新。
4. **Bot 設定**（sheet）：開、每個欄位可捲到、改名／改 model 儲存、Esc／✕ 關、髒表單關閉時的確認框。
5. **群組聊天**：點 project 標題進群組、@ mention 選單在手機上點得到選項、送一則給測試 bot。
6. **Team 面板**（有現成 team 就用它看，不要按會改變它狀態的鍵；沒有就開 TeamLaunchPanel 看到可以填但不要建）：phase chip、`⋯` 選單、角色摘要展開／收合、成員列橫捲、統計換行、插話輸入框。
7. **主機／身分／環境設定** popover 與面板：開、捲、關；「開 shell」開得了一個本機 shell、在 shell 輸入框打 `echo hi` Enter 有輸出、URL 可點、結束 shell。
8. **圖片暫存**（底部 bar）：展開／收合、`+` 選檔對話框（不用真的傳）。
9. **額度 popover**（header 的 27% 那顆）：開、每列點得到、關。
10. **通用**：每頁 `document.documentElement.scrollWidth === innerWidth`；所有可點元素最小 40×40（用 `getBoundingClientRect` 掃 `button, [role=button], a`，列出不合格的並修）；橫向捲的區域用 `scrollLeft` 驗證真的能捲。

## 驗證與收尾
- 修過的：`cd web && bunx tsc --noEmit && bunx oxlint src && bun test src/components && bun run build`；`OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs` 六張沒有橫向溢出；桌面 `OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 沒走樣。
- 測試 project / bot / shell 全部刪掉，ego task space `completeTaskSpace(..., {keep:false})`。
- `WALKTHROUGH.md` 每項一行：✅ 正常／🔧 修了（commit）／❌ 未修（原因）。

## 回報
三到五行：走了幾項、修了幾個（commit hash）、需重啟 daemon（先 build 好不要重啟）、未修的。
