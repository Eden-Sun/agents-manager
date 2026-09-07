# Goal：圖片暫存托盤（2026-09-07）

## 需求
使用者常常在 A 對話視窗手上有一張圖（截圖、拖進來的檔案），但想丟給的是另一個 bot。
現在附件托盤跟著單一對話走，換分頁就沒了。要一個**右側的暫存托盤**：

- 圖片可以先 drop 到畫面右邊一個常駐的暫存區（不屬於任何 bot），暫時 cache 住。
- 切到想要的對話視窗後，再把暫存區的圖 **拖（或點）進**那個對話的附件托盤 / 輸入框，送出時就跟著那則訊息走。
- 暫存區的東西跨 bot、跨 project、跨 team 群組都在；重新整理頁面可以消失（用記憶體 / object URL 即可，不用 daemon 存）。
- 可以移除單張、清空全部；每張顯示縮圖與檔名，數量上限與單檔上限沿用現有附件的限制（`attach::MAX_BYTES`、`useAttachments`）。

## 先看再做
- `web/src/components/Attachments.tsx`、`AttachButton.tsx`、`web/src/hooks/useAttachments.ts`（如果在 hooks 裡）——現在的附件托盤怎麼收檔、怎麼上傳（`POST /api/bots/{id}/attachments`）、怎麼在送出時帶上。
- `web/src/App.tsx` 的版面：右側常駐區要放哪（建議：主面板右緣一條可收合的窄欄，或右下角浮動托盤；手機寬度用底部）。
- `docs/UI-DECISIONS.md` 已定案的東西不要翻案，新的取捨補寫進去。

## 硬規則
- 先 `git pull --rebase`。另一個 agent 同時在改 `daemon/`、`HostsPanel.tsx`、`IssuesBar.tsx`（遠端 gh 登入），**不要碰那些**。
- 只改 `web/`（加 `docs/UI-DECISIONS.md`、`docs/FRONTEND.md`）。不要動 daemon、不要 cargo build、不要重啟 daemon。
- 上傳仍走現有端點：暫存托盤裡只存 File / blob，**拖進某個對話時才對那個 bot 上傳**（附件是 bot 範圍的，daemon `attach::resolve` 以 project 為範圍，跨 bot 直接沿用 attachment id 會失敗）。
- `cd web && npx tsc --noEmit && npx oxlint src && npm run build` 通過。5173 是 vite dev server（真 daemon），可用 `OUT=/tmp/tray node scripts/ui-goal-shots.mjs` 截圖看整體版面沒壞；自己的功能用 headless Chrome 或 Read 截圖驗證拖放後托盤內容正確。
- 深色 / 淺色、桌機 / 手機都要能用；鍵盤可達（托盤項目可 focus、Enter / Delete）。
- 只 `git add` 你自己改的檔案，commit 訊息 `feat(web): …`，`git push origin main`；push 被拒就 `git pull --rebase` 再推。

## 進度
- [ ] 設計（寫在這裡：位置、互動、狀態放哪）
- [ ] 暫存托盤 UI
- [ ] 拖 / 點進對話托盤並上傳
- [ ] 手機版
- [ ] 截圖驗證、UI-DECISIONS 補記
