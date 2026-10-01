# Spike #751：codex native thread Goals 能否承接 AGM 長期 assignment

版本化 canary 報告。**結論先講：可行但目前不採用；不改任何正式 dispatch。** 原因與下一步在 §4、§5。
原型（旗標後面、無任何呼叫端）：`daemon/src/codex_goal.rs`，旗標 `AGM_CODEX_NATIVE_GOAL=1`，預設關。

## 1. 版本與 protocol（2026-10-01 實測）

- `codex-cli 0.159.3`，`codex features list` 的 `goals` = `stable true`。
- `codex app-server generate-json-schema` 產出的 v2 schema 有：
  `thread/goal/set|get|clear`；通知 `thread/goal/updated`（`goal`、`threadId`、`turnId?`）與 `thread/goal/cleared`（`threadId`）。
- `ThreadGoal` = `{threadId, objective, status, tokenBudget?, tokensUsed, timeUsedSeconds, createdAt, updatedAt}`；
  `status` ∈ `active | paused | blocked | usageLimited | budgetLimited | complete`。
  `set` 的 `objective`/`status`/`tokenBudget` 都可省略（只改要改的），`clear` 回 `{cleared: bool}`。
- 只用公開 app-server JSON-RPC（stdio），沒有讀寫 codex 的 SQLite。

## 2. 真機結果（抛棄式 `CODEX_HOME`，沒帶任何憑證，所以沒有真的跑模型回合）

| # | Canary 項目 | 結果 | 證據 |
|---|---|---|---|
| 1 | 建 thread → set goal | **部分**：thread/start、`goal/get`（無 goal 回 `{"goal":null}`）、`goal/set`（`paused`、budget 5000）都成功 | 回傳 goal 物件與 `tokensUsed:0` |
| 1 | start work | **未測**：需要已登入帳號跑真回合 | — |
| 2 | app-server 重啟後 resume → goal 還在 | **通過**：kill app-server、新行程 `thread/resume`，`goal/get` 回同一個 objective / status / budget / createdAt | 同 thread id |
| 3 | interrupt / steer | **未測**（需真回合） | — |
| 4 | token budget / usage 不因 resume 歸零 | **部分**：budget 重啟後仍在；`tokensUsed` 只看過 0，沒有累積過的值可比 | — |
| 5 | complete / clear 事件綁 thread | **部分**：`status:complete` 可設；`goal/clear` 回 `cleared:true`，再 clear 回 `false`（冪等）；clear 當下收到的是 `thread/goal/updated` 而不是 `thread/goal/cleared`，順序／語意要在真回合再確認 | 見 §3 |
| 6 | queued user input 與 native continuation 不打架 | **未測**；原型只提供裁決函式 `continuer()` | 單元測試 |
| 7 | CLI 不支援 → 完全回現有流程 | **設計通過**：`plan()` 在旗標關、版本未知／< 0.159.0、非 child、非長期 assignment、thread 已有別的 goal 時一律 `Fallback(<原因>)` | 單元測試 |

錯誤處理：假 thread id 回 JSON-RPC `-32600 invalid thread id`，不會半成功。

## 3. 風險

1. **AGM 抓不到 TUI 的 app-server（最大）。** `lifecycle/start.rs` 啟 codex 一律帶 `--no-daemon`（每顆 TUI 自帶私有 app-server）。
   AGM 另起的 app-server（`models.rs` 那種一次性 RPC）看到的是同一份磁碟狀態，但**碰不到 TUI 行程記憶體裡的 live thread**：
   在外面 `goal/set` 不會讓正在跑的 TUI 開始或停止續跑。要真的驅動得二選一：
   (a) TUI 改用 `--remote unix://…` 連 AGM 擁有的 app-server（改啟動模型，影響面大，與「native transport」同級的決定）；
   (b) 走 TUI 內的 `/goal` 之類 slash 指令（要另外確認 0.159.x 有沒有；那就是畫面解析，回到現有的脆弱度）。
   這是 canary 前要先答的問題，答案決定值不值得做。
2. **雙重續跑。** native goal `active` 時 codex 自己會續跑；AGM 的 resume_nudge／queued prompt 若再推一次就是兩邊打架。
   原型 `continuer()` 規定：`active` → 只有 native；`budgetLimited/usageLimited/blocked` → 誰都不推；有 queued user input 時 `paused` 也不推。
3. **權威反轉。** goal 狀態只能當觀測值；cancel 用 objective 做 CAS（`should_clear_on_cancel`），晚到或重送的 cancel 不會清掉較新的 objective。
4. **相容性歷史**：Goals 過去出過 DB migration／resume 問題，每次 CLI 升級都要重跑 §5 的 canary；`MIN_CLI` 是下限不是保證。
5. **額度**：goal 續跑會自己燒額度，必須帶 `tokenBudget`，且 `usageLimited` 要接到 AGM 既有的 limit_hit 流程，不能被續跑蓋掉。

## 4. 結論

- 可行性：RPC 面完整、重啟後持久、冪等 clear、budget 欄位齊全 —— 契約本身可用。
- 但驗收要求的「找出至少一個比現有 resume_nudge／queued prompt **更穩**的可量測案例」**目前拿不到**：
  沒有帳號就沒有真回合，且風險 1 讓 AGM 現在根本沒有通道驅動 live thread。
- 因此本 spike 的暫定結論：**不採用、不開 implementation issue**；issue 保留到 §5 的 canary 有結果再決定關或開下一張。
  原型只留 pure 函式與測試，沒有任何呼叫端。

## 5. 建議的 canary 步驟（需要人／父 bot 提供一個已登入的拋棄式 codex 帳號環境）

1. 先答風險 1：`codex --remote unix://<sock>` 連一個由腳本起的 `codex app-server --listen unix://…`；
   另一個 client 對同一 thread `goal/set`，看 TUI 是否即時反應。答不出來就停，結論＝不採用。
2. 通過才做真回合：set `active` + `tokenBudget`，量 (a) 重啟後續跑是否自動、(b) `tokensUsed` 重啟前後單調、(c) interrupt 後 status、(d) complete／clear 通知順序與 `turnId`。
3. 對照組：同一個「做到驗收」任務，用現有 resume_nudge 跑 N 次，量**漏續跑／重複續跑／卡住**的次數；native 跑同樣 N 次。贏才有「更穩」的證據。
4. 全程只開在一顆非正式 bot、`AGM_CODEX_NATIVE_GOAL=1` 只設在該 bot；其他 bot 與正式 dispatch 不受影響。
5. 通過後第二階段才限定 managed_by=child、明確長期 assignment、一 thread 一 objective，另開 implementation issue。

## 6. 重現

```sh
H=$(mktemp -d); CODEX_HOME=$H codex app-server   # stdio JSON-RPC：initialize → initialized → thread/start → thread/goal/set …
codex app-server generate-json-schema --out <dir>  # 看 v2 schema 的 ThreadGoal*
```
用完刪掉 `$H`。
