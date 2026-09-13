# 群組任務（mission）P4 端到端驗收（2026-09-13）

驗證者：agents-manager-9g62ss（c0-fable-畫面部分，cc0 / Fable 5.1）。對象：正式 daemon 7788 @ `59abe7d`（pid 45184，11:23 上線），
含 P1a/P1b/P1c daemon 與 CLI、P2 runbook（SPEC §18.14）、P3 web。任務 `01M2CDAG4QY36YGVJMJK8Q11SA`；負面案例用第二個任務
`01M2CDBWWJY33RRZ8XEAHHZ9V8`（建立後立刻取消）。截圖在 `docs/screenshots/missions-e2e/`。

## 結果一覽

| # | 項目 | 結果 |
|---|---|---|
| 1 | UI 建任務（7788，桌機 1400／手機 390） | **通過** |
| 2 | AGM 照 §18.14 跑：phase／assignments／events | **通過**（流程正確；缺陷見下） |
| 3 | CLI `mission list/get/pick` 與 API.md 一致 | **通過** |
| 4 | 負面：already_closed／not_verified／mission_id 缺 role | **通過** |
| — | 阻塞性缺陷 | **無** |

## 1. UI 建任務

步驟：群組 `/projects/01M1Y7BNVP843V9MFEDJ2KW9NQ` → 按「交給 AGM」→ 選項列出現（交付／執行者／撞到 5h 上限）→ 選
`開 PR 給我看`／`claude`／`等重置` → 輸入指示 → 按「交給 AGM」。

- `01-desktop-before-send.png`：開關開著、三組選項、送出鈕文字變成「交給 AGM」，收件者 chip 列隱藏，鎖定提示不出現。
- `02-desktop-after-send.png`：任務卡立刻出現：`規劃` 標籤、指示全文、`開 PR`／`claude`／`來回 0/2`、六段進度（規劃 亮）、
  「暫停」「取消任務」。
- `03-mobile-after-send.png`：390 寬同一張卡，高 249px，六段進度一行放得下，按鈕整寬。
- API：`GET /api/projects/{id}/missions?status=all` 回 `status=open, phase=planning, pr/claude/wait, rounds 0/2`。
- AGM inbox：`mission_created`，`event_key = mission:01M2CDAG…:created`，payload 含 mission_id。

## 2. AGM 執行過程（每分鐘輪詢 `GET /api/missions/{id}`）

| 時間 | phase | assignments | 事件 |
|---|---|---|---|
| 11:36 | planning | — | instruction |
| 11:38 | executing | executor `awaiting_review` (turn completed) | |
| 11:43 | awaiting_agm | executor `completed` | |
| 11:44 | reviewing | + reviewer `awaiting_review` | |
| 11:53 | awaiting_agm | reviewer `completed` | report（reviewer approve） |
| 11:54 | verifying | + verifier `delivered`（turn_status null） | |
| 11:55 | verifying | verifier `awaiting_review` | |
| 12:03 | — | verifier `completed` | verified → delivered（PR #64）→ completed |
| 12:04 | done | 三件都 `completed`，`turn_error` 皆 null | |

- phase 推導與 API.md 一致：交辦開著＝依 role；都結案但任務開著＝`awaiting_agm`；還沒交辦＝`planning`。
- 執行者 cc2/opus（commit `e7bcd18`，只改 `docs/API.md` +2）→ reviewer cc1/opus approve（AGM 用 `--exclude cc2`，cc1 Fable 週桶
  100% → pick 自動降 opus，符合規則）→ 驗證者 cc0/fable pass → deliver `pr` → PR #64（`mission/01m2cdag…` → main，state OPEN，
  files 只有 docs/API.md）→ complete。輪數 0/2。
- `04-desktop-done.png`／`05-desktop-done-list.png`／`06-mobile-done-list.png`／`07-desktop-done-row-expanded.png`：任務卡消失、
  「已完成任務（2）」摺疊列，展開後有摘要、`驗證` 證據、`開 PR` 分支名。
- 群組時間軸：執行者的回覆（「完成，未 push。worktree…」）、AGM 派給三顆臨時 bot 的 relay、reviewer／verified／delivered／
  completed 事件都在。

## 3. CLI

- `agm mission list --project … --status open` → `{project_id, missions:[…]}`，每筆含 `phase`、`assignments`；
  `agm mission get` 多 `events[]`——欄位與 API.md「物件」一節逐一對上。
- `agm mission pick --role executor` → `use cc2, model null`；`reviewer` → `use cc2`（未帶 `--exclude`）；
  `verifier` → **`use cc0, model "fable"`，reason「Fable 週桶有額度」**（cc2 讀不到 Fable 桶、cc1 用盡、cc0 43%）。
  pick verifier 後任務仍 `open`（只有 `ask_user` 才會停）。

## 4. 負面

| 操作 | 期望 | 實際 |
|---|---|---|
| `POST /missions/{id}/deliver`（沒有 verified） | 409 `not_verified` | 409 `{"error":"conflict","reason":"not_verified"}` |
| cancel 後 `pause` | 409 `already_closed` | 409 `{"reason":"already_closed","status":"cancelled"}` |
| cancel 後 `round` | 409 `already_closed` | 同上 |
| CLI `agm mission pause` 對已取消 | 非 0 exit ＋ http_error | exit 4，`{"error":"http_error","status":409,"detail":{…already_closed}}` |
| assignment 只給 `mission_id` 不給 `role` | 400 | 400 `mission_id and role (…) go together` |

## 5. 缺陷（依嚴重度；都不阻塞）

1. **臨時 bot 沒有刪除，只有停止**。`/api/state` 裡 `agm-mission-Q11SA-exec/-review/-verify` 三顆 `run=null`、`deleted_at=null`，
   側欄留著、群組 `@all` 從 10 個 bot 變 13 個（下一則 `@all` 會 fan-out 給它們並被 `skipped`），未讀列也多三顆 chip。
   §18.14 第 6 步寫「停止並刪除」。建議 owner：AGM runbook／agm.py（`complete` 之後 `DELETE /api/bots/{id}`），或 daemon 在
   `complete` 時把該任務所有 assignment 的 `target_bot_id` 且名稱以 `agm-mission-` 開頭的 bot 軟刪。
2. **AGM inbox 的 `mission_created` 沒有 ack**。兩則（含負面那個）都 `acked_at=null`；`pending_count` 會一直算著它們。
   建議 owner：AGM runbook（收到即 `agm ack`）；daemon 端可在任務 `done/cancelled` 時自動 ack `mission:<id>:created`。
3. **群組時間軸 relay 訊息標籤重複**：AGM 派給 bot 的那則同時顯示「你 → X」與「AGM → X」（`07-…png` 上方、
   `05-…png` 12:03 那則）。`relay_from` 有值時不該再畫「你 →」。owner：web（GroupChatPanel 的訊息標頭）。
4. **任務卡把指示印兩次**（標題行＋內文同一段，`02`／`03` 截圖）。手機 390 那張卡 249px，一半是重複的字。
   建議只留標題行，內文改放摘要或省略。owner：web `MissionsBar.tsx`。
5. **「已完成任務」清單含已取消的任務**（⊘ 那筆），API.md 寫「已完成任務清單＝`status=done`」。二擇一：文件改成 `done+cancelled`，
   或 UI 只列 done、取消的另放。owner：web／docs。
6. 小：verifier 交辦剛派出去時 `turn_status=null`、status `delivered`——API.md 只列了 `completed/…/turn_missing` 五種，
   沒說「還在跑是 null」。補一句即可。owner：docs。
7. 小：browser-gc 在 12:00 關掉了驗證用的 ego task space（違反 2 小時無活動規則），AGM 已修任務文；記在這裡供追溯。

## 6. 沒做到的

- 「執行中人為讓 cc2 撞限 → 自動換 cc1 接手」（goal §7 第二條端到端）沒有跑：正式環境無法安全地讓 cc2 撞限，需要玩具專案＋
  獨立 daemon，屬另一輪。
- review 退回一次的路徑（`mission round`）沒有實跑（AGM 這次一輪 approve）；只驗了 `round` 對已關閉任務的 409。
