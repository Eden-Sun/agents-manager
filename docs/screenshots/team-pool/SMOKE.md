# 併行數工作池：真機端對端煙霧測試（2026-09-08）

- Issue：[#56](https://github.com/Eden-Sun/agents-manager/issues/56)（丟棄用，已關閉）
- Team：`01M1ZQQBXYDS0EJB7T3SEKTRK7`，整合分支 `team/i56-ektrk7`
- 組態：PM claude/sonnet/cc1、workers `{count: 2, claude, sonnet, cc1}`、**沒有 reviewer**、`deliver: branch`、
  budget `{max_relays: 16, max_review_rounds: 1, max_wall_clock_min: 25, quota_stop_pct: 95}`、`supervised: false`
- daemon 沒有重啟，用的是 7788 上跑著的既有版本。

> API 路徑注意：`docs/SPEC-team.md` §10 寫的是 `POST /projects/{id}/teams`，實際 router 全掛在 `/api` 底下
> （`daemon/src/api.rs:126` 的 `.nest("/api", api)`），少了前綴會拿到 405。

## 時間軸（T0 = 建立 team 的回應時間，`1788845208` / 05:26:48Z）

| T+ | 絕對時間 | 事件 |
|---|---|---|
| 0s | 05:26:48 | `POST /api/projects/…/teams` → `200 {"team_id": …}`，phase `starting`，PM + dev-1 + dev-2 三個成員當場建好 |
| 12s | 05:27:00 | phase `starting → planning`，PM 收到 `first` relay |
| 23s | 05:27:11 | PM 回了計畫，phase `planning → working` |
| **24s** | **05:27:12.4 / .8** | **PM 一次派 3 筆**：`dispatch` t1→dev-1、t2→dev-2；t3 **沒有** dispatch。daemon 對 PM 回 `note`：「收到 3 筆。併行數 2：現在跑 2 筆（t1→dev-1、t2→dev-2），排隊 1 筆——排隊的會在有執行者空下來時自動派出，你不用再派一次。」 |
| 33s | 05:27:21 | 輪詢快照確認 **2 `working`（各有 `worker_bot_id`）+ 1 `queued`（`worker_bot_id == null`）** → `smoke-1-queued.json` |
| **40s** | **05:27:28.79 → .805** | t2 合併進整合分支（`b21c630c`）後 **16 毫秒**，daemon 自動把排隊的 t3 派給剛空下來的 dev-2 → **補位成立** |
| 46s | 05:27:34.5 | t1 合併（`942994fb`） |
| 53s | 05:27:41.7 | t3 合併（`5fe8cad9`） |
| **63s** | **05:27:51** | `PATCH /api/teams/{id} {"workers":{"count":3}}` → **`200 {"applied":"now"}`**，約 5 秒後 dev-3（`01M1ZQSBAV3MNVFWRXPFZS4YFW`）出現在 `members` → `smoke-2-scaled.json` |
| 62s | 05:27:50 | phase `working → finishing`，`delivered {branch: team/i56-ektrk7, pushed: false}` |
| 67s | 05:27:55 | phase `finishing → done` |

整條 team 從建立到 `done` **67 秒**，`relays` 用掉 9/16，`elapsed_min` 0。
交付內容正確：`team/i56-ektrk7` 上有 `docs/smoke/a.md`=`A`、`b.md`=`B`、`c.md`=`C`。

## 與預期相符 / 不符

**相符**

1. PM 不指定 `to` 一次派 3 筆，daemon 只放行 2 筆、第 3 筆 `queued` 且 `worker_bot_id == null`。
2. 一筆合併後**立刻**自動補位（16ms），不需要 PM 再派一次，PM 也沒有被逼著多跑一輪。
3. `PATCH {"workers":{"count":3}}` 回 `applied: "now"` 並真的多起一個執行者成員。

**不符 / 要注意**

1. **`applied:"now"` 的 PATCH 沒能驗到「排隊那筆當場被派出去」**——三筆一行檔太快，補位在 T+40s 就把佇列清空了，
   PATCH（T+63s）到達時已經沒有 `queued` task，phase 甚至已經走到 `finishing`。所以這裡只驗到了
   「當場多開一個執行者」，「多開後立刻補位」這半條沒有真實佇列可證。要驗那半條得用**執行者比 task 慢很多**的
   題目（例如 6 筆、每筆要跑測試）。
2. **`finishing` 的 team 也吃得下 `count` 調大**，多開的 dev-3 一個 task 都沒接就跟著 team `done` 了。
   §10.5 只規定「終態 409」，`finishing` 不是終態所以不算違規，但花成本起一個必然閒置的 bot 是浪費；
   值得考慮把 `count` 調大在 `finishing` 也擋掉（或只寫 `roles_json` 回 `next_batch`）。
3. **PM 給的三個 task 標題全都是字面上的 `"task"`**（見兩份 JSON 的 `title`）。看板與 `merge_note`
   都直接印這個字串，`t1 「task」已合併` 完全沒有資訊。這是 PM persona 的提示問題，不是工作池的問題，沒有動它。

## Relay 裡多餘的來回

`max_relays` 16 用掉 9，其中 **2 個 relay 是純浪費**：

- 05:27:25.9 `protocol_error`（`tektrk7-i1-dev-2`）→ `repair` relay → 05:27:28.7 才 `reply_ok`
- 05:27:31.6 `protocol_error`（`tektrk7-i1-dev-1`）→ `repair` relay → 05:27:34.4 才 `reply_ok`

兩次錯誤一模一樣：`report.status 必須是 done 或 blocked`。**兩個執行者第一次回報都寫錯了同一個欄位**，
各賠掉一個 relay 加約 3 秒。三筆任務就撞兩次，命中率 2/3——執行者的 persona 對 `report.status` 的
合法值講得不夠清楚（或沒給範例）。這是這次觀察到最值得修的浪費點，但屬於 persona 文案、不在本次任務範圍內。

PM 那側沒有多餘來回：沒有出現被迫 `wait` 的空轉輪次，`note` 只有 daemon 主動回的那則佇列說明。

## 清理

`POST /teams/{id}/cleanup` → `200`；issue #56 已 `gh issue close`；本機分支
`team/i56-ektrk7`、`-t1`、`-t2`、`-t3` 都 `git branch -D` 掉了（`deliver=branch`，從未 push）。
`docs/smoke/` 只存在於那條已刪除的分支上，沒有進 main。

> 另外：`team/i56-1rammb`（team `01M1ZQSECY4PW1E242TJ1RAMMB`，phase `paused` / `workspace_missing`，
> 組態是 fable + opus + cc0/cc2）在同一個 issue 上，但**不是這次測試建的**，沒有動它，也沒有刪它的分支。
