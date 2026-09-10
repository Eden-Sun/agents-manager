# Goal：team 做完之後，把「沒解決的任務」交給某一個成員收尾（2026-09-11 使用者）

## 現況
`phase=done` 的 team，`team_tasks` 裡可能留著 `failed` / `skipped` 的列。使用者只能重新排一次 issue
（§2.5 reopen），整個流程從 PM 規劃重跑一遍；沒有「只把沒解決的那幾件交給某個人處理掉」的路。
使用者的話：**擇一個 model 來處理所有失敗的，通常就是讓 reviewer 來做**。

## 要做
1. `POST /api/teams/{id}/rescue`，body `{"bot_id"?: "<成員 bot id>"}`。
   - 放行條件：`phase=done`、未 cleanup（PM 的 bot 還在）、且至少有一個 `failed`/`skipped` 的 task。
   - 收尾者預設 reviewer；沒有 reviewer 就要求指定。**PM 不能當收尾者**：它的 cwd 是整合工作樹 `main/`，
     在那裡切任務分支會弄髒整合現場。
2. 建**一個**收尾 task（不是每個失敗的各建一個）：`team_tasks_one_open_per_worker` 本來就限制一個成員
   同時只能有一個未終態的 task，而使用者要的就是「一次處理掉全部」。
   - brief 列出每個未解決 task 的 seq / 標題 / 原本的 brief 與最後回報。
   - 分支從那個 issue 的整合分支切出來，checkout 在收尾者自己的 worktree。
3. team 回到流程：`team_issues` 那一列改回 `working`、`teams.ended_at=NULL`、phase → `starting`
   （成員在 done 時被停掉了，要重新起來），並記一則 `team_rescue` note。
4. scheduler：
   - `teams.rescue_bot_id` 有值時，該 issue 的執行者就是它一個人（`workers_for`）。
   - `starting` 的 rescue 分支：重啟 PM／reviewer／收尾者，不建新執行者、不重新規劃，直接 `working`。
   - **自己不審自己**：`reported` 的 task 若執行者就是 reviewer，直接進 `merging`。
   - 收尾 task 進終態、issue 收掉時清掉 `rescue_bot_id`。
5. web：TeamPanel 在終態 team 上，若有未解決 task 就給一顆「交給 <成員> 收尾」（可選成員）。
6. 文件：`docs/SPEC-team.md` 新增 §2.6、`docs/API.md` 補端點。

## 不要做
- 不改既有 reopen（§2.5）與 decide（§8.2）的行為。
- 不動 `aborted` / `failed` 的 team：它們沒有可靠現場。

## 驗證
`cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`（含新測試：放行條件、
只有一個收尾 task、rescue 期間的執行者只有收尾者、自己不審自己）；web `bunx tsc --noEmit && bun run build`。
