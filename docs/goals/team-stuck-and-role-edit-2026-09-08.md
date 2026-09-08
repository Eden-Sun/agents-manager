# Team 暫停後看得到、推得動，以及能換成員角色（2026-09-08）

執行者：opus。每項一個 commit，做完在此打勾。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。改了 API 同步 `docs/API.md`，改了 team 行為同步 `docs/SPEC-team.md`。

## 背景（實測）
issue #53 的 team（`01M1YJ8N0WGSF6K9CX5BRXS6E5`）`phase=paused`、`pause_reason=budget_time`（`max_wall_clock_min` 120，`usage.elapsed_min` 119），5 個 task 有 4 個 merged、第 5 個 `reported`。使用者從側欄只看到「2/20 已 4 小時 38 分」跟一顆褐色點，以為卡住。TeamPanel 裡其實有「加碼預算」「繼續」（`TeamPanel.tsx` ~1000–1030、1120）。

## 1. 側欄要看得出暫停、而且能當場推進
- [x] `web/src/components/TeamNodes.tsx`：`phase === 'paused'` 時在 team 節點那行**直接寫出**「已暫停 · <teamPauseLabel>」（沿用 `teamPauseLabel`），不要只靠 title tooltip 與顏色點。
- [x] 暫停原因是 `budget_time` / `budget_relays` 時，側欄節點加一顆小按鈕「加碼並繼續」（同 TeamPanel 的做法：budget 各加一倍後 `resume`）；其他原因給「繼續」（`controlTeam(id,'resume')`），`ask_user` / `gate:*` / `member_*` 則不給按鈕、只顯示原因（要進面板處理）。按鈕要 `e.stopPropagation()` 不要觸發選取。
- [x] 「已 X 小時 Y 分」在 paused / 終態時要**停住**：用 `usage.elapsed_min`（daemon 算的）或最後一次 `team_changed` 的時間，不要拿 `started_at` 到現在一直跳。
- [x] 查「2/20」是什麼：這個 team 只有一個 issue、5 個 task、4 個 merged，數字對不上。找出 `d525700`（「跑到第幾個」）算的是什麼，改成使用者看得懂的（例如 `task 4/5 merged`；有 issue 佇列時再加 `issue 1/3`）。若是算錯就修，並補 `teamPanelLogic.test.ts` 或新 test。
  - 查出來：**沒算錯，是沒寫單位**。這個 team 的佇列有 20 個 issue（`issues_summary.total = 20`），
    `teamProgressOf` 遇到佇列就只回 issue 序（done 1 + 當前 working 1 = 2），task 那組完全沒送出去，
    畫面上又是裸的 `2/20`。改成兩組都算、都畫，並寫出單位：`issue 2/20 · task 4/5`。

## 2. 能編輯／換成員角色
現況：`PATCH /api/teams/{id}` 的 `workers` 只接受 `model` / `effort` / `fast`（`daemon/src/team.rs` `patch` ~2320），`RoleSpec` 其實有 `kind` / `identity` / `persona_extra`。
- [ ] daemon：`PATCH /teams/{id}` 的 `workers` 多接受 `kind`、`identity`；並新增 `pm` 與 `reviewer` 兩個同形狀的欄位（`{"kind"?,"model"?,"effort"?,"fast"?,"identity"?,"apply"?}`）。
  - 只改 model/effort/fast/identity：同現有流程，寫回 `roles_json.<role>.spec`、更新該 bot 欄位，`apply:"now"` 就重啟該成員。
  - 改 `kind`：必須換 bot。做法：在同一 host/cwd 用同名規則建一個新 kind 的 member bot（沿用 `create_member`／建 team 時的路徑，含 persona、hooks、team 欄位），停掉舊 bot 並標 `deleted_at`（訊息保留），`team_tasks.worker_bot_id` 未終態的 task 指到新 bot，`roles_json` 更新。team 若在 `paused` 就維持 paused 讓使用者按繼續；若 `working` 且該成員有 in-flight turn，回 409 `{"reason":"member busy"}` 要使用者先暫停。
  - `check_role` 照舊驗 kind 已安裝、identity 存在且 kind 相符。
  - 記一筆 `team_events` note（`action:"patch"` 加 `role`、`from`、`to`）。
- [ ] `docs/API.md` §10.5 與 `docs/SPEC-team.md` §10.5 更新表格；§7 補一段「換成員 kind = 換 bot」。
- [ ] web：`TeamPanel.tsx` 現有「執行者模型」那塊改成三個角色各一列（PM / 執行者 / Reviewer），每列可改 kind（`KindTag` 三選一、未安裝的 disabled）、身分（claude 用 `IdentityOptions`）、模型／強度／fast（沿用 `ApiModelFields`），apply 選「下一批 / 立即」。新邏輯放新檔 `TeamRoleEditor.tsx`，`TeamPanel.tsx` 只掛進去。`store.ts` 的 `patchTeam` 型別擴充，`api/mock.ts` 對應。
- [ ] 側欄成員節點（`TeamNodes.tsx`）的成員列加一個小齒輪或用既有 `openSettings`，點了直接開到 TeamRoleEditor 對應角色（不用開 bot 設定；bot 設定改 team 成員會跟 roles_json 不同步）。

## 驗證
- `cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`（現有 team 測試要過，`resume_refuses_while_a_member_is_lost` 等）；補 `patch_changes_worker_kind_swaps_bot` 之類的 test。
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun test src/components && bun run build`。
- UI：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 或自寫 headless 腳本截側欄 paused 的 team 與 TeamRoleEditor，放 `docs/screenshots/team-role-edit/`。用 mock（`VITE_MOCK=1`）截也可以。
- 不要對 #53 這個真 team 做 kind 換人；只做 PATCH model 之類可逆的實測，或用 mock。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon、沒做到的與原因。
