# Team：執行者改成「併行數」的工作池——PM 不斷派、daemon 派給最多 n 個執行者併行（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的 hunk、不要重啟 daemon、只改任務需要的檔）。
daemon 一個 commit、web 一個 commit、文件可併入各自的 commit。做完在此打勾。
⚠️ `web/src/components/TeamRoleEditor.tsx` 等檔可能是別人未提交的 WIP，不要動、不要 add。

## 現況（要改掉的）
- `workers.count`（1–4，預設 2）是「幾個執行者」；PM `dispatch` 每筆 task **必須**寫 `to: dev-n`；
  該 worker 還有未完成 task 就整筆被拒（§4.5 派工上限、`team_sched.rs` ~1444 `open_task_of`）。
- 所以 PM 派完 n 筆就只能 `wait`，計畫是一批一批的；執行者忙碌與否 PM 得自己算。
- `team_tasks.worker_bot_id NOT NULL`（`db.rs:153`）、唯一索引 `team_tasks_one_open_per_worker`（每個 worker 一個未終態 task）。
  分支在 dispatch 當下就從整合分支切出（`tg::checkout_task_branch`）。

## 目標
- 執行者那一格改叫「**併行數**」，**預設 1**，範圍仍 1–4（`MAX_WORKERS`）。API 欄位名 `workers.count` **不改**（相容），語意改成「最多同時跑幾個 task」。
- PM 派工**不用指定 `to`**（`to` 仍可寫，向下相容；寫了就照舊指定）。沒指定的 task 進**佇列**，daemon 派給**閒著的執行者**；
  併行數 n 就是執行者 bot 的數量（`dev-1`…`dev-n`，建 team 時照舊一次建好）。
- PM 可以**隨時再 `dispatch`**，不必等前一批做完；派 10 筆、併行數 2，就是 2 筆在跑、8 筆排隊，跑完一筆就補一筆。
- `done` 在還有排隊或未終態 task 時照舊 `reject`。

## 1. daemon
- [ ] **schema**：`team_tasks.worker_bot_id` 改為可 NULL。SQLite 不能就地改 NOT NULL，照 `team_issues_number` 那次的做法寫開機遷移：
  `CREATE TABLE team_tasks_new(...)` → `INSERT INTO … SELECT` → `DROP` → `RENAME` → 重建兩個索引；用 `PRAGMA table_info` 判斷是否已遷移（`notnull == 0` 就跳過）。
  `team_tasks_one_open_per_worker` 保持 partial unique on `worker_bot_id`（NULL 不受唯一約束，正好）。`db::TeamTask.worker_bot_id: Option<String>`，所有讀它的地方（`team.rs` / `team_sched.rs` / `team_json`）跟著改。
- [ ] `DEFAULT_WORKER_COUNT = 1`（`team.rs:33`）。
- [ ] **解析**（`team_sched.rs` ~366）：`to` 變成可選；`brief` 仍必填。`DispatchItem.to: Option<String>`。
- [ ] **dispatch**（~1430–1530）拆成兩段：
  1. **收單**：每筆 task 寫 `team_tasks(state='queued', worker_bot_id = to 對到的 bot 或 NULL, branch = 先算好名字但不 checkout)`。
     `to` 指到的 worker 正忙 → 不再拒絕，改成**指定給他排隊**（`worker_bot_id` 設好但等他空）。`to` 對不到人才拒。
     `pm_repeat`：改成「整個 issue 內同一個 brief 出現第二次」（不分 worker）。檔案重疊警告照舊。
     回 PM 一則 note：「收到 N 筆。併行數 n：現在跑 M 筆（tK→dev-1…），排隊 K 筆。」
  2. **補位** `fill_workers(ctx)`（新函式）：對每個**在跑（run running）且沒有未終態已派 task**的執行者，依 `seq` 取最早一筆 `state='queued'` 且（`worker_bot_id IS NULL` 或 `= 他`）的 task → 設 `worker_bot_id`、**這時才** `checkout_task_branch`（從整合分支的**現在**切，才會含先前合併的東西；§6.2 的保證不變）→ `enqueue` 派工 relay（文字照舊）→ task 進 `working` 的既有路徑。
     呼叫點：dispatch 收單後、任一 task 進終態（`merged`/`skipped`/`failed`）後、`resume` 後、`Tick`。全部走同一個 scheduler 迴圈，不要另開 task。
  - `gate:dispatch`（§4.6）：在**收單後、補位前**停，放行後再補位。
  - **phase**：有任何未終態 task（含排隊）就 `working`；全部終態才回 `planning`（既有的 `set_phase(.., "planning")` 條件補上「沒有 queued」）。
  - `done`：有 queued 或未終態 task → reject，訊息寫出還有幾筆排隊。
- [ ] **PM 那邊的文字**（`team.rs` persona 與 relay 模板，SPEC 附錄 A.1 / A.4）：
  - persona：「用 `dispatch` 派工，**不用指定 `to`**，daemon 會派給有空的執行者（併行數 n）；可以隨時再派，不必等前一批做完；沒事就 `wait`；全部合併後 `done`。」
  - 首則 relay：「併行數 n（執行者：dev-1…）」取代「目前有 k 位執行者可派」。
  - A.4 回報批次的「目前 task 狀態」表加一欄 `排隊中` 與 `(併行 M/n)`。
- [ ] `PATCH /teams/{id}` 的 `workers.count`：**跑動中改大**→ 當場多建並啟動 `dev-(舊n+1)…dev-新n`（沿用 `insert_member` + `start_bot` + pretrust 那條路，§7.6 已有），啟動後 `fill_workers`；**改小**→ 只寫進 `roles_json`，下一批執行者（下一個 issue / `replace`）才生效，多出來的人做完手上的就不再被派。回應帶 `{"applied": "now" | "next_batch"}`。
- [ ] 測試（`team_sched.rs` 既有測試框架 `s.reply(...)`）：
  - dispatch 三筆、併行 1 → 一筆 working、兩筆 queued 無 worker；第一筆 merged 後第二筆自動派出、分支在那一刻才切。
  - 併行 2、`to` 指定忙碌的 dev-1 → 排在 dev-1 後面，不給 dev-2。
  - `done` 在有 queued 時被 reject。
  - 沒有 `to` 的 dispatch 解析成功；沒有 `brief` 仍失敗。
  - 遷移：舊 schema 的 DB 開起來 `worker_bot_id` 變可 NULL、資料還在。
- [ ] `docs/SPEC-team.md`：§4.4（`to` 可選）、§4.5「派工上限」改寫成「併行數」、§7.1 表（`worker` 行：併行數 1–4 預設 1）、§8.2 加「`queued` 可未指派」、§10.1 表（`count` 語意）、§10.5 PATCH 的 `applied`、附錄 A.1/A.4、附錄 B 的 SQL。標 2026-09-08。`docs/API.md` 有重複的地方同步。

## 2. web
- [ ] `TEAM_WORKERS_DEFAULT = 1`（`api/types.ts:873`）；`TeamLaunchPanel.tsx` 那格標籤改「併行數」、單位「人」改成「個」、hint 改成「最多同時跑幾個 task；PM 派幾筆都可以，多的排隊。每個併行位一個獨立 worktree。」
- [ ] `TeamPanel.tsx` 的 task 列：`worker` 為 null 的顯示「排隊中」（淡色、無執行者名）；標題列 task 計數旁加「併行 M/n」。`teamProgress.ts` 若把 queued 算進 total 就照舊。
- [ ] `api/types.ts`：`TeamTask.worker_bot_id: string | null`、`normalize.ts` 對應；`api/mock.ts` 給一個有排隊 task 的 team。
- [ ] `docs/UI-DECISIONS.md` 補「併行數」：為什麼叫併行數不叫人數（使用者關心的是同時跑幾個，不是有幾個 bot）、為什麼預設 1（省額度；多開是明確的選擇）、為什麼 PM 不指定人。`docs/FRONTEND.md` 對應段落。

## 驗證
- `cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`（HEAD 現在 340 個全過）。
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`（既有 36 warning 不算）。
- 真機不必開一個 team（會燒額度）；用 mock 截 TeamLaunchPanel 的併行數格與 TeamPanel 的排隊列各一張，放 `docs/screenshots/team-pool/`。
- 做完**需要重啟 daemon** 才生效（有遷移），回報時寫明，不要自己重啟。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon、沒做到的與原因。
