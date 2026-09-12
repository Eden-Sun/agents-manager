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
本規範針對直接派在 repo 主樹上的 agent 與它們的子 agent；team 成員使用各自 worktree，照 `.agents-manager/team/TEAM.md`。
1. 開工先找自己的 worktree：`git worktree list` 若已有 `.claude/worktrees/<你的 agent 名>` 就使用它；沒有就從主樹執行
   `git worktree add .claude/worktrees/<你的 agent 名> -b <分支>`，之後只在那棵 worktree 改與 commit。
2. 進入自己的 worktree 後跑 `git status`，先辨認**其他 agent 未提交的改動**；那些不是你的，不要動，
   也不要在主樹 `git stash`、`--autostash` 或 `git checkout -- <file>`。HEAD 已等於 `origin/main` 時不用 pull；要 pull 一律
   `git pull --rebase --no-autostash`。
3. 有 goal 檔（`docs/goals/*.md`）就照它做；新 goal 請參考 [`docs/goals/TEMPLATE.md`](docs/goals/TEMPLATE.md)，做完在檔裡打勾。

## 改動邊界
- 只改任務需要的檔案與行；共用檔（`store.ts`、`ChatPanel.tsx`、`Sidebar.tsx`、`styles.css`、`api.rs`、`lifecycle.rs`）hunk 要小，新邏輯優先獨立成新檔。
- 不要翻案 `docs/UI-DECISIONS.md` 已定案的決定；新的取捨補寫進去。
- 改了 API 要同步 `docs/API.md`；改了 team 行為要同步 `docs/SPEC-team.md`。
- 不要加新功能、不要順手重構任務以外的東西。

## 驗證（收尾前必跑）
- 先跑一鍵檢查：`scripts/check.sh`。只驗單一側可用 `web` 或 `daemon` 參數。
- daemon 個別指令：`cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`、`cargo clippy -p agents-managerd`（目前既有 32 個 warning，暫不加 `-D warnings`）。
- web 個別指令：`cd web && bunx tsc -p tsconfig.app.json --noEmit && bunx oxlint src && bun run build`（既有 warning 不算，新增的要清）。
- 工作樹裡別人的 WIP 讓編譯掛掉時，對**你 staged 的內容**驗：`git archive` 出來或用 `git stash --keep-index` 以外的方式，總之不能碰別人的檔。
- UI 改動要看真畫面：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs`（headless Chrome 七張）或 ego-browser；截圖放 `docs/screenshots/<feature>/`。
- daemon 在 `127.0.0.1:7788`，token 在 `~/.config/agents-manager/ui-token`，header `X-AM-Token`。

## 提交
- 只在自己的 worktree 改與 commit；只 `git add` 自己改的檔案與 hunk（混檔用 `git apply --cached` 過濾），一個功能一個 commit。
- 訊息：`feat(scope): …` / `fix(scope): …` / `perf` / `docs` / `chore`，scope 用 `daemon` / `web` / `team` / `hosts` / `quota` 等，第一行說**為什麼**。
- 回報 commit hash，讓派工者在主樹只做 `git -C <主樹> merge --ff-only <你的分支>`；需要 rebase 時在自己的 worktree 做完再推自己的分支，禁止在主樹 stash、`checkout --` 或 autostash。
- 不要 push 編不過的 HEAD（別人的半成品被你的 commit 依賴到時，把那部分一起帶上並在訊息裡註明）。整合完成後移除自己的 worktree：`git worktree remove .claude/worktrees/<你的 agent 名>`。

## daemon 重啟
預設**不要**自己重啟（其他 agent 與使用者正在用）。回報時寫「需要重啟 daemon 才生效」。派工者明確允許時才用：
```sh
OLD=$(lsof -nP -iTCP:7788 -sTCP:LISTEN -t | head -1); [ -n "$OLD" ] && kill $OLD; sleep 2
nohup ./target/release/agents-managerd serve >> ~/.config/agents-manager/daemon.log 2>&1 & disown
```

## 用 herdr 開子 agent
- 名稱一律 `<你的 agent 名>-<字尾>`（`$AM_AGENT_NAME` 有值；PATH 上的 `herdr` shim 會自動補前綴），daemon 才會把它掛在你底下。
- 子 pane 用 `herdr pane split --pane $HERDR_PANE_ID`，帳號與 hook 環境會繼承。
- 子 agent 一樣要遵守本檔；派工 prompt 必須帶上：「先 `git worktree list` 找自己的 `.claude/worktrees/<你的 agent 名>`，沒有就 `git worktree add .claude/worktrees/<你的 agent 名>-<字尾> -b <分支>`；只在自己的 worktree 改與 commit，禁止在主樹 `git stash` / `--autostash` / `git checkout --`，只 `git add` 自己的檔案，收尾前跑 `scripts/check.sh`，完成後移除自己的 worktree。」
- 做完的子 agent 關掉 pane（`herdr pane close`），不要留一堆 done 的 pane。

## 回報格式
三到五行：做了什麼（commit hash）、怎麼驗的（數字）、要派工者做的事（例如重啟 daemon）、沒做到的與原因。不要貼整段 diff。
