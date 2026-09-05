先唯讀規格與 herdr 相關資料，再針對矛盾、流程競態、hook、schema 與里程碑做第二輪審視。# Agents Manager 規格書 v2 第二輪審視（唯讀）

結論先講：**v2 已把「hook 早於 RPC 回應」寫進 §6.3，但仍有一條更致命的狀態缺口：Turn 先寫成 `queued`、hook 配對卻不認 `queued`。** 再加上 token 契約自相矛盾、active Run 沒有真正唯一約束、備援與晚到 hook 的「最近一筆」規則會配錯回合。這些都足以讓 demo 出現「有回覆卻進錯氣泡 / 重複氣泡 / 卡死不能再送」。其餘多半是可先砍範圍或補狀態機的問題。

---

## 1. 仍存在的矛盾、遺漏、不可實作處

- **§7.1 vs §4.1 / §4.4 / §4.1 v2 註記**：hook 驗證寫成「per-run token」，正文卻已改成 **per-bot**（`X-AM-Bot-Token`、`bots.hook_token`）。實作時若照 §7.1 會讓收養後的 hook 全部 401。
- **§6.3 步驟 3–5 vs §6.7 步驟 2（不可實作的狀態機缺口）**：送訊先把 Turn 寫成 `queued`，成功後才變 `sent`。配對候選卻只有 `{sent, delivery_unknown, completed_fallback}`。Claude `Stop` / Codex `notify` 常在 `agent.prompt` 的 socket 回應回來**之前**就打到 daemon → 此時 Turn 仍是 `queued` → 被當成 `origin=external` 新回合，稍後 RPC 成功再把原 Turn 標 `sent`，最後備援或下一則 hook 再配一次。**會雙氣泡或配錯。** v2 註記只保證「先寫 DB 再 prompt」，沒保證「寫完就是可配對狀態」。
- **§2 Turn 狀態含 `queued`，§6.3 又說第一階段「拒絕並 409」、同時「可排隊」**：`queued` 在送出前是暫態還是佇列？API 契約不清。前端重送、WS 重播都會無所適從。
- **§6.3 步驟 1「沒有進行中的 Turn」未定義集合**：`queued` / `sent` / `delivery_unknown` / `completed_fallback`（120 秒窗內）算不算進行中？`failed` 可否立即再送？不寫死會讓 409 與備援互相打架。
- **§7.2 `PATCH /bots/:id` 可改 `label`**：Bot 模型與 TOML 都沒有 bot `label`，只有 Project `label`。
- **§3.1「TOML 是期望設定的唯一權威」vs 附錄 C 的 `projects`/`bots` 表**：雙寫誰贏？UI 新增寫 TOML 再投影 SQLite，還是 SQLite 為執行時權威？daemon 啟動「補寫 id」與 UI 同時 PATCH 只有 mutex + mtime，**沒有 TOML↔SQLite 對帳步驟**（對帳 §6.5 只對 herdr）。
- **§6.2 步驟 2 在步驟 3 寫 Run 之前就 `pane.split`**：crash / 超時後 orphan pane。§6.5 只收養「snapshot 裡 name 匹配 Bot 的 agent」，空 shell pane 會永久殘留，下一輪「workspace 任一 pane」可能對到別人的 root。
- **§6.2「workspace 剛建立且 root 未被使用則用 root」**：同一 Project 第二個 Bot、或對帳後 root 已有無關 shell 時沒有判定函式（什麼叫 unused）。
- **§6.2 `env` 含 `AM_RUN_ID` vs 收養產生新 `run_id`（§4.1）**：hook 已改 bot_id，但環境變數仍過期；若之後有人用 env 除錯/配對會錯。應標「僅診斷，不作身分」。
- **§4.4 `AM_PORT` 在啟動 Run 時注入 vs daemon 重啟換 port（§5 `listen` 可改）**：舊 agent 的 hook 會打錯 port，只能靠 spool；但 spool 路徑在 bot 目錄、**舊程序不知道新 port**，要等對帳讀 spool——這點成立；若使用者改 `listen` 卻沒停 agent，應寫進對帳前提。
- **§4.1 Codex「轉呼叫原 notify」**：若使用者原 `notify` 已是本系統 hook，或原 notify 是長時間腳本，會遞迴或卡住（與 §4.4「絕不能卡住」衝突）。未定義偵測/跳過自身、以及 chain 的逾時。
- **§4.1 Claude 注入 `Notification` hook，§6.7 只處理 `Stop` / `agent-turn-complete`**：Notification（permission、idle reminder）會進 `/hook/claude` 卻無語意。若不忽略，會誤建 external Turn。
- **§6.7 去重鍵 vs 附錄 C `UNIQUE(native_session_id, native_turn_id)`**：web Turn 建立時這兩欄為 NULL。SQLite 的 UNIQUE **允許多個 NULL**，不會擋重複；若程式寫空字串 `''` 則第二筆 web Turn 直接 INSERT 失敗。未定義 NULL vs 空字串。
- **§6.7「候選 = 該 Run 中 created_at 最近者」且含 `completed_fallback`**：前一回合備援完成後 120 秒內，新回合已 `sent`，晚到的**舊** hook 會配到**新** Turn（或反向）。沒有用 `native_turn_id` / 時間窗 / 單調序對齊。
- **§4.2 去重 `(provider, native_session, turn_id)` vs 表上 UNIQUE 不含 `provider`**：Claude/Codex 理論上不會同表衝突，但規格兩套鍵不一致。
- **§6.5 收養「DB 無 active Run 則新建 Run」**：沒寫是否沿用 Conversation、如何處理舊 Run 仍 `running` 但 pane 已換 id（pane move 會換 `w*:p*`，§herdr skill）。`workspace_id`/`pane_id` 過期會對錯 pane。
- **§6.6 訂閱名 `pane.agent_status_changed` vs 附錄 A 線上事件 `pane_agent_detected`（底線）**：client 必須同時文件化「RPC method 用點、event 欄用 snake」。現在分散在兩處，實作易訂閱失敗。
- **§3.1 `events.subscribe` 後「每行為 event」**：未寫 subscribe 的 params（全域 vs 逐 pane filter）。若 herdr 要求每次訂閱帶 `pane_id`，§6.6「全域 + 逐 Run」要分兩條長連線或多次 subscribe；**一條連線一 RPC** 的約束下，事件連線能否後續再送 subscribe？附錄 A 暗示 subscribe 是該連線唯一請求。則「對每個 active Run 重新訂閱」可能做不到，除非一次 params 帶清單，或每 pane 一條長連線。**這是潛在不可實作點，v3 必須對 schema 寫死。**
- **§6.4 停止：`ctrl+c`×2 等 `pane.exited`**：agent 可能只中斷回合未退出；10 秒後 `pane.close` 合理，但 Turn 進行中的終態沒寫（應 `failed` 或 `delivery_unknown`）。
- **§6.3 步驟 6「blocked 時 Turn 維持進行中直到 hook 或回到 idle」** vs §4.3「working→idle/blocked 且 5 秒無 hook 就備援」：blocked 會觸發備援，把半成品畫面當回覆；與「維持進行中」矛盾。
- **§2.1 合成規則缺 `stopping`**：UI 狀態燈未定義。
- **§7.2 `POST /bots/:id/start` 無 `expect` 條件**：連點兩次、與 `autostart` 並行，未規定 409 vs 回傳既有 run。
- **§1 demo「依目錄列出」vs §3.2「依 Project 分組列出 Bot」**：小矛盾；以 Project 為準即可。
- **§8 `tokio-tungstenite（經 axum ws）`**：axum 0.8 通常用 `axum::extract::ws`，不必單列，易誤加第二套 WS。
- **本機存取：`GET /api/session` 發 token**：未寫檔案權限（`token`、`hook_token`、settings JSON 內嵌 token）。任何本機行程可讀 `~/.config/agents-manager/token`。demo 可接受，但與「僅 Origin/Host」敘事重疊不清。

---

## 2. §6.2 / §6.3 / §6.7 狀態機與 race：明確解法

### 2.1 §6.2 啟動

剩餘 race：

1. **並行 start**（UI 連點 + autostart + 對帳收養）。
2. **先 split 後寫 Run** 中途失敗。
3. **`agent.start` 已 `launch_pending`，`agent.wait` timeout**，agent 隨後才 idle（Run 被標 exited 又被 §6.5 收養成第二個 Run）。

解法（建議寫進 v3）：

- 以 `bot_id` 為鍵的 **async mutex**（整個 start/stop/reconcile/prompt 共用一把 per-bot 鎖）。
- DB 用 **部分唯一索引**（見第 4 節）保證最多一個 active Run；第二個 start `INSERT` 失敗 → `GET` 既有 Run，回 200 或 409（二選一寫死：demo 建議 409 + `run_id`）。
- 順序改為：**先 INSERT Run `starting`（佔位）→ 再 workspace/pane → `agent.start`**。失敗則 `exited` + 盡力 `pane.close`。
- wait timeout：**不要**立刻 `exited`+close；標 `running/unknown`，交給事件與對帳（規格 6.2 最後一點已有「有殘留則保留」，應刪掉「無則 close」裡對「其實還在啟動」的誤判，或 wait 後再 snapshot 一次）。
- root pane：僅當 `agent.get(pane)` 無 agent 且 pane 為 workspace root 且沒有其他 Bot 的 `pane_id` 指向它。

### 2.2 §6.3 送訊息

剩餘 race（最重要）：

1. **`queued` 窗口 vs 早到 hook**（上一節）。
2. **`agent.prompt` 回 `delivery_unknown` 但實際已送出**，hook 到達；若此時又允許使用者重送（不同 `client_request_id`），hook 配「最近一筆」會配到新 Turn。
3. **409 檢查與 INSERT 非原子**（兩請求都看到無進行中 Turn）。
4. **備援 5s timer 與 hook、blocked 交錯。**

解法：

- **單一寫入交易**（同一把 per-bot 鎖內）：
  1. 若存在 status ∈ `{queued, sent, delivery_unknown}` → 409（`completed_fallback` **不算**擋下一回合，或算——必須二選一；建議**算進行中直到 hook 補正或 120s 窗結束**，demo 可縮短窗到 15s）。
  2. `INSERT` Turn，**直接 `sent` 預留**或新增狀態 `pending_send`，但 **§6.7 候選必須包含這個狀態**。較乾淨的做法：狀態改為 `in_flight`（涵蓋現在的 queued+sent），RPC 結果只更新 `delivery` 欄（`ok | unknown | failed`），完成仍靠 hook。
  3. 若堅持 `queued→sent`：hook 配對候選改為 `{queued, sent, delivery_unknown, completed_fallback}`，且 `queued` 配上後仍標 `completed`（允許 RPC 稍後把 delivery 從 unknown 改 ok，用 CAS：`queued|sent → completed`）。
- `client_request_id`：`UNIQUE(conversation_id, client_request_id)` **WHERE client_request_id IS NOT NULL**；重送同 id 回同一 `turn_id`（已寫但未完成也回 200）。
- `delivery_unknown`：**禁止**對同一 bot 再 `prompt`，只允許 interrupt/stop 或使用者確認「當失敗」的 API。否則下一個 hook 必配錯。
- 備援 timer：只在 `delivery=ok` 且 `agent_status` 從 `working` 進入 `idle`（**排除 blocked**）啟動；到期時 `UPDATE turns SET status=completed_fallback WHERE id=? AND status IN ('sent','in_flight')`（CAS）。blocked 不啟動備援。

### 2.3 §6.7 配對

剩餘 race：

1. **「最近一筆」跨回合錯配**（fallback 窗 + 新 Turn）。
2. **spool 重放順序** vs 即時 HTTP hook。
3. **無 active Run 的 external** 與稍後收養 Run 重複入帳。
4. **重複 hook** 在 UNIQUE 尚未填 native id 時無法去重。

解法：

- 配對鍵優先級：
  1. 若 payload 有 `native_turn_id` 且表中已有相同 `(native_session_id, native_turn_id)` → 去重忽略。
  2. 否則配 **該 Run 唯一的 in-flight Turn**（不靠 `created_at` 搶最近）。不變式：**每個 active Run 最多一個 in-flight Turn**（與 §6.3 一致）。
  3. 若唯一 in-flight 是 `completed_fallback` → 更新該則 assistant message。
  4. 若沒有 in-flight → `external`。
- **禁止**在有 in-flight 時用「最近 completed_fallback」去吃新 hook。
- spool：daemon 對每個 `bot_id` **先停 HTTP 處理、鎖 bot、replay spool（檔案 flock + rename）、再接受 HTTP**。replay 與 HTTP 不可並行。
- SessionStart / 第一次 notify：只回填 `runs.native_session_id` / `transcript_path`，**不建 Turn**。
- Notification / 未知 `type`：ack 後丟棄，不建 Turn。

---

## 3. §4.4 hook 子命令是否足以不卡住 agent

**方向對（逾時、exit 0、空 stdout、spool），但不足以覆蓋真實卡住與資料遺失。**

已覆蓋：daemon 掛掉、HTTP 5xx、3 秒逾時、Stop hook 被當成決策 JSON。

遺漏情境：

| 情境 | 後果 | 建議 |
|---|---|---|
| Codex **轉呼叫原 notify** 無逾時 | agent 等 notify 結束 | chain 另計 1s，失敗忽略；若 argv 指向本 binary 則跳過 |
| stdin 極大 / 不 EOF | 讀 stdin 卡滿 3s 仍可能截斷 | 限制位元組（例如 1MiB），超限仍 exit 0 並 spool 標記 truncated |
| `AM_PORT` 錯誤、連 `127.0.0.1` 被 proxy | 3s 內失敗應 spool；若環境有 `HTTP_PROXY` 會怪 | hook 強制 `NO_PROXY=127.0.0.1`、連線逾時獨立（例如 300ms） |
| spool 磁碟滿 / 權限 | 寫入失敗若仍 exit 0 → **事件遺失** | 仍 exit 0（不卡 agent），但寫 stderr 到獨立 log（Claude 是否讀 stderr 需實測；較安全是只寫自己的 log 檔） |
| 併發 hook 寫同一 `hook-spool.jsonl` | 交錯 JSONL | `O_APPEND` + 一行一 JSON，或 per-event 檔 + 目錄 |
| Claude Stop 若未來「無 stdout = 阻停」 | 規格假設空輸出安全 | 在附錄 B 釘死；v3 加「禁止印 JSON，即使錯誤」 |
| `Notification` 高頻 | 打爆 daemon / spool | 子命令對非 Stop 可直接 exit 0 不 POST，或 debounce |
| settings 裡 `<bin>` 相對路徑 / 升級後 binary 搬家 | hook 失敗、agent 不一定卡但回覆消失 | 注入**絕對路徑**（附錄 B 已對 `--settings` 要求絕對路徑，bin 也要） |
| port 只在 env、agent 清環境 | hook 不知 port | `--port` 寫進 command 列（Claude command 字串、Codex argv 都要），env 當備援 |
| HTTPS-only / IPv6 (`::1`) | 連錯 | 寫死 `http://127.0.0.1` |
| daemon 處理 hook 的 handler 做 transcript I/O | 子命令 3s 內拿不到 200 → spool 重複 | handler 必須：**驗 token → enqueue → 200**，配對在背景 |

**保證不卡住的最低契約（建議升格為規範）：** 子命令 wall-clock ≤ 3s、永遠 exit 0、永遠空 stdout、永不等 chain 完成超過 1s、HTTP 只 POST loopback、失敗只 best-effort spool。

---

## 4. 附錄 C SQLite schema

- **缺「每個 bot 最多一個 active Run」的真正約束**：`runs_bot_active` 是普通部分索引，不是 `UNIQUE`。應為  
  `CREATE UNIQUE INDEX runs_one_active ON runs(bot_id) WHERE state IN ('starting','running','stopping');`
- **`turns UNIQUE(native_session_id, native_turn_id)`**：NULL 可重複；空字串會誤傷。改  
  `UNIQUE(native_session_id, native_turn_id) WHERE native_turn_id IS NOT NULL`  
  並加 `provider` 或規定 session id 已跨 provider 唯一。
- **`UNIQUE(conversation_id, client_request_id)`**：外部 Turn 的 `client_request_id` 為 NULL 沒問題；若誤寫 `''` 則只能一筆。應部分唯一 + 應用層禁止空字串。
- **`conversations.bot_id UNIQUE`**：永遠只能一條對話，與「可另行清除」尚可；若以後要多 conversation 必炸。demo 可留，在 §2 寫死「1:1 永不拆」。
- **FK 無 `ON DELETE`**：DELETE bot 保留歷史（§6.4）則 **不能** `ON DELETE CASCADE` 從 bots 刪；但 `bots.project_id` 刪專案時未說是否刪 bots 列。建議：軟刪或先刪子表；SQLite 預設 `PRAGMA foreign_keys` **關閉**，必須在規格寫「每個連線 `PRAGMA foreign_keys=ON`」。
- **`runs.bot_id` 無 `ended_at` 與 state 一致檢查**：`stopped/exited` 應 NOT NULL `ended_at`；active 應 `ended_at IS NULL`。可用 CHECK 或只在應用層（demo 可應用層）。
- **`messages.source` / `turns.status` / `runs.state` 無 CHECK**：typo 會讓狀態機靜默失效。建議 CHECK 或 enum 表。
- **時間欄 `TEXT`**：必須規定 RFC3339 UTC（或 integer epoch）。否則 `created_at` 排序與「最近一筆」不穩。
- **`last_read_tail TEXT`**：無長度上限；用 hash+offset 較穩。demo 可留但標「可能極大」。
- **缺表**：hook spool 在檔案不在 DB，OK；但 **workspace 映射**寫在 `projects.workspace_id`，對帳發現 workspace 消失時的 NULL 語意未寫。
- **`unread INTEGER` 在 bots**：與 messages 無關聯，重啟後無法從訊息重算。demo 可接受。
- **缺 `seq` 持久化**：WS 重播 1000 則若只在記憶體，daemon 重啟 `since` 必 resync。應在 §7.3 寫明（不必進 SQLite）。
- **TOML 與 bots 表同步**：schema 有 `name UNIQUE`（全域），TOML 也說全域唯一——好。缺 `hook_token` 輪換與「改 name 時 herdr 仍用舊 name」的遷移。
- **索引**：`messages(turn_id)`、`runs(pane_id)`、`turns(conversation_id, created_at)` 查歷史會用到，建議補。

---

## 5. 附錄 D 里程碑

順序大體合理（client → config/db → start/stop → hook 主路徑 → 備援/blocked → WS → UI → 內嵌）。問題在 **風險集中與驗收不夠「失敗態」**。

**風險最高：M4（hook 配對 + 不卡 agent），其次 M3（對帳/收養/不重複啟動）。**  
沒有這兩塊，M7 UI 只是空殼；M5 備援會把 M4 的 bug 放大成雙訊息。

降風險：

- **M3 拆出 M3a/M3b**：M3a 只 start/stop 單 bot、不殺 session；M3b 對帳（殺 daemon 留 herdr、再啟動、assert 同一個 pane_id、不第二個 agent）。驗收加：並行 `POST start`、root pane 第二個 bot。
- **M4 必須有時序測試（可先假 hook，不依賴真 Claude）**：  
  - hook 在 prompt RPC 回應前到達；  
  - daemon 停機時 Stop hook → spool → 重啟補訊息；  
  - 重複 Stop 同 `prompt_id`；  
  - Notification 不產生 Turn。  
  真 Claude/Codex 各一條 `Reply PONG` 即可，不要把「時序正確」綁在真實 CLI 上。
- **M5 延後或縮**：blocked + keys 對 demo 很加分，但依賴 herdr 辨識 blocked，不穩。可先手動 `POST keys` + 假 `agent_status=blocked`。
- **M6 放在 M7 前合理**；但「兩個客戶端 + 1000 seq」對 demo 過度，改成單客戶端重連一次 resync 即可。
- **M8 最後**：內嵌前端不要擋 M7（M7 用 Vite proxy）。

---

## 6. 對 demo 過度設計、可先留介面後實作

可 **API/型別先留、本體 stub** 的：

- **Transcript 回補（§4.2）**：格式不穩定，M4 不需要。留 `source=transcript` enum 與「重新載入」按鈕 disable。
- **Codex notify chain（§4.1）**：demo 宣告「覆蓋 notify」即可。
- **備援後 120s hook 覆蓋（§6.7.2）**：改成「備援後不再覆蓋，UI 標可能不完整」能少一整類 race；或窗改 15s 且僅當仍無新 in-flight。
- **WS 1000 則重播（§7.3）**：只實作 `resync` + `GET /state`。
- **`toml_edit` 保留註解 + mtime 衝突**：demo 用全量 serde 寫回；衝突回 409。
- **`rust-embed` / M8**：開發期 Vite + 反向代理。
- **終端分頁 `recent_unwrapped` 定時刷新（§3.2）**：blocked 時 `visible` 就夠。
- **未讀點、`POST /read`**：可硬編碼 0。
- **`expect_revision` on keys**：先只 `expect_run_id`。
- **手寫 herdr 全 subset 型別**：只包 M1–M5 用到的 method。
- **Project DELETE 關 workspace**：demo 可只停 bot。
- **Claude `Notification` / SessionStart 入對話**：SessionStart 只回填欄位。

**不要砍（demo 成敗）：** per-bot 鎖、active Run 唯一、in-flight 單一 Turn、hook 3s+spool+exit 0、對帳不重複 start、prompt 冪等 id。

---

## v2 → v3 應修改的具體條文清單

1. **§7.1**：`/hook/*` 改為驗證 **per-bot** `X-AM-Bot-Token`（與 §4.4、`bots.hook_token` 一致）；刪除 per-run token。
2. **§6.3 + §2 Turn**：定義 `in_flight` 集合；規定先寫 DB 的狀態**必須可被 §6.7 配對**（取消裸 `queued` 或把 `queued` 納入候選）。
3. **§6.3**：進行中 Turn 時一律 409（含 `delivery_unknown`）；同 `client_request_id` 冪等回傳；禁止 unknown 後再送新 prompt。
4. **§6.7**：刪「created_at 最近者」；改「每 Run 至多一 in-flight」為唯一配對目標；SessionStart/Notification 不建 Turn；去重用部分 UNIQUE。
5. **§4.3 / §6.3.6**：blocked **不**觸發終端備援；備援 CAS 僅 `idle`。
6. **§6.2**：先 INSERT Run `starting` 再 split；並行 start 409；wait timeout 不誤 close 仍在啟動的 pane。
7. **§6.5**：寫清事件訂閱如何在「一連線一 RPC」下做多 pane（一次 subscribe 多 filter **或** 全域 `agent_status_changed` 若 schema 允許；若都不行則每 pane 一長連線）。對齊 event 名稱點 vs snake。
8. **§6.5**：收養時更新 `pane_id`；orphan 無 agent pane 的回收策略；TOML↔SQLite 對帳。
9. **§4.4**：command 含**絕對路徑**與 `--port`；`NO_PROXY`；stdin 上限；spool flock；**禁止等待 notify chain**（chain 標為第二階段）；handler 先 200 再處理。
10. **§7.2**：PATCH 刪 bot `label`；`POST start` 重複行為；interrupt/stop 對 in-flight Turn 的終態。
11. **§2.1**：補 `stopping` 燈號。
12. **附錄 C**：`UNIQUE` 部分索引（active run、native turn、client_request_id）；`PRAGMA foreign_keys=ON`；時間格式；status CHECK；補索引；NULL 語意（native_*、workspace_id、ended_at）。
13. **§7.3**：daemon 重啟後 seq 從 0、客戶端必須 resync；1000 重播改為可選。
14. **§3.1 / §5**：TOML 與 SQLite 的權威與投影時機（建議：TOML=期望 bots/projects，SQLite=Run/Turn/Message/token）。
15. **附錄 D**：M3 拆對帳驗收；M4 加假時序測試；M5/M6/M8 縮範圍；§4.2 transcript、notify chain、備援覆蓋標「第二階段 / stub」。

以上皆不需改程式；v3 把狀態機收成「每 bot 一把鎖、每 Run 一個 in-flight、hook 只配那一筆」，demo 才接得起來。
