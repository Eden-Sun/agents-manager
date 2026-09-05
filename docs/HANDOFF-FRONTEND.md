# 交接：v4.0 前端（接手者：Grok）

工作樹狀態：半成品已以 `wip:` commit 保存（新元件 AttachButton / IssuesBar / KindTag / ModelPicker 與多處修改）。契約：`docs/API.md` §12（v4.0）。只動 `web/src/**`、`docs/FRONTEND.md`、`docs/screenshots/`。不要在主 repo 執行 git checkout / switch。

## 要完成的項目
1. attach 指令按鈕（`hosts[].attach_command`）：bot 標題列與群組標題列，一鍵複製。
2. 側欄專案標題整列可點、高度 ≥32px、hover 底色（另一分支 grok/project-select 也改了這裡，最後合併時以較完整者為準，這裡可先保留現狀）。
3. 專案「＋」開新增 Bot 時不顯示 Project 選擇、直接聚焦名稱。
4. codex 模型 / 強度 / Fast：`GET /api/models` 選項列，選模型後顯示該模型 `efforts` 與 Fast 開關（`service_tiers` 含 priority 才顯示）；grok 模型從 API 取；claude 靜態。`fast` 欄位貫通 types/normalize/mock/表單/設定面板。
5. 頂欄中央各 kind 的 5h/7d 剩餘徽章（`GET /api/quota`、WS `quota_updated`），hover 顯示重置時間與 plan，`claude:<identity>` 以小字附上；低於 20% 警示色。
6. 工具偵測與安裝：`hosts[].tools` 徽章；缺少時頂欄下方可收合提示列＋「用現有 agent 安裝」（選 running bot → `POST /hosts/:name/tools/install`）；新增 Bot 表單未安裝的 kind disabled。
7. kind 顯示「圖示 / 文字」全域切換（localStorage），套用所有 kind-tag（class 名保留）。
8. 對話草稿以 bot / 群組為單位保留（store `drafts` + localStorage），送出成功才清空，刪除 bot 清掉。
9. bot 人設 `persona`：設定面板與新增表單的 textarea；側欄有人設時小圖示。
10. GitHub issues 列：專案 `github` 非 null 時對話頂部「Issues」下拉（搜尋、open/closed、labels、插入 `#n 標題\nurl`、插入完整內容為引用）。

## 驗收
mock：`cd web && VITE_MOCK=1 npx vite --port 5186`，headless Chrome（CDP 9360、獨立 user-data-dir）截圖 `docs/screenshots/180-*.png`；真後端：`npx vite --port 5187`（proxy 到 7788；注意 7788 的 daemon 是舊版，沒有 v4.0 端點，前端要能優雅退回）截圖 `181-*.png`。`npm run build` 與 `npx tsc -p tsconfig.app.json --noEmit` 通過。commit 只 add `web docs/FRONTEND.md docs/screenshots`，訊息最後一行 `Co-Authored-By: grok <noreply@x.ai>`。
