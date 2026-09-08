# 併行數工作池：真機 end-to-end 煙霧測試（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、**不要重啟 daemon**——7788 已經是含 `3e5dedf`/`80bd8c5` 的新版，遷移也已跑過：`PRAGMA table_info(team_tasks)` 有 `want_worker_bot_id`）。

## 目的
`docs/goals/team-worker-pool-2026-09-08.md` 只用單元測試與 mock 驗過。這裡要用**真的 team** 證明：
PM 不指定 `to` 派 3 筆、併行數 2 → 2 筆在跑、1 筆排隊；跑完一筆自動補位；跑動中把併行數改成 3 會當場多一個執行者並把排隊的派出去。

## 步驟
1. 在**這個 repo**（`Edison/agents-manager`，用 `gh`）開一個丟棄用 issue：標題 `[test] worker pool smoke（自動測試，會關閉）`，內容要求「在 `docs/smoke/` 下各新增三個一行的檔案 `a.md`、`b.md`、`c.md`，內容分別是 A / B / C；三個檔互不相干，請拆成三個 task」。記下 issue 號。
2. 用 API（`docs/API.md` §team、`docs/SPEC-team.md` §10.1）對 project「AG Man」（id `01M1Y7BNVP843V9MFEDJ2KW9NQ`）開 team：
   - `pm`：claude，`model: "sonnet"`，identity `cc1`；`workers`：`{count: 2, kind: "claude", model: "sonnet", identity: "cc1"}`；`reviewer: null`（不審直接合，省 relay）。
   - `deliver: "branch"`，`budget: {max_relays: 16, max_review_rounds: 1, max_wall_clock_min: 25, quota_stop_pct: 95}`，`supervised: false`。
   - token 在 `~/.config/agents-manager/ui-token`，header `X-AM-Token`。
3. 用 `GET /teams/{id}` 與 `GET /teams/{id}/events` 每 10 秒看：
   - PM 第一次 `dispatch` 後：`tasks` 應有 3 筆，2 筆 `working`（各有 `worker_bot_id`），1 筆 `queued` 且 `worker_bot_id == null`。把這個時刻的 JSON 存到 `docs/screenshots/team-pool/smoke-1-queued.json`（只留 tasks 與 phase）。
   - 這時 `PATCH /teams/{id}` `{"workers": {"count": 3}}` → 回應要有 `"applied": "now"`；之後應多一個 `dev-3` 成員，排隊那筆變 `working`。存 `smoke-2-scaled.json`。
   - 若 PM 沒有一次派 3 筆（例如只派 2 筆），記下來、不要硬改 PM；改成觀察「跑完一筆後 PM 再派、補位」是否成立即可。
4. 讓它跑到 `done`（三個一行檔沒有 reviewer，應該 5–10 分鐘）；超過 25 分鐘預算會自己 `paused`，那就 `abort`。
5. 收尾（**一定要做**）：`POST /teams/{id}/cleanup`；`gh issue close <n> -c "smoke done"`；把 team 的整合分支 `team/i<n>-*` 與 task 分支 `git branch -D`（本機）——它們**不會**被 push（deliver=branch 只在本機）。確認 `git branch --list 'team/*'` 沒有這次的。`docs/smoke/` 不要留在 main（它只在 team 分支上）。
6. 把過程寫成 `docs/screenshots/team-pool/SMOKE.md`：時間軸（dispatch 幾筆、哪一刻補位、PATCH 後多久 dev-3 起來）、與預期不符的地方、以及你看到 PM / 執行者 relay 裡任何**多餘的來回**（例如 PM 被迫 `wait` 好幾輪、note 太多）。
7. 期間如果發現 daemon bug（補位沒發生、PATCH 沒起人、phase 卡住），**先記錄、再修**：修在 daemon 的話 commit 但**不要重啟 daemon**，在回報裡寫需要重啟。

## 回報
三到五行：issue 號與 team id、三個關鍵時刻的秒數、PATCH 是否 `applied: now`、有沒有 bug 與 commit hash、清理是否完成。
