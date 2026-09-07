# 三個修正（2026-09-08）

執行者：opus / low。每項一個 commit，做完在此打勾。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。

## 1. 選身分不能動其他設定值
- [x] 現象：Bot 設定面板（`web/src/components/BotSettingsPanel.tsx`）切換 identity 後，effort / fast 被清掉。
- 原因：`ModelPicker.tsx` 的 `ApiModelFields` 用 `kind@host@identity` 當模型快取 key；換身分 → 快取 `undefined`（loading）→ 暫用 `staticModels()` → 兩個「修正」effect（`!hasFast && fast → onFast(false)`、`efforts` 不含 `effort → onEffort(null)`）拿靜態清單把值清掉。
- 修法：這兩個 effect 在 `loading`（`cached === undefined`）時不要跑；只在使用者明確 `pickModel` 或 API 清單載入完成後才修正。順便確認 identity 換掉時 `model` 也不會被動掉（若被清的是 model 也一併擋）。
- 驗證：開一個有 effort 的 claude bot 設定，切身分，儲存前 patch 內容只含 `identity`（可在 dev console 看 `changedKeys`，或改完後儲存的 PATCH body 只有 identity）。

## 2. 拖圖時背景不要糊，且要能丟到「圖片暫存」
- [x] 現象：macOS 截圖後拖縮圖進視窗，`.drop-veil`（`styles.css` ~2669，`accent-soft` 82%）把整個對話區蓋成一片，且無法丟到右側「圖片暫存」rail。
- 修法（`Attachments.tsx` / `ImageShelf.tsx` / `styles.css`）：
  1. `.drop-veil` 改成幾乎透明（只留虛線框 + 中央小標籤，背景 ≤ 15% 或 transparent），不要 `backdrop-filter`。
  2. 查為什麼丟不到 shelf：拖進 chat 後 `useDropTarget` 的 depth 計數在外部拖曳（截圖縮圖）取消時不會歸零 → veil 卡住；比照 `useFileDragActive` 在 window 的 `drop` / `dragend` / `dragleave`（relatedTarget 為 null）時把 depth 歸零並 `setOver(false)`。
  3. shelf 收合時 `shelf-pad`、展開時 `.shelf-body` 都要吃得到 drop：確認 `.shelf` 的 z-index / pointer-events 沒被 chat veil 或 `.dropping` 蓋到；不夠就把 `.shelf` 拉到 veil 之上。
- 驗證：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 一張拖曳中的畫面 + 用 ego-browser 或手動確認截圖縮圖能丟進 shelf（收合與展開各一次）。截圖放 `docs/screenshots/image-shelf/`。

## 3. Project 也可以改名
- [x] daemon：新增 `PATCH /api/projects/{id}` `{"label"}`（`daemon/src/api.rs`，仿 `patch_bot`）：trim 後非空，改 `cfg.projects[].label`，`reproject`，emit `project_changed`。不擋 active run（agent name 用 bot id，不用 label；只有 legacy 名稱與**下次啟動**的 `agent_name` 會變，回傳 `{"needs_restart": false}` 即可）。
- [x] `docs/API.md`：在 project 表格加這條，註明 label 影響之後啟動的 agent 名稱 slug。
- [x] web：`api/index.ts` 加 `patchProject(id, {label})`、`api/types.ts` 型別、`store.ts` 加 `patchProject`（成功後 `refreshState`，錯誤 `notify`）、`api/mock.ts` 對應。
- [x] web UI：新檔 `web/src/components/ProjectNameField.tsx`，仿 `BotNameField` 的 row 模式（已選取的 project 再點一下標題才進編輯；Enter 存、Esc 取消、空白不存），放進 `Sidebar.tsx` 的 `ProjectTitle`（`.project-label`）；群組聊天標頭若有顯示 label 也換上 head 模式。
- 驗證：`cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`、`cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`；用 curl PATCH 一次確認 200 與 `project_changed`。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon 才生效（第 3 點）、沒做到的。
