# 設計草案：群組任務（AGM 自動調度的 mission）

狀態：**設計定稿，待 AGM 分配 ownership 後實作**（2026-09-13，agents-manager-mkng2n 起草；c0-fable-畫面部分 review；使用者已決策 D1–D8）。第 9、10 節優先於前文。

## 0. 使用者要的

1. 在**群組**（Project group chat，SPEC §13）裡直接下指示。
2. 由 **AGM 自動調度**完成任務，並且**派發 reviewer 與驗證者**。
3. 帳號調度策略：cc2 → cc1 → cc0。
4. 有一份**已完成任務**清單。
5. 用途和 Team Issue 很像但不同。

使用者已拍板的決策（2026-09-13）：

- D1 「已完成任務」＝專案底下一份可回顧的完成清單（原始指示、結果摘要、commit、驗證證據）。
- D2 交付方式**讓使用者選**：每個任務可選「直接推 main」或「開 PR 給我看」。
- D3 **驗證者必須使用「Fable 模型、且該身分 Fable 週桶還有有效額度」**的 bot。
- D4 帳號策略是**把一個身分的額度用盡才往下一個**（cc2 用到撞限才換 cc1，再換 cc0），**必須能抓到 hit limit 並切換**。

## 1. 跟 Team Issue 的差別

| | Team Issue（SPEC-team） | 群組任務（本案） |
|---|---|---|
| 入口 | IssuesBar 挑一個 GitHub issue | 群組裡一句話（`@agm …` 或輸入框「交給 AGM」開關） |
| 誰調度 | daemon Team Scheduler + PM bot（`am-team` 協定、星狀轉送） | **AGM**（LLM 判斷拆工與驗收），daemon 只提供狀態機與確定性的帳號挑選 |
| 角色 | PM / 執行者 / reviewer | 執行者 / reviewer / **驗證者**（真的跑 test、build、截圖） |
| 帳號 | 每個角色手選 kind/model/identity | 自動：cc2→cc1→cc0 **用盡才換**；驗證者強制 Fable 有效額度 |
| 隔離 | 每個執行者一個 worktree + 整合分支 | 每個任務一個 worktree（沿用 team 的 worktree helper） |
| 交付 | PR 或留分支 | 使用者每個任務選：推 main／開 PR |
| 結果 | team 面板 | 回報到群組時間軸＋「已完成任務」清單 |

## 2. 骨幹：沿用 supervisor assignment，不另寫 scheduler

現況（`docs/API.md` supervisor 段）：assignment 已有完整生命週期
`queued → delivered/unknown → awaiting_review → completed|failed|cancelled|superseded`（`blocked` 未結案），
`review` 是唯一結案路徑（`accept/fail/cancel/block/followup`），有冪等、退避重試、ownership 衝突回報、
稽核紀錄，送出的訊息帶 `relay_from = AGM`。

本案**每一步都是一筆 assignment**，只補兩個欄位把它們串成一個任務。

## 3. daemon（additive）

### 3.1 資料
- `missions`：`id, project_id, source_group_id, text, delivery_mode('push_main'|'pr'), status, result_summary,
  created_at, completed_at, max_rounds, rounds_used`。
  `status`：`planning → executing → reviewing → verifying → delivering → done | failed | cancelled | paused`。
- `supervisor_assignments` 加 `mission_id TEXT NULL`、`role TEXT NULL ('executor'|'reviewer'|'verifier')`。

### 3.2 入口
- `POST /api/projects/:id/chat`：目前沒有有效 mention 回 400 `no_mention`。新增 `@agm` 為有效目標 →
  建 mission（`delivery_mode` 由輸入框帶，預設 `pr`），寫一則群組時間軸訊息，並以 `relay_from="daemon"`
  通知 AGM（或放進 AGM inbox）。
- `POST /api/missions/:id/{pause,resume,cancel}`、`GET /api/projects/:id/missions?status=done`（已完成任務）。
- WS：`mission_updated`。

### 3.3 帳號挑選（確定性，放 daemon 不交給 LLM）
`GET /api/supervisor/pick-identity?kind=claude&role=executor|reviewer|verifier`

- **executor / reviewer**：固定順序 `cc2, cc1, cc0`，回傳**第一個尚未撞限**的身分——
  「用盡才往下」的意思是：只要 cc2 還能用就一直回 cc2，**不因為 `low` 就提早換**；
  判斷「不能用」只看 **hit limit**（見 3.4）與 `critical`（剩 < 5%）二者之一，
  以及身分被使用者停用。
- **verifier**：只收「Fable 週桶未撞限、且 `fable.critical=false`」的身分，模型固定 `fable`；
  順序同上。沒有任何身分符合 → 驗證步驟**排隊**（`waiting_quota`，帶最近的 `resets_at`），不降級成 opus。

### 3.4 抓 hit limit 並切換（本案最大的缺口）
現況：
- **codex**：已能從 pane 解析 `You've hit your usage limit … try again at …` 橫幅（`lifecycle.rs`
  `codex_limit_hit_line` / `join_wrapped_limit_hit`），並回寫額度（`apply_codex_limit_hit_quota`）。
- **claude**：只認得「usage limit reset available」這類提示行（`capture/claude.rs`），**沒有**回合中撞限的偵測；
  額度百分比來自 statusLine／`/usage` 探針，有延遲。quota 物件已有 `limit_hit` 欄位但 claude 端沒有填。

要補：
1. claude 回合中撞限的偵測（hook 回的錯誤訊息、終端橫幅如 `usage limit reached · resets …`、API 429），
   命中就把該身分的 `limit_hit = {at, resets_at, window}` 寫進 quota，發 `quota_updated`。
2. 撞限的回合 → 該 assignment 標 `failed`（`turn_status=limit_hit`）→ AGM 用 3.3 重挑身分，
   在**同一個 worktree** 開新 bot（下一個身分）接手，`followup` 帶上前一顆的進度摘要。
3. `limit_hit` 過了 `resets_at` 自動清掉，順序回到 cc2 優先。

## 4. AGM 的調度流程（persona / runbook）

1. **規劃**：讀指示，拆子任務，每個標 ownership；與其他未結案 assignment 前綴重疊 → 排隊。
   子任務太大或不確定 → 在群組問使用者，不猜。
2. **執行者**：`pick-identity(executor)` → 開臨時 bot（worktree）→ assignment(role=executor)。
3. **reviewer**：換一顆 bot（能換身分就換）只讀 diff，回 `am-review` 區塊（`approve|changes` + findings）。
   `changes` → `followup` 退回執行者；**上限 2 輪**，超過 → mission `paused` 並在群組問人。
4. **驗證者**：`pick-identity(verifier)`（Fable 有效額度）→ 乾淨 worktree 跑 repo 規定的驗證
   （本 repo：`cargo test`、`tsc -p tsconfig.app.json`、oxlint、build、UI 截圖），回 `am-verify`（數字＋截圖路徑）。
   失敗 → `followup` 退回執行者（計入同一個輪數上限）。
5. **交付**：依 `delivery_mode` 推 main 或開 PR；需要重建正式 daemon 時照既有規則申請。
6. **回報**：群組時間軸貼摘要（`AGM → 群組`，附 commit / PR、驗證證據），mission `done`，
   臨時 bot 停止並刪除。

## 5. web

- 群組輸入框：「交給 AGM」開關＋交付方式（推 main／開 PR）。
- 時間軸任務卡：五段進度、每一步的 bot／身分／輪數、撞限換手的紀錄。
- 專案底下「已完成任務」清單。
- 所有訊息帶 `relay_from`，藍色右欄只給使用者。

## 6. 防呆

- 每個任務的上限：review+驗證輪數、總回合數、總時間；碰到 → `paused` 並在群組問人。
- 所有身分都撞限 → `paused(waiting_quota)`，帶最近的重置時間，到點由 AGM 接回。
- daemon 重啟：沿用 assignment 對帳接回；mission 狀態由其 assignments 推導。

## 7. 驗收

- 純函式測試：`pick-identity`（用盡才換、verifier 只收 Fable 有效、全撞限排隊）、mission 狀態機、
  claude 撞限偵測（各種橫幅／錯誤字串）。
- 端到端（玩具專案、獨立 daemon）：群組下指令 → 執行 → review 退回一次 → 驗證通過 → 群組收到摘要，
  另跑一次「執行中人為讓 cc2 撞限 → 自動換 cc1 接手」。
- 文件：SPEC 新增一節、`API.md`、SPEC-team 註明差異。

## 8. 請 reviewer 特別看的疑點

1. 「用盡才換」與 `critical`（剩 < 5%）一起當門檻是否合理，還是只認 hit limit？
2. 撞限換手時「同一個 worktree 開新 bot 接手」是否可行（前一顆 bot 的未提交改動、session 無法跨帳號續接）。
3. verifier 找不到 Fable 有效額度時「排隊不降級」會不會讓任務卡很久。
4. mission 用 assignment 串起來，還是應該直接複用 Team Scheduler 的資料表。
5. AGM 是 LLM，拆工與驗收的一致性是否足夠，哪些判斷應該改成 daemon 的確定性規則。

---

## 9. Review 結果（2026-09-13，c0-fable-畫面部分 / cc0 fable）與修訂

已查證並採納（以下取代前文對應段落）：

- **3.4 更正**：claude 撞限已有偵測——`turn_error.rs:308 mark_claude_limit_hit` 會寫 `quota.limit_hit`；
  缺的只剩 hook 端 429 等其他字串，以及 claude 的自動清除（`hookrecv.rs:450` 只有 codex）。
- **撞限要進 assignment**：`turn_status` 沒有 `limit_hit`（SPEC §18.8），撞限在 `runs.turn_error`，supervisor 沒讀。
  controller 在 `park_awaiting_review` 時把 `run.turn_error` 抄進 assignment 新欄位 `turn_error`，換手才能確定性觸發。
- **換手不能續 session**：claude session 在各身分的 `CLAUDE_CONFIG_DIR` 底下（`tools.rs`），跨身分 `--resume` 找不到。
  只能「同 worktree、新 bot、新 session」＋ followup 摘要；執行者每輪回報必含「已做／未做／未提交檔案」，
  daemon 換手時自動附 `git status`。
- **入口只留「交給 AGM」開關**，不做 `@agm`（AGM 不是專案成員，會和 §13.2 `no_mention` 衝突）。
- **身分停用搬到 daemon**：目前停用是前端 localStorage（`web/src/store/quotaHide.ts:30,82`），daemon 看不到。
- **`push_main` 要新寫**：team 只有 `deliver=pr|branch`。推 main 一律 rebase origin/main → 整樹驗證 → fast-forward-only。
- **遠端專案**：team 的 worktree helper 本機限定（`team.rs:1164`），MVP 同樣只支援本機專案。
- **簡化**：不開 `pick-identity` route，改 daemon 內純函式讀 `GET /api/quota`；`missions` 不存 status，由最新 assignment 推導；
  「已完成任務」＝ assignments 的投影。
- **reviewer 用另一個身分才有意義**（同身分同 config dir、同額度）。
- **缺漏補上**：verifier 用私有 `CARGO_TARGET_DIR` 且同時最多一個 Rust verifier；mission `done` ≠ 上線（重建走 §18.10 租約）；
  換手留下的撞限 bot 由 daemon 在換手成功後軟刪；bot → 群組的回報路徑目前不存在，要新增；
  同一 mission 內不可平行改同一 ownership（`ownership_conflicts` 只回報不阻擋）。

## 10. 使用者決策（2026-09-13，第二輪）

- **D5 5h 撞限：開任務時選**「等 5h 重置」或「不等，直接換下一個身分」（mission 欄位 `on_5h_limit = wait|switch`，預設 `wait`）。
  7d 撞限或任一桶 `limit_hit` → 一律換下一個身分（D4）。
- **D6 驗證者找不到 Fable 有效額度 → 停下來問使用者**（mission `paused`，在群組說明各身分 Fable 的重置時間），不自動降級、不乾等。
- **D7 執行者 kind：開任務時選**（claude／codex／grok）。帳號輪換只對 claude 有效；codex／grok 撞限時只能等重置或停下來問（沿用 D5 的選項）。
- **D8 推 main 失敗 → 停下來問使用者**（非 fast-forward、rebase 衝突、整樹驗證沒過都算），不自動改開 PR。

以下兩點使用者未另外指定，採 review 建議當預設，實作時若要改再問：
- executor 撞到 **Fable 桶**：同一身分把模型切到 opus，不換身分（與 AGM 自己的 fable↔opus 規則一致）。
- **沒有第二個可用身分當 reviewer**：略過獨立 review，由執行者自審 ＋ 驗證者把關，並在任務卡與摘要上標「無獨立 reviewer」。

## 11. 開任務時的選項（綜合 D2、D5、D7）

群組輸入框的「交給 AGM」展開後：交付方式（推 main／開 PR）、執行者 kind（claude／codex／grok）、5h 撞限（等／不等）。

## 12. 實作分期（待 AGM 分配 ownership）

- **P1 daemon**：`missions` 表（不存 status）、assignment 加 `mission_id/role/turn_error`、controller 抄 `run.turn_error`、
  身分挑選純函式（D4/D5/D6/D7）、身分停用搬進 daemon、claude 撞限補 429 與自動清除、群組入口與 bot→群組回報、
  `push_main`（rebase → 整樹驗證 → ff-only，失敗 paused，D8）、WS `mission_updated`、測試、`API.md`／SPEC。
- **P2 AGM runbook/persona**：規劃→執行→review→驗證→交付→回報、`am-review`／`am-verify` 區塊、輪數上限、
  換手附 `git status` 與進度三段、私有 `CARGO_TARGET_DIR`、清理撞限 bot。
- **P3 web**：開任務選項、任務卡（階段、身分、輪數、換手紀錄、「無獨立 reviewer」「驗證者 Fable」標記）、已完成任務清單。
- **P4 端到端驗收**：玩具專案＋獨立 daemon，含「review 退回一次」「cc2 撞限換 cc1」「Fable 全用完停下問人」三條。
