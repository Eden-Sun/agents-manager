# Issue Team 規格提案（SPEC-team v0.1）

> **狀態：提案，尚未併入 `docs/SPEC.md`，待使用者裁決。**
> 本文是「以 issue 為單位叫出一整個 team」的設計規格，只設計、不實作。所有小節編號獨立於 SPEC.md；
> 正式採納時預計併為 SPEC §14「Issue Team」，API 併入 `docs/API.md` §13，前端併入 `docs/FRONTEND.md`。
> 每個決定都附「理由」與「否決的替代方案」；真正需要使用者拍板的項目集中在 §12。
>
> 修訂紀錄
> - v0.2（2026-09-06）：修正 §6.2 的事實錯誤——`attach.rs` 的 `.gitignore *` 只蓋 `<project>/.agents-manager/attachments/` 這一層，`teams/` 不會被 ignore；worktree 佈局改為 daemon 資料目錄（repo 外），補 git 元資料（`.git/worktrees/`）與 cleanup 順序、兩種佈局比較表；連動 §2.2 / §6.5 / §6.6 / §7 / §11.3 / §13 驗收 / 附錄 A、C。
> - v0.1（2026-09-06）：初稿。依 SPEC v3.6 §2 / §6 / §13、API.md、FRONTEND.md、`daemon/src/{lifecycle,hookrecv,group,github,db}.rs`
>   與 `docs/herdr-schema.json` 撰寫；採信本 session 查證的事實（bot 之間無任何路由、每個 active Run 至多一筆 in-flight Turn、
>   Project 以 `(host, path)` 唯一、`persona` 已在程式碼但未進 SPEC、多 agent 同時改同一檔案實際造成過編譯失敗）。

---

## 0. 一句話

使用者在 IssuesBar 挑一個 issue → 選 PM 的 kind、執行者的 kind 與人數、reviewer 的 kind → daemon 建一個 **Team**：
在該 Project 底下建立臨時 Bot（各自在 daemon 資料目錄下的 `git worktree`，使用者的 checkout 不動）、把 issue 交給 PM、PM 以**結構化回覆區塊**分派工作、
daemon 負責在 PM / 執行者 / reviewer 之間**轉送**訊息、以 `git merge` 把通過審查的分支合進**整合分支**、
最後開 PR（或留分支）並停掉所有成員。使用者全程可看、可暫停、可插話、可中止；預算用完自動暫停。

---

## 1. 目標與非目標

### 1.1 目標
1. 從一個 issue 一鍵組隊，角色的 kind / model / effort / fast / identity 可選，執行者 1–4 人。
2. **bot 之間能傳話**（PM→執行者、執行者→PM、PM→reviewer、reviewer→執行者 / PM），且**不會無限迴圈、不會燒光額度**。
3. 執行者**互相隔離**：各自的 worktree 與分支，**使用者自己的 checkout 從頭到尾不被碰**。
4. 沿用既有機制：Bot / Run / Turn / Message、hook 回覆擷取、SPEC §13 群組時間軸、燈號、WS 事件、額度列。
5. daemon 重啟後能接回進行中的 team（沿用 SPEC §6.5 對帳）。

### 1.2 非目標（本提案）
- bot 之間**自由對話**（任意 A↔B）。只允許星狀拓撲（§4.4）。
- 非 git 目錄的 team（400 `not_a_git_repo`）。
- 執行者之間共享工作樹。
- daemon 幫 agent 跑測試 / 建置當作合併門檻（列為第二階段選項，§13）。
- 自動關 issue（交給 PR 的 `Closes #n`）。

---

## 2. 名詞與資料模型

| 概念 | 說明 | 主鍵 | 對應 |
|---|---|---|---|
| **Team** | 一個 issue 的一次組隊。屬於一個 Project；同一 Project 可同時有多個 Team（不同 issue） | `team_id`（ULID） | 一條**整合分支** + 一個 worktree 根目錄 |
| **Member** | Team 裡的一個 Bot，帶 `role ∈ {pm, worker, reviewer}`。**就是普通 Bot**（`bots` 表），只多了 `managed_by='team'`、`team_id`、`team_role`、`cwd` | `bot_id` | 一個 herdr pane（沿用 Run） |
| **Task** | PM 分派給某個 worker 的一件工作，有自己的分支與審查回合數 | `task_id` | `team/<...>/t<seq>-<worker>` 分支 |
| **Relay** | daemon 在成員之間轉送的一則訊息（PM 的 brief → worker、worker 的 report → PM…）。落地為 `team_events` 一列 + 收件 bot 的一個普通 Turn | `event_id` | 收件 bot 的 `turns.team_event_id` |

### 2.1 資料表（additive；完整 SQL 見附錄 B）

- `teams`：定義（issue、角色設定、預算、交付方式）+ 執行狀態（`phase`、`pause_reason`、`branch`、`base_sha`、`usage_json`、`pr_url`）。**SQLite 為權威，不進 TOML**（理由見 §5.3）。
- `team_tasks`：每件 task 的狀態機（§8.2）、分支、回合數、最後一次 report / verdict。
- `team_events`：team 的稽核日誌與**待送信箱**合一：`kind ∈ {relay, phase, merge, note, user}`；`relay` 類帶 `status ∈ {pending, delivered, dropped}` 與 `delivered_turn_id`。
- `bots` 新增：`managed_by TEXT NOT NULL DEFAULT 'user'`（`user | team`）、`team_id TEXT`、`team_role TEXT`、`cwd TEXT`（NULL = `project.path`）。
- `turns` 新增：`team_id TEXT`、`team_event_id TEXT`（這個 Turn 是哪一則 relay 送出的；NULL = 使用者 / 外部）。
- `messages` 新增：`team_id TEXT`、`relay_from TEXT`。值是**轉送來源 bot_id**，或保留字 **`'daemon'`**（daemon 自己產生的轉送：首則指派、合併通知、協定修復提示），`NULL` = 使用者親自發的。
  三者必須可分辨——§11.3 要求 daemon 自己的訊息顯示 `daemon → pm` 而不是 `你 → pm`，只有 bot_id / NULL 兩種值做不到。`'daemon'` 不是合法 bot_id（ULID），不會撞。

不改任何既有 `CHECK`（`turns.origin` 維持 `web`；SQLite 改 CHECK 要重建表，SPEC §12.5 加 grok 時的教訓），team 語意全部靠新欄位。

### 2.3 Issue 佇列（一組隊伍解多個 issue）

一個 Team 帶一份 **issue 佇列**，依 `seq` 順序處理。**PM 與 reviewer 全程是同兩個 bot**（保住累積的上下文），
執行者**預設每個 issue 換一批**，但 PM 在 `done` 時可用 `workers: keep` 決定沿用（依它對執行者上下文長度與相關性的判斷）；每個 issue 有自己的整合分支並各自交付。

- 新表 `team_issues`：`(team_id, seq)` 唯一；`(team_id, issue_number)` **只在還在佇列上的列之間唯一**
  （partial unique index `team_issues_number_open`，`WHERE state IN ('queued','working')`，見下方「再排同一個 issue」），
  `state ∈ {queued, working, done, failed, skipped}`，各自帶 `branch` / `base_sha` / `summary` / `pr_url` / `fail_reason`。
- `teams` 的 `issue_number` / `issue_title` / `issue_url` / `branch` / `pr_url` / `summary` / `issue_closed_at`
  **保留為「當前這一項的鏡像」**，換 issue 時由 daemon 同步。既有查詢、`team_json`、UI 標題都不必改。
- `team_tasks.issue_id` / `team_events.issue_id`：additive、可為 NULL。舊資料由開機遷移回填成該 team 唯一的那個 issue。
- **不新增 `phase` 值、不新增 `team_events.kind` 值**（兩者都是 CHECK 約束，改了要重建表）。
  issue 邊界事件走 `kind='note'` + `payload.action ∈ {issue_started, issue_finished, issue_failed, issues_queued, issue_unqueued, issue_start_failed}`。

#### 多 issue 同時進行（無限模式，2026-09-09）

`workers.count = 0`（UI 上的「∞ 無限」，§4.5）時**佇列裡的 issue 同時開工**，不再一次一個：

- `team_issues` 可以同時有多列 `state='working'`（上限 `MAX_CONCURRENT_ISSUES = 6`；超過的等有 issue 結束再起）。
  DB 沒有任何 partial unique 擋這件事，`team_issues_number_open` 擋的是「同一個 issue 號同時排兩次」，不受影響。
- `teams` 的鏡像欄位（`issue_number` / `branch` / …）改成**第一個** working 的 issue，**只給舊 UI 的標題用，不再是真相**。
  所有判斷一律走 `team_issues` 與 `team_tasks.issue_id`：task 分支從自己那個 issue 的整合分支切、合併合回自己的整合分支、
  review 的 diff base 也是自己的。唯一的整合 worktree（`main/`）在合併前會先切到該 task 所屬 issue 的分支。
- 每個 issue 有自己的執行者批（命名已經是 `t<tid6>-i<seq>-dev-<n>`、worktree `i<seq>-dev-<n>`，天生不會撞），
  **先建 1 個**，PM 派工超過在跑的數量就按需再建（每 issue 上限 `MAX_WORKERS = 4`、全隊 `MAX_TEAM_WORKERS = 12`）。
  建執行者走 §7.6 的 `insert_member` + pretrust + `start_bot`。收掉一個 issue 時只退掉**它自己**那批。
- 文件：每個 issue 一份 `ISSUE-<n>.md`（不再是單一 `ISSUE.md`），`TEAM.md` 列出所有進行中的 issue、各自的整合分支與執行者，
  在 issue 開工與執行者增減時重寫。
- PM 與 reviewer 仍是同兩個 bot。PM 同時管多個 issue（§4.4 的 `issue` 欄位、附錄 A.4 的分組表）；
  reviewer 一次審一個 task、跨 issue 依回報順序排隊（維持現有行為）。
- 終止：`done` 只結束**那一個** issue；`merge_conflict` / `review_exhausted` / `budget_time` 只把**那個** issue 標 `failed`
  並繼續其他的；`quota_low` / `member_*` / `protocol_error` / `pm_abort` 仍暫停整隊。所有 issue 終態且佇列空 → team `done`。

**有限模式（1–4）行為完全不變**：一次一個 issue、固定執行者數，所有新邏輯都以 `workers.count == 0` 分岔。

**分支與 base**：每個 issue 都從 team 建立時解析的 `base_sha` 切自己的 `team/i<issue>-<tid6>`，
彼此獨立、也不受使用者中途合併的影響。PM 的 `main/` worktree 不搬家，只切分支（切之前要求乾淨，跟合併前同一條規則）。

**目錄**：`main/` 與 `reviewer/` 全程固定（這兩個角色從不重啟，`bots.cwd` 只在開 pane 時讀）；
執行者是 `i<seq>-dev-<n>`，issue 結束就 `git worktree remove`。

**命名**：成員暱稱改為 team 範圍 —— `t<tid6>-pm`、`t<tid6>-rev`、`t<tid6>-i<seq>-dev-<n>`。
協定短名（`pm` / `rev` / `dev-1`）不變。舊 team 的 `i<issue>-` 前綴仍然剝得掉。

**persona**：PM 與 reviewer 的 persona **不得**提到 issue 號或分支名（persona 只在 agent 啟動時套用，而它們不重啟）。
每個 issue 會變的東西一律走 `ISSUE.md` / `TEAM.md`（每次邊界重寫）與一則 `next_issue` relay。

**預算**：`max_relays` 與 `max_wall_clock_min` 改為**每個 issue** 計算（relay 依 `issue_id` 過濾、時間從
`team_issues.started_at` 起算）。單 issue 的 team 數值完全等價。

**失敗處理**：`DECISION_PAUSES`（`merge_conflict` / `review_exhausted` / `pm_abort`）發生時，
**若佇列還有下一個**，該 issue 標成 `failed`（保留分支）並自動前進；佇列空了才照舊 `paused` 等使用者 `decide`。
其餘 pause 原因是團隊級問題，一律暫停整隊。

**最後一個 issue**：`finish` 照舊停掉所有成員、寫 `done`，**不動 worktree** —— 清理仍是 `cleanup` 這個人工動作。

**再排同一個 issue（2026-09-07）**：「已在佇列」只算 `queued` 與 `working` 這兩個狀態。
`done` / `failed` / `skipped` 是**做完那一趟的紀錄**，不再佔住 issue 號，所以同一個 issue 可以再排一次
（失敗後重試、或使用者看了成果想再做一趟）：**舊列原封不動留著當紀錄，新列取下一個 `seq`**。
只有同號 issue 還在佇列上（`queued` / `working`）才回
`409 {"error":"conflict","reason":"issue already queued","issue_number":n}`；同一個請求裡自己重複（`[57, 57]`）也算。
第 n 趟（n ≥ 2）的整合分支是 `team/i<issue>-<tid6>-r<n>`（§6.2）——第一趟那條分支還在
（`cleanup` 與 `finish` 從不刪分支，§6.5），同名 `git checkout -b` 會直接失敗。
`POST /teams/{id}/close-issue` 省略 `issue_id` 時取**最後一趟**（`teams` 的鏡像就是它）。
> 改這條之前，一個 issue 在同一隊做過（或失敗過）一次就永遠不能再排，UI 的「追加 issue」只會回
> `409 issue already queued`。全表唯一的舊索引 `team_issues_number` 由開機遷移 `DROP`。

**UI 的進度口徑（2026-09-08）**：daemon 沒有、也不打算有一個現成的「回合數／總回合數」，
所以側欄 team 卡片與佇列進度條上的 `N/M` 是前端從既有欄位算出來的
（`web/src/components/teamProgress.ts`，附 `teamProgress.test.ts`）：

- **issue 計數**：`N` = `done + failed + (當前那一項是 working ? 1 : 0)`，夾在 `total` 以內；
  `M` = `issues_summary.total`。也就是「正在做第幾個 issue」——交付 1 個、正在做第 2 個 → `2/20`，
  全部結束時 `20/20`。`total <= 1`（沒有佇列）時不畫。
- **task 計數**：`N` = `tasks_summary` 裡 `merged` + `skipped` + `failed` 的和，`M` = `tasks_summary.total`。
  這是**當前這一個 issue** 的 task。PM 還沒拆 task 時 `total` 是 0，就不畫計數。
- **兩個計數同時給，而且要寫出單位**（2026-09-08 修）：側欄原本二選一畫一個裸的 `N/M`，
  20 個 issue 的隊伍畫的是 issue 序，於是「已合併 4/5 個 task」的隊伍在側欄寫著 `2/20`，
  使用者只能理解成「卡在 2」（issue #53）。現在畫成 `issue 2/20 · task 4/5`。
- **耗時**：從**當前這一項**的 `team_issues.started_at` 起算（跟 §9 的 `max_wall_clock_min` 同一個起點），
  凍結在該項的 `ended_at`；佇列裡沒有對應那一項的舊 team 退回 `teams.started_at` / `ended_at`，
  整隊結束時一律凍結（否則做完的卡片會一直跳秒）。
- **`paused` 與終態也要凍結**（2026-09-08 修）：暫停的隊伍沒人在燒時間，計時器卻照著
  `started_at` 一路跳（#53 停在 119 分卻寫「已 4 小時 38 分」，看起來像卡死在跑）。
  沒有 `ended_at` 可用時改讀 daemon 的 `usage.elapsed_min`——它跟 `max_wall_clock_min`
  是同一個數字，而且 scheduler 在暫停後就不再更新它，正好是停下來的那一刻。

**側欄的暫停列（2026-09-08）**：`phase = paused` 的 team 節點在進度那一行底下多一列，
**直接寫出**「已暫停 · <pause_reason 的中文>」（不再只有一顆褐色點加 tooltip），並依原因給一顆按鈕：

| `pause_reason` | 側欄按鈕 |
| --- | --- |
| `budget_time` / `budget_relays` | 「加碼並繼續」＝ `PATCH` 把 `max_relays`、`max_wall_clock_min` 各 ×2，成功後 `resume`（同 TeamPanel 標題列的兩顆） |
| `ask_user`、`gate:*`、`member_*` | 不給按鈕，只顯示原因——回答 PM、放行閘門、救成員都得在 TeamPanel 裡做 |
| 其餘（`user`、`quota_low`、`merge_conflict`…） | 「繼續」＝ `resume`（`quota_low` 的成員名單在 tooltip，見 §4.5 `pause_detail`） |

### 2.4 Submodule 的 issue（2026-09-07）

專案的 git submodule 各自是一個 repo、各自有 GitHub issue。一個 team 可以指定 `repo`（相對於專案根目錄的 submodule 路徑，`""` = 專案本身），之後：

- **一切 git 操作都在 `<project.path>/<repo>` 裡**：`resolve_commit`、`create_branch`、`worktree add / remove / prune`、`branch -D`、`remote_base_branch`。submodule 的 gitdir 在 `.git/modules/<repo>`，`git worktree add` 對它照樣可用；成員的 cwd 因此是 submodule 的 worktree，改的、commit 的都是 submodule 的內容。
- **GitHub 操作都對 submodule 的 origin**：issue 讀取、`deliver=pr` 的 `gh pr create`、使用者按下的 close-issue。
- **驗證**：`repo` 必須是 `git config --file .gitmodules` 列出的路徑之一（這份清單是唯一能把使用者輸入變成 git 工作目錄的東西），且該 submodule 有 GitHub origin；否則 400。
- `teams.repo TEXT NOT NULL DEFAULT ''`（additive）；`TEAM.md` 多一行說明成員的 cwd 是哪個 submodule 的 worktree。
- UI：IssuesBar 在專案有（掛 GitHub 的）submodule 時多一個 repo 選單；組隊會帶著它；team 標題與側欄節點在 `#號` 前面標 submodule 路徑。
- **不做**：跨 repo 的單一 team（一個 team 同時改 root 與 submodule）——那是兩個 team；root 裡的 submodule 指標更新仍是人工動作。

### 2.5 Reopen：done 之後追加 issue（2026-09-07）

一個跑完（`phase=done`）但**還沒 cleanup** 的 team，現場都還在：`main/` 與 `reviewer/` worktree、PM 與 reviewer 的對話、
整合分支、team workspace。使用者常在這時才想到「順便把 #57 也做了」，目前只能重新組一隊，PM 對這個 codebase 累積的脈絡全部丟掉。
本節讓 `POST /teams/{id}/issues` 對 `done` 放行，把 team 從 `done` 拉回佇列流程；**這只是 §2.3「佇列前進」的一個新入口，不是另一套流程**。

#### 2.5.1 放行條件

- `add_issues` 目前對所有終態一律 `409 team is finished`。改為：**`done` 且未 cleanup → 放行並觸發 reopen**；`aborted` / `failed` 仍 409（它們沒有可靠的現場可續，使用者請重新組隊）。
- 「未 cleanup」的判準是 **PM 成員 bot 的 `deleted_at IS NULL`**（`cleanup` 與 `delete` 都會標它；`workspace_id` 在 §6.4a 之前建的舊 team 上不可靠）。
  已 cleanup 的 `done` 回 `409 {"error":"conflict","reason":"team is cleaned up","phase":"done"}`。
- 其餘驗證不變：issue 存在（`gh issue view`）、不與佇列上的同號重複（§2.3「再排同一個 issue」）、總數 ≤ `MAX_QUEUED_ISSUES`、kind 已安裝、額度未低於 `quota_stop_pct`（reopen 等於一次啟動，套 §7.4 建 team 時的額度檢查）。
- 非 `done` 的正常執行中 team 走原路徑（只排隊，不動 phase）；本節只描述 `done` 分支。

#### 2.5.2 daemon 流程

同步部分（在 `add_issues` 內，回 200 之前）：

1. 寫入 `team_issues`（`state='queued'`，`seq` 接續），照舊記 `issues_queued`。
2. 記 `team_events` note：`{action:"team_reopened", by:"user", issue_numbers:[…], from_phase:"done"}`。
3. `UPDATE teams SET ended_at = NULL`；`set_phase("starting")`（phase 事件 `done → starting`，reason `reopen`）。
   `starting` 是必要的中繼站：成員要先起來才能 `planning`，而 §7.4 的 `startup` 只在 `starting` 跑。
4. `spawn_scheduler`（`live_teams` 以 phase 篩選，daemon 重啟後 `respawn_schedulers` 也會自然接手）。

背景部分（scheduler 的 `startup`，判斷「這是 reopen」的條件是 **`starting` 且沒有 `working` 的 issue、但有 `queued` 的 issue**）：

5. **收掉上一批執行者**：最後一個 issue 結束時 `end_team` 只停成員、不 retire（§2.3「最後一個 issue」），
   所以它的 worker 還掛在 `bots`（`deleted_at IS NULL`、cwd 在 `i<seq>-dev-<n>`）。對最後一個 `done` 的 issue 跑 `retire_issue_workers`
   （停 pane、標 `deleted_at`、`worktree remove`）。**不讀 `worker_plan.keep`**：那是 PM 在上一個 issue 結束時對「下一個」的判斷，
   當時它以為沒有下一個；reopen 一律換新批。
6. **PM 與 reviewer 原地重啟**：對兩者各呼叫 `start_bot`，帶 §2.5.3 的續接選項。
   worktree `main/`、`reviewer/` 與 `bots.cwd` 完全不動；persona 不含 issue 號（§2.3），所以重啟後套同一份 persona 是對的。
   - PM 起不來 → `paused(member_failed:<pm>)`，**不是** `failed + cleanup`（§7.4 那條是建 team 失敗才適用；reopen 失敗不能把做完的成果清掉）。使用者修好後 `start` 該 bot 再 `resume`，scheduler 回到 `starting` 重跑本段。
   - reviewer 起不來 → 同 §7.4：`paused(member_failed:<rev>)`。
7. **建新一批執行者**：依 `roles_json.workers.spec` / `count`，走既有 `start_issue(next, keep_workers=false)`
   （切 `main/` 到新整合分支 `team/i<issue>-<tid6>`、`worktree add` `i<seq>-dev-<n>`、insert 成員、重寫 `ISSUE.md` / `TEAM.md`、鏡像 `teams` 欄位），
   再 `start_issue_workers`。`start_issue` 對 `main/` 的「必須乾淨」檢查照舊，髒了 → note `issue_start_failed` + `paused(upstream)`。
8. **交給 PM**：`hand_issue_to_pm(kept_workers=false)` —— `set_phase("planning")` 並送 `next_issue` relay。relay 文字多一句視 §2.5.3 結果而定：
   續接成功 →「這是同一段對話的延續」；退回新對話 →「你是重新啟動的 PM，先前的對話不在了，請先讀 `TEAM.md` 與 `ISSUE.md`」。

之後就是普通的 §2.3 佇列：PM 派工、審查、合併、交付、`issue_finished`，佇列空了再回 `done`。
**再 reopen 幾次都可以**，每次都是一組新的 `team_issues` 列與一則 `team_reopened`。

#### 2.5.3 PM 脈絡保留：`start_bot` 用 native session 續接

PM 的價值在它記得這個 repo、記得上一個 issue 的取捨。停掉的 pane 救不回來，但 CLI 自己的對話可以：

- `runs.native_session_id` 目前**已由 hook 回填**（claude `SessionStart` 的 `session_id`、codex notify 的 `thread-id`、grok 的 `sessionId`），只是 `start_bot` 從來不讀。
  它在 `db::Run` 的預設值是 `None`，實際上 claude / codex 的 run 幾乎都有值。
- `start_bot` 加一個選項（例如 `start_bot_with(app, bot_id, StartOpts { resume_native: bool })`；既有 `start_bot` = `resume_native=false`，**使用者手動啟動的行為完全不變**）。
  `resume_native=true` 時：取該 bot **最近一個已結束的 run** 的 `native_session_id`（`ORDER BY started_at DESC`），依 kind 組指令：

  | kind | 續接方式 | 備註 |
  |---|---|---|
  | `claude` | argv 多 `--resume <session_id>` | 其餘旗標（`--settings`、`--append-system-prompt`、`--model`…）照舊。session 檔案按 cwd 存放，PM 的 cwd 沒變所以找得到 |
  | `codex` | `codex resume <thread_id>` 子指令形式，其餘 `-c …` / `--yolo` 旗標接在後面 | 若該版 codex 不接受這組旗標，視為續接失敗 |
  | `grok` | 不支援 | 一律新對話 |

- **拿不到就退回新對話**：沒有 session id、kind 不支援、或 CLI 啟動後 `SessionStart` 回報的 `session_id` 與要求續接的不同（表示 CLI 靜默開了新對話）
  → 照常啟動，但記 note `{action:"member_context_lost", bot, role, why:"no_session_id"|"unsupported_kind"|"resume_mismatch"}`，
  並在該成員對話插一則 system 訊息說明。§2.5.2 第 8 步的 relay 據此換句。
- 續接**不是** reopen 專屬：`resume` 後由使用者手動 `start` 的成員仍是新對話（維持現狀）；只有 daemon 自己重啟 PM / reviewer 的路徑帶 `resume_native=true`。
  要不要開放給一般 bot 是另一個題目，不在本節。

#### 2.5.4 預算、關 issue、鏡像欄位

- **預算不變**：`max_relays` / `max_wall_clock_min` 本來就按 issue 算（§2.3），新 issue 從自己的 `team_issues.started_at` 起算、relay 依 `issue_id` 過濾。`quota_stop_pct` 在 reopen 時檢查一次，之後照常。
- **`close-issue` 在 reopen 後仍對做完的 issue 有效**。目前 §10.7 只寫 `teams.issue_closed_at`（當前鏡像），`team_issues.issue_closed_at` 欄位存在卻沒人寫；`start_issue` 換 issue 時又把鏡像清成 NULL，
  等於「做完 #42、reopen 做 #57」之後，#42 有沒有關過就查不到了。改法：
  1. `close_issue` **同時**寫 `team_issues.issue_closed_at`（`WHERE team_id=? AND issue_number=?`），並以那一列判斷「關過一次 409」。
  2. `POST /teams/{id}/close-issue` body 多一個可選 `issue_id`；省略 = 當前鏡像（`teams.issue_number`）。§10.7 規則 1 從「team `phase=done`」改為**「該 `team_issues` 列 `state='done'`」**——
     team 整體 `done` 時當前鏡像必然 `state='done'`，所以舊行為是新規則的特例；reopen 進行中也能回頭關 #42。規則 2（只有使用者按）不變。
  3. `teams.issue_closed_at` 維持「當前鏡像」語意：只在關的是當前 issue 時更新；`start_issue` 清 NULL 的行為保留（新 issue 本來就沒關過）。
  4. UI：`IssueQueue` 每一列 `state='done'` 且未關的 issue 給「關閉 issue」小按鈕（同一個 `ConfirmDialog`，帶 `issue_id`）；既有 done 卡片的大按鈕不動。
- **`ISSUE.md` / `TEAM.md`**：由 `start_issue` 重寫，內容只提新 issue；上一個 issue 的成果留在它自己的整合分支與 `team_issues.summary`。

#### 2.5.5 UI

- `TeamPanel` 的 done 卡片（目前只有 composer-lock 文字 + 「清理」）多一顆 **「追加 issue 繼續」**：點了展開一個 issue 號輸入（多個以逗號或空白分隔，
  沿用 IssuesBar 的 issue 挑選器亦可），送 `store.addTeamIssues`（store 已有，之前沒有任何元件呼叫）。
  已 cleanup 的 team（PM bot `deleted_at` 非空，或 daemon 回 409 `team is cleaned up`）不顯示這顆按鈕。
- 送出後 `team_changed` 會把 phase 推成 `starting` → `planning`，既有面板自動切回進行中視圖（composer 解鎖、成員 lamp 亮起）。
- Timeline 多兩種 note 的呈現：`team_reopened`（「使用者追加 #57、#58，team 重新啟動」）、`member_context_lost`（「PM 沒能續接先前對話，已改為新對話」）。
- `IssueQueue` 目前 `issues.length < 2` 就不畫；reopen 後至少 2 個，會自然出現。

#### 2.5.6 驗收條件

1. `done` 且未 cleanup 的 team `POST /teams/{id}/issues {"issue_numbers":[57]}` → 200；`team_events` 依序出現 `issues_queued`、`team_reopened`、phase `done→starting`、`issue_started`、phase `starting→planning`、一則 `next_issue` relay（`to=pm`）。
2. 同一 team 在 `aborted` / `failed` 呼叫 → `409 team is finished`；`done` 但已 `cleanup` → `409 team is cleaned up`。兩者都不寫任何 `team_issues` 列。
3. reopen 後 PM 與 reviewer 的 `bot_id`、`bots.cwd`、`main/` 與 `reviewer/` 目錄都與 reopen 前相同；上一個 issue 的 worker bot 全部 `deleted_at` 非空、worktree 已移除；新 worker 的 `cwd` 是 `i<新seq>-dev-<n>`、kind/model/effort 等於 `roles_json.workers.spec`。
4. `main/` 的 HEAD 在 `team/i57-<tid6>`，且該分支的第一個 commit 是 team 建立時的 `base_sha`。
5. PM 是 claude 且上一個 run 有 `native_session_id` → 新 run 的 argv 含 `--resume <該 id>`，啟動後 `SessionStart` 回報同一個 `session_id`，沒有 `member_context_lost` note。把 `native_session_id` 清成 NULL 再 reopen → 照常啟動，有 `member_context_lost{why:"no_session_id"}`，relay 文字含「重新啟動」那句。
6. 第二個 issue 跑完、佇列空 → 回 `done`，`teams.ended_at` 重新寫入；再 reopen 一次仍成立（1–5 可重複）。
7. reopen 進行中對 #42（`state='done'`）`POST close-issue {"issue_id":…}` → 200，`team_issues.issue_closed_at` 有值、`teams.issue_closed_at`（鏡像 #57）仍為 NULL；再關一次 → 409。#57 尚未 done 時對它 close → 409。
8. `PATCH /teams/{id}` 的 `workers` 設定改過之後 reopen，新批執行者採用改後的 `roles_json.workers.spec`。
9. daemon 在 `starting`（reopen 中）被重啟 → `respawn_schedulers` 接手，不重複建 worker（`start_issue` 只在沒有 `working` issue 時跑）。
10. 使用者手動 `POST /bots/{pm}/start` 的 argv **不含** `--resume`（既有行為不變）。

### 2.2 `bots.cwd`（新，通用欄位）

`start_inner` 目前 `pane.split { cwd: project.path }`；改為 `bot.cwd.unwrap_or(project.path)`。這是 team 成員能住在 worktree 的**唯一**必要改動，
同時對一般 bot 也有用（使用者想讓某個 bot 固定在子目錄）。`POST /projects/:id/bots` / `PATCH /bots/:id` 順便開放 `cwd?`；使用者自設時必須位於 `project.path` 之下，daemon 替 team 成員設的則是資料目錄裡的 worktree（§6.2），其他一律 400。

---

### 2.6 Rescue：把沒解決的 task 交給一個人收尾（2026-09-11）

跑完的 team（`phase=done`）常常還留著 `failed` / `skipped` 的 task。重排一次 issue（§2.5）會把
規劃整個重跑，已經合併的成果也失去意義；使用者要的是**擇一個成員把沒解決的一次處理掉，通常是 reviewer**
——它已經看過這個 issue 的每一份 diff，而且工作樹是空的。

`POST /teams/{id}/rescue`（body `{"bot_id"?}`，省略 = reviewer）：

1. **放行條件**：`phase=done`、未 cleanup（PM 的 bot 還在，同 §2.5.1 的判準）、且至少一個 task 是
   `failed` / `skipped`。收尾者必須是這個 team 還活著的成員，且**不能是 PM**：PM 的 cwd 是整合
   工作樹 `main/`，在那裡切任務分支會弄髒 daemon 要合併的現場。
2. **一個 task，不是一個失敗一個 task**：`team_tasks_one_open_per_worker` 本來就限制一個成員同時只有
   一個未終態的 task；而且使用者交代的是「把這幾件處理掉」這一件事。brief 逐條列出每個未解決 task 的
   seq、標題、原 brief 與最後回報，`files` 取聯集，分支照常是 `<整合分支>-t<seq>`。
3. **回到佇列**：那個 issue 的列改回 `working`（`ended_at` 清掉）、`teams.ended_at=NULL`、
   `teams.rescue_bot_id=<收尾者>`，phase → `starting`，並記一則 `team_rescue` note。
4. **scheduler**：
   - `rescue_bot_id` 有值時，該 issue 的執行者就是它一個人（`workers_for`）——上一批 dev 執行者早就
     retire 了。
   - `starting` 走 `rescue_startup`：PM / reviewer / 收尾者用 native session 重啟（起不來就
     `paused(member_failed:*)`，跟 §2.5.2 同樣的理由：不能為了一個啟動錯誤丟掉已交付的成果），
     然後直接 `working`——task 已經在表裡，沒有東西要規劃。PM 收到一則 `rescue` relay 說明現在的情況。
   - **自己不審自己**：`reported` 的 task 若執行者就是 reviewer，直接進 `merging`。要一個 bot 審查
     自己剛寫的 diff 沒有意義。
   - issue 收掉時清 `rescue_bot_id`，之後的 reopen 才會照常建新執行者。
5. 之後就是普通流程：回報 → （不是自己寫的才）審查 → 合併 → PM `done` → team 回到 `done`。

### 2.6b Retry：把失敗的 issue 重新排回佇列（2026-09-11）

佇列上的 `failed` 列帶著理由（`合併衝突無法自動解決`、`時間預算用完`）。那些不是決定，是**還沒做完的
工作**；使用者要的是接力做完，而不是把四個號碼再打一次進「追加 issue」。

`POST /teams/{id}/issues/retry-failed`：把每個 issue 號碼的**最後一次嘗試**中，狀態是 `failed` /
`skipped` 的那些重新排進佇列——就是 §2.3 的「再排同一個 issue」，只是號碼由 daemon 填。
一個號碼失敗過但重排後交付了，它是完成的、不會再被撿回來；一個號碼排三次失敗兩次也只回來一次。
另外記一則 `issues_retried` note，帶上每一次失敗的 `reason` 與分支，讓時間軸看得出這次重排是在接誰的力。

放行條件與 §2.5 reopen 完全相同（`done` 且未 cleanup 才會走 reopen 分支；執行中的 team 就只是排隊）。

## 3. 架構：daemon 內的 Team Scheduler

```
                       ┌──────────────────────────── daemon ────────────────────────────┐
  Web UI ──REST/WS──►  │  api.rs ──► team.rs::TeamScheduler（每個 active team 一個 tokio task） │
                       │                    ▲                │ prompt_grouped()（SPEC §6.3）       │
                       │   turn 完成事件（內部 broadcast）      ▼                               │
                       │  hookrecv.rs ──────┘         lifecycle.rs ──► herdr agent.prompt      │
                       │  (Stop hook → assistant Message)                                     │
                       │  git（run_on_host，同 github.rs）：worktree / branch / merge / gh pr   │
                       └──────────────────────────────────────────────────────────────────┘
```

- **Scheduler 是唯一會替 bot 說話的東西。** 它只做四件事：(1) 收 turn 完成事件、解析回覆末尾的 `am-team` 區塊；(2) 依狀態機決定下一則 relay 給誰；(3) 用既有 `prompt_grouped()` 送出（`client_request_id = team:<team_id>:<event_id>`，天然冪等）；(4) 跑 git。
- 每個 team 的排程是**單執行緒事件迴圈**（`tokio::sync::mpsc`，事件：`TurnDone{bot_id, turn_id, status}`、`RunChanged{bot_id}`、`User{action}`、`Tick`）。所有狀態轉移在同一個迴圈內、寫 DB 後才發 WS，避免兩個 worker 同時回報時互相踩。
- 內部事件來源：`lifecycle::emit_turn()` 之外加一個 `app.turn_bus`（`tokio::sync::broadcast`），hook 配對 / 備援 / watchdog / stop 把 Turn 標成非 `in_flight` 時都會發。**不走 WS**（WS 是給前端的，有 200 則環形緩衝，不可靠）。
- daemon 重啟：`teams.phase ∉ {done, aborted, failed}` 的 team 在 SPEC §6.1 步驟 3 對帳之後重建 scheduler（本文 §7.5）。

---

## 4. A. bot→bot 通訊

### 4.1 決定

**daemon 攔截回覆 + 結構化區塊協定**：每個成員的回覆（既有 hook 路徑拿到的 `last_assistant_message`）**最後一個** ```` ```am-team ```` fenced 區塊是給 daemon 的機器指令；
daemon 解析後決定下一步，並用既有的 prompt 路徑代發給收件人。人類可讀的部分照常進時間軸。

不做「PM 直接呼叫 daemon API」；「使用者在迴圈中」降級為可選的 **supervised 模式**（§4.6），不是預設。

### 4.2 理由
1. **零新能力**：回覆本來就 100% 經過 daemon（hook → `hookrecv::process_locked` → assistant Message），攔截點現成，遠端主機也照走反向通道。三種 kind 都不需要學會打 HTTP。
2. **單一編排者**：迴圈防護、預算、狀態機、稽核全在 daemon；agent 端沒有任何可以「繞過」的路。
3. **可觀察**：每一則 relay 都是普通 Turn + Message，時間軸、燈號、即時輸出（`turn_progress`）、終端備援全都直接適用。

### 4.3 否決的替代方案

| 方案 | 為何否決 |
|---|---|
| PM 透過工具呼叫 daemon HTTP API（`curl` + token） | UI token 要進 pane env，agent 可 `cat` 出來；`/api/*` 有 Host 檢查，遠端主機經反向通道會被 403（附錄 E 實測）；三種 kind 的沙箱 / 網路權限不一；agent 可無限呼叫、daemon 只能事後擋。可做為第二階段「輔助工具」（例如 `agents-managerd team status`），不做為主路徑。 |
| daemon 用 LLM / 啟發式從自然語言解析指令 | 解析失敗率高且不可預期；成本再加一層。結構化區塊讓失敗是**確定的**（JSON 壞了就是壞了），可用固定修復提示處理。 |
| 使用者在迴圈中（每步人工確認） | 這就是 SPEC §13 群組聊天已經能做的事；使用者要的是「叫出 team 來解決」。保留為 `supervised` 選項。 |
| 讓 agent 互相在同一個 pane 打字 / 讀對方 transcript | 違反每 Run 一筆 in-flight Turn；transcript 回補是第二階段（SPEC §4.2）尚未實作。 |

### 4.4 協定：`am-team` 區塊

fenced 語言標記固定 `am-team`，內容為**一個 JSON 物件**；daemon 只認回覆中**最後一個**這種區塊。依角色允許的 `action`：

| 角色 | action | 欄位 | 語意 |
|---|---|---|---|
| pm | `dispatch` | `tasks:[{to?, issue?, title, brief, files?[]}]` | 派工。**`to` 可省略**（2026-09-08）：省略時 daemon 派給空著的執行者，一次可派任意筆，超過併行數的排隊（§4.5）。寫了 `to`（成員暱稱如 `dev-1`）就指定那一位；他正忙時**排在他後面**，不再被拒。`to` 對不到人才拒。**`issue`（issue 號，2026-09-09）**：只有一個 issue 進行中時可省；無限模式（§4.5）下**必填**，對不到進行中的 issue 就拒那一筆並回報 PM |
| pm | `wait` | — | 目前沒事做，等回報 |
| pm | `done` | `summary`, `issue?`, `workers?: keep \| replace` | 宣告完成。daemon 檢查所有 task 已 `merged | skipped` 才接受，否則回 `reject`。`workers` 是 PM 對**下一個 issue**的決定：`keep` 沿用這批執行者（同 bot、同 worktree、上下文保留），`replace`（預設）換一批新的。**`issue`（2026-09-09）**：同 `dispatch`，只驗**那個** issue 的 task 全終態，也只交付那一個 issue，其他 issue 照跑 |
| pm | `ask_user` | `question` | 需要人 → team `paused(ask_user)`，UI 顯示問題，使用者用 §10.6 回覆後續跑 |
| pm | `abort` | `reason` | PM 認為做不了 → team `paused(pm_abort)`（不直接 abort，讓人決定） |
| worker | `report` | `status: done \| blocked`, `summary`, `notes?` | 工作回報。`done` 進審查；`blocked` 轉給 PM 決定 |
| reviewer | `verdict` | `result: approve \| request_changes`, `summary`, `must_fix?[]` | 審查結論 |

daemon 對每個 relay 都回一句**系統提示格式**（附錄 A），明說「回覆結尾必須有 am-team 區塊、允許哪些 action」。

`dispatch` 每筆必填的只有 `brief`（無限模式再加一個 `issue`）。`issue` 接受 `48` 與 `"#48"` 兩種寫法。

**解析失敗處理**（區塊缺失、JSON 壞、action 不合法、`to` 對不到人）：
1. 記 `team_events{kind:note, payload:{error}}`；
2. 送一則**修復提示**給同一個 bot：「你上一則回覆沒有有效的 am-team 區塊：<錯誤>。請只回傳該區塊。」（很短、很便宜）；
3. 同一個 relay 最多修復 **2 次**；仍失敗 → team `paused(protocol_error)`，UI 顯示原文讓人判斷。

`completed_fallback`（終端備援、可能不完整）的回覆一律先當「區塊缺失」走修復提示，因為那份文字本來就可能被截掉。
**例外（2026-09-08）**：備援擷取到的是**還在跑的畫面**（codex 的 `• Working (4s • esc to interrupt)`、claude 的
`✢ Baking…`、`Running …`）時，那不是回覆而是進度——不送修復提示（會打進正在跑的回合）、也不算一次修復；
記一筆 `note{action: fallback_busy}` 等 hook 把真正的回覆送來。#50 實測：兩個 codex 執行者都因此被算了兩次而整隊
`paused(protocol_error)`，其實回覆在 36 秒後就到了。

**使用者插話後的回覆（2026-09-08）**：成員回覆使用者插話（`kind=user` 的 turn）本來一律不解析（不能因此吃到修復提示）。
現在**若那則回覆結尾有合法的 am-team 區塊就照常套用**——回報漏掉時，使用者只要在群組聊天 `@dev-2 請再 report 一次`
就能把隊伍救回來；沒有區塊或區塊不合法仍當一般對話，不修復、不計次。

**重複的 `report`（2026-09-09）**：task 已經不在執行者手上（`reported` / `reviewing` / `merging`）時又收到同一位的
`report{done}`——hook 晚到、使用者 nudge、修復提示在第一則已經通過之後才被回答——一律當作重送：記
`note{duplicate_report}`、不改狀態、不再送審一次（#48 t21 實測 reviewer 收到同一回兩次）。

**`report` 的寬鬆解析（2026-09-08）**：執行者常把欄位包在 `report` 物件裡、用自己的字眼當 `status`。daemon 接受
`{"action":"report","report":{…}}` 的巢狀寫法；`status` 同義詞 `completed / complete / finished / success / ok → done`、
`stuck / failed / need_help → blocked`；沒有 `summary` 時把其餘欄位序列化當摘要給 PM。派工 relay 也改成直接印出固定格式的區塊。

### 4.5 防無限迴圈（多層，缺一不可）

| 層 | 機制 |
|---|---|
| **拓撲** | 星狀：PM↔worker、PM↔reviewer、reviewer→worker（只有 `request_changes` 這一種）。worker 之間、worker→reviewer 不存在路徑；daemon 只轉送狀態機允許的邊。 |
| **狀態驅動** | relay 不是「A 說完就叫 B」，而是「事件讓某個 task 換狀態，狀態決定要送什麼」。同一個 task 在同一狀態下不會重複送同一種 relay。 |
| **回合預算** | `budget.max_relays`（預設 40）：所有 relay（含修復提示）計數；到頂 → `paused(budget_relays)`。使用者插話不計。無限模式下每個 relay 都被戳上「第一個 working 的 issue」，按 issue 分不出來，因此上限改成 `max_relays × 已開工的 issue 數`——同樣是「每個 issue 一份預算」的意思。 |
| **審查回合** | 每個 task `budget.max_review_rounds`（預設 2）：`request_changes` 第 N+1 次 → task `exhausted`，team `paused(review_exhausted)`，由人決定強制合併 / 跳過 / 再給一回合。 |
| **併行數**（2026-09-08，原「派工上限」） | `workers.count`（1–4，預設 1）是**同時能跑幾個 task**，不是 PM 要自己分配的人頭。每個執行者同時最多 1 個未完成 task（DB 的 `team_tasks_one_open_per_worker` 保證），所以併行數 n = 最多 n 筆同時在跑。PM 可以隨時 `dispatch`，不必等前一批做完：多的進佇列，跑完一筆補一筆。**同一個 issue 內出現第二次完全相同的 `brief`**（不分執行者）→ `paused(pm_repeat)`。 |
| **時間** | `budget.max_wall_clock_min`（預設 120）：從當前 issue 的 `started_at` 起算，**扣掉暫停的時間**（從 phase 事件加總；2026-09-09 前不扣，隔夜的 `quota_low` 一 resume 就撞 `budget_time`），到頂 → `paused(budget_time)`。 |
| **額度** | 每次送 relay 前查 `quota::get(kind)`：任一週期 `used_pct ≥ budget.quota_stop_pct`（預設 90）→ `paused(quota_low)`；額度沒資料時不擋；**`quota_stop_pct = 100` 視為關掉額度檢查**（2026-09-09，UI 的「無視額度繼續」就是 PATCH 成 100 再 resume，建 team / reopen 的預檢同樣不擋）。**暫停要指名道姓**（2026-09-09）：`quota_low` 一律附 `pause_detail`（見下），寫明是哪個成員、哪個身分、哪個視窗、剩多少、幾點 reset。 |
| **停頓不是中止** | 所有上限都只 `paused`，成員 pane 還活著；使用者 `PATCH budget` 後 `resume`。中止只有人能按。 |

**`pause_detail`（2026-09-09 新增）**：`pause_reason` 是機器碼，`quota_low` 這一種光看碼不知道要去處理誰——
一隊有 pm / dev-1 / dev-2 / rev，各自可能掛在不同身分（`cc0` / `cc2` / codex）上。daemon 因此在暫停的同時
把當下的額度快照寫進 `teams.pause_detail_json`，`team_json` 與 `team_changed` 以 `pause_detail` 送出：

```json
{"stop_pct": 90,
 "members": [{"bot_id": "01M1…", "name": "ttxka1d-i2-rev", "short": "rev", "role": "reviewer",
              "kind": "claude", "identity": "cc2", "host": "local",
              "window": "five_hour", "used_pct": 96.0, "remaining_pct": 4.0,
              "resets_at": "2026-09-09T03:20:00Z"}]}
```

- `members` 是**當下所有**過線的成員，`used_pct` 由大到小；`window` 是該成員最接近上限的那個視窗
  （`five_hour` / `seven_day`，即 `GET /api/quota` 的欄位名）。一次列全部的理由是：只寫一個的話，
  使用者處理完那一個按「繼續」，下一秒又停在同一個原因上。
- 其他 `pause_reason` 目前都是 `null`（碼本身已經說完了，例如 `member_lost:dev-1`）。
- **每次 phase 變動都重寫**這個欄位，所以 `resume` 之後不會留下上一次暫停的細節。
- 舊 daemon 沒有這個欄位 → 前端退回只寫「已暫停：額度過低」的舊文案。
- UI：TeamPanel 的黃色橫幅寫「已暫停：rev（cc2）5h 額度剩 4%，03:20 才 reset。」，兩位以上寫最嚴重的
  那一位 + 「（另有 N 位額度也不足）」，完整名單在 `title`；側欄那一列太窄，只把名單放進 tooltip。

### 4.6 supervised 模式

`teams.supervised = 1` 時，三個閘門各停一次：`gate:dispatch`（PM 派工後、送給 worker 前）、`gate:merge`（審查通過後、`git merge` 前）、`gate:deliver`（開 PR / 推分支前）。停在 `paused(gate:*)`，UI 顯示即將發生的事，`POST /teams/:id/approve` 放行。預設關（§12）。

---

## 5. B. Team 住在哪裡

### 5.1 決定

**Team 是 Project 底下的一層新實體；成員是該 Project 的普通 Bot。** 不開新 Project。

### 5.2 理由
- Project 主鍵是 `(host, path)`；一個 issue 一個 Project 會直接撞唯一索引，就算用 worktree 路徑當 Project path 繞過去，也會讓 sidebar / TOML 塞滿一次性 Project，而且 PM 要住哪個 Project 說不清。
- 成員是普通 Bot ⇒ `start / stop / prompt / hook / 對帳 / 燈號 / 終端快照 / blocked 面板 / 群組時間軸`全部零改動可用。`GET /projects/:id/messages` 天然就是 team 的時間軸（多加 `team_id` 過濾）。
- 多個 team 共存：靠 `team_id` 區分分支名與 worktree 目錄，不靠 Project。

### 5.3 否決的替代方案

| 方案 | 為何否決 |
|---|---|
| 每個 issue 一個新 Project（path = worktree） | 撞 `(host,path)` 唯一；PM 沒有自然歸屬；TOML 與 sidebar 被一次性物件污染；Project 刪除語意（不關 workspace、不刪目錄）跟臨時物件相反。 |
| Team 成員寫進 TOML `[[projects.bots]]` | TOML 是「使用者期望設定」的權威（SPEC §3.1）；team 成員是 daemon 的執行期產物，寫進去使用者手改 TOML 時會看到一堆莫名 bot、而且 SPEC §6.4 DELETE 流程要跟著改。改為：`managed_by='team'` 的 bot **不受 TOML→SQLite 投影管轄**（投影的「TOML 沒有的 bot 標 deleted_at」加 `WHERE managed_by='user'`）。這是投影邏輯唯一的特例。 |
| 用 herdr 的 `worktree.create`（schema 有 `worktree.create/open/list/remove`） | 本專案未實測；它會另開 workspace（打亂「一 Project 一 workspace」），遠端主機也要另走 ssh。第一階段用 daemon 自己跑 `git worktree`（`run_on_host` 已有 `github.rs` 前例）；herdr worktree RPC 列為第二階段評估。 |

### 5.4 對既有規則的影響
- SPEC §13 群組聊天：team 成員也是群組成員，使用者可以 `@dev-1` 直接對它說話（會被記為 `kind:user` 事件，不計 relay 預算，但 scheduler 會等它這一回合結束再送 relay）。
- SPEC §6.4 DELETE Project：需所有 Bot 已停 → 加「無 active team」。
- 對帳（SPEC §6.5）：agent_name 由 `bot.id` 推得，team 成員自動被收養，不需改。

---

## 6. C. 工作樹隔離與整合（最高優先）

### 6.1 硬性保證
1. **使用者的 checkout 不被碰**：daemon 與所有成員都不在 `project.path` 的主工作樹執行任何寫入 git 操作。
2. **一個 worker 一個 worktree**；同一時間一個 worktree 只有一個 agent。
3. **合併由 daemon 用 git 做**（確定性），不是由 LLM「幫忙合一下」。
4. **衝突由寫那段程式的人解**：merge 失敗 → 該 worker 在**自己的** worktree 上 rebase 到整合分支並解衝突 → daemon 再試。

### 6.2 目錄與分支佈局

**決定：worktree 放在 repo 外，daemon 資料目錄底下**（`AM_DATA_DIR`，預設 `~/.config/agents-manager`；遠端主機用該主機的家目錄，與既有 `bots/<bot_id>/` hook 材料同一個地方）。
使用者的主 checkout 裡**不新增任何檔案**。

```
<project.path>/                                   使用者的主 checkout：工作樹不碰、不新增檔案
<project.path>/.git/worktrees/<name>/             git 自己的 worktree 登記項（見下方「git 元資料」）

<data_dir>/teams/<team_id>/                        team 根目錄（= teams.worktree_root）
    ISSUE.md                                      daemon 寫入：issue 標題 / 連結 / 全文（gh issue view）
    TEAM.md                                       daemon 寫入：成員名單、各自路徑與分支、協定摘要（附錄 A）
    main/            ← worktree，分支 team/i<issue>-<tid6>        整合分支；PM 的 cwd；daemon 在這裡 merge
    dev-1/           ← worktree，分支依 task 切換                 worker 1 的 cwd
    dev-2/           ← worktree                                   worker 2 的 cwd
    reviewer/        ← worktree，detached HEAD                    reviewer 的 cwd；審查哪個 task 就 checkout 哪個分支
    <每個 worktree>/.agents-manager/team/{ISSUE.md, TEAM.md, .gitignore}   ← 同兩份檔案的副本，在成員 cwd 內；.gitignore = "*"
```

| 項目 | 規則 |
|---|---|
| `tid6` | `team_id` 尾 6 碼小寫（與 SPEC §2 agent_name 的 hash 同法） |
| 整合分支 | `team/i<issue>-<tid6>`，從 `base_sha` 建立。`base` 預設 = 建 team 時主 checkout 的 `HEAD`（記 `base_ref` / `base_sha`）；使用者可改成任何 ref。同一隊**第 n 趟**做同一個 issue（§2.3「再排同一個 issue」，n ≥ 2）用 `team/i<issue>-<tid6>-r<n>`，因為前幾趟的分支還在 |
| task 分支 | `team/i<issue>-<tid6>/t<seq>-<worker暱稱>`，**派工當下**從整合分支 HEAD 建立（後派的 task 自動包含先前已合併的工作） |
| worker worktree | 建 team 時 `git -C <project.path> worktree add --detach <data_dir>/teams/<id>/dev-1 <base_sha>`；派工時 `git -C <wt> checkout -b <task branch> <integration HEAD>`（worktree 固定、分支隨 task） |
| reviewer worktree | 建 team 時 `--detach`；送審時 daemon 先 `git -C <wt> checkout --detach <task branch>`，讓 reviewer 能在該分支上跑 build / test |
| 同一分支不能被兩個 worktree 同時 checkout（git 限制） | 整合分支只在 `main/`；reviewer / worker 一律 detached 或自己的 task 分支 |
| `ISSUE.md` / `TEAM.md` | 正本在 team 根目錄；每個 worktree 內另放一份於 `<wt>/.agents-manager/team/`，同目錄寫 `.gitignore` 內容 `*`（由 `team.rs` 寫，**與 `attach.rs` 無關**：`attach.rs` 那份只蓋 `<project.path>/.agents-manager/attachments/`，不需改動）。副本在成員 cwd 內，agent 讀取不需跨目錄；被 ignore 所以 worktree 的 `git status` 乾淨、daemon 的 `git add -A` 也不會把它 commit 進 task 分支 |

**git 元資料（`.gitignore` 管不到的部分）**：`git worktree add` 會在主 repo 的 `.git/worktrees/<name>/` 建登記項（`<name>` 取路徑最後一段，撞名時 git 自動加序號），`git worktree list` 會列出所有 team worktree。
這是**可接受的**：它在 `.git/` 內、不出現在工作樹、不會被 commit、`git status` 看不到；反而讓使用者能用 `git worktree list` 找到 team 的所有 checkout。代價是 cleanup **必須**走 `git worktree remove` + `git worktree prune`（§6.5），
直接 `rm -rf` 資料目錄會留下孤兒登記項（無害，但 `git worktree list` 會顯示 `prunable`，且同名路徑要 `worktree add -f` 才能再用）。對帳時 daemon 對每個有 team 的 project 跑一次 `git worktree prune` 收尾。

**為何放 repo 外（與「放 `<project.path>/.agents-manager/teams/` 內」的比較）**：

| 面向 | repo 內（否決） | repo 外（採用） |
|---|---|---|
| 主 checkout `git status` | 目錄本身不被 ignore（`attach.rs` 的 `*` 只在 `attachments/` 那一層）；要靠 daemon 另寫 `<project.path>/.agents-manager/.gitignore`，且使用者的全域 `core.excludesFile`、`git clean -fdx` 都能繞過或**直接刪掉正在跑的 worktree** | 主工作樹零新增檔案，乾淨是**構造上**保證的，不依賴任何 ignore 規則 |
| nested worktree | 四份完整原始碼樹住在專案目錄裡：編輯器索引、`rg`（若專案本身沒 ignore 規則）、備份工具、專案自己的 build（例如 Rust workspace glob、`npm workspaces`）都可能掃到 | 專案目錄內看不到任何副本 |
| agent 沙箱 / 路徑核准 | 沒有優勢：核准是以 **cwd** 為準，成員的 cwd 是它自己的 worktree，跟 worktree 放哪無關。`attach.rs` 把附件放 cwd 內的理由是「附件要給住在 `project.path` 的 bot 讀」，對 team 成員反而是**兩種佈局都在 cwd 外**（見下） | 同左；`ISSUE.md` / `TEAM.md` 靠 worktree 內副本解決 |
| 新目錄 trust 提示（Claude / grok 的 folder trust） | 「子目錄繼承父目錄信任」對三種 CLI 都**未驗證**，不能當理由 | 一樣可能出現 → 走既有 blocked 面板（§7.5）；第二階段可對 grok 成員設 `GROK_FOLDER_TRUST=0`（SPEC 附錄 F.4） |
| 遠端主機 | 路徑跟著 project.path 走 | 路徑跟著 `bots/<id>/` 走（`remote_bot_dir` 已有同樣的家目錄展開機制） |
| 清理 | 刪一個目錄 + prune | 同樣刪一個目錄 + prune |
| 使用者找得到嗎 | 就在專案裡 | UI 標題列顯示 `worktree_root` 並可複製；`git worktree list` 也列得出 |

**已知限制**：使用者對 team 成員拖圖片時，`attach.rs` 仍把檔案放到 `<project.path>/.agents-manager/attachments/`（在成員 cwd 之外）。三種 kind 的 `auto_approve` 在本 UI 恆開（v3.8），實測上不會卡核准；正式做法是第二階段讓 `attach.rs` 改用 `bot.cwd`。**這個限制兩種佈局都有**，不是選邊的理由。

### 6.3 Task 的 git 生命週期

```
派工       daemon: git -C dev-1 checkout -b team/i42-k3f9x2/t1-dev-1 team/i42-k3f9x2
           relay → dev-1（brief + 路徑 + 分支 + 規則：只在此目錄工作、要 commit、不要 push、不要碰 ../）
回報       worker: report{done}
           daemon: git -C dev-1 status --porcelain 非空 → 自動 `git add -A && git commit -m "wip(dev-1): uncommitted at report"`
                   git rev-list <integration>..<task branch> 為 0 個 commit → 視為「沒做事」→ 修復提示（算 relay）
送審       daemon: git -C reviewer checkout --detach <task branch>
           relay → reviewer（task brief + `git diff <integration>...HEAD` 指引 + 規則：唯讀、不 commit）
審查通過   daemon: git -C main merge --no-ff --no-edit <task branch>
             成功 → task merged（記 merge_sha）→ 通知 PM（併入下一批 report）
             衝突 → git -C main merge --abort → task `rebasing`
                    relay → worker：「整合分支已前進，請在你的 worktree 執行 git rebase <integration>，解衝突、確認可建置後回報」
                    worker report → 再 merge；rebase 最多 2 次 → paused(merge_conflict)
打回票     relay → worker（must_fix）；worker 在同一分支繼續 commit；再 report → 再送審（round+1）
```

- **合併順序 = 審查通過順序**（單一整合佇列，reviewer 本來就一次只審一個）。
- daemon 每次 merge 前檢查 `git -C main status --porcelain` 必須乾淨；不乾淨（PM 手癢改了東西）→ `paused(integration_dirty)`，UI 顯示 `git status`。
- 派工時 daemon 檢查 `tasks[].files` 是否與其他進行中 task 重疊 → 只**警告**（寫進 PM 下一則 relay：「注意：t2 與 t1 都列了 src/api.rs」），不擋。PM persona 明說「請以檔案 / 模組切分，避免兩人改同一檔」。
- worker 的 persona 明說：cwd 就是你的 worktree、不得 `cd ..`、不得動 `../main` / `../dev-2`、不得進入 `<project.path>`（使用者的 checkout）、不得 `git push`、不得改分支、每個邏輯段落 commit。這些是「指示」不是「強制」；強制層是 worktree（改錯地方也只汙染自己的 worktree）與 daemon 只合併它建的那條分支。

### 6.4 交付（`teams.deliver`）

| 值 | 行為 | 何時 |
|---|---|---|
| `branch` | 整合分支留在 repo，team 摘要告訴使用者分支名；不 push | 永遠可用 |
| `pr` | `git push -u origin <integration>` → `gh pr create --base <base_ref 對應的遠端分支> --title "<issue title> (#n)" --body "<PM summary>\n\nCloses #n"` → 記 `pr_url` | `project.github` 非 null 且 `gh` 可用；否則自動降為 `branch` 並註記 |

不提供「合進使用者目前分支」——那會碰主 checkout，違反本文 §6.1 第 1 條。使用者想要就自己 `git merge team/...`。

### 6.4a Team 自己的 herdr workspace（2026-09-06 使用者要求）

**每個 team 開一個獨立的 herdr workspace**，成員的 pane 都 split 在它裡面，`teams.workspace_id` 記錄。

- 建立時機：解出 `base_sha`、建好 worktree 之後、建成員 bot 之前。路徑用 team 根目錄 `<data_dir>/teams/<team_id>/`。
- 為什麼不共用 Project 的 workspace：一個 team 會塞進 4+ 個 pane，混進使用者平常在看的 workspace 裡會把它擠爆，而且 team 是**臨時**的、Project 是常駐的，生命週期不同。分開之後「關掉整個 team」就等於關掉一個 workspace，乾淨。
- 這是 SPEC §2「一個 Project 對應一個 workspace」的**延伸而非違反**：Project 的 workspace 照舊，team 的是另外一個，只是它的擁有者是 team 而不是 Project。
- workspace 建不出來 → 整個建 team 失敗並走 §6.5 的 cleanup，不要退回去用 Project 的 workspace（那會把臨時 pane 留在使用者的工作區裡）。
- 清理 / 刪除時關掉它（見 §6.5、§6.5a）。對帳發現 workspace 不存在 → `paused(workspace_missing)`，比照 `worktree_missing`。

### 6.5 清理
- `POST /teams/:id/cleanup`（team 在終態時）：停成員（若還活著）→ 對每個 worktree `git -C <project.path> worktree remove --force <path>`（被鎖住時再加一次 `--force`）→ `git -C <project.path> worktree prune` → 刪 `<data_dir>/teams/<id>/`（此時只剩 `ISSUE.md` / `TEAM.md`）→ 成員 bot 標 `deleted_at`（訊息保留，同 SPEC §6.4）。**分支一律保留**（便宜、可追溯；使用者自己刪）。順序很重要：先 `remove`/`prune` 再刪目錄，否則 `.git/worktrees/` 留下孤兒登記項（§6.2「git 元資料」）。
- 建 team 失敗（任何一步）走同一個 cleanup，避免留下半套 worktree。
- 對帳時（SPEC §6.5）對每個有 team 的 project 跑 `git worktree prune`，收掉目錄已被人手動刪除的登記項；若某個 active team 的 worktree 目錄不見了 → `paused(worktree_missing)`。

### 6.5a 刪除（2026-09-06 使用者要求）

`cleanup` 是「收掉現場但**留下紀錄**」——`teams` 這一列還在，UI 仍看得到這個 team 與它的時間軸。使用者要的是能把它整個移除。

**`DELETE /api/teams/:id?branches=keep|delete`（預設 `keep`）**

- **任何 phase 都可以刪**，不像 `cleanup` 只限終態。非終態時等於「先 abort 再刪」：停所有成員 → 走 §6.5 的 worktree 清理 → 關掉 team 的 workspace（§6.4a）→ 刪 `team_events` / `team_tasks` / `teams` 三張表的列 → 成員 bot 標 `deleted_at`。
- **成員的對話訊息保留**（同 SPEC §6.4 刪 Bot 的既有語意）。刪掉的是 team 這個容器與它的排程紀錄，不是使用者與 agent 講過的話。
- **分支預設保留**。`?branches=delete` 才會 `git branch -D` 掉整合分支與所有 task 分支——**唯一會銷毀工作成果的路徑**，UI 必須明示「已合併的內容也會消失」並要求二次確認。已經 push 過的遠端分支**一律不動**（那是對外的東西，只有人能決定）。
- 冪等：team 不存在回 `404 {"error":"not_found","what":"team"}`；重複刪不報錯。
- 刪除中推 `team_changed`（phase `deleting`）→ 完成推一則帶 `deleted: true` 的 `team_changed` 與 `bot_changed` ×N，讓前端把節點移掉。

> `cleanup` 與 `delete` 並存，語意不同：`cleanup` 收現場、留紀錄（可以事後翻 task 與 relay）；`delete` 連紀錄一起移除。UI 兩個都要有，`delete` 走 `ConfirmDialog`。

### 6.6 遠端主機
所有 git / gh 指令走 `github.rs::run_on_host`（本機 `/bin/sh -c`、遠端 `ssh_exec_path`）。`<data_dir>` 在遠端 = 該主機的 `~/.config/agents-manager`（與 `lifecycle::remote_bot_dir` 同一套家目錄展開）。`ISSUE.md` / `TEAM.md` 與各 worktree 內的副本用 `hosts.rs::ssh_put` 寫入。**第一階段只做本機**（§13）。

---

## 7. D. 角色與生命週期

### 7.1 角色與併行數

| 角色 | 數量 | cwd | 可設欄位 | persona 來源 |
|---|---|---|---|---|
| `pm` | 恰 1 | `<data_dir>/teams/<id>/main` | kind, model, effort, fast, identity, `persona_extra` | daemon 產生的角色人設（附錄 A.1）+ `persona_extra` |
| `worker` | **併行數** `0` = 無限 / 1–4（預設 1） | `<data_dir>/teams/<id>/dev-<n>` | 同上（同一組設定套用到所有 worker；第二階段可逐人不同） | A.2 |
| `reviewer` | 0–1（預設 1） | `<data_dir>/teams/<id>/reviewer` | 同上 | A.3 |

「併行數」是使用者面對的名字，`workers.count` 是 API 欄位名（不改，相容）。它決定同時能跑幾個 task，
而不是 PM 要自己分配的人頭：daemon 一樣建 n 個執行者 bot（`dev-1`…`dev-n`），但誰做哪一筆由 daemon 決定（§4.5）。

**`0` = 無限（2026-09-09）**：佇列裡有幾個 issue 就同時做幾個（§2.3「多 issue 同時進行」），每個 issue 先 1 個執行者、
PM 派多少就開多少（每 issue 最多 4、全隊最多 12）。硬上限是 pane 與額度的天花板，不是預算——額度會用得很快。
`0` 是這個欄位的新值，1–4 的意思一個字都沒變。

persona 走既有 `bots.persona` → `--append-system-prompt` / `--rules` / `developer_instructions`（API.md §12.8），三種 kind 都有落點。**這是 `persona` 欄位正式進 SPEC 的時機**（本文 §12 第 9 項）。

### 7.2 臨時 vs 常駐

**臨時。** 成員 bot 在建 team 時建立、team 結束後停掉、cleanup 時軟刪除。理由：persona 與 cwd 都綁著這個 issue；常駐會讓 bot 清單隨 issue 數線性膨脹；重用又要處理「上次 team 留下的對話脈絡」。使用者想留著某個成員，cleanup 前可按「轉為一般 bot」（`PATCH /bots/:id {managed_by:"user"}`，會寫進 TOML、cwd 清空 → 第二階段）。

### 7.3 命名

| 東西 | 規則 | 例 |
|---|---|---|
| bot 暱稱（專案內唯一） | `i<issue>-pm`、`i<issue>-dev-<n>`、`i<issue>-rev`；撞名（同 issue 開第二個 team）加 `-<tid6>` | `i42-pm`、`i42-dev-1`、`i42-rev` |
| am-team 區塊裡的 `to` | 暱稱去掉 `i<issue>-` 前綴的短名 | `dev-1` |
| herdr agent name | 不變：`<project slug>-<bot id 尾 6 碼>` | `agents-manager-k3f9x2` |
| 整合分支 / task 分支 | 本文 §6.2 | `team/i42-k3f9x2/t1-dev-1` |
| worktree 目錄 | 本文 §6.2 | `~/.config/agents-manager/teams/<team_id>/dev-1` |

### 7.4 啟動與停止

- **建 team**（`POST /projects/:id/teams`，同步部分）：驗證（git repo、issue 存在、kind 已安裝、額度未低於 `quota_stop_pct`）→ **先 `git rev-parse` 解出 `base_sha`、算出 `worktree_root`** → 寫 `teams`（`phase=starting`）→ 建 worktree → 建成員 bot → 回 `{team_id}`。
  - ⚠️ `base_sha` / `worktree_root` 是 `NOT NULL`，所以**必須在寫 row 之前就解出來**（附錄 C 的序列即為此）。早期版本把「寫 row」排在解析之前，兩節不一致，以此處為準。worktree 目錄本身可以在寫 row 之後才建 —— 路徑是算出來的，不需要先存在。之後**背景**逐一 `start_bot`（沿用 SPEC §6.2；每個 60 秒上限）。
  - PM 起不來 → `failed` + cleanup。worker 部分起不來 → 少一個人繼續（≥1 即可），記 note。reviewer 起不來 → `paused(member_failed)`，人決定「不審直接合」或重試。
  - 全部就緒 → `planning`，送 PM 第一則 relay（附錄 A.1）。
- **停止**：`done / aborted / failed` 時 daemon 對所有成員 `stop_bot`（SPEC §6.4）。成員 pane 不會在 team 還活著時被 daemon 自動停。
- **使用者手動停某個成員**（既有 `POST /bots/:id/stop`）：scheduler 收到 `RunChanged` → 該成員相關 relay 留在 pending → `paused(member_lost:<name>)`。使用者重新 `start` 該 bot 後按 `resume`；暫停橫幅上的「啟動並繼續」（2026-09-10，`TeamMemberLost`）會把沒在跑的成員全部 `start`、都有 run 之後自動 `resume`，仍是人按的。**不自動重啟**（維持 §13「絕不自動啟動」的精神；自動重啟會讓額度在無人看管下持續消耗）。

### 7.5 異常與恢復

| 情況 | 行為 |
|---|---|
| daemon 重啟 | SPEC §6.1 對帳收養成員 Run（agent_name 由 bot id 推得）→ 重建 scheduler → 對每個成員：有 in-flight Turn 就等它（hook 晚到仍可配對，SPEC §6.7）；沒有就看 `team_events` 的 pending relay 重送（冪等鍵相同）。任何成員 Run 被判 `exited` → `paused(member_lost)`。 |
| 成員 crash（`pane.exited`） | 既有 SPEC §6.6 把 Run `exited`、in-flight Turn `failed` → scheduler `paused(member_lost)`。 |
| 成員 `blocked`（trust 提示、權限詢問、codex 升級選單） | `paused(member_blocked:<name>)`；暫停橫幅上有「回應 <成員>」按鈕（2026-09-10，`TeamMemberBlocked`），就地彈出該成員的整張終端畫面送鍵；狀態離開 blocked 時 **自動 resume**（唯一會自動 resume 的原因，因為它不是預算問題）。2026-09-10 起由 daemon 做（`team::resume_if_member_unblocked`，掛在 `pane.agent_status_changed` 的 blocked→非 blocked 邊緣），所以在哪裡回完提示都一樣；開機對帳時也補一次（那個邊緣可能發生在 daemon 沒開的時候）。UI 那顆按鈕仍會在自己開的視窗裡補送 `resume`，重複的 `resume` 只會拿到 409。 |
| relay `delivery=unknown` | `paused(delivery_unknown)`；使用者 `abandon` 後 `resume`，scheduler 重送同一 relay（冪等鍵相同 → 既有邏輯會回同一 turn；因此重送用新的 `event_id`）。 |
| Turn `failed`（stall watchdog、interrupt） | 修復提示規則同 §4.4（算一次），2 次後 `paused(protocol_error)`。 |

team 狀態全部在 DB（§2.1），記憶體只有 scheduler 的 mpsc 與計時器。

### 7.6 換成員 kind = 換 bot（2026-09-08）

`PATCH /teams/{id}` 的三個角色（§10.5）都能改 `kind`，但 **`kind` 不是一個可以就地改的欄位**：
成員跑哪一個 CLI 是 `agent.start` 開 pane 時決定的，把 `bots.kind` 改掉只會讓那一列跟正在跑的
行程對不上（模型清單、hook 寫法、身分規則全部跟著 kind 走）。所以 daemon 換一個 bot：

1. **先擋**：該角色的任一成員有 in-flight turn → `409 {"reason":"member busy", "role", "bot_id", "name"}`。
   把成員從一個跑到一半的 turn 底下抽掉，等於把整隊在等的那則回覆丟掉；請使用者先暫停。
2. **停掉並軟刪舊 bot**（`deleted_at`，訊息與對話保留——那是這一隊的紀錄）。
3. **同名、同 cwd、同 `team_role` 建一個新 kind 的成員**（`insert_member`，接著補上 `full_persona`）。
   同名是重點：協定短名（`pm` / `dev-1`）就是路由鍵，改名等於換一個座位。
4. **未終態的 `team_tasks.worker_bot_id` 指到新 bot**（`team_tasks_one_open_per_worker` 是對 bot 唯一的，
   舊 bot 留著一件沒結的 task 會把這個座位永遠卡住）。
5. **`roles_json.<role>` 寫成新的 spec**，下一批執行者、reopen 之後重建的成員都照它。
6. **啟動新成員**（pretrust + `start_bot`）。沒有 run 的成員在 PM 下一次派工時就是 `member_lost`，
   而 `resume` 本來就要求每個成員都在跑。啟動失敗記一筆 `member_start_failed`，不讓整個 PATCH 失敗。

**phase 一律不動**：`paused` 的隊伍維持 `paused`，由使用者按「繼續」；跑動中的隊伍就帶著新成員繼續。

---

## 8. E. 流程狀態機

### 8.1 Team phase

```
starting ──成員全部 running──► planning ──PM dispatch──► working ──PM done（全部 task 終態）──► finishing ──交付完成──► done
   │                              ▲                        │  ▲                                     │
   │ PM 起不來                     │ 所有 task 終態、PM 尚未 done│  │ 新 dispatch                          │ push / gh 失敗
   ▼                              └────────────────────────┘  │                                     ▼
 failed                                                       │                              paused(deliver_failed)
                                                              │
 任一 phase ──(預算 / 額度 / 協定錯 / 成員問題 / ask_user / gate)──► paused(reason) ──resume──► resume_phase
 任一非終態 phase ──使用者 abort──► aborting（停成員）──► aborted
```

- `paused` 保存 `resume_phase`；`resume` 回到原 phase 並重送 pending relay。
- `finishing`：deliver=`pr` 時 push + `gh pr create`；`branch` 時只寫摘要。成功 → `done`（停成員）。
- 終態：`done | aborted | failed`。終態後只剩 `cleanup`——**例外**：`done` 且未 cleanup 的 team 可由使用者追加 issue 而 reopen，
  走 `done ──追加 issue──► starting ──成員重啟、建新執行者──► planning`（§2.5）；`aborted` / `failed` 沒有這條路。

### 8.2 Task state

```
queued ──relay 送達──► working ──report{done}──► reported ──(有 reviewer)──► reviewing ──approve──► merging ──merge ok──► merged
                          ▲                        │                          │                       │
                          │ rework relay            │ (無 reviewer)            │ request_changes        │ conflict
                          │                        ▼                          ▼                       ▼
                          │                     merging               changes_requested ──round<max──► working
                          │                                                   │ round=max                │
                          │                                                   ▼                          ▼
                          │                                             exhausted ──使用者 decide──► working | merging | skipped
                          │                                                                           rebasing ──report──► merging（≤2 次）
 report{blocked} ──► blocked_by_worker ──PM 下一則 relay 決定：改派 / 補充 brief（→ working）/ skip
```

- `queued` 有兩種（2026-09-08）：`worker_bot_id IS NULL` = 還在佇列裡等併行位，分支**還沒切**；
  有 `worker_bot_id` = 已經派給某個執行者、relay 還沒送達。前端的 task 列把前者顯示成「排隊中」。
- 終態：`merged | skipped | failed`。
- `round` 從 0 起算；`request_changes` 讓 `round += 1`；`round == max_review_rounds` 時再收到 `request_changes` → `exhausted`。
- 沒有 reviewer（`reviewer: null`）：`reported → merging` 直接整合。

### 8.3 誰判定完成
- **task 完成**：reviewer `approve`（或無 reviewer）且 daemon merge 成功。不是 worker 說 done 就算。
- **team 完成**：PM 發 `done` **且** daemon 驗證所有 task ∈ 終態。PM 在所有 task 終態後若只回 `wait`，daemon 送一則「所有 task 已合併，請 `done` 或再 `dispatch`」（算 relay）。
- PM 的 `done.summary` 成為 PR body / team 摘要。

### 8.4 PM 收件箱與「每 Run 一筆 in-flight」
PM 同時只能收一則 prompt，但兩個 worker 可能幾乎同時回報。規則：
- 給 PM 的 relay 一律先進 `team_events(pending)`；PM 一 idle（該 bot 無 in-flight Turn）就把**所有 pending** 合併成**一則** prompt（「以下是 N 則回報：…」）送出。
- 給 worker / reviewer 的 relay 同理逐 bot 排隊；reviewer 一次只審一個 task（送審順序 = 回報順序）。
- 使用者用群組聊天 `@i42-pm` 插話時，scheduler 讓路：等該 Turn 結束再送 pending。
- 這就是 SPEC §13.3 的 `in_flight` 情況在 team 裡的處理：**排隊，不略過**。

---

## 9. F. 失敗與成本

### 9.1 送不到成員：不用 `skipped`，改用「排隊或暫停」

| SPEC §13.3 的 reason | team 的處理 |
|---|---|
| `in_flight` | 排隊（§8.4） |
| `blocked` | `paused(member_blocked)`，離開 blocked 自動 resume |
| `not_running` / Run 非 running | `paused(member_lost)`；不自動啟動 |
| `unknown_delivery` | `paused(delivery_unknown)` |
| `conflict` / `upstream` | 重試 1 次（5 秒後）→ `paused(upstream)` |

理由：群組聊天少一個人只是少一個回答；team 少 PM 整個流程停擺、少 reviewer 合併沒人把關。「略過」在這裡等於默默降級，不如明確停下來讓人看。

### 9.2 成本

- **預算物件**（建 team 時給，`PATCH` 可加碼）：
  ```json
  { "max_relays": 40, "max_review_rounds": 2, "max_wall_clock_min": 120, "quota_stop_pct": 90 }
  ```
- **用量物件**（`usage_json`，UI 畫進度）：`{ relays, review_rounds_total, elapsed_min, per_bot: {<bot_id>: {turns}} }`。
- **建 team 前的預檢與預估**：面板顯示所選 kind 目前的 5h / 7d 額度（沿用 `GET /api/quota`）；任一 `used_pct ≥ 70` 顯示黃色警告、`≥ quota_stop_pct` 拒絕建立（400 `quota_low`）。**不做 token 預估**——沒有可靠的每回合 token 數據來源；改以「最多 N 回合 × M 成員」的上限呈現。
- **執行中**：TeamPanel 標題列掛既有 `QuotaStrip`（只顯示成員用到的 kind）；每次 `quota_updated` 進來 scheduler 也重新檢查 `quota_stop_pct`。
- 中止閘門：`paused` 各種原因 + 使用者的 `abort`。**沒有任何自動 abort**（暫停可續、中止不可逆）。

---

## 10. G. API 草案（照 `docs/API.md` 寫法；前綴 `/api`）

### 10.1 `POST /projects/{id}/teams`

```json
{
  "issue_number": 42,
  "pm":       { "kind": "codex",  "model": "gpt-6-astra", "effort": "high", "fast": false, "identity": null, "persona_extra": "" },
  "workers":  { "count": 2, "kind": "claude", "model": "opus", "effort": null, "fast": false, "identity": "cc1", "persona_extra": "" },
  "reviewer": { "kind": "grok",   "model": null, "effort": "high", "fast": false, "identity": null, "persona_extra": "" },
  "base": "HEAD",
  "deliver": "pr",
  "supervised": false,
  "budget": { "max_relays": 40, "max_review_rounds": 2, "max_wall_clock_min": 120, "quota_stop_pct": 90 }
}
```

| 欄位 | 必填 | 規則 |
|---|---|---|
| `issue_number` | ✅ | 以 `gh issue view` 取得全文；找不到 → 502（同 issues 端點） |
| `pm` / `workers.kind` | ✅ | kind 驗證與 `POST bots` 相同；`model / effort / fast / identity` 規則同 API.md §12.2 |
| `workers.count` | | **併行數** 1–4，預設 1（2026-09-08）：最多同時跑幾個 task。欄位名沿用 `count` 以相容 |
| `reviewer` | | 可為 `null`（不審查直接合併） |
| `base` | | git ref，預設 `HEAD`；解析失敗 400 |
| `deliver` | | `branch \| pr`，**預設 `branch`**（§12 #1 已裁決）|
| `supervised` | | 預設 `false` |
| `budget` | | 每個欄位皆可省，預設如上 |

`workers.count` 接受 `0`（無限，§4.5）與 `1`–`4`；其他值 `400`。`0` 建立時只開第一個 issue 的 1 個執行者，
其餘 issue 與執行者由 daemon 在 `startup` 之後長出來。

回應 `200 {"team_id":"01M1…"}`（成員尚在啟動中，之後靠 WS）。錯誤：`400 {"error":"not_a_git_repo"}`、`400 {"error":"quota_low","kind":"claude","used_pct":93}`、
`409 {"error":"conflict","reason":"host is not connected"}`、`404 project`、`502 gh / git`。

### 10.2 `GET /api/state` 新增

```json
{ "projects": [ { "id": "…", "teams": [
    { "id": "01M1…", "issue_number": 42, "issue_title": "…", "issue_url": "…",
      "phase": "working", "pause_reason": null, "pause_detail": null, "branch": "team/i42-k3f9x2", "deliver": "pr", "supervised": false,
      "members": [ {"bot_id": "…", "role": "pm"}, {"bot_id": "…", "role": "worker"}, {"bot_id": "…", "role": "reviewer"} ],
      "tasks_summary": { "queued": 0, "working": 1, "reviewing": 1, "merged": 1, "total": 3 },

  > `tasks_summary` 的 key 是 §8.2 的 task 狀態值。**每個計數非零的狀態都必須出現**（`total` 恆在），計數為 0 的可省略。上例只是某一刻的快照，不是 key 的白名單 —— 前端要能接受任何 task 狀態當 key，否則像 `exhausted`（需要人決定）這種狀態會在標題列與 sidebar 數不出來。
      "budget": { "…": "…" }, "usage": { "relays": 12, "elapsed_min": 18, "…": "…" },
      "pr_url": null, "created_at": "…", "started_at": "…", "ended_at": null } ] } ],
  "…": "每個 bot 物件新增 managed_by, team: {team_id, role} | null, cwd" }
```

### 10.3 `GET /teams/{id}`

`state` 的 team 物件 + `tasks:[{id, seq, title, brief, files, worker_bot_id, branch, state, round, last_report, last_verdict, merge_sha, updated_at}]` + `summary` + `base_ref/base_sha` + `worktree_root`。

### 10.4 `GET /teams/{id}/events?before=&limit=100`

team 日誌，倒序分頁、正序回傳（同 messages）。每則：
`{id, kind: "relay"|"phase"|"merge"|"note"|"user", from_bot_id, to_bot_id, task_id, turn_id, status, payload, created_at}`。
`payload` 依 kind：relay 帶 `{action, text_excerpt}`；phase 帶 `{from, to, reason}`；merge 帶 `{branch, result, sha|conflict_files}`。

> **所有 team 端點的 404 一律回 `{"error":"not_found","what":"team"｜"task"｜"project"}`。**
> 前端靠這個 body 分辨「team 被清掉了」與「這個 daemon 根本沒有 team 功能」（舊 daemon 會回裸 404 或 SPA fallback）——
> 前者顯示「team 已不存在」，後者要靜默隱藏整個 team 區塊。沒有機器可讀的 body 就無法區分。

### 10.5 控制

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/teams/{id}/pause` | — | `200 {}`；已是終態 409 |
| POST | `/teams/{id}/resume` | — | `200 {}`；非 paused 409；`member_lost` 且成員仍未 running → 409 `{reason:"member not running", bot_id}` |
| POST | `/teams/{id}/approve` | — | supervised 閘門放行；非 `paused(gate:*)` 409 |
| POST | `/teams/{id}/abort` | `{"reason"?}` | `200 {}`；停所有成員 |
| POST | `/teams/{id}/cleanup` | — | 非終態 409；成功 `200 {}`，推 `bot_changed` ×N + `team_changed` |
| DELETE | `/teams/{id}` | `?branches=keep\|delete`（預設 `keep`）| **任何 phase 都可刪**（§6.5a）：非終態時先停成員 → 清 worktree → 關 workspace → 刪三張表的列 → 成員 bot 標 `deleted_at`（訊息保留）。`branches=delete` 才 `git branch -D`，遠端分支一律不動。成功 `200 {}` 並推帶 `deleted: true` 的 `team_changed` + `bot_changed` ×N；不存在 `404 {"error":"not_found","what":"team"}` |
| PATCH | `/teams/{id}` | `{"label"?: string, "budget"?: {...部分}, "supervised"?: bool, "deliver"?: "branch"\|"pr", "pm"?: <角色>, "workers"?: <角色>, "reviewer"?: <角色>}` | `200 {}`；終態 409。三個角色同一個形狀，見下表 |

**`label`（2026-09-09 新增）**：使用者自己取的短名，最長 60 字；`""` = 清掉，改回顯示 `#編號 issue 標題`。
issue 標題常常是一整句規格，手機的標題列與切換器只看得到前幾個字。**唯一不受終態 409 限制的欄位**
——名字是給人事後找東西用的，已經 done / aborted 的隊伍一樣要能改；只送 `label` 時回 `{"applied":"label"}`。
`GET /teams/{id}` 與 `GET /api/state` 的 team 物件多一個 `label`（`null` = 沒取）。
| POST | `/teams/{id}/say` | `{"text", "to", "client_request_id"}`；`to` 接受**角色**（`pm` / `reviewer`）、**短名**（`dev-1`，同 §4.4 協定用的）、**暱稱**（`i42-pm`）、**bot_id**，可帶 `@` 前綴 | 使用者插話（記 `kind:user`，不計預算），走 §13 群組路徑；回同 `POST chat` 的單筆 `sent` |
| POST | `/teams/{id}/tasks/{tid}/decide` | `{"action": "rework" \| "force_merge" \| "skip", "note"?}` | 只在 task `exhausted` / `blocked_by_worker` / rebase 用盡時有效；其餘 409 |
| POST | `/teams/{id}/answer` | `{"text"}` | 回 PM 的 `ask_user`；等同 `say` 到 pm + `resume` |

**角色（`pm` / `workers` / `reviewer`，2026-09-08）**：`{"kind"?, "model"?, "effort"?, "fast"?, "identity"?, "apply"?}`。
省略一個 key = 不動它；`model` / `effort` / `identity` 送 `null` = 清成該 kind 的預設。

| 欄位 | 行為 |
|---|---|
| `model` / `effort` / `fast` / `identity` | 寫回 `roles_json.<role>`（`workers` 是 `roles_json.workers.spec`），**下一批據此建立**，同時更新該角色現有 bot 的欄位。`apply: "now"` 再把有 run 的成員逐一重啟（進行中的工作會中斷）；預設 `next` 只等重啟或換批時生效 |
| `kind` | **換 bot**（§7.6）：同名、同 cwd 建一個新 kind 的成員，舊的停掉並 `deleted_at`（訊息保留），未終態的 task 指到新 bot，新成員直接啟動。`apply` 對 `kind` 沒有意義 |
| `apply` | `next`（預設）/ `now`；其他值 `400 {"message":"<role>.apply must be `next` or `now`…"}` |
| `count`（只有 `workers`） | 併行數 `0`（無限）或 1–4，其他值 400。**改成 `0`**（2026-09-09）：立刻把佇列裡的 issue 全部開工（上限 6），回 `applied: "now"`。**從 `0` 改成 `n`**：不再開新 issue，在跑的做完，執行者數之後照 `n`。以下是 1–4 之間的行為，一個字沒變：**改大**：當場建並啟動 `dev-(舊n+1)`…`dev-新n`（走 §7.6 的 `insert_member` + pretrust + `start_bot`），啟動後立刻補位，佇列裡的 task 馬上開跑。**改小**：只寫進 `roles_json`，下一批執行者（下一個 issue / `replace`）才生效；多出來的執行者做完手上那筆就不會再被派 |

`PATCH` 的回應是 `{"applied": "now" | "next_batch"}`：`now` = 這次真的當場多開了執行者；
其餘情況（只改小、只改 spec）都是 `next_batch`。

驗證同建立時（`check_role`）：kind 必須是已安裝的三種之一、`effort` 對得上該 kind、`identity` 存在且 kind 相符（`404 identity` / `400`）。
這隊沒有 reviewer 時 `reviewer` 回 `400 {"message":"this team has no reviewer"}`——加一個 reviewer 要 worktree 與啟動，不是 PATCH 做的事。
換 `kind` 時該角色有成員正在跑一個 turn → `409 {"reason":"member busy","role","bot_id","name"}`（先暫停再改）。
每次 PATCH 記一筆 `team_events` 的 `note`：`{"action":"patch", …, "roles": {"<role>": {"role","from","to","model","effort","fast","identity","apply","restarted","swapped"}}}`。

### 10.6 WebSocket

| type | data |
|---|---|
| `team_changed` | `{"team_id", "project_id", "phase", "pause_reason", "pause_detail", "usage"}` — phase / 預算用量變化 |
| `team_task_updated` | `{"team_id", "task": <task 物件>}` |
| `team_event` | `{"team_id", "event": <10.4 的事件物件>}` |

成員的 `bot_status` / `message_added` / `turn_updated` / `turn_progress` 照舊；message 物件多 `team_id` / `relay_from`，turn 物件多 `team_id` / `team_event_id`。前端靠 `bot.team` 把它們歸到 TeamPanel。

### 10.6a Issue 佇列（§2.3）

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/teams/{id}/issues` | `{"issue_numbers":[n,…]}`（或單數 `{"issue_number":n}`） | `200 {issues:[…]}`；`aborted` / `failed` 回 409，**`done` 且未 cleanup 則放行並 reopen（§2.5）**，`done` 但已 cleanup 回 `409 {"reason":"team is cleaned up"}`；同號 issue **還在佇列上**（`queued` / `working`）回 `409 {"reason":"issue already queued","issue_number":n}`，`done` / `failed` / `skipped` 的同號 issue **可以再排**（新列，§2.3） |
| DELETE | `/teams/{id}/issues/{issue_id}` | — | `200 {}`；只有 `state="queued"` 可移除，其餘回 409 |

`POST /projects/{id}/teams` 的 body 改用 `issue_numbers: [n,…]`（依序處理）；舊的 `issue_number` 仍然接受，
等同一個元素的佇列。上限 `MAX_QUEUED_ISSUES = 20`，**只算還在佇列上的**（`queued` / `working`；2026-09-09）：已交付／失敗／略過的不佔額，分批追加沒有總數上限。

`GET /api/state` 與 `GET /teams/{id}` 的 team 物件多三個欄位：`issues[]`（每項見 §2.3）、`current_issue_id`、
`issues_summary {total, done, failed, queued}`。`team_changed` 是淺層 patch，前端要對 `issues[]` 做明確合併。

### 10.7 `POST /teams/{id}/close-issue` — 完成後關掉 issue（經使用者同意）

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/teams/{id}/close-issue` | 省略、`{}`、`{"comment": "…"}` 或 `{"comment": ""}` | `200 {number, url, title, repo, state:"CLOSED", already_closed}` |

規則（兩條，缺一不可）：

1. **只有 `phase === "done"` 的 team**。`done` 的定義是 PM 宣告完成**且** daemon 驗證所有 task 進入終態（§8.3）；
   `aborted` / `failed` / 還在跑的一律 `409 {"error":"conflict","phase":"…"}`。
   > §2.5.4 把這條放寬成「**該 `team_issues` 列 `state='done'`**」，並讓 body 多一個可選 `issue_id`（省略 = 當前鏡像）；
   > team 整體 `done` 時當前 issue 必然 `done`，所以原本的行為是新規則的特例。reopen 後回頭關上一個 issue 走這條。
2. **只有使用者**。scheduler 沒有任何一條路徑會呼叫它——這個端點存在的唯一理由，就是讓 UI 在 team 完成後
   問一次「要不要關掉 issue」，issue 是因為有人按了按鈕才關的。**不做自動關閉，也不做「N 分鐘後自動關」**。

其他：

- `comment` 省略 → daemon 寫預設留言（PM 總結、整合分支與 base、已合併的 task 與 commit；`deliver=pr` 附 PR 連結，
  否則明寫「這條分支還沒有合併進<base>、也沒有開 PR，若最後沒有採用請重開這個 issue」）。傳空字串 → 不留言。
- 關過一次就 `409 {"error":"conflict","closed_at":"…"}`（`teams.issue_closed_at`，additive migration）。
- 別人先關掉的 issue 回 `200 already_closed:true`，**不會重複留言**：使用者要的狀態已經成立。
- 專案沒有 GitHub origin → `400 project has no GitHub origin`（UI 也不會顯示按鈕）。
- 成功後推 `team_changed`，並在 team 日誌留一則 `note {action:"issue_closed", by:"user", number, url, already_closed, comment}`。

**一鍵關閉全部（2026-09-09）**：佇列裡有兩個以上「`done` 且未關」的 issue 時，UI 在佇列摘要旁給「關閉全部已交付（k）」，
確認一次後**逐一**呼叫同一個端點（前端迴圈，daemon 沒有批次端點——每一次關閉仍是使用者按出來的那一次），
一個失敗不擋其他，最後一則通知寫幾個成功幾個失敗。

> 為什麼不做成 `deliver` 的第三種模式（`branch | pr | close`）：`deliver` 是 team 跑完自己會做的事，
> 而關 issue 依定義要等人點頭；把它塞進 `deliver` 等於讓 daemon 自動關 issue，正是這一節要避免的。

---

## 11. G. 前端（照 `docs/FRONTEND.md` 寫法）

### 11.1 入口：IssuesBar

每列多一顆 `mini-btn` **「組隊」**（`title`：「為這個 issue 建立一個 team」）；`project.github` 為 null 時整條 IssuesBar 本來就不顯示，所以不需要額外判斷。點了開 **TeamLaunchPanel**（右側主區域的暫時性 sheet，同 UI-DECISIONS「新增 Bot 表單改為 sheet」的做法，不用 modal）。

### 11.2 `TeamLaunchPanel.tsx`

- 頂部：`#42 <title>` + 連結 + label 徽章（沿用 IssuesBar 的 `labelStyle`），可展開 issue 全文（`fetchIssue`）。
- 三張角色卡（PM / 執行者 / Reviewer），每張：kind 選擇（沿用 `KindTag` 樣式與 `hosts[].tools` 的「未安裝→停用並標原因」規則）→ `ModelPicker`（既有元件，含 effort / Fast）→ 身份下拉 → 「額外人設」收合文字框。執行者卡多一個 1–4 stepper；Reviewer 卡有「不審查」開關。
- 交付方式（分支 / PR；`gh` 不可用時 PR 選項停用並說明）、`supervised` 開關、預算四個數字（預設值先填好）。
- 額度預覽：所選 kind 各一顆 quota pill（沿用 `QuotaStrip` 的 pill 元件），`≥70%` 琥珀、`≥ quota_stop_pct` 紅並停用「建立並啟動」。
- 「建立並啟動」→ `api.createTeam()` → 成功後 `selectTeam(team_id)`。

### 11.3 `TeamPanel.tsx`（team 進行中的視圖）

沿用 `GroupChatPanel` 的骨架（成員燈號列、合併時間軸、`Bubble`、`LiveBubble`、composer），差異：

- **標題列**：`⚙ Team · #42 <title>` + phase chip（`starting` 黃閃 / `planning`、`working` 藍 / `paused` 琥珀並顯示 reason 中文 / `done` 綠 / `aborted`、`failed` 灰）+ 整合分支名與 `worktree_root`（各自點擊複製）+ 預算量條（`relays 12/40 · 18 分鐘`）+ 既有 `QuotaStrip`（只列成員 kind）+ 操作：暫停 / 繼續 / 放行（supervised）/ 中止（走 `ConfirmDialog`，顯示 team 與 project 全名）/ 清理（終態）。
- **成員列**：每個成員一顆燈 + 角色徽章（`PM` / `dev-1` / `rev`）+ 目前 task 標題；點成員跳到它的單獨對話（既有）。
- **Task 看板**（時間軸上方可收合，高度上限 200px）：欄位 `待派 / 進行中 / 審查中 / 整合中 / 完成 / 需要你`；卡片顯示 `t1 · dev-1 · round 1/2`；`需要你` 欄的卡片帶 decide 按鈕（再給一回合 / 強制合併 / 跳過）。
- **時間軸**：`GET /projects/:id/messages` 過濾 `team_id`；relay 的 user 訊息 `from` 顯示 `pm → dev-1`（依 `relay_from` + 收件 bot）、daemon 自己的（修復提示、合併結果）顯示 `daemon → pm` 並用 system 樣式；`am-team` 區塊在氣泡內折疊成一行 chip（`dispatch ×2` / `report done` / `verdict approve`），點開看 JSON。`team_event` 的 merge / phase 事件插成系統列。
- **composer**：預設收件者 `@i42-pm`（收件者列沿用 UI-DECISIONS P0 的 chip），送 `POST /teams/:id/say`；`paused(ask_user)` 時 composer 上方顯示 PM 的問題、送出即 `answer`。
- **paused 橫幅**：44px 警示列（沿用「缺 CLI 情境式警示」的樣式）寫明原因與下一步按鈕（加碼 / 繼續 / 重啟成員 / 處理 blocked）。

### 11.4 Sidebar

Project 底下新增 **Team 節點**（`⚙ #42 <title 截斷>` + phase 燈 + 未讀），可收合；成員 bot 縮排列在節點下，不與一般 bot 混排。點節點 → `selectTeam()`。終態的 team 節點變灰並提供「清理」。

### 11.5 store 與資料流

| 來源 | 前端處理 |
|---|---|
| `GET /api/state` | `normalize.toState()` 多攤平 `teams`（`projects[].teams[]` → `teams` map）與 bot 的 `team` / `managed_by` / `cwd` |
| `GET /teams/:id` + `/events` | `selectTeam(id)` 時載入；存 `teamDetail[teamId]`、`teamEvents[teamId]` |
| WS `team_changed` / `team_task_updated` / `team_event` | 直接寫進上述 map；`team_changed` 若 phase 進入終態跳一則通知 |
| WS `message_added` | 既有路徑不變；`message.team_id` 非 null 時同 frame 追加到 `teamMessages[teamId]`，未開啟時 `teamUnread[teamId] += 1` |
| `resync` | 重載目前開著的 team |

`selectedTeamId`（與 `selectedProjectId` / `selectedBotId` 互斥，`App.tsx` 據此切 `TeamPanel`）。

### 11.6 mock（`VITE_MOCK=1`）
`api/mock.ts` 加一個假 scheduler：`createTeam` 後每 1.5 秒推進一個假事件（starting → planning → dispatch ×2 → report → verdict → merge → done），
訊息用 `[mock am-team]` 標記；`pause`/`resume`/`abort`/`decide` 都改狀態。用來驗 UI 與截圖，不驗協定。

---

## 12. 待使用者裁決

| # | 事項 | 建議 | 理由 |
|---|---|---|---|
| 1 | ~~**預設交付方式** `deliver`~~ **已裁決（2026-09-06）：預設 `branch`** | `branch` | 使用者裁決：push 到 origin 是對外且難回收的動作，不讓 agent 的產出未經過目就上遠端。`pr` 仍是合法值、UI 可選，且必須明示「會 push 到 origin」。**本文其餘各節一律以 `branch` 為預設**。 |
| 2 | **預算預設值**：`max_relays=40`、`max_review_rounds=2`、`max_wall_clock_min=120`、`quota_stop_pct=90` | 照此 | 40 relay ≈ 2 個 worker × (派工 + 回報 + 審查 + 一次打回 + 合併通知) ≈ 20 條再留一倍餘裕；超過通常代表流程卡住而不是工作量大。 |
| 3 | **supervised 預設**：關 | 關 | 預設全自動才符合「叫出 team 來解決」；閘門留給敏感 repo。 |
| 4 | **`quota_stop_pct` 是 team 專屬還是全域設定**（config.toml `[team]`） | 先 team 專屬、面板記住上次值（localStorage） | 少一個設定面；等用久了再決定要不要進 TOML。 |
| 5 | **要不要在 issue 上留言**（開始 / 完成時 `gh issue comment`） | 第二階段，預設關 | 對外可見的副作用，先不做；PR 本身已經連到 issue。 |
| 6 | **team 結束後成員是否自動 cleanup**（例如 24 小時後） | 不自動；終態後 UI 提示「清理」 | worktree 是可追溯的證據；自動刪掉可能讓人來不及看 rebase / 衝突現場。 |
| 7 | **執行者上限 4** | 4 | 一個 reviewer 序列化審查，超過 4 個 worker 只會排隊；額度也撐不住。 |
| 8 | **PM 是否允許自己動手改碼** | 不允許（persona 禁止 + merge 前 `status` 乾淨檢查） | PM 改碼會讓整合分支變成第 N+1 個沒人審的工作樹。若你希望「1 個 worker 時 PM 自己做」，那就是 `workers.count=1` 且 PM 與 worker 同 kind，而不是放寬規則。 |
| 9 | **`persona` 欄位正式收進 SPEC.md** | 是，與本提案一起 | 已在程式碼與 API.md §12.8，SPEC.md 缺，team 依賴它。 |

其餘（協定格式、拓撲、worktree 佈局、命名、狀態機、送不到的處理、API 形狀）本文已決定並附理由；若不同意請直接在對應小節批註。

---

## 13. 分階段實作建議

### 第一階段（最小可用：一個 issue、本機、全自動跑完）
1. DB：`teams` / `team_tasks` / `team_events` + `bots.{managed_by,team_id,team_role,cwd}` + `turns.{team_id,team_event_id}` + `messages.{team_id,relay_from}`（全部 additive）。
2. `bots.cwd` 進 `start_inner`；投影排除 `managed_by='team'`。
3. `team.rs`：建 team（資料目錄下的 git worktree、ISSUE.md / TEAM.md 與各 worktree 內副本、成員 bot、背景啟動）、scheduler 事件迴圈、`am-team` 解析 + 修復提示、PM 收件箱合併、task 狀態機、daemon merge + rebase task、預算 / 額度檢查、pause / resume / abort、`deliver=branch|pr`、cleanup。
4. `app.turn_bus` 內部 broadcast（`emit_turn` 處發）。
5. API §10 全部（`say` / `answer` / `decide` 可先做最簡版）+ WS 三個事件。
6. 前端：IssuesBar「組隊」、`TeamLaunchPanel`、`TeamPanel`（標題列 + 成員列 + 時間軸 + paused 橫幅；看板可先是清單）、sidebar Team 節點、mock。
7. 驗收（照 SPEC 各節格式）：T1 建 team 三角色皆 running、worktree 與分支存在；T2 PM dispatch 兩個 task 各到對的 worker（`turns.team_event_id` 對得上）；T3 兩個 worker 同時回報 → PM 收到**一則**合併回報；T4 reviewer request_changes → 同一 worker 收到 rework、round=1；T5 人為製造衝突 → rebase relay → 第二次 merge 成功；T6 `max_relays=5` → `paused(budget_relays)` → 加碼 resume 跑完；T7 kill daemon 再起 → team 續跑；T8 `deliver=branch` 結束後主 checkout `git status --porcelain` 為空、`HEAD` 未動、`<project.path>` 下沒有新增任何檔案（`find -newer` 比對建 team 前的快照）、`git worktree list` 列出四個 team worktree；T9 cleanup 後 `git worktree list` 只剩主 checkout、`<data_dir>/teams/<id>/` 不存在、整合分支與 task 分支仍在。

### 第二階段
- 遠端主機（git / 檔案寫入走 ssh；路徑規則相同）。
- supervised 閘門完整 UI；`ask_user` 的問答 UI。
- Task 看板拖拉、逐 worker 不同 kind、成員「轉為一般 bot」。
- issue 留言、cleanup 自動化、herdr `worktree.*` RPC 評估。
- 可選的合併門檻 `check_command`（daemon 在整合 worktree 跑 `cargo build` / `npm test`，失敗當 `request_changes` 回給 worker）。
- `agents-managerd team status` 之類的唯讀 CLI，讓 PM 能主動查 task 狀態（不是寫入路徑）。

### 第三階段
- 以 transcript（SPEC §4.2）取代 `last_assistant_message` 作為 relay 內容來源（更完整）。
- 非 git 目錄（純目錄隔離 + rsync）——目前不建議。

---

## 附錄 A：relay 模板（daemon 產生；中文，指令部分英文以免 CLI 誤讀）

所有模板結尾固定附：

```
---
回覆結尾必須包含一個 ```am-team fenced 區塊（JSON），允許的 action：<依角色列出>。區塊之外的文字給人看，區塊給系統看。
```

### A.1 PM persona（`bots.persona`）與首則 relay
- persona：「你是 issue #<n> 的 PM。你不寫程式、不 commit。你的 cwd 是整合分支的 worktree（唯讀參考）。成員：<名單與 cwd>。工作：讀 `.agents-manager/team/ISSUE.md` 與 `.agents-manager/team/TEAM.md`（都在你的 cwd 內），把 issue 拆成互不重疊（以檔案 / 模組切分）的 task，用 `dispatch` 派工——**不用指定 `to`**，daemon 會派給有空的執行者（併行數就是同時能跑幾筆）。你可以隨時再 `dispatch`，不必等前一批做完：多出來的會排隊，跑完一筆補一筆。沒事做就 `wait`；收到回報後決定下一步；所有 task 合併後 `done` 並寫摘要。不確定就 `ask_user`。」（2026-09-08）
- 首則 relay：「Issue #<n>「<title>」。全文在 `.agents-manager/team/ISSUE.md`。**併行數 <n>（執行者：<短名>）**——`dispatch` 不用指定 `to`，派幾筆都可以，超過併行數的會排隊。請先讀取檔案再派工。」（2026-09-08）
- 收單後的 note（只在**有排隊**時送，否則是白白多一輪 turn）：「收到 <N> 筆。併行數 <n>：現在跑 <M> 筆（t1→dev-1…），排隊 <K> 筆——排隊的會在有執行者空下來時自動派出，你不用再派一次。」

**無限模式（2026-09-09）**：
- persona 末尾追加：「這個 team 的併行數是**無限**：佇列裡的 issue 會同時開工，你會同時管好幾個。因此 `dispatch` 的**每一筆 task 都必須寫 `issue`**，`done` 也必須寫 `issue`——`done` 只交付那一個 issue，其他的照跑。哪些 issue 在進行中、各自的整合分支與執行者，一律以 `TEAM.md` 為準；每個 issue 的全文在 `ISSUE-<n>.md`。」
- 首則 relay 改成列表：「併行數是**無限**：你同時在管 <N> 個 issue（這一批新開的是 #48、#49）。<每個 issue 一行：整合分支、`ISSUE-<n>.md`、執行者>。因此每一筆 `dispatch` 都要寫 `issue`，`done` 也要寫 `issue`……每個 issue 先給 1 個執行者，你派幾筆就開幾個（每 issue 最多 4、全隊最多 12）。」佇列裡還有等著的 issue 時補一句「等這裡有 issue 結束就會自動開始」。
- 每一則關於某個 issue 的 relay（合併通知、`blocked` 回報）開頭標 `[#48]`。

### A.2 worker persona 與派工 relay
- persona：「你是 <短名>。你的 cwd `<path>` 是專屬 worktree，你只能在這裡工作：不要 `cd` 出去、不要動 `../`、不要 `git push`、不要切換分支。每個邏輯段落 `git commit`。完成後用 `report` 回報：`summary` 說明改了什麼、如何驗證。做不下去用 `status: blocked` 說明原因。」
- 派工 relay：「Task t<seq>「<title>」（分支 `<branch>` 已建好並 checkout）。<brief>。相關檔案：<files>。」
- rework relay：「Reviewer 打回（第 <round> 回）：<summary>。必修：<must_fix>。在同一分支繼續，完成後再 `report`。」
- rebase relay：「整合分支已前進。請執行 `git rebase <integration>`，解掉衝突並確認可建置後 `report`。」

### A.3 reviewer persona 與送審 relay
- persona：「你是 reviewer。cwd `<path>` 已 checkout 到待審分支（detached）。唯讀：可以跑 build / test，不要 commit、不要改檔。用 `git diff <integration>...HEAD` 看變更。判斷是否正確、是否符合 issue、是否會破壞其他部分；用 `verdict` 回覆，`request_changes` 時 `must_fix` 要具體到檔案與行為。」
- 送審 relay：「請審查 task t<seq>「<title>」（分支 `<branch>`，第 <round+1> 回）。執行者的回報：<summary>。」

### A.4 PM 的回報批次
「以下 <N> 則回報：
1. `dev-1` t1「…」→ `done`：<summary>（已送審 / 已合併 / 衝突處理中）
2. `dev-2` t2「…」→ `blocked`：<notes>
目前 task 狀態（**併行 <M>/<n>**）：<表，還沒有執行者的那幾筆寫「排隊中」>。請決定下一步（`dispatch` / `wait` / `done` / `ask_user`）。」（2026-09-08）

無限模式下狀態表**按 issue 分組**（2026-09-09），每組標「`#48 · <整合分支>`（∞ · 執行者 k · 進行中 m）」再列該 issue 的 task。

relay 內文上限 8 KB（超過截斷並註明「完整內容見時間軸」）；issue 全文永遠走各 worktree 內的 `.agents-manager/team/ISSUE.md`（無限模式是 `ISSUE-<n>.md`，一個 issue 一份），不塞進 prompt。

## 附錄 B：SQLite（additive）

```sql
CREATE TABLE IF NOT EXISTS teams (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
  issue_number INTEGER NOT NULL, issue_title TEXT NOT NULL, issue_url TEXT NOT NULL,
  phase TEXT NOT NULL CHECK (phase IN ('starting','planning','working','finishing','done','paused','aborting','aborted','failed')),
  pause_reason TEXT, resume_phase TEXT,
  pause_detail_json TEXT,              -- §4.5：quota_low 是誰的額度不夠（暫停當下的快照）
  base_ref TEXT NOT NULL, base_sha TEXT NOT NULL, branch TEXT NOT NULL, worktree_root TEXT NOT NULL,
  workspace_id TEXT,
  deliver TEXT NOT NULL CHECK (deliver IN ('branch','pr')),
  supervised INTEGER NOT NULL DEFAULT 0,
  roles_json TEXT NOT NULL,            -- 建立時的 pm / workers / reviewer 設定（供 UI 與重建）
  budget_json TEXT NOT NULL, usage_json TEXT NOT NULL DEFAULT '{}',
  pr_url TEXT, summary TEXT,
  created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT
);
CREATE INDEX IF NOT EXISTS teams_project ON teams(project_id);
CREATE TABLE IF NOT EXISTS team_tasks (
  id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
  seq INTEGER NOT NULL, title TEXT NOT NULL, brief TEXT NOT NULL, files_json TEXT NOT NULL DEFAULT '[]',
  -- 2026-09-08：可 NULL = 已收單但還在排隊，沒有執行者在做（§4.5）。
  worker_bot_id TEXT REFERENCES bots(id),
  -- PM 在 dispatch 裡指名的執行者（沒指名就是 NULL）。和 worker_bot_id 分開，是因為
  -- team_tasks_one_open_per_worker 不允許同一個 worker 有第二列未終態的 task。
  want_worker_bot_id TEXT REFERENCES bots(id),
  branch TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued','working','reported','reviewing','changes_requested','exhausted',
                                       'blocked_by_worker','rebasing','merging','merged','skipped','failed')),
  round INTEGER NOT NULL DEFAULT 0, rebase_attempts INTEGER NOT NULL DEFAULT 0,
  last_report TEXT, last_verdict TEXT, merge_sha TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS team_tasks_seq ON team_tasks(team_id, seq);
-- SQLite 的 UNIQUE 不管 NULL，所以「多筆排隊中、每人最多一筆在跑」剛好就是這個索引的語意。
CREATE UNIQUE INDEX IF NOT EXISTS team_tasks_one_open_per_worker ON team_tasks(worker_bot_id)
  WHERE state NOT IN ('merged','skipped','failed');
CREATE TABLE IF NOT EXISTS team_events (
  id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
  kind TEXT NOT NULL CHECK (kind IN ('relay','phase','merge','note','user')),
  from_bot_id TEXT, to_bot_id TEXT, task_id TEXT, turn_id TEXT,
  status TEXT CHECK (status IN ('pending','delivered','dropped')),   -- 只有 relay 用
  payload_json TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS team_events_pending ON team_events(team_id, to_bot_id) WHERE status = 'pending';
-- additive migrations（沿用 db.rs 的 (table, col, ddl) 清單）
-- bots:     managed_by TEXT NOT NULL DEFAULT 'user' | team_id TEXT | team_role TEXT | cwd TEXT
-- turns:    team_id TEXT | team_event_id TEXT
-- messages: team_id TEXT | relay_from TEXT
```

## 附錄 C：daemon 執行的 git 序列（本機；遠端加 ssh 前綴）

```sh
# 建 team（所有 git 指令以 -C <project.path> 執行；ROOT=<data_dir>/teams/<team_id>）
git -C <project.path> rev-parse --is-inside-work-tree     # 否 → 400 not_a_git_repo
BASE=$(git -C <project.path> rev-parse <base>)            # 記 base_sha
git -C <project.path> branch team/i42-k3f9x2 "$BASE"
mkdir -p "$ROOT" && 寫入 "$ROOT/ISSUE.md" "$ROOT/TEAM.md"
git -C <project.path> worktree add "$ROOT/main" team/i42-k3f9x2
git -C <project.path> worktree add --detach "$ROOT/dev-1" "$BASE"        # 每個 worker
git -C <project.path> worktree add --detach "$ROOT/reviewer" "$BASE"
for wt in main dev-1 … reviewer; do                       # 成員 cwd 內的副本，被 * ignore
  mkdir -p "$ROOT/$wt/.agents-manager/team" && printf '*\n' > "$ROOT/$wt/.agents-manager/team/.gitignore"
  cp "$ROOT/ISSUE.md" "$ROOT/TEAM.md" "$ROOT/$wt/.agents-manager/team/"
done
# 派工
git -C …/dev-1 checkout -b team/i42-k3f9x2/t1-dev-1 team/i42-k3f9x2
# 回報
git -C …/dev-1 status --porcelain                        # 非空 → add -A && commit -m "wip(dev-1): uncommitted at report"
git -C …/dev-1 rev-list --count team/i42-k3f9x2..HEAD    # 0 → 修復提示
# 送審
git -C …/reviewer checkout --detach team/i42-k3f9x2/t1-dev-1
# 合併
git -C …/main status --porcelain                         # 非空 → paused(integration_dirty)
git -C …/main merge --no-ff --no-edit team/i42-k3f9x2/t1-dev-1 || { git -C …/main merge --abort; → rebasing; }
# 交付（pr）
git -C …/main push -u origin team/i42-k3f9x2
gh pr create --repo <owner/repo> --head team/i42-k3f9x2 --base <base 對應分支> --title "…(#42)" --body-file <summary>
# 清理
for wt in main dev-1 dev-2 reviewer; do git -C <project.path> worktree remove --force "$ROOT/$wt"; done
git -C <project.path> worktree prune && rm -rf "$ROOT"   # 先 remove/prune 再刪目錄
```
