# agents-manager — agent 工作規則

這份給所有在這個 repo 裡工作的 agent（claude / codex / grok，含 AG Man 派出的子 agent）。人類讀的說明在 `README.md`，規格在 `docs/SPEC.md`、`docs/SPEC-team.md`，API 在 `docs/API.md`，前端在 `docs/FRONTEND.md`，UI 取捨在 `docs/UI-DECISIONS.md`。

## 修正 Bot 直接向 AGM 申請（使用者授權，2026-09-12）

- 修正 Bot 在既有任務範圍內，可直接向 AGM 申請 ownership 協調、跨 Bot 調度、Rust release rebuild 或 daemon 重啟；不必先問使用者是否可以聯絡 AGM，也不必請使用者轉達。
- AGM 核對其他 Bot 的 WIP、進行中回合與預計影響後，直接核准、排程或拒絕。AGM 明確核准後，修正 Bot 可直接執行並回報證據，不再要求使用者二次同意。
- AGM 忙碌或尚未核准時，等待 AGM 調度，不把例行申請退回使用者。此授權限於原任務；刪除設定／歷史等原本明訂需使用者確認的操作仍依既有規則。

## 回覆語言
繁體中文（zh-TW）。程式註解與 commit 訊息可中可英，禁止日文。

## 專案長相
- `daemon/`：Rust（axum + sqlx/SQLite），唯一狀態源；透過 herdr socket 管 pane，hook 為主、終端快照為備援。
- `web/`：React + zustand，只做投影；dev 用 `cd web && npx vite`（5173，走真 daemon），mock 用 `VITE_MOCK=1`。
- 正式 UI 嵌在 daemon 二進位裡：前端改完要 `bun run build` **再** `cargo build --release -p agents-managerd` 才會進到 7788。

## 開工前
1. `git status`：工作樹常有**其他 agent 未提交的改動**。那些不是你的，不要動、不要 `git stash`、不要 `--autostash`、不要 `git checkout -- <file>`。
2. HEAD 已等於 `origin/main` 時不用 pull；要 pull 一律 `git pull --rebase --no-autostash`。
3. 有 goal 檔（`docs/goals/*.md`）就照它做，做完在檔裡打勾。

## 改動邊界
- 只改任務需要的檔案與行；共用檔（`store.ts`、`ChatPanel.tsx`、`Sidebar.tsx`、`styles.css`、`api.rs`、`lifecycle.rs`）hunk 要小，新邏輯優先獨立成新檔。
- 不要翻案 `docs/UI-DECISIONS.md` 已定案的決定；新的取捨補寫進去。
- 改了 API 要同步 `docs/API.md`；改了 team 行為要同步 `docs/SPEC-team.md`。
- 不要加新功能、不要順手重構任務以外的東西。

## 驗證（收尾前必跑）
- daemon：`cargo build --release -p agents-managerd` 與 `cargo test -p agents-managerd`。
- web：`cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`（既有 warning 不算，新增的要清）。
- 工作樹裡別人的 WIP 讓編譯掛掉時，對**你 staged 的內容**驗：`git archive` 出來或用 `git stash --keep-index` 以外的方式，總之不能碰別人的檔。
- UI 改動要看真畫面：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs`（headless Chrome 七張）或 ego-browser；截圖放 `docs/screenshots/<feature>/`。
- daemon 在 `127.0.0.1:7788`，token 在 `~/.config/agents-manager/ui-token`，header `X-AM-Token`。

## 提交
- 只 `git add` 自己改的 hunk（混檔用 `git apply --cached` 過濾），一個功能一個 commit。
- 訊息：`feat(scope): …` / `fix(scope): …` / `perf` / `docs` / `chore`，scope 用 `daemon` / `web` / `team` / `hosts` / `quota` 等，第一行說**為什麼**。
- `git push origin main`；被拒就 `git pull --rebase --no-autostash` 再推。
- 不要 push 編不過的 HEAD（別人的半成品被你的 commit 依賴到時，把那部分一起帶上並在訊息裡註明）。

## daemon 重啟
重啟 daemon、`cargo build --release -p agents-managerd`、或任何會替換正在使用的 Rust binary 之前，**必須先詢問 AGM**。在 AGM 使用者入口對話提出申請（不要用 worker assignment 自派給 AGM）；AGM 忙碌時不要插入或中斷它，等它下一次可回覆。詢問時附上：目前 agent 名稱、要改的檔案／範圍、工作樹中其他 agent 的 WIP、為什麼需要重啟或重建，以及預計影響。先查 AGM 的健康與派工狀態：

```sh
AGM_BIN="$HOME/.config/agents-manager/supervisor/AGM/bin/agm"
"$AGM_BIN" health
"$AGM_BIN" assignments --status queued
```

只有 AGM 明確回覆「可以」且確認不影響使用者回合、其他 agent 的 pane／assignment 與未提交改動，才可執行。AGM 未回覆、回覆不明確或 health 為 `degraded`／`critical` 時，停止並回報「等待 AGM 調度」，不要自行猜測。需要重疊檔案、同一模組或同一工作目錄時，交回 AGM 分配 ownership；不要直接插入其他 bot 的工作。

重啟前仍要保留其他 agent 的 WIP，不得 stash、reset、checkout 或覆蓋別人的改動。AGM 同意後才用：

```sh
OLD=$(lsof -nP -iTCP:7788 -sTCP:LISTEN -t | head -1); [ -n "$OLD" ] && kill $OLD; sleep 2
nohup ./target/release/agents-managerd serve >> ~/.config/agents-manager/daemon.log 2>&1 & disown
```

重建完成後先回報 AGM build/test 結果，再依 AGM 指示重啟；重啟後確認 `/api/supervisor/health` 與使用者入口 AGM 仍可用。若只是閱讀、`cargo check` 或不會替換執行中 binary 的局部驗證，不必重啟，但仍不可改動其他 agent 的工作範圍。

## 多 agent 協作與 AGM 調度
- AGM 是本 repo 的唯一調度者。開始新工作先讀 `AGM health` 與 `AGM assignments`，確認是否已有 bot 處理同一目標。
- 建立 child 前，先用 `herdr agent list`、`agm state` 與 `agm assignments` 搜尋同一 project、cwd、模組或任務脈絡的既有 child；優先恢復並重用同 context 的 child，不要因為目前閒置就另開重複 bot。找不到明確對應者或無法判斷 session 是否可恢復時，交給 AGM 選擇。
- 發現另一個 bot 正在改相同檔案、模組、API 或工作目錄時，停止擴大改動，將範圍、檔案與衝突點交給 AGM 決定；不要自行合併、覆蓋或替別人收尾。
- 需要平行工作時，先請 AGM 指定每個 bot 的 ownership、完成條件與驗證責任。沒有明確 ownership 就保持等待。
- AGM 也負責定期清理長時間未使用的 child：只候選沒有 active run、in-flight turn、未結案 assignment、user ownership 或最近活動的 child；先停止/關閉其 pane 並留下 bot id、最後活動時間與原因，刪除設定或歷史必須另取得使用者確認。
- 所有重啟、release Rust rebuild、跨 bot 改派與可能中斷使用者 session 的操作，都在 AGM 同意後執行並回報證據。

## 用 herdr 開子 agent
- 名稱一律 `<你的 agent 名>-<字尾>`（`$AM_AGENT_NAME` 有值；PATH 上的 `herdr` shim 會自動補前綴），daemon 才會把它掛在你底下。
- 子 pane 用 `herdr pane split --pane $HERDR_PANE_ID`，帳號與 hook 環境會繼承。
- 子 agent 一樣要遵守本檔；派工 prompt 裡把「不要 stash、只 add 自己的檔」再講一次。
- 做完的子 agent 關掉 pane（`herdr pane close`），不要留一堆 done 的 pane。

## 回報格式
三到五行：做了什麼（commit hash）、怎麼驗的（數字）、要派工者做的事（例如重啟 daemon）、沒做到的與原因。不要貼整段 diff。

## scratchpad 與 outbox（使用者 2026-09-16）
- scratchpad 只放中間產物，不再作為給使用者的輸出目錄，也不可放私鑰／憑證／DB 複本。
- 要交給使用者的檔案放 outbox：`~/.config/agents-manager/outbox/<你的 AM_BOT_ID>/`（自己 `mkdir -p`；daemon 之後會注入 `$AM_OUTBOX` 指到同一處）。這個目錄只保留 1 小時，AGM 每 10 分鐘清掉超過 1 小時的檔案；要長期保留的放 repo 或 `reports/`。
- 私鑰／憑證／DB 一律不得放 scratchpad 或 outbox。
