# Team：「無限併行」——同時做多個 issue，執行者數隨 issue 數放大（2026-09-09）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的 hunk、**不要重啟 daemon**、只改任務需要的檔）。
分三個 commit：daemon 核心、daemon API/PATCH、web。做完在此打勾。這是大改，**先讀** `docs/SPEC-team.md`
§2.3（issue 佇列）、§4.5（併行數，2026-09-08 版）、§6.2（分支佈局）、§8、附錄 A，以及 `daemon/src/team_sched.rs`
的 `Ctx` / `fill_workers` / `close_issue_and_advance` / `start_issue_workers`、`daemon/src/team.rs::start_issue`。

## 為什麼
- 併行數 2 的隊伍做 #48：PM 把 issue 拆成 t19‖t20 → t21（第三段要等前兩段合併），t21 期間 dev-2 空著。
  佇列裡還有 #49 等著，但現在的設計是**一次一個 issue**（`teams` 的鏡像欄位 = 當前那一項），閒著的執行者不能先做下一個。
- 使用者要的是：**併行數設「無限」時，佇列裡有幾個 issue 就同時開幾個 issue 的工**，執行者數量跟著放大，把可用的座位用滿。

## 設計
1. **`workers.count = 0` = 無限**（API 欄位不變，`0` 是新值；1–4 照舊）。UI 上是「無限」那一格。
2. **無限模式 = 多個 issue 同時 `working`**：
   - `team_issues` 可以同時有多列 `state='working'`。`teams.issue_number / branch / …` 這組鏡像改成**第一個** working 的 issue（只給舊 UI 標題用），不再當真相；所有判斷改走 `team_issues` + `issue_id`。
   - 起 issue：目前 `close_issue_and_advance` 完成一個才 `start_issue` 下一個。無限模式改成 `start_issues_up_to_capacity()`：佇列裡每個 `queued` 的 issue 都起（上限 `MAX_CONCURRENT_ISSUES = 6`，防 pane 爆炸；超過的等有 issue 結束再起）。每個 issue 照舊切自己的整合分支、寫自己的 `ISSUE.md`（**改成每個 issue 一份** `ISSUE-<n>.md`，`TEAM.md` 列出所有進行中的 issue 與各自的執行者）。
   - 每個 issue 有自己的執行者批：命名已經是 `t<tid6>-i<seq>-dev-<n>`，worktree `i<seq>-dev-<n>`，不會撞。無限模式下每個 issue **先建 1 個**執行者，PM 派工超過在跑的數量就**按需再建**（每個 issue 上限 `MAX_WORKERS` = 4；全隊執行者總數上限 12）。建執行者走 §7.6 的 `insert_member` + pretrust + `start_bot`。
   - **PM 與 reviewer 仍是同兩個 bot**。PM 同時管多個 issue：
     - `dispatch.tasks[].issue`（issue 號）在無限模式**必填**（只有一個 working 時可省）；對不到 working 的 issue → 拒。
     - `done` 帶 `issue`（同上）；只驗那個 issue 的 task 全終態，然後**只交付那個 issue**（分支 / PR），其他 issue 照跑。
     - 給 PM 的每則 relay 開頭標 `[#48]`；A.4 回報批次改成**按 issue 分組**的表。persona / 首則 relay 說明「你同時在管 N 個 issue，每筆 task 與 done 都要寫 `issue`」。
     - `pm_repeat`、relay 預算、wall clock 都**按 issue** 算（relay 已是；wall clock 用各 issue 的 `started_at`，扣暫停）。
   - reviewer 一次審一個 task，跨 issue 依回報順序排隊（現有行為）；每次送審前 `checkout_detach` 到那個 task 的分支（現有）。
   - `fill_workers` 改成**按 issue** 跑：對每個 working issue，用**那個 issue** 的整合分支切 task 分支（現在用 `ctx.team.branch`，那是鏡像，多 issue 時會切錯——這是最容易出錯的地方，要有測試）。`Ctx.issue: Option<TeamIssue>` 改成 `issues: Vec<TeamIssue>`（working 的），需要單一 issue 的地方改成傳 `issue_id`。
   - merge：合進**該 task 所屬 issue** 的整合分支（`task.issue_id → team_issues.branch`）。
   - 暫停：`DECISION_PAUSES`（merge_conflict / review_exhausted / pm_abort）只標**那個 issue** `failed` 並繼續其他（現有邏輯已是「佇列還有就前進」，改成「其他 working 的照跑」）；`quota_low` / `member_*` / `protocol_error` 仍暫停整隊。
   - team `done`：所有 issue 都終態且佇列空。
3. **有限模式（1–4）行為完全不變**：一次一個 issue、固定執行者數。所有新邏輯用 `ctx.unlimited()` 分岔，避免動到既有路徑。
4. **PATCH**：`workers.count` 0↔n 可在跑動中改：改成 0 → 立刻 `start_issues_up_to_capacity`；改成 n → 不再起新 issue，現有的做完，執行者數之後照 n。回 `applied: now`。

## 1. daemon 核心（`team_sched.rs` / `team.rs` / `db.rs`）
- [ ] `db`：`team_issues` 允許多列 `working`（檢查有沒有 partial unique 擋住）；新增 `db::working_team_issues(team_id)`；`Ctx.issues`。
- [ ] 解析：`dispatch.tasks[].issue`、`done.issue`（`Action::Dispatch` items 帶 `issue_id`，`Action::Done { issue_id }`）。
- [ ] `start_issues_up_to_capacity`、按需建執行者（`ensure_workers_for(issue, needed)`）。
- [ ] `fill_workers` / `dispatch` / merge / review 全部用 task 的 issue 分支。
- [ ] 每 issue 的 `ISSUE-<n>.md`、`TEAM.md`、PM persona 與首則／`next_issue` relay 文案、A.4 分組表。
- [ ] `done` 只收那個 issue：`close_issue(issue)` 不再 advance 單一下一個，而是 `start_issues_up_to_capacity`。
- [ ] 測試（既有 `S` 框架）：
  - 無限模式、佇列 3 個 issue → 3 個同時 working，各自 1 個執行者、各自整合分支；
  - 對 #A 派 3 筆 → 自動多建 2 個 `iA-dev-2/3`，全隊上限 12 生效；
  - task 分支從**自己的** issue 分支切（改 #B 的分支後 #A 的 task 看不到）；
  - `done{issue:A}` 只交付 A，B 照跑；
  - 有限模式所有既有測試不變。
- [ ] `docs/SPEC-team.md`：§2.3 加「多 issue 同時進行（無限模式）」、§4.4（`issue` 欄位）、§4.5 表（`0` = 無限）、§7.1、§8.1（phase 不變，issue 各自狀態）、§10.1/§10.5、附錄 A/B。`docs/API.md` 同步。

## 2. web
- [ ] `TeamLaunchPanel`：併行數控制加「∞ 無限」（count=0），hint：「佇列裡有幾個 issue 就同時做幾個；每個 issue 先 1 個執行者，PM 派多少就開多少（每 issue 最多 4、全隊 12）。額度會很快用掉。」
- [ ] `TeamRoleEditor`（執行者卡的 PATCH）同樣可以切 0↔n。
- [ ] `TeamPanel`：issue 佇列列表裡可以有多個「進行中」；Task 表按 issue 分組（每組標 `#n · 整合分支`、併行 M/n 或 `∞ · 執行者 k`）；成員列依 issue 分段（`i9-dev-1`、`i10-dev-1`…）。
- [ ] 側欄 team 節點：`issue 進行中 2 · task 4/9`。`teamProgress.ts` 跟著改並補測試。
- [ ] `api/types.ts` / `normalize.ts` / `mock.ts`（mock 給一個無限模式、兩個 issue 同時跑的 team）。
- [ ] `docs/UI-DECISIONS.md`「無限併行」：為什麼是 issue 級併行而不是把一個 issue 硬拆；為什麼每 issue 先 1 個執行者；為什麼有 6/12 的硬上限（pane 與額度，不是預算）。`docs/FRONTEND.md`。

## 驗證
- `cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`（HEAD 358 全過）。
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`。
- UI 用 mock 截 launch 的「無限」與 TeamPanel 多 issue 各一張，放 `docs/screenshots/team-unlimited/`。
- 真機不要開新 team（正在跑的 `1rammb` 別碰）；做完**需要重啟 daemon**，回報時寫明。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon、沒做到的與原因（尤其是有限模式有沒有任何行為改變）。
