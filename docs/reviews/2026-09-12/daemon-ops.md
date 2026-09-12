# agents-manager daemon 運維面 code review（origin/main 3c6bf8a）

範圍：`daemon/src` 的 team.rs、team_git.rs、team_sched.rs、group.rs、hosts.rs、herdr.rs、herdr_shim.rs、pane_identity.rs、tools.rs、gh_auth.rs、github.rs、git_quick.rs、quota.rs、quota_claude.rs、quota_grok.rs、memstat.rs、memproc.rs、models.rs、supervisor/*、supervisor_evidence.rs。
方法：先讀 CLAUDE.md、SPEC.md、SPEC-team.md、API.md（FRONTEND / UI-DECISIONS 只掃標題），再逐檔讀碼；clippy `--all-targets` 當輔助（39 個 warning，其中本範圍相關的死碼已納入）。每一條「確定」都由我回頭對照原始碼行號再確認過；只憑推論或依賴外部行為的一律標「可能」。

## 總評

team 排程是這個範圍裡最複雜、也最容易出事的部分：狀態機本身設計得很仔細（phase CAS、pending 信箱、guard pause），但**無限模式**（`workers.count = 0`）是後來疊上去的，好幾條路徑仍然讀 `teams` 的鏡像欄位而不是 `team_issues`，最嚴重的一條會把 task 合進錯的整合分支（S1）。第二類問題是「失敗後不可重入」：`start_issue`、`finish`、`reopen_startup`、`flush` 的 `delivery_unknown` 在中途失敗後，使用者按「繼續」要嘛重複交付、要嘛永遠撞同一個錯，而且有幾種卡死狀態既不是 `paused` 也沒有 note。安全面乾淨：所有 `/api/*` 路由在 token middleware 之下，五個組 shell 字串的檔案都走 `sh_quote`，gh token 只經 stdin、錯誤訊息有打碼；唯一的例外是 `hosts[].remote_path` 未經引用就拼進每一段 ssh 腳本。hosts／herdr／quota／supervisor 這幾塊的問題偏可靠性與效能（逾時參數沒傳到、探測沒有退避、快取與 inbox 只增不減），沒有 panic 級的路徑。維護性上最大的兩處是 claude／grok 探測骨架逐字重複，以及 SPEC-team §6.2/§7.3、SPEC §14.2 跟程式碼脫節。

統計：確定 44 條（S1×1、S2×9、S3×22、S4×6、S5×6）；可能 66 條。

---

## 確定發現（依嚴重度）

### S1（資料遺失）

**1. team_sched.rs:1615 — 無限模式下 `merge_one` 以 `teams.branch` 鏡像判斷要不要切分支，會把 task 合進別的 issue 的整合分支。** 信心：確定
- 觸發情境：`workers.count=0`、佇列 [#42, #43]。`start_issue(#43)`（team.rs:1682）在 `main/` 做 `checkout -b team/i43-…`，接著 `sync_issue_mirror`（team.rs:1902–1913）把 `teams.branch` 改回第一個 working 的 #42。此時 `main/` HEAD 在 #43、鏡像是 #42。#42 的 task 通過審查 → `merge_one`：`integration_of(t)` == `ctx.team.branch` → 跳過 `checkout_branch` → `git merge --no-ff` 在 #43 的分支上執行。#42 交付／push 的是一條沒有成果的分支，#43 多了不屬於它的 commit。
- 建議修法：拿掉 1615 的條件，合併前一律 `tg::checkout_branch(main_wt, &integration)`（已在該分支時是 no-op）；或比對 `git symbolic-ref --short HEAD` 的實際值。
- 現有測試 `done_delivers_one_issue_and_leaves_the_others_running` 正好走這條路，但只斷言 `state == "merged"`。

### S2（主路徑正確性）

**2. team_sched.rs:990–1024（`flush`）— `delivery=unknown` 的 relay 已先標 `delivered`、task 已轉 `working`，abandon + resume 後永不重送。** 信心：確定
- 觸發情境：`agent.prompt` RPC 逾時 → `out.delivery == "unknown"`，但 990–995 已把 pending rows 改 `delivered`、1013–1014 把 task 改 `working`，才在 1022 `paused(delivery_unknown)`。使用者 `abandon` 該 turn 再 `resume` → `pending_for` 為空 → 什麼都不送；prompt 若沒送到，task 永遠 `working`、無 in-flight turn、無 pause。`resume_inner`（team.rs:2384）對 `delivery_unknown` 也沒有任何特殊處理。SPEC-team §7.5 要求「重送用新的 event_id」。
- 建議修法：`delivery == "unknown"` 時把這批 rows 標 `dropped` 並在 `resume_inner` 複製成新的 pending rows（新 id → 新 crid）；task 狀態改動移到確認 `delivery == "ok"` 之後。

**3. main.rs:241 vs 252 ＋ team_sched.rs:155 — 開機時 `replay_host` 早於 `respawn_schedulers`，spool 重放出的 TurnDone 沒有 scheduler 訂閱；`tick`／`step` 沒有補漏路徑。** 信心：確定
- 觸發情境：daemon 停機期間 worker 回報完成（本機 hook HTTP 失敗會寫 spool，hook_cmd.rs:104–105）。重啟：`replay_host` 把 turn 標 `completed` 並 `publish_turn`，此時 `spawn` 的 `subscribe_turns`（team_sched.rs:136）還沒跑。之後 `tick`（181–191）只做 `startup` + `step`；`advance_once`（1515–1596）只看 task state、`flush` 只看 pending rows；`on_turn_done` 唯一的呼叫點是 bus 迴圈（155）。task 卡在 `working`，回覆永遠不被 `apply_reply`。`Lagged` 分支（160–164）同理，那段註解「next tick 會重讀 DB」不成立。
- 建議修法：`step` 開頭補一段「找 `status='delivered'` 且對應 turn 已非 `in_flight`、又沒有後續 note 的 relay，呼叫 `on_turn_done`」；或至少把 `respawn_schedulers` 移到 `replay_host` 之前。

**4. team_sched.rs:2565–2585 ＋ 2676–2745 — `finish` 不可重入：交付後 `start_issue(next)` 失敗而暫停，resume 會再交付一次；`pr` 變 `deliver_failed` 死循環，`branch` 把佇列丟掉直接 `done`。** 信心：確定
- 觸發情境：有限模式、佇列 [#42, #43]。`finish` → `deliver_branch_or_pr` 成功（已 push / 已開 PR）→ `close_issue_and_advance`：#42 標 `done`（2694）、`retire_issue_workers`（2730）→ `start_issue(#43)` 因 `main/` 髒而 Err → `pause(upstream)`（2739）。`sched_pause` 以列上當下的 `finishing` 當 `resume_phase`（team.rs:936）。resume → `finish` 再跑：`pr` → `gh pr create` 回「already exists」→ `paused(deliver_failed)`，永遠到不了 #43；`branch` → 多一則 `delivered`，`close_issue_and_advance` 重載 `Ctx`，`ctx.issue()`（= `working_team_issues` 第一筆，team_sched.rs:224/230）為 `None` → 2690 `end_team` → team `done`，#43 仍 `queued`、執行者已退。SPEC-team §2.5.2 第 7 步明寫這個情況會發生。
- 建議修法：`close_issue_and_advance` 先 `start_issue`（或先確認 `main/` 乾淨）再把列改 `done`／retire；`finish` 開頭若該 issue 已有 `pr_url`／`delivered` note 就跳過交付；沒有 working issue 但 `next_queued_issue` 有值時不得 `end_team`。

**5. team_sched.rs:1955–1966 — 無限模式下 `dispatch.to` 對全隊執行者解析，短名剝掉 `i<seq>-` 後兩個 issue 的 `dev-1` 同名，指到別的 issue 的執行者後 task 永遠排隊。** 信心：確定
- 觸發情境：#42 與 #43 都 working，PM 送 `{"issue":43,"to":"dev-1"}`。`ctx.workers()` 是全隊，`ctx.short(w)`（`short_name`，team.rs:415–425）對 `t…-i1-dev-1` 與 `t…-i2-dev-1` 都回 `dev-1`，`find` 取到 #42 的。`fill_issue(#43)`（2128–2145）只走 `workers_for(#43)`（`workers_of_issue` 以 `-i2-dev-` 前綴過濾，team.rs:103–111），`want == worker.id` 永遠不成立 → task 永遠 `queued`；`pm_done(#43)` 因未終態一律 reject → PM `wait` 兩次 → `paused(pm_stalled)`。
- 建議修法：無限模式下 `to` 只在 `ctx.workers_for(&issue)` 裡找；短名對照時接受 `i<seq>-dev-n` 全名。

**6. team.rs:3280–3290（`swap_member`）— 換 kind 只把未終態 task 的 `worker_bot_id` 改指新 bot，`want_worker_bot_id` 與 `teams.rescue_bot_id` 仍指已軟刪的舊 bot。** 信心：確定
- 觸發情境：PM 兩次 `dispatch{to:"dev-1"}`，第二筆排隊（`worker_bot_id NULL`、`want_worker_bot_id=舊 id`）→ `PATCH workers.kind` → `fill_issue` 的 `want == worker.id` 對新 bot 永遠 false → 永遠 `queued` → `pm_stalled`。rescue 進行中換 reviewer 的 kind：`workers_for` 因 `rescue_bot_id` 指舊 bot 回空集合，收尾 task 永遠不派。`grep want_worker_bot_id team.rs` 只有 rescue 的 INSERT 與測試。
- 建議修法：同段加 `UPDATE team_tasks SET want_worker_bot_id=? WHERE team_id=? AND want_worker_bot_id=? AND state='queued'` 與 `UPDATE teams SET rescue_bot_id=? WHERE id=? AND rescue_bot_id=?`。

**7. team_sched.rs:598–604 ＋ 2684–2688 — 無限模式下 PM `abort` 走 `DECISION_PAUSES` 自動前進，把「第一個 working 的 issue」標 `failed` 並繼續，而非暫停整隊。** 信心：確定
- 觸發情境：`count=0`、#42／#43 working、佇列還有 #44。PM 針對 #43 回 `{"action":"abort"}`（協定沒有 `issue` 欄位）→ `pause_with_detail`：`pm_abort ∈ DECISION_PAUSES`（team.rs:68）且有 next → `close_issue_and_advance("failed")` → 無限分支取 `ctx.issue()` = #42 → #42 執行者被退、#44 補上、#43 照跑、整隊沒停。SPEC-team §2.3「多 issue 同時進行」明寫 `pm_abort` 仍暫停整隊。
- 建議修法：`pause_with_detail` 在 `team::is_unlimited` 時不走 DECISION_PAUSES 分支（無限模式的 issue 級失敗已由 `pause_issue` 處理），`pm_abort` 一律 `sched_pause`。

**8. team.rs:1682–1743（`start_issue`）— 失敗中途不回滾、也不可重入；reopen／無限 startup 路徑 resume 後永遠撞 `checkout -b` 同名分支。** 信心：確定
- 觸發情境：`checkout_task_branch`（1682，`git checkout -b`，team_git.rs:206）成功後，`worktree_add`（1695）、`insert_member`（1699）、docs（1719–1743）任一步 `?` 回傳。呼叫端 `startup`／`start_issues_up_to_capacity` 記 `issue_start_failed` 後 `paused(upstream)`，`resume_phase = starting` → resume → 再 `start_issue` → `fatal: a branch named … already exists` → 再 `paused(upstream)`，無限重複。就算過關，上次建的目錄讓 `worktree add` 失敗、上次插的 bot 讓 `insert_member` 走到 `{nick}-{tid6}` 備援名（2005），短名變成 `dev-1-<tid6>`，PM 的 `to: dev-1` 對不到。
- 建議修法：做成冪等（分支已存在就 `checkout`、worktree 已在 `worktree list` 就跳過、同暱稱且活著就沿用），或在 Err 路徑照 `rollback_create` 的方式收掉本次建立的東西。

**9. team.rs:3017–3107（`grow_workers`）＋ 1959（`refresh_unlimited_docs`）— 事後長出的執行者 worktree 沒有 `.agents-manager/team/.gitignore`，之後裸 `put_file` 寫進去的 `TEAM.md` 會被 `git add -A` 提交進 task 分支。** 信心：確定
- 觸發情境：無限模式 PM 對一個 issue 派兩筆 → `ensure_workers_for` → `grow_workers`：只做 `worktree_add` + `insert_member` + persona + `start_bot`，沒有呼叫 `write_team_docs`／`write_issue_doc`（全檔只有 1370 / 1724 / 1740 三個呼叫點）。接著 `refresh_unlimited_docs` 用 `tg::put_file`（team_git.rs:367，只 `create_dir_all`）寫 `<cwd>/.agents-manager/team/TEAM.md`。dev-2 `report` 時 `commit_all`（team_sched.rs:2400 → team_git.rs:235 `git add -A`）把它提交 → merge → `deliver=pr` 時一起 push。此外 persona（511）叫它讀 `ISSUE.md`，該檔不存在。有限模式 `PATCH workers.count` 調大同樣整個 `.agents-manager/team/` 不存在。
- 建議修法：`grow_workers` 建完 worktree 後呼叫 `write_team_docs`（有限）或 `write_issue_doc`（無限）；`refresh_unlimited_docs` 改走先寫 `.gitignore` 的 helper。

**10. team_sched.rs:1139–1143（`reopen_startup`）＋ team.rs:1490–1499（`check_add_issues`）— 沒有任何 `done` issue 的 `done` team 被 reopen／retry-failed 後永遠停在 `starting`，不是 `paused`、不能 resume、沒有 note。** 信心：確定
- 觸發情境：無限模式單 issue 撞 `budget_time`／`review_exhausted` → `close_one_issue(failed)` → 無 working、無 queued → `end_team` → `done`。`POST issues/retry-failed` → `check_add_issues` 只驗 `phase == done` 且 PM 未刪 → 放行、`set_phase(starting)`。`startup` → `reopen_startup` `filter(state == "done")` 為空 → `Err`；`spawn` 迴圈（138–144）對 Err 只 `warn` 後 `sleep(TICK)` 再試。同一個「`startup` 出錯只寫 log」的洞也吃掉 `Ctx::load` 失敗等錯誤。
- 建議修法：`previous` 改取 seq 最大的終態 issue（它只用來退執行者）；`tick` 對 `startup` 的 Err 寫 note 並 `sched_pause(upstream)`。

### S3（次要正確性／可靠性）

**11. team_git.rs:268–280（`merge_task`）— 任何非零退出都當 conflict；`git()` 回 Err 時不跑 `merge --abort`。** 信心：確定
- 觸發情境：(a) `index.lock` 殘留、`refusing to merge unrelated histories`、分支不存在 → `Conflict{files:[]}` → task `rebasing`、worker 被叫去「解衝突」（訊息寫「見 git 輸出」），兩次後 `paused(merge_conflict)`，原因誤導。(b) merge 逾時 → 269 行 `?` 直接回 Err，278 行的 `--abort` 不跑 → `main/` 留 MERGE_HEAD → resume 後 `integration_dirty`。
- 建議修法：只有 `diff --diff-filter=U` 非空（或輸出含 `CONFLICT`）才回 `Conflict`；Err 分支也執行一次 `merge --abort`。

**12. team_git.rs:82–91、github.rs:100–105、gh_auth.rs:224–229 — 本機 `sh -c` 逾時後子程序不會被殺（未設 `kill_on_drop`），卡住的 git／gh 繼續持鎖。** 信心：確定
- 觸發情境：`git merge`（pinentry）、`git push`、`gh pr create` 超過 timeout → `tokio::time::timeout` 丟掉 future，tokio `Child` 預設不殺 → hung git 仍握著 `index.lock`／`MERGE_HEAD`，下一次 `merge_one` 直接失敗（走第 11 條）。對照 gh_auth.rs:246 與 hosts.rs:145 都有 `kill_on_drop(true)`。
- 建議修法：三處加 `.kill_on_drop(true)`（或抽成一個共用 helper）。

**13. team_git.rs:98–102 ＋ hosts.rs:33 — `sh()` 遠端分支丟掉 `timeout` 參數，改走固定 30 秒的 `ssh_exec`；git_quick 的 push／pull 180 秒預算在遠端只剩 30 秒。** 信心：確定
- 觸發情境：遠端 project 按標題列 push／pull，大 repo 30 秒回 `502 ssh … timed out`，遠端 git 仍在跑。team 目前有 local-host 守門，所以 worktree add 的 300 秒暫未受影響。
- 建議修法：`hosts.rs` 給 `ssh_exec`／`ssh_exec_path` 一個 timeout 參數（`ssh_exec_stdin` 已有）。

**14. team.rs:2630–2651（`cleanup`）— `stop_bot` 失敗仍標 `deleted_at`；`workspace.close` 失敗（主機未連線，`close_workspace` 2163–2169 直接 return）仍無條件把 `workspace_id` 清 NULL，之後沒有任何路徑能再關那個 workspace。** 信心：確定
- 建議修法：`close_workspace` 回 `Result`，成功才清 `workspace_id`；主機未連線回 409；`stop_bot` 失敗不標 `deleted_at`。

**15. team.rs:3032–3054（`grow_workers`）— 先 `worktree add` 再 `insert_member`，任一步 `?` 失敗中途返回：已建 worktree 留在磁碟、前幾顆已啟動，但 `roles_json.workers.count`（patch 2972）沒寫回；重送同一 PATCH 時 `worktree_add` 撞既有目錄。** 信心：確定
- 建議修法：每顆失敗就地 `worktree_remove`；或先寫 `count` 再逐顆 best-effort 記 `member_start_failed`。

**16. team.rs:3383–3394（`say_with_delivery`）— 先 `record_event(kind=user)` 再 `prompt_grouped`：409 時時間軸多一則沒送出的插話；同 crid 重送每次多一則 user 事件並改寫 `turns.team_event_id`。** 信心：確定
- 建議修法：先 `prompt_grouped` 成功再 `record_event`（直接帶 `turn_id`）。

**17. team.rs:3992、3764、3782 — reconcile／respawn 的 guard pause 走無條件 `set_phase`（`write_phase` expect=None → `phase = COALESCE(NULL, phase)` 恆真），不是 SPEC-team §8.1 要求的 CAS；`resume_phase` 取自幾秒前的快照。** 信心：確定
- 觸發情境：reconcile 在 `check_worktrees`（git 子行程／ssh）期間使用者按暫停或 scheduler 進 `finishing` → 判定缺失 → 蓋掉使用者的 `pause_reason`，或把 `finishing` 改回 `working`（PM `done` 已消費，繼續後不再進 `finish`）。
- 建議修法：三處改用 `sched_pause(app, &t.id, reason, None)`（team.rs:930，已是 CAS）。

**18. team.rs:64 ＋ 3666–3683（`decide`）＋ team_sched.rs:1671–1683 — `decide` 對任何 `rebasing` task 放行（SPEC §10.5 只在 rebase 用盡時），UPDATE 不帶 `AND state=?`；第三次衝突後 task 留在 `rebasing`、不送 relay 就 `merge_conflict` 暫停，單純 resume 後整隊靜默停擺。** 信心：確定
- 觸發情境：(a) 第一次衝突、worker 正在 rebase → 使用者按「強制合併」→ 直接 `merging`，用沒 rebase 完的分支再合併；worker 恰好 `report` → scheduler 改 `merging`，`decide` 盲寫蓋回 `working`。(b) `attempts > MAX_REBASES` → `pause_issue(merge_conflict)`；SPEC §2.3 側欄表把 `merge_conflict` 歸到「繼續 = resume」，`resume_inner` 只擋 `member_lost`；繼續後 `advance_once` 對 `rebasing` 沒分支、`fill_issue` 因 open task 而 continue、PM `wait` 因有未終態 task 直接 return（2196），連 nudge 都沒有。
- 建議修法：`rebasing` 只在 `rebase_attempts >= MAX_REBASES` 時可決定；UPDATE 加 `AND state=?`，`rows_affected()==0` 回 409；`resume_inner` 對 `merge_conflict`／`review_exhausted` 仍有 `DECIDABLE_STATES` task 時回 409 要求先 `decide`。

**19. team.rs:1830–1843（`retire_workers`）— 忽略 `stop_bot` 的錯誤，仍軟刪 bot、`purge_bot_dir`、`worktree remove --force --force`。** 信心：確定
- 觸發情境：issue 收尾時主機／herdr 斷線：`stop_bot` 回 Err 被 `let _` 吞掉，agent 還在裡面跑的 worktree 被強制移除；重連後 agent 的 cwd 已被刪、`runs` 仍 active 但 bot 已刪。
- 建議修法：`stop_bot` Err 時只記 note 並跳過該成員（函式已冪等，留給下次 retire）。

**20. team.rs:1233–1254（`create_with_issues`）— `teams` 列寫入後，`team_issues` 迴圈失敗不走 `rollback_create`，留下沒有成員的 `starting` 殭屍 team。** 信心：確定
- 觸發情境：`issues` 參數沒去重（只有 `create` 的 `issue_list` 有），撞 `team_issues_number_open` → `?` 回傳；下次開機 `respawn_schedulers` 撿起 → `startup` 零成員 → `planning` 的空 team，沒有任何事件說明。
- 建議修法：`teams` + `team_issues` 放同一交易；或錯誤路徑呼叫 `rollback_create`。

**21. team_sched.rs:1351 ＋ 2130–2135 — startup 起不來的執行者仍留在 pool，PM 第一次派到它就 `paused(member_lost)`，`resume` 又要求它 running；§7.4「少一個人繼續」實際做不到。** 信心：確定
- 建議修法：startup 對起不來的 worker 直接 `deleted_at` + 退 worktree（或加 disabled 標記讓 `Ctx::workers()` 排除），note 寫明「少一個人繼續」。

**22. team_sched.rs:956–966（`flush`）— `prompt_grouped` 的 `needs_login`／`dialog_open`／`picker_open` Conflict（lifecycle.rs:3482/3501/3518）被歸成 `member_lost:<name>`，成員其實在跑；橫幅與「啟動並繼續」指錯方向，resume 立刻再暫停一次。** 信心：確定
- 建議修法：對映成 `member_blocked:<name>`（要人去終端處理，且 blocked→非 blocked 的自動 resume 對得上），note 帶原 reason 與 `message`。

**23. team_sched.rs:2400–2406（`report`）— `commit_all` 失敗只記 `auto_commit_failed` note 就繼續送審／合併；未提交的改動被排除在合併之外，之後 `retire_workers` 用 `--force` 移除 worktree 就真的丟了。** 信心：確定
- 觸發情境：worktree 沒有 `user.email`、磁碟滿、`index.lock` 殘留 → `commit_all` Err → 若之前有 commit 過則 `commits_ahead ≥ 1` → `reported` → 合併的是不含最後那批改動的分支。§6.3「nothing the worker wrote is allowed to be lost」。
- 建議修法：Err 時改為 `repair(...)`（請執行者自己 commit）或 `pause(upstream)`。

**24. team_sched.rs:2379–2394 ＋ 421–513（`Action::parse`）— `blocked_by_worker` 沒有任何 PM 端轉場（§8.2 寫「PM 下一則 relay 決定：改派／補充 brief／skip」），實際只有使用者 `decide` 能解；team 不暫停、UI 不提示。** 信心：確定
- 觸發情境：dev-1 回 `report{blocked}` → PM 收到「請決定下一步」，但 PM 的 `dispatch`／`wait`／`done`／`ask_user`／`abort` 沒有一個會碰這筆：`done` 因未終態被拒、`wait` 因有未終態 task 靜默返回、新 `dispatch` 只會排在它後面。
- 建議修法：至少讓 `blocked_by_worker` 進 `pause_issue`／`paused(blocked_by_worker)`；或實作 §8.2 讓 PM `dispatch` 可帶 `task` 欄位改回 `working`、`done` 時視同可 skip。

**25. tools.rs:259–267（`parse_shell_identities`）— 跑 claude 但 `CLAUDE_CONFIG_DIR=` 不在句首的 alias（`env CLAUDE_CONFIG_DIR=… claude`、`claude --settings CLAUDE_CONFIG_DIR=…`）被建成空 env 身份＝預設帳號，而不是跳過。** 信心：確定
- 觸發情境：`config_dir_of` 因前面有 `env` 這個字回 None（228–230），但 259–267 仍插入 `IdentityCfg{env:{}}` → 以該身份啟動的 bot 跑在預設帳號、額度折進裸 `claude` key、`pane_identity::child_identity` 也會把預設帳號的子 agent 判成它。註解 225–227 說「skipped rather than guessed at」只跳過了 config dir。測試 `ignores_aliases_that_are_not_ours`（942–945）以 `got.is_empty() || got[0].env.is_empty()` 兩種結果都接受。
- 建議修法：`config_dir_of` 回 None 且 `cmd.contains("CLAUDE_CONFIG_DIR=")` 時 `continue`。

**26. hosts.rs:234、243 ＋ api.rs:1500 — `hosts[].remote_path` 未經引用直接拼進 `export PATH={p}:$PATH`，`POST /api/hosts` 零驗證；plist（301）也未 XML escape。** 信心：確定
- 觸發情境：值含空白（`/Users/m4p/my tools/bin`）→ 每段 ssh 腳本第一行 `export: not a valid identifier`，PATH 只剩 `/Users/m4p/my`，之後 `herdr`／`claude`／`gh` 全找不到；含 `;`／`$(…)` 則以 ssh 使用者身分執行。信任邊界：持 UI token 的人本來就能開主機 shell，所以是「合法設定值把主機弄壞」而非提權。
- 建議修法：`format!("export PATH={}:\"$PATH\"\n", sh_quote(&p))`；`create_host` 驗證字集。

**27. herdr_shim.rs:88–93 — `--` 之後只認 `--model`／`--model=`，codex／grok 的 `-m` 與 codex 的 `-c model=` 不算「自己有寫」，同 kind 時再補 `--model $AM_MODEL` 蓋掉子 agent 自己的模型。** 信心：確定
- 建議修法：`--model | --model=* | -m | -m=* | model=*) _has_model=1 ;;`。測試 `a_child_without_a_model_inherits_the_parents` 只覆蓋 `--model`。

**28. herdr_shim.rs:138–148 — `agent prompt` 對任何不帶自家前綴的目標都改名（`am_child_name` 51–65），含 AGM、其他頂層 bot、pane id → herdr `unknown_target`，訊息沒送到，stderr 只印「已改名」。** 信心：確定
- 觸發情境：bot 依 CLAUDE.md「修正 Bot 直接向 AGM 申請」執行 `herdr agent prompt agm-pxf2pv …` → 目標變 `proj-abc123-agm-pxf2pv`。
- 建議修法：改名前先 `"$AM_HERDR" agent get "$1"`，原名存在就不動；`relay/announce` 用最後決定的名字。若規格真的只允許 prompt 子代，至少在 SPEC §6.5d 寫明。

**29. tools.rs:704 vs hosts.rs:33 — 遠端身份探測走 `ssh_exec_path` 受固定 30 秒（本機 `IDENTITY_PROBE_TIMEOUT` 90 秒）；多身份遠端逾時後所有身份都寫「auth status 探測失敗」，alias poller 不會重跑。** 信心：確定
- 建議修法：同第 13 條，`ssh_exec` 加 timeout 參數並傳 `IDENTITY_PROBE_TIMEOUT`。

**30. supervisor/controller.rs:86–91 — assignment 的 Conflict 分支無次數上限（Err 分支 110 行有 `RETRY_BACKOFF.len()` 上限），且全 repo 沒有任何地方寫 `status='cancelled'`（store.rs:64 與 API.md 都列了）；派給停掉的 bot 的工作每 5 分鐘重試到永遠，`pending_count`／`health.pending_assignments` 永不歸零。** 信心：確定
- 建議修法：加 `POST /api/supervisor/assignments/{id}/cancel`（只允許 queued／unknown）；或對 `bot has no active run` 這類 409 也套上限後 `failed`。

**31. supervisor/watchdog.rs:92 ＋ 108–112 — 第 5 次自動啟動「成功但 CLI 隨即死掉」時 `Plan::GaveUp` 是 no-op：沒有 status_detail、沒有事件、沒有 inbox，與 API.md「連續 5 次失敗就把原因寫進 status_detail」不符（只有第 5 次 `start_manager` 本身回 Err 才會報告）。** 信心：確定
- 建議修法：`GaveUp` 分支在第一次進入時寫 `set_status_detail` + `set_watchdog(5, None)` + `emit("supervisor_changed")` + 推 inbox。

**32. team.rs:2001–2009（`insert_member`）— 撞名備援 `{nick}-{tid6}` 會改變協定短名（`dev-1-<tid6>`），PM 的 `to: dev-1` 對不到；暱稱已帶 `t<tid6>`，跨 team 撞名不可能，備援只在 team 自己撞自己（第 8 條的重試）時觸發。** 信心：確定
- 建議修法：team 成員撞名直接回 409。

### S4（效能）

**33. pane_identity.rs:249–264 — 「這台還沒有該 kind 的身份」的檢查排在 `ps`／ssh 之後且不標 probed，每次 reconcile 都重問；正是註解 205–210 要防的重連風暴。** 信心：確定
- 觸發情境：主機沒有 codex／grok 的 `[[identities]]`（`ccN` 只產 claude），一個 codex 母 bot 開 3 個子 agent → 每個 `pane.agent_detected` 都 `pane.process_info` + `ps eww`（遠端一次 ssh、30 秒預算）× 3，永遠不停。
- 建議修法：把 `identities.iter().any(kind)` 移到 `env_of` 之前；`app.tools` 已有偵測結果而清單為空時視為終局並 `probed().insert`。

**34. github.rs:332–359 — `issues_cache` 只加不減，每個不同的 `q` 永久佔一份 issue JSON（唯一清理是 `close_issue` 的前綴 retain）。** 信心：確定
- 建議修法：insert 前 `retain` 過期項，或每 project 只快取無 `q` 的清單。

**35. quota.rs:352–358 ＋ models.rs:62、71–76 — 遠端主機沒裝 codex 時 `refresh_codex` 永遠不會回 `Ok(false)`：遠端分支 exe 退成字面 `"codex"`，pipeline 退出碼是 awk 的 0、stdout 空 → `Err("no response")` 不含 `is not installed` → 每 5 分鐘一次 ≥6 秒 ssh 並 warn；`GET /api/models?kind=codex&host=…` 同樣每次 6 秒後 502。** 信心：確定
- 建議修法：`refresh_codex` 開頭比照 `quota_grok.rs:294–299`，`cached_path` 為 None 直接 `Ok(false)`。

**36. quota_grok.rs:290–335 ＋ 342–349 — grok 探測沒有任何退避：裝了 grok 但 `/usage` 畫不出來（未登入、信任提示）的主機每 30 秒 `workspace.create` → `agent.start` → `agent_wait`（最長 60 秒）→ 讀畫面 25 秒 → 關 → warn → 再來；claude 那邊有 park（quota_claude.rs:541–607）與 §16.4 的 `logged_in == false` 不探，grok 完全沒有。** 信心：確定
- 建議修法：`tools` 對 grok 的 `logged_in == Some(false)` 就跳過；失敗後對 `quota_key(host,"grok")` 套同一套 park。

**37. quota.rs:372、quota_claude.rs:748、quota_grok.rs:342 — 三個 poller 對主機是依序 `for host … .await`，SPEC §14.3／API.md §12.4 寫「同一輪各主機併發（JoinSet）」；全 repo 只有 db.rs:1866 用到 JoinSet。** 信心：確定
- 觸發情境：一台慢的遠端 8 個 claude target 各最長 40 秒序列跑完，本機那一輪要等它；本機額度更新間隔被拉成數分鐘。
- 建議修法：每輪 `JoinSet::spawn(refresh_x(app.clone(), host))`（`probe_lock` 已 per-host）；或把文件改回「依序」。

**38. supervisor/store.rs ＋ health.rs:127–129 — `supervisor_inbox`／`supervisor_notes` 只增不刪（全 repo 無 `DELETE FROM supervisor_*`），`health_changed` 事件的 payload 是整份 health snapshot（含 assignment 全文＋整個 quota map）；`GET /api/supervisor/handoff` 每次對 inbox 全表排序。** 信心：確定
- 建議修法：`health_changed` 只存摘要；加 `created_at` 索引；handled 事件保留 N 天後清掉。

### S5（維護性／文件）

**39. team.rs:2533 — `close_issue` 是死碼（api.rs 只呼叫 `close_issue_for`），clippy 已報。** 信心：確定

**40. github.rs:214–215 — `.gitmodules` 路徑含空白時 `awk '{print $2}'` 截到第一個字，該 submodule 永遠對不上。** 信心：確定。修法：`sed 's/^[^ ]* //'`。

**41. github.rs:83–88（`parse_github_remote`）— `ssh://git@github.com:22/owner/repo` 被解成 owner=`22`、repo=`owner`（`strip_prefix(':')` 把 port 當 scp 語法）。測試只蓋無 port 的形狀。** 信心：確定

**42. team_sched.rs:1348 vs 1172、1242 — `member_failed:<name>` 三處寫法不一致：`startup` 用完整暱稱（`t<tid6>-pm`），reopen／rescue 用短名（`pm`）；`member_blocked`／`member_lost` 全用完整名。** 信心：確定

**43. docs — SPEC-team §6.2 表寫 task 分支 `team/i42-k3f9x2/t1-dev-1`，程式碼是 `<integration>-t<seq>`（team.rs:370–372，註解說明 git ref 目錄／檔案不能同名且不再帶執行者短名）；§7.3 表仍寫暱稱 `i42-pm`，程式碼是 `t<tid6>-pm`／`t<tid6>-i<seq>-dev-<n>`（401–407）；附錄 C 同樣過時。SPEC §14.2 對 claude 探測寫「`agent.wait idle|blocked`、信任對話框送 Down+Enter、hook 經反向通道」，程式碼是純 shell pane 打一行 `claude auth status --json; claude -p '/usage'`（quota_claude.rs:416–428），反向通道在 v4.3 已拆；API.md §12.4 已更新，SPEC 沒跟上。** 信心：確定

**44. quota_claude.rs:296–394 ↔ quota_grok.rs:211–287 — 探測骨架逐字重複（`probe_client`、`sweep_stale`、`client_for`、`month_num`、`clean`、`Probe` + `Drop`），兩邊都是 `sh -lc "exec herdr --session am-quota server"`；任何一邊修（退避、多掃一個 session）另一邊都會漏。另 herdr.rs:243、539、548 `socket_path`／`agent_prompt`／`agent_read` 死碼（lifecycle 直接 `call_timeout("agent.prompt", 10s)`，與 `agent_prompt` 的 15 秒兩套逾時漂移）；PATH_FIX 常數在 github.rs:120 與 team_git.rs:74 各一份；supervisor/controller.rs:88 Conflict 退避記進 `assignments.error` 的永遠是 `v.get("error")` = 字串 `"conflict"`，真正理由在 `reason` 鍵；supervisor `status` 文件列了 `failed` 但沒有任何 `set_status("failed")`，`get_handoff` 的 `open_assignments` 用截斷過的 200 筆算、`pending_count` 用全表算。** 信心：確定

---

## 可能發現

### team.rs / team_sched.rs
- **[可能] S3 team_sched.rs:952 — 冪等鍵綁 `pending[0].id`**：`prompt_grouped` 成功到 990 行 UPDATE 之間 daemon 被殺，重啟前同一 bot 又多了一則 pending E2 → 重試 crid 仍是 E1 → 冪等命中回舊 turn → E1、E2 都標 delivered，E2 內容 PM 沒看到。視窗很窄。修法：crid 含整批 id 的 hash，或冪等命中時只標 `turn_id` 對上的 rows。
- **[可能] S3 team_sched.rs:782 ＋ team.rs:1053–1079 — 額度閘門與建 team 預檢都不看 `fable` 桶**：成員 model=fable、`fable.used_pct=98` 而 5h/7d 都低 → 不擋 → relay 送出 → 回合失敗走修復提示到 `protocol_error`。是否真的拒答取決於 CLI。修法：`worst` 候選加 `fable`（或只在成員 model 是 fable 時計入）。
- **[可能] S3 team.rs:2424（`resume_inner`）— `set_phase(back)` 無條件寫入，中間夾 `check_worktrees` 空窗**：期間的 `abort` 會被改回 `planning`（`ended_at` 仍有值），`cleanup` 從此拒絕。修法：`write_phase(expect=Some("paused"))` 並要求 `pause_reason` 未變。
- **[可能] S3 team.rs:2451 ＋ team_sched.rs:1325–1330 — `abort` 與 `startup` 競態**：`starting` 階段按中止只停「當下已有 run」的成員，`startup` 繼續起 reviewer／dev（`sched_phase(planning)` 被拒後 return 但不停成員）→ team `aborted` 但 pane 還在跑。修法：`startup` 每起一顆前重讀 phase。
- **[可能] S3 team.rs:3737 ＋ team_sched.rs:1476 — `aborting` 中 daemon 重啟**：`respawn` 只 spawn，`step` 對 `aborting` 直接 return → 永遠停在 `aborting`，要再按一次 abort。
- **[可能] S3 team.rs:3261 — 無限模式 `patch_role(workers)` 對所有 issue 的執行者一起 swap，persona 用 `teams` 鏡像的 `issue_number`／`branch`**：i2 那顆拿到 #48 的 issue 與分支。
- **[可能] S3 team.rs:3183 — `patch_role` 換 kind 多顆逐一 swap，中途失敗 `?` 丟出**：已換的留新 kind、`roles_json.kind` 沒寫；busy 檢查（3173）與 `stop_bot` 之間無鎖，scheduler `flush` 可在中間送 relay 後被 `fail_in_flight` 標 failed → 走 repair。
- **[可能] S3 team.rs:3522–3551（`rescue`）— 把所有 issue 的 failed/skipped task 塞進一筆，分支只切自最新 task 所屬 issue**：#48 的修正 commit 進 #49 的分支。§2.6 是單一 issue 語意。
- **[可能] S3 team.rs:2714（`delete`）— `JoinHandle::abort` 砍 scheduler，await 中的 git 子行程（無 `kill_on_drop`）繼續跑，與緊接的 `worktree remove --force --force`／`rm -rf` 競爭**。修法同第 12 條，或先 CAS 到 `aborting` 讓 scheduler 自己退出。
- **[可能] S3 team.rs:2333（`pause`）— 沒拒絕 `aborting`**：寫成 `paused(user, resume_phase=aborting)`，繼續會寫回 `aborting` 並 spawn。§8.1 沒有這條邊。
- **[可能] S3 team.rs:2972（`patch`）— `budget_json`／`roles_json` 讀改寫無版本檢查**：「加碼並繼續」與另一分頁改 `workers.count` 同時 → 後寫蓋先寫，兩邊都 200。
- **[可能] S3 team.rs:1749–1756 — `start_issue` 的 `team_issues` UPDATE 不看 `rows_affected`**：與 `remove_queued_issue`（1599，只檢查 `state==queued`、不與 scheduler 互斥）競態時建出沒有 issue 列的執行者與分支。修法：UPDATE 加 `AND state='queued'` 並檢查影響列數。
- **[可能] S3 team.rs:1645–1655（`refresh_issue`）— gh 失敗靜靜變成「這個 issue 沒有內文」**：PM 只憑標題拆 task，沒有 note、沒有 pause；`create` 對同樣的錯是 502。
- **[可能] S3 team.rs:1193 ＋ team_git.rs:139 — 使用者的 `base` 直接接在 `git rev-parse --verify --quiet` 後面，沒有 `--end-of-options`**：`sh_quote` 擋住 shell，但 git 會把 `--show-toplevel` 之類當旗標；純自傷。
- **[可能] S3 team.rs:2057–2086（`rollback_create`）— 只寫 `failed`，不記事件、不推 WS**：失敗原因只活在 HTTP 回應裡，重新整理後側欄多一個灰 team 不知為何。
- **[可能] S3 team_sched.rs:2366 — 重送判定只擋 `report{done}`**：task 在 `reviewing` 時晚到的 `report{blocked}` 仍把它拉回 `blocked_by_worker`，reviewer 之後的 `verdict` 找不到 `reviewing` task 而吃修復提示。
- **[可能] S3 team_sched.rs:2153（`fill_issue`）— `checkout -b` 分支已存在就永遠失敗**：`checkout -b` 成功後、2159 UPDATE 前被殺 → 之後每次 resume 都 `branch_failed` + `paused(upstream)`。修法：先 `rev-parse --verify`。
- **[可能] S3 team_sched.rs:2103–2126（`fill_now`）— 由 HTTP handler（`resume_inner`、`decide`、`patch`）直接呼叫，與 scheduler task 的 `step → fill_workers` 沒有 per-team 互斥**：兩邊對同一分支 `checkout -b`，第二個失敗 → `paused(upstream)`；各挑不同 task 則撞 `team_tasks_one_open_per_worker` 回 500。SPEC §3 原設計是 mpsc 事件。
- **[可能] S3 team_sched.rs:2852–2861（`start_issues_up_to_capacity`）— 第 k 個 `start_issue` 失敗直接 `pause(upstream)` 返回**：前 k−1 個已 `working` 的 issue 沒有 `start_issue_workers`、沒有 `hand_issues_to_pm`；resume 後 `fill_now` 派工 → `member_lost`。
- **[可能] S3 team_sched.rs:1186–1188 — reopen 排進佇列後、`startup` 跑之前使用者移除那個 `queued`**：`Err("reopened team has no queued issue")`，同第 10 條的卡死。
- **[可能] S5 team_sched.rs:1376 — 無限模式 startup 在 `start_issues_up_to_capacity` 因 upstream 暫停（回 0）後仍 `hand_issues_to_pm`**，resume 後 startup 重跑再排一則，PM 收到兩則「第一批」relay。
- **[可能] S5 team_sched.rs:1777–1782 — `apply_reply` 失敗的 retry 文字說「沒有任何 task 被建立」，但 `dispatch` 是逐筆 INSERT 無交易**：PM 照指示重送會撞 `pm_repeat` 整隊暫停。
- **[可能] S5 team_sched.rs:1355 — 所有執行者都起不來 → `failed` 但不 `cleanup`，PM 起不來（1342）會 cleanup**；§6.5「建 team 失敗（任何一步）走同一個 cleanup」。有測試鎖定現行為，可能是刻意留現場。
- **[可能] S5 team_sched.rs:661 — `usage_json.per_bot` 以 `bot.name` 為 key，SPEC §9.2 寫 `<bot_id>`。**
- **[可能] S5 team.rs:805–812 — `record_event` 一律戳成「第一個 working 的 issue」**：無限模式 #43 的 relay／merge／note 全部 `issue_id=#42`；SPEC §4.5 只為 relay 預算讓步，但 `events?issue_id`／時間軸分組也用它。
- **[可能] S5 team.rs:1969–1979 / 1409 — `fail_open_tasks` 不 `emit_task_updated`；persona UPDATE 錯誤被 `let _` 吞**（成員用短 persona 啟動，之後每則回覆都沒有 am-team 區塊 → `protocol_error`，看不出根因）。
- **[可能] S4 team.rs:3835–3852（`is_within`）— 在 tokio 執行緒上同步 `std::fs::canonicalize`**，每個 scheduler pass 呼叫多次；慢碟／NFS 會卡 runtime worker。
- **[可能] S4 team.rs:1877–1891 — 撞到 `MAX_TEAM_WORKERS` 後每一次 `dispatch` 都記一筆 `worker_cap` note**，時間軸被同一句灌滿。
- **[可能] S4 team.rs:3969（`reconcile_teams_inner`）— 每次 reconcile 對每個 live team 各跑 `workspace.get` + `git worktree list`（遠端一次 ssh），reconcile 風暴時線性放大**；`respawn_schedulers` 有 `pruned` 去重，這裡沒有。
- **[可能] S4 team_sched.rs:1275 — `pm_context_lost_after_latest_reopen` 每次 reopen 讀整張 `team_events`（無 LIMIT）**。

### team_git.rs / github.rs / gh_auth.rs / group.rs
- **[可能] S3 team_git.rs:297–318 ＋ team_sched.rs:2653–2656 — PR 已存在時 `deliver_pr` 永遠失敗**：第一次 `gh pr create` 成功但寫 `pr_url` 前重啟，或使用者自己先開了 PR → 每次 resume 都 `deliver_failed`，沒有出口（與第 4 條疊加）。修法：失敗時 `gh pr list --head <branch>` 有就當成功。
- **[可能] S3 team_git.rs:344–348（`remote_base_branch`）— `base_ref` 是 sha／tag 時原樣回傳**，`gh pr create --base <sha>` 失敗 → `deliver_failed`。
- **[可能] S4 group.rs:205–223 — `@all` fan-out 序列 await 每個 `prompt_grouped`**，每個 RPC 逾時 10 秒，handler 可拖到 N×10 秒。
- **[可能] S4 gh_auth.rs:368–375 — `GET /hosts/{name}/gh` 每次都跑 `gh auth status`（遠端一次 ssh）**，device flow 期間前端每 2 秒打一次。
- **[可能] S5 group.rs:42 — `@` 前一個字是 CJK 時不算 mention 邊界**（`請@小幫手看一下` → 400 `no_mention`）。
- **[可能] S5 gh_auth.rs:306–315 / 432 — `auto` 模式會對使用者的 gh 設定做 `gh auth logout`**，API.md 只寫「switch」。
- **[可能] S4 gh_auth.rs:131–147 — `redact_secrets` 每個字元都重建剩餘字串（`chars[i..].iter().collect::<String>()`），O(n²)**；每個 502 訊息都經過它，實際毫秒級。

### hosts.rs / herdr.rs / herdr_shim.rs / tools.rs / pane_identity.rs
- **[可能] S3 herdr.rs:556–566（`subscribe`）— connect 與 ack `read_line` 沒有逾時，呼叫端（events.rs:70、247）也沒包**：herdr 半死時 `global_loop` 永久卡住，本機沒有 watchdog 會 abort 它。
- **[可能] S3 hosts.rs:397–411 — `apply_config`／`reconnect` 先 `abort()` supervisor 再 `kill_master()`**：task 停在 spawn 之後、`*self.master = Some(child)` 之前被取消，`ssh -M`（`kill_on_drop(false)`）殘留。修法：`t.abort(); let _ = t.await;` 再 kill。
- **[可能] S3 tools.rs:438、424 — 身份登入 pane 的 12 秒啟動寬限在慢主機會砍掉正在進行的登入**；`client_for` 失敗直接 return 不關 pane。
- **[可能] S3 herdr_shim.rs:142–146 — hook token 以 curl argv（`-H "X-AM-Bot-Token: …"`）傳遞**，Linux `/proc/*/cmdline` 對所有使用者可讀，≤2 秒視窗；遠端 `hook.sh` 本來就把 token 放 argv，這是把既有接受面延伸到本機。修法：`curl -H @-` 由 stdin 餵。
- **[可能] S3 tools.rs:632–639（`host_home`）— 遠端 `home()` 失敗退回本機家目錄**，identity env 的 `$HOME` 靜靜展開成本機路徑；`pane_identity` 用它比對不上後標 probed，該 pane 永遠不再問。
- **[可能] S4 hosts.rs:486 — 每次 (re)connect 都 `spawn_detect`，無去重**，主機抖動時偵測任務疊加、`app.tools.insert` 後寫的贏。
- **[可能] S5 tools.rs:342–368、757 — `record_identity_login` 不清 `reason`；`detect()` 整包覆蓋會把 claude 身份由 `/usage` 探測學到的登入答案清回 None**；另 doc 說會建列，程式碼對未知身份 `return false`（新用 `POST /api/identities` 加的身份在下次 detect 前登入答案被丟掉）。
- **[可能] S5 hosts.rs:623–637（`remove`）— 只清 quotas，`app.tools` 的 HostTools 快取留著**：同名加回另一台時 60 秒內 `identities_for_host` 回舊機器的 `ccN`。
- **[可能] S5 hosts.rs:39–43（`short_dir`）— 只用 uid 命名，兩個 daemon 互踢**（SPEC §14.6 已記）；家目錄不存在時 uid 退回 0。
- **[可能] S5 herdr_shim.rs:156–163 — 子 pane 只靠 `--env PATH` 帶 shim，沒有 daemon 那句 `pane.send_text export PATH=` 的補救**；孫代的 `herdr agent start` 打到真 herdr，命名前綴與 `--env` 轉發都失效。
- **[可能] S5 pane_identity.rs:120–131 — `ps eww` 的命令列也被當環境解析**，argv 裡出現 `CLAUDE_CONFIG_DIR=…` 字樣（persona 文字）會被誤認。

### quota / mem / models
- **[可能] S3 quota_claude.rs:322–336 / quota_grok.rs:217–231 — 開機時兩個 poller 同時對 `am-quota` session 各 spawn 一個 `herdr server`**（`sweep_stale` 幾乎同時 ping 失敗）；spawn 出來的 `Child` 也沒人 `wait`。
- **[可能] S3 quota.rs:275–286 — `fable`／`reset_credits`「新讀數沒有就沿用舊值」會把已消失的值永久留住**：codex 券用掉後若整個省略 `rateLimitResetCredits`、Max 降級後 `/usage` 不再印 Fable → 舊值蓋回直到重啟。修法：沿用只針對不認識該欄位的來源。
- **[可能] S3 quota_claude.rs:140–141、191 ＋ quota_grok.rs:69–117 — `resets_at` 把畫面牆鐘時間當成 daemon 主機的本地時區，括號裡的 `(Asia/Taipei)` 直接丟掉**：探遠端或帳號時區不同時差整個偏移，進 `pause_detail`、supervisor `quota_reset_at`。現有測試只斷言日期或兩個小時候選，時區錯也會過。
- **[可能] S3 models.rs:370–373、390 — `default_effort` per-model 覆寫用子字串比對，同一 alias 對到多個 key 時取字典序第一個**（`claude-opus-4-7` 排在 `claude-opus-5` 前）。
- **[可能] S3 memstat.rs:289–293 — `mem_updated` 的變化判斷不含 `browsers`**，分頁數 29→31 本身不推。
- **[可能] S3 memproc.rs:273–279 — `GET /api/mem/processes/pane?socket=` 讓持 UI token 的呼叫端指定任意本機 unix socket 給 daemon 連並送一行 JSON**；同使用者權限、API.md 明文如此設計，風險低。修法：限制在 `~/.config/herdr/**/herdr.sock`。
- **[可能] S4 api.rs:2035–2056 — `GET /api/quota?refresh=1` 在 HTTP 請求裡序列跑三種探測 × 每台主機**，7 個 `ccN` 全開時可到數分鐘；client 逾時砍掉後 handler 仍持 `probe_lock` 跑完。
- **[可能] S4 models.rs:480–492 — `list()` 只快取成功結果**，沒裝 codex 的遠端每次開模型選單 6 秒後 502（與第 35 條同一個洞）。
- **[可能] S3 memstat.rs:106–109 — `exe_name` 取第一個空白前的 token**，路徑含空白的 herdr 不被認成樹根，整棵樹不計、`kill` 一律 `NotInTree`。
- **[可能] S3 memproc.rs:98–104 — `env_value` 以空白切 token**，含空白的 `HERDR_SOCKET_PATH` 被截斷 → `pane_preview` 回「socket 不在了」。
- **[可能] S5 api.rs:2019 ＋ memproc.rs:319 — `signal` 不是 TERM/KILL 時靜默當 TERM**，API.md 寫只認兩者（應 400）。
- **[可能] S5 quota_claude.rs:654–658 ＋ hookrecv.rs:472–476 — 「哪些 identity 折進裸 `claude`」用 `env.is_empty()`，API.md §12.4 寫「有獨立 `CLAUDE_CONFIG_DIR` 的才各探一次」**；程式碼自洽，文件說法不同。

### supervisor
- **[可能] S3 supervisor/watchdog.rs:46 ＋ api.rs:67 — 只有 `POST /supervisor/stop` 會清 `desired_running`**（全 repo 唯一呼叫點）；使用者從側欄／`POST /api/bots/{AGM}/stop`／`bin/agm bot stop` 停掉 AGM，30 秒後 watchdog 自動拉回；刪掉 AGM bot 也會被連試 5 次。API.md 確實寫「不是經由這支 stop 而停掉…會自動再 start」，所以是設計；但 UI 的停止按鈕若沒改打 `/supervisor/stop` 就是使用者可感的錯行為。修法：`stop_bot`／delete handler 遇 `supervisors.bot_id` 順手 `set_desired_running(false)`。
- **[可能] S3 supervisor/controller.rs:81 ＋ store.rs:585 — `prompt` 冪等命中一筆 `delivery='pending'` 的舊 turn 時（daemon 在 `tx.commit()` 後、RPC 前被殺），assignment 被標 `delivered` 但 turn 永遠不結束**，目標 bot 從此 409；需要 lifecycle 開機對帳的行為才能定案。
- **[可能] S4 supervisor/mod.rs:156 ＋ api.rs:48 — 一把全域 `op_lock` 握著跨越 `lifecycle::prompt`（10 秒 RPC）與 `start_bot`（`agent.wait` 最長 60 秒）**，期間 `bin/agm assign`、controller tick、watchdog 全部排隊。不是死鎖。
- **[可能] S4 supervisor/controller.rs:309–328 — 一次 digest 沒有上限**，AGM 長時間 busy 時 pending 累積成幾百 KB 的 `agent.prompt` 文字。
- **[可能] S5 supervisor/api.rs:54–56 — `post_start` 的 `start_manager` 失敗 `?` 直接回錯，`set_desired_running(true)` 沒寫**，第一次啟動失敗後 watchdog 不接手；與 API.md「start 同時把總管標成應該在跑」有落差。
- **[可能] S4 supervisor_evidence.rs:57 — `LIKE '%q%'` 對 messages 全表掃描**，`limit` 只限回傳不限掃描；設計決定，表大時才痛。

---

## 最值得補的 5 個測試

1. **無限模式合併目標分支**（對應第 1 條）：`unlimited(vec![issue(), issue2()])` 後對 #42（鏡像）的 task 走完 report → approve → merge，斷言 `git rev-list team/i42-…` 含該 commit、`team/i43-…` 不含。現有 `done_delivers_one_issue_and_leaves_the_others_running` 只查 `state=="merged"`，`a_task_branches_off_its_own_issue_not_the_mirror` 只驗切分支不驗合併。
2. **`finishing` 內暫停後的 resume 與 `delivery_unknown` 的重送**（第 2、4 條）：佇列兩個 issue、把 `main/` 弄髒讓 `start_issue` 失敗 → resume → 斷言沒有第二則 `delivered`／`pr_created`、#43 進 `working`、team 不是 `done`；另一支用 mock `prompt_grouped` 回 `delivery=unknown` → abandon → resume → 斷言 pending 重新出現且 crid 不同。現有測試沒有任何 `delivery=unknown` 情境，`the_queue_carries_the_pm_across_issues_and_swaps_the_workers` 只走成功路徑。
3. **開機 replay 早於 scheduler 訂閱的補漏**（第 3 條）：先寫一筆 `completed` 的 turn（relay 已 `delivered`、task `working`）再跑 `step`，斷言 task 會前進。測試都直接呼叫 `step`／`apply_reply`（`enabled()` 在 cfg(test) 關掉背景 task），這個狀態沒被任何測試建構過。
4. **`swap_member` 對 `want_worker_bot_id`／`rescue_bot_id` 的重指 ＋ 無限模式 `to` 指名**（第 5、6 條）：兩筆 `to:"dev-1"` 後 `PATCH kind`、`fill_now`，斷言第二筆拿到新 bot；`dispatch {"issue":43,"to":"dev-1"}` 斷言 `worker_bot_id` 是 #43 的執行者。現有 `patch_changes_worker_kind_swaps_bot` 只種 `worker_bot_id=old`，`a_named_task_waits_for_its_own_executor` 只在單 issue 跑。
5. **`merge_task` 對非衝突失敗的分類 ＋ `sh()` 逾時後子程序是否還活著**（第 11、12 條）：分支不存在／`index.lock` 存在時斷言不回 `Conflict`；`sh(app, LOCAL, "sleep 30", 1s)` 後 `pgrep -f 'sleep 30'` 應為空。現有 `a_conflicting_merge_is_aborted_and_named` 只驗真衝突，`sh_local_preserves_exit_status_and_output` 只驗 exit code。

另外零覆蓋、值得排進下一輪的：`resume_if_member_unblocked`、`team::answer`、`retry_failed_issues` 觸發 reopen 的整段、`retire_workers` 在 `stop_bot` 失敗時、`reopen` 對「全部 issue 都 failed」的 team、`supervisor::mod::assign` 的三個 409／`controller::dispatch` 的 Conflict 分支／`watchdog::tick` 完整序列（supervisor 的狀態機路徑目前只測純函式）、`parse_shell_identities` 非句首 `CLAUDE_CONFIG_DIR=`（現有斷言兩種結果都接受）、`HerdrClient::subscribe` 逾時（herdr.rs 的 RPC 路徑零測試）、`quota::set` 的欄位沿用語意、reset 時間的時區。

---

## 沒讀到的檔／範圍限制

- 範圍內的 26 個檔（含 supervisor/ 八個）主體程式碼全部讀完；各檔的 `#[cfg(test)]` 模組（team.rs 3999–6125、team_sched.rs 3025–5281 等）只掃測試名稱與關鍵斷言，沒有逐行審測試碼本身的 bug。
- 被呼叫端只讀了驗證所需的片段：lifecycle.rs（`prompt_grouped` 3416–3565、`start_bot`／`stop_bot` 簽名、`needs_login` 等 Conflict、`child_agent_rules`）、db.rs（team／supervisor schema、`working_team_issues`、`team_members`）、api.rs（router 100–160、hosts／quota／mem handler、`create_host`）、events.rs 25–130、reconcile.rs 660–715、main.rs 228–302、hook_cmd.rs 的 spool、state.rs 的型別、scripts/agm.py 的 token 處理。這些檔**沒有審**。
- 沒有實跑 daemon／herdr 驗證任何一條；所有「確定」都是讀碼確認的靜態結論。特別是第 3 條（開機 replay 順序）依賴「本機 hook 在 daemon 停機時會寫 spool」這一點，我只以 hook_cmd.rs:104–105 確認，沒有實測。
- docs/FRONTEND.md 與 docs/UI-DECISIONS.md 只掃標題，沒有拿來核對 daemon 行為。
- web/ 的 `bunx oxlint` 不在我的範圍，沒跑。
