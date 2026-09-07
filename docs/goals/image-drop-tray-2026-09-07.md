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

## 設計（2026-09-07 定案）

### 位置
- **桌機（>1024px）**：`.app` 的 grid 加第三欄，畫面最右緣一條常駐的「圖片暫存」窄欄
  （`.shelf`）。收合時 38px 的直立握把（寫著張數），展開 176px 顯示縮圖清單。
  收 / 展狀態記在 `localStorage`（`am.shelf.open`）——那只是版面偏好，圖片本身不存。
- **≤1024px（平板 / 手機）**：`.app` 改成上下兩列，暫存區變成**底部**一條橫向捲動的托盤，
  收合時只剩一列 38px 的握把。永遠是 grid 的一格，所以不會蓋住訊息或輸入框。
- 元件掛在 `App.tsx` 的 `.app` 下、`<main>` **外面**：換 bot / 換 project / 換 team
  都不會被 unmount，暫存內容自然跨 bot、跨 project、跨 team。
- 桌機上收合時如果有檔案正被拖進視窗，握把旁會浮出一塊虛線 drop pad（絕對定位、不佔版面，
  所以拖曳中不會有 reflow）——不然 38px 的握把太難命中。

### 狀態
- 新檔 `web/src/store/shelf.ts`，自己一個 zustand store（`useShelf`），**不放進主 store**：
  主 store 會把草稿鏡射到 localStorage、`refreshState` 會整批換掉 slice，而 `File`
  與 object URL 既不能序列化、也不該被覆寫。
- 每張：`{ key, file, name, size, url }`，`url` 是 `createObjectURL`，移除 / 清空時 revoke。
  重整頁面就空了（需求允許），daemon 完全不知道這一層。
- 上限：單檔 12MB（沿用 `Attachments.tsx` 的 `MAX_BYTES` = `attach::MAX_BYTES`）；
  張數 24。張數是這次**新加**的記憶體護欄——daemon 與 `useAttachments` 都沒有張數上限，
  但那兩處都只活到送出，暫存區會一直活著。

### 互動
- **進**：OS 拖放到暫存區、暫存區自己的 📎 檔案選擇器（手機唯一入口）、暫存區有 focus 時貼上。
  這三條路都**只 cache，不上傳**。
- **出**：
  - 點一下縮圖 → 放進「目前這個對話」的附件托盤，**那一刻**才對那隻 bot
    `POST /api/bots/{id}/attachments`（附件是 bot / project 範圍的，不能跨 bot 重用 id）。
  - 桌機可拖：item `draggable`，帶自訂 mime `application/x-am-shelf`（內容是 key）。
    `useDropTarget` 認這個 type，drop 時用 key 回 shelf 取 `File`，再走既有的 `files.add`。
    所以 ChatPanel / GroupChatPanel 的 drop 接線一行都不用改。
  - 鍵盤：item `tabIndex=0`，Enter / Space = 放進目前對話，Delete / Backspace = 移除。
- **複製語意，不是搬移**：放進對話後暫存區仍留著（同一張截圖常常要餵好幾隻 bot），
  只閃一下「已放入」。要清掉自己按 × 或「清空」。誤觸也就不會弄丟圖。
- 「目前這個對話」怎麼找到：shelf store 存一個 sink（`{ add, label }`），由當下掛著的
  ChatPanel / GroupChatPanel 在 effect 裡註冊 / 註銷。沒有 sink（Team 面板、沒選 bot）時，
  點擊 / Enter 給一則 notice「先開一個對話」，拖也沒有目標。

## 硬規則
- 先 `git pull --rebase`。另一個 agent 同時在改 `daemon/`、`HostsPanel.tsx`、`IssuesBar.tsx`（遠端 gh 登入），**不要碰那些**。
- 只改 `web/`（加 `docs/UI-DECISIONS.md`、`docs/FRONTEND.md`）。不要動 daemon、不要 cargo build、不要重啟 daemon。
- 上傳仍走現有端點：暫存托盤裡只存 File / blob，**拖進某個對話時才對那個 bot 上傳**（附件是 bot 範圍的，daemon `attach::resolve` 以 project 為範圍，跨 bot 直接沿用 attachment id 會失敗）。
- `cd web && npx tsc --noEmit && npx oxlint src && npm run build` 通過。5173 是 vite dev server（真 daemon），可用 `OUT=/tmp/tray node scripts/ui-goal-shots.mjs` 截圖看整體版面沒壞；自己的功能用 headless Chrome 或 Read 截圖驗證拖放後托盤內容正確。
- 深色 / 淺色、桌機 / 手機都要能用；鍵盤可達（托盤項目可 focus、Enter / Delete）。
- 只 `git add` 你自己改的檔案，commit 訊息 `feat(web): …`，`git push origin main`；push 被拒就 `git pull --rebase` 再推。

## 進度
- [x] 設計（寫在上面「設計」那節：位置、互動、狀態放哪）
- [x] 暫存托盤 UI（`components/ImageShelf.tsx` + `store/shelf.ts`，`App.tsx` 掛在 main 外面）
- [x] 拖 / 點進對話托盤並上傳（`useShelfSink` 註冊當下的 composer；`useDropTarget` 認
      `application/x-am-shelf`，drop 時才對那隻 bot 上傳）
- [x] 手機版（≤1024px 移到底部橫向捲動，≤640px 空狀態換短提示；點一下就進對話）
- [x] 截圖驗證、UI-DECISIONS / FRONTEND 補記

驗收（真 daemon + headless Chrome，全數 OK）：drop 兩張進暫存 → bot A 點第一張（上傳完成、
暫存仍 2 張，複製語意）→ 換 bot B（暫存還在、B 的 tray 空的）→ 從暫存拖第二張進 B（payload
只有 `application/x-am-shelf`，key=s2，上傳成功）→ Enter 再放一張、Delete 移除一張 →
淺色 / 收合握把 / 拖曳中的 drop pad → 390px 手機底部托盤與收合列。
`npx tsc --noEmit`、`npx oxlint src`（0 error）、`npm run build` 皆通過；
`scripts/ui-goal-shots.mjs` 七張整體版面重拍，其他面板沒被擠壞。
