# agents-manager — agent 工作規則

這份給所有在這個 repo 裡工作的 agent（claude / codex / grok，含 AG Man 派出的子 agent）。人類讀的說明在 `README.md`，規格在 `docs/SPEC.md`、`docs/SPEC-team.md`，API 在 `docs/API.md`，前端在 `docs/FRONTEND.md`，UI 取捨在 `docs/UI-DECISIONS.md`。

## 回覆語言
繁體中文（zh-TW）。程式註解與 commit 訊息可中可英，禁止日文。

## 專案長相
- `daemon/`：Rust（axum + sqlx/SQLite），唯一狀態源；透過 herdr socket 管 pane，hook 為主、終端快照為備援。
- `web/`：React + zustand，只做投影；dev 用 `cd web && npx vite`（5173，走真 daemon），mock 用 `VITE_MOCK=1`。
- 正式 UI 嵌在 daemon 二進位裡：前端改完要 `npm run build` **再** `cargo build --release -p agents-managerd` 才會進到 7788。

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
- web：`cd web && npx tsc --noEmit && npx oxlint src && npm run build`（既有 warning 不算，新增的要清）。
- 工作樹裡別人的 WIP 讓編譯掛掉時，對**你 staged 的內容**驗：`git archive` 出來或用 `git stash --keep-index` 以外的方式，總之不能碰別人的檔。
- UI 改動要看真畫面：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs`（headless Chrome 七張）或 ego-browser；截圖放 `docs/screenshots/<feature>/`。
- daemon 在 `127.0.0.1:7788`，token 在 `~/.config/agents-manager/ui-token`，header `X-AM-Token`。

## 提交
- 只 `git add` 自己改的 hunk（混檔用 `git apply --cached` 過濾），一個功能一個 commit。
- 訊息：`feat(scope): …` / `fix(scope): …` / `perf` / `docs` / `chore`，scope 用 `daemon` / `web` / `team` / `hosts` / `quota` 等，第一行說**為什麼**。
- `git push origin main`；被拒就 `git pull --rebase --no-autostash` 再推。
- 不要 push 編不過的 HEAD（別人的半成品被你的 commit 依賴到時，把那部分一起帶上並在訊息裡註明）。

## daemon 重啟
預設**不要**自己重啟（其他 agent 與使用者正在用）。回報時寫「需要重啟 daemon 才生效」。派工者明確允許時才用：
```sh
OLD=$(lsof -nP -iTCP:7788 -sTCP:LISTEN -t | head -1); [ -n "$OLD" ] && kill $OLD; sleep 2
nohup ./target/release/agents-managerd serve >> ~/.config/agents-manager/daemon.log 2>&1 & disown
```

## 用 herdr 開子 agent
- 名稱一律 `<你的 agent 名>-<字尾>`（`$AM_AGENT_NAME` 有值；PATH 上的 `herdr` shim 會自動補前綴），daemon 才會把它掛在你底下。
- 子 pane 用 `herdr pane split --pane $HERDR_PANE_ID`，帳號與 hook 環境會繼承。
- 子 agent 一樣要遵守本檔；派工 prompt 裡把「不要 stash、只 add 自己的檔」再講一次。
- 做完的子 agent 關掉 pane（`herdr pane close`），不要留一堆 done 的 pane。

## 回報格式
三到五行：做了什麼（commit hash）、怎麼驗的（數字）、要派工者做的事（例如重啟 daemon）、沒做到的與原因。不要貼整段 diff。
