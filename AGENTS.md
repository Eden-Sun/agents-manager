# agents-manager — agent 工作規則

這份給所有在這個 repo 裡工作的 agent（claude / codex / grok，含 AG Man 派出的子 agent）。檔名是 `AGENTS.md`（2026-10-04 使用者：「改使用 agents md」，原本叫 CLAUDE.md）：AG Man 的 bot 由 config 的 `[agents.projects]` 注入這份（SPEC §6.5i），改了重啟 bot 生效；直接在 repo 裡開的 claude 沒有 CLAUDE.md 時會自己讀 AGENTS.md、codex 本來就讀它。人類讀的說明在 `README.md`（英文為主，`README.zh-TW.md` 為繁中版；改一份要同步另一份），規格在 `docs/SPEC.md`，API 在 `docs/API.md`，前端在 `docs/FRONTEND.md`，UI 取捨在 `docs/UI-DECISIONS.md`。

## 修正 Bot 直接向 AGM 申請（使用者授權，2026-09-12）

- 修正 Bot 在既有任務範圍內，可直接向 AGM 申請 ownership 協調、跨 Bot 調度、Rust release rebuild 或 daemon 重啟；不必先問使用者是否可以聯絡 AGM，也不必請使用者轉達。
- AGM 核對其他 Bot 的 WIP、進行中回合與預計影響後，直接核准、排程或拒絕。AGM 明確核准後，修正 Bot 可直接執行並回報證據，不再要求使用者二次同意。
- AGM 忙碌或尚未核准時，等待 AGM 調度，不把例行申請退回使用者。此授權限於原任務；刪除設定／歷史等原本明訂需使用者確認的操作仍依既有規則。

## 回覆語言
繁體中文（zh-TW）。程式註解與 commit 訊息可中可英，禁止日文。

## 專案長相
- `daemon/`：Rust（axum + sqlx/SQLite），唯一狀態源；透過 herdr socket 管 pane，hook 為主、終端快照為備援。
- `crates/`：從 daemon 抽出去的獨立 crate（crate 拆分專案）：`am-core`／`am-ports`（值型別與窄介面）、`am-config`（設定檔 `ConfigStore`；與 daemon 的連結在 `projection::app_ports_p2`）。這些 crate 的測試由 `scripts/check.sh` 的 `-p <crate>` 另跑。
- `web/`：React + Vite（SPA，沒有 Next.js）＋ zustand，只做投影；dev 版 `agm:5173` 由 `com.agm.dev-server` 維持（`bun run dev`，代理到真 daemon），mock 用 `VITE_MOCK=1`。
- 正式 UI 嵌在 daemon 二進位裡：前端改完要 `bun run build` **再** `cargo build --release -p agents-managerd` 才會進到 7788。

## 開工前
本規範針對直接派在 repo 主樹上的 agent 與它們的子 agent。
1. 開工先找自己的 worktree：`git worktree list` 若已有 `.claude/worktrees/<你的 agent 名>` 就使用它；沒有就從主樹執行
   `git worktree add .claude/worktrees/<你的 agent 名> -b <分支>`，之後只在那棵 worktree 改與 commit。
2. 進入自己的 worktree 後跑 `git status`，先辨認**其他 agent 未提交的改動**；那些不是你的，不要動，
   也不要在主樹 `git stash`、`--autostash` 或 `git checkout -- <file>`。HEAD 已等於 `origin/main` 時不用 pull；要 pull 一律
   `git pull --rebase --no-autostash`。
3. 文件只寫現況：功能做完，把仍有效的決定併進 `docs/SPEC.md`／`docs/API.md`／`docs/UI-DECISIONS.md`，不另留 goal、進度或驗收紀錄檔（git 歷史就是紀錄）。

## 改動邊界
- 只改任務需要的檔案與行；共用檔（`store.ts`、`ChatPanel.tsx`、`Sidebar.tsx`、`styles.css`、`api.rs`、`lifecycle/mod.rs`）hunk 要小，新邏輯優先獨立成新檔。
- 不要翻案 `docs/UI-DECISIONS.md` 已定案的決定；新的取捨補寫進去。
- 改了 API 要同步 `docs/API.md`；改了行為要同步 `docs/SPEC.md`。
- **新增 API 路由＝在 `daemon/src/bot_scope_matrix.rs` 的允許表登記**（bot principal 是 Allow 還是 Forbidden），推之前跑 `cargo test -p agents-managerd -- bot_scope_matrix`。漏登記 `bot_a_against_bot_b_matches_the_allow_table` 會報 UNLISTED、main CI 變紅（2026-10-04 #839）。
- 不要加新功能、不要順手重構任務以外的東西。
- **改注入規則檔會讓全部 bot 的共用 prompt prefix 失效一次**（#1941）：`lifecycle::child_agent_rules`（注入給所有 bot 的規則本文，`crates/am-lifecycle/src/lifecycle/setup.rs`）每顆 bot 逐字相同，改動後所有 bot 的快取都要重算一次。少改、合併改，不為了一句措辭單獨發；規則本文不寫任何一顆 bot 的名字、id、日期或 pane（具體名字只放最後一段）。
- **debug 建置的堆疊**（#1465）：`tokio::spawn`／`TaskTracker::spawn` 一個大的 async 區塊（幾 KB 以上）寫成 `spawn(Box::pin(async move { … }))`——debug 下 tokio 的 spawn 會把 future 以值一層層傳下去，一個 25 KB 的 future 就吃掉 1 MiB 以上；一個 async fn 裡不要排二十幾段各自 await 大 future 的程式（poll 框是每個 await 暫存的總和），按段拆成函式、各自裝箱（見 `runners/supervisor/runtime.rs` 的 `tick_once`）；呼叫端放的是被呼叫 future 的整個值，所以大框的 async fn 要在**被呼叫端**裝箱（`fn x<'a>(…) -> impl Future<Output = T> + 'a { Box::pin(x_body(…)) }`，見 `start.rs` 的 `start_inner`），在呼叫端 `Box::pin(x())` 沒用。守門是 1 MiB 執行緒上的 `the_plain_bot_gate_path_fits_in_a_one_mib_stack`、`a_controller_tick_fits_in_a_one_mib_stack`、`a_restart_fits_in_a_one_mib_stack`（`per_principal_auth_tests`）。
- **新增／修改資料表、欄位、索引、trigger（含各子模組的 `migrate`）＝升 `SCHEMA_VERSION`**：在 `crates/am-base/src/db.rs` 的 `SCHEMA_HISTORY` 最後加一行 `(N+1, "<指紋>")`（指紋照 `schema_guard` 測試失敗訊息給的值），不准改既有那幾行。沒升版＝pin 測試紅，而且舊 binary 會照開一個它不懂的 schema。已合流的別人的 schema 變更沒升版時，以主幹現況重算、併進同一版，不要只顧自己那一行。

## Rust 開發建置
- Linux／Mac 開發用 `build`／`run` 預設要求 Blaze（`AGM_RUST_ENGINE=blaze`）；設 `AGM_RUST_ENGINE=cargo` 可明確切換原生 Cargo：受管 Cargo shim 在本專案有 `.cargo/agm-blaze` 時自動轉交 `scripts/project-cargo.sh`；尚未刷新 shim 或非受管環境直接使用該腳本。`cargo dev` 建 daemon 也走此入口。
- 每台主機先跑 `scripts/install-blaze.sh --source`；預設缺工具／工具鏈時直接失敗，不自動退回 Cargo；排錯比對設 `AGM_RUST_ENGINE=cargo`。明確設 `AGM_RUST_ENGINE=auto` 才允許缺工具時退回 Cargo。不要用 `blaze enable` 改使用者全域 Cargo 設定，也不要直接呼叫 Blaze 繞過名額。
- `test` 一律保留原生 Cargo／既有 r7 分流，即使要求 `AGM_RUST_ENGINE=blaze` 也不走 JIT；專案共用背景服務預設 `RA_JIT_MEMORY_MAX=8G`（軟預算）、`RA_JIT_IDLE_SECS=600`，由上游服務閒置退出回收；不可隨意殺別的工作樹仍使用的服務。
- release、跨目標、自訂 profile、check／Clippy／fmt、`scripts/check.sh` 與 CI 保留原生 Cargo；Blaze 開發建置不能取代收尾驗證。工具鏈／記憶體與使用方式見 [docs/BUILD-CACHE.md](docs/BUILD-CACHE.md)。

## 驗證（收尾前必跑）
- 等待背景 log 完成用 `scripts/wait-log.py`（用法見 `--help`）。
- 開始驗證前，必讀 [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md)；完整的負載閘、Mac 原生驗證、UI 真畫面與 CI 分工都在該檔，這裡不複製第二份。
- 在自己的 worktree 跑 `CHECK_MAX_LOAD=40 scripts/check.sh changed`；共用基礎或大範圍改動才依細則擴大，不為保險另跑整樹或等待 GitHub Actions。
- 修改 shell／行程／signal 或平台差異仍須 Mac 原生驗證；UI 改動仍須 agent-browser 真畫面驗證。不可把未執行或中斷的檢查報成通過。

- clippy 的固定範圍：`cargo clippy --workspace --exclude agents-manager-desktop --exclude agents-managerd --lib --bins`，另跑 `cargo clippy -p agents-managerd --all-targets`（完整用法見驗證細則）。

## 提交
- 只在自己的 worktree 改與 commit；只 `git add` 自己改的檔案與 hunk（混檔用 `git apply --cached` 過濾），一個功能一個 commit。
- **推完不必等 CI**：`scripts/check.sh changed` 綠了就推、繼續做事。`ubuntu-ci` 之後回報紅燈時，紅的是你造成的就修；不是就回報派工者，指出**哪一條測試、哪個 sha**（log 在 agm-host 的 `~/.cache/agents-manager/ci/logs/<sha>.log`）。
- 訊息：`feat(scope): …` / `fix(scope): …` / `perf` / `docs` / `chore`，scope 用 `daemon` / `web` / `hosts` / `quota` 等，第一行說**為什麼**。
- 回報 commit hash，讓派工者在主樹只做 `git -C <主樹> merge --ff-only <你的分支>`；需要 rebase 時在自己的 worktree 做完再推自己的分支，禁止在主樹 stash、`checkout --` 或 autostash。
- 不要 push 編不過的 HEAD（別人的半成品被你的 commit 依賴到時，把那部分一起帶上並在訊息裡註明）。整合完成後移除自己的 worktree：`git worktree remove .claude/worktrees/<你的 agent 名>`。

## daemon 重啟
預設**不要**自己重啟（其他 agent 與使用者正在用）。回報時寫「需要重啟 daemon 才生效」。派工者明確允許時才用：
```sh
OLD=$(lsof -nP -iTCP:7788 -sTCP:LISTEN -t | head -1); [ -n "$OLD" ] && kill $OLD; sleep 2
nohup ./target/release/agents-managerd serve >> ~/.config/agents-manager/daemon.log 2>&1 & disown
```
部署（自動或立即）等換版窗口超過 3 分鐘，daemon 會通知使用者（側欄「⏳ 部署等 N 分」＋AGM），由使用者按「現在換版」或「先等」調度（SPEC §18.10）；agent 不要自己 kill daemon 繞過窗口。

## 用 herdr 開子 agent
- **派任何 issue 之前先認領**（#425）：`python3 scripts/agm.py issue claim <n> --child <子 agent 名> --worktree <path> --branch <b>`（部署過的總管用 `bin/agm issue claim …`）。回 exit 3 就是別的 bot 正在做——**不要派**，看它印出的 `claimed_by` 去協調。動靜只看當事人 marker，不被他人留言或 issue updatedAt 刷新；持有人每 24 小時至少 `agm issue touch <n>` 一次（或留 claim／release），24 小時都沒動靜的認領才可以接手（它會自己判斷並在留言裡講）。child 收尾（或你決定不做了）時 `agm issue release <n>`；票關掉就不用。
- 名稱一律 `<你的 agent 名>-<字尾>`（`$AM_AGENT_NAME` 有值；PATH 上的 `herdr` shim 會自動補前綴），daemon 才會把它掛在你底下。
- 子 pane 用 `herdr pane split --pane $HERDR_PANE_ID`，帳號與 hook 環境會繼承。
- 子 agent 一樣要遵守本檔；派工 prompt 必須帶上：「先 `git worktree list` 找自己的 `.claude/worktrees/<你的 agent 名>`，沒有就 `git worktree add .claude/worktrees/<你的 agent 名>-<字尾> -b <分支>`；只在自己的 worktree 改與 commit，禁止在主樹 `git stash` / `--autostash` / `git checkout --`，只 `git add` 自己的檔案，收尾前跑 `scripts/check.sh changed`，推完不等 CI（完整驗證由 ubuntu-ci 背景跑，commit status `ubuntu-ci` 紅了再處理），完成後移除自己的 worktree。」
- 派工只走 herdr pane：禁止用 CLI 內建子代理（Claude 的 `Agent`／`Task`／`Workflow`、codex／grok 的同類功能）或在自己 pane 另起 agent CLI 做事，AG Man 追不到。
- 開子 agent 一律 `herdr agent start <名稱> --kind … --pane <新 pane>`；**禁止** `herdr pane run <pane> "claude …"`／`"codex …"` 在新 pane 直接起 CLI（就算加了 `--name`）：那樣 agent 在 herdr 沒有名稱，AG Man 認領不到。要換身份（別的帳號）就加 `--identity <身份>`（寫在 `--` 之前；這台可用的身份看 `$AM_IDENTITIES`，issue #1775）。
- 子 agent 不再開子 agent（pane 有 `$AM_CHILD_OF`，shim 會 exit 77 拒絕 `agent start`）：要人手就在回報裡寫清楚，由 parent 決定重用閒置 child 或另開兄弟。派工 prompt 要帶上這條。
- 做完的子 agent 關掉 pane（`herdr pane close`），不要留一堆 done 的 pane。

## OB（網頁 GPT 外腦，使用者 2026-09-15）
- 「問 OB／請 OB review」＝透過共用 worker 操作的 ChatGPT 網頁取得第二意見（worker 自己用 ego lite，是「一律 agent-browser」的唯一例外；bot 不直接碰那個瀏覽器）。先查本機事實；一般諮詢不用先問 AGM，也不是每個請求必問。
- 透過 `python3 scripts/ob.py ask --project-id <AG Man project ID> --request-id <穩定請求ID> --wait 600 "問題"` 提交。用 ID，不用 label、worktree 或 bot 名。父 bot 派工／重用 child 時傳遞同一 project ID 與本指引。
- 共用一個本機 worker，**不呼叫 LLM**（#1919，直接操作瀏覽器，不吃訂閱額度）；每專案獨立固定 ChatGPT 對話。原任務 bot 自己判讀與驗證回覆，不把 OB 建議當授權。
- `done` 才有答案；`pending/running` 用 `status <id>` 查原單。`unknown` 要對帳／`collect`，不可換 request ID 重送。不要各自直接操作瀏覽器。
- 不貼 token／密碼／客戶資料；任何 bot（含 AGM/browser-gc）都不可關「ChatGPT 決策顧問」task space 與分頁或刪登錄資料。啟用與舊對話綁定見 `docs/CHATGPT-CONSULT.md`。

## 給使用者的檔案：outbox，不是 scratchpad（使用者 2026-09-16）
- scratchpad 只放中間產物，不再當成給使用者的輸出目錄。
- 要交給使用者的檔案放 outbox：`$AM_OUTBOX`＝`~/.config/agents-manager/outbox/<AM_BOT_ID>/`（本機 bot 啟動時 daemon 注入並建好，子 pane 繼承；沒有這個變數的舊 pane 自己拼同一個路徑）。空目錄會被清理收掉，**寫之前一律 `mkdir -p "$AM_OUTBOX"`**。只保留 1 小時（從檔案**搬進** outbox 起算，`mv`／`cp -p` 進來的舊檔不會立刻被清），AGM 的 `com.agm.outbox-gc` 每 10 分鐘清掉超過 1 小時的檔；要長期保留的放 repo 或 `reports/`。網頁「檔案暫存」只列 outbox 的檔案，scratchpad 一律看不到（SPEC §6.5f）。
- 私鑰、憑證、DB（含 DB 複本、瀏覽器 profile 這類會帶 cookie／token 的目錄）一律不得放 scratchpad 或 outbox；瀏覽器一律走 agent-browser，不另起別的 headless、不留 profile 目錄。登入態（使用者 2026-10-15；2026-10-18 拿掉「bot 不替使用者登入」）：使用者要 bot 登入時可以照做；使用者也可以在自己的環境用 ab 登入，再把登入態（`--state` 檔、cookie、session）交給 bot 的 ab session 接著執行。登入態檔算憑證，一樣不放 scratchpad、outbox 或 repo，用完就刪。複本 migrate 之類要用 DB 複本的驗證，做完當下刪掉複本。

## CodeGraph（程式碼圖譜，使用者 2026-10-31）
- 找程式、看呼叫關係時先用 CodeGraph，再退回 grep：MCP 工具 `codegraph_explore`（claude 由 `.mcp.json`、codex 由 `.codex/config.toml` 載入），或 shell `codegraph explore "<符號或問題>"`、`codegraph callers <符號>`、`codegraph impact <符號>`。
- 索引只建在主樹的 `.codegraph/`（不進 git）；worktree 裡查到的是主樹那份，會提示「來自不同的 worktree」，只在你自己分支新增的符號查不到，改用 grep。**禁止在 worktree 跑 `codegraph init`**（一份索引約 180 MB）。查不到明顯該有的符號時，在主樹跑 `codegraph sync` 補索引。

## 回報格式
三到五行：做了什麼（commit hash）、怎麼驗的（數字）、要派工者做的事（例如重啟 daemon）、沒做到的與原因。不要貼整段 diff。
