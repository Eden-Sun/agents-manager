# agents-manager daemon 核心 code review（origin/main 3c6bf8a）

範圍：`daemon/src` 的 main / api / state / db / config / projection / lifecycle / hookrecv / hook_cmd / events / reconcile / attach / turn_error / tui_prompts / codex_live / agent_relay / default_session / bulk_restart / update_watch / changelog / assets / trust / shell / statusline_cmd（共 23 檔、約 21,700 行，全部逐行讀完）。輔助：`cargo clippy --all-targets`（0 error、48 warning，皆為 style／dead code），未跑 web lint（不在本範圍）。

## 總評

daemon 核心的不變量（per-bot 鎖、Turn 狀態轉場全走 CAS、`runs_one_active` / `turns_one_in_flight` 唯一索引、先寫 Run 再碰 herdr）設計清楚，2026-09-10/11 那兩次 reconcile／restart 競態的修法（單次持鎖、`agent.get` 二次確認、`agent.rename` 補名）都有對應的回歸測試，輸入處理（bot id 白名單、`sh_quote`、附件檔名清洗）也做得謹慎。剩下的風險集中在三處：一是 herdr 不配合時把 DB 留在中間狀態的路徑（子 agent 原地重啟卡在 `stopping`、DELETE team 成員假成功、default session 的使用者 pane 被關掉）；二是 reconcile 的子 agent 收編在名字撞到時會用 `?` 讓整台主機的對帳中止且每輪重複；三是鎖紀律有一個洞（progress poller 在鎖外呼叫 `try_fallback`，而 hook 端又不檢查 CAS 結果）。安全面最大的暴露是「開發版預設對 LAN 開放且 `/api/session` 直接發 token」，這是文件明寫的設計，但等同對同網段的無認證 RCE，值得再收斂一層。

## 確定的發現（依嚴重度）

### 1. 子 agent 原地重啟失敗會把 run 永久留在 `stopping`
- **位置**：`daemon/src/lifecycle.rs:2336-2364`（`restart_child_in_pane`）；`daemon/src/reconcile.rs:299`、`:366`（只把 `starting` 轉成 `running`）。
- **問題**：先把 run 設成 `stopping` 並 fail 掉 in-flight turn，之後若 20 次 `pane.get` 都沒看到 `agent: None`（或 `pane.get` 一直回錯），直接回 `Err` 離開，run 狀態沒有還原也沒有結束。
- **觸發**：`POST /api/bots/restart-idle` 或 `POST /api/bots/{id}/restart` 對一顆子 agent；agent 對 ctrl+c ×2 沒反應（例如停在一個 modal、或 herdr 對該 pane 的 `pane.get` 暫時失敗）→ 10 秒後回 502，run 停在 `stopping`。之後 `prompt` 回 409 `run is not running`、`start_bot` 對 child 一律拒絕、reconcile 的 `(Some(run), Some(agent))` 分支只會把 `starting` 改成 `running`，`bulk_restart::settle_failed_restart` 又因 `run_alive` 為真而不收尾——這顆 bot 在側欄變成永遠黃燈，直到父 agent 關掉 pane。
- **建議**：`!empty` 分支在回 `Err` 前 `UPDATE runs SET state='running'` 並 `emit_bot_status`；同時讓 reconcile 的 keep 分支把 `stopping` 且 agent 仍在的 run 也轉回 `running`（`CASE WHEN state IN ('starting','stopping')`）。
- **信心**：確定。

### 2. 子 agent 收編時名字撞到會讓整台主機的 reconcile 中止，且每輪重複
- **位置**：`daemon/src/reconcile.rs:505-563`（`existing` 查詢、`UPDATE bots SET parent_bot_id`、`INSERT INTO runs`）。
- **問題**：既有 child 的查詢鍵是 `(project_id, name, managed_by='child')`，不含 parent。同專案兩顆母 bot 各自開 `<自己>-ui` 時，第二顆的 child 會命中第一顆的 child 列：先把它 **re-parent** 到第二顆母 bot，再 `INSERT INTO runs`——那顆 child 已有 active run，撞 `runs_one_active` → `?` 直接讓 `reconcile_host` 回 `Err`。另一種：專案裡使用者自己有一顆叫 `review` 的 bot，某顆母 bot 開 `<parent>-review` → `existing` 因 `managed_by='child'` 條件查不到 → `INSERT INTO bots` 撞 `bots_name_project_live` → 同樣中止。
- **觸發**：任一上述命名組合。中止的後果：後面的 agent 不收編、orphan pane 不清、`reconcile_teams_on_host` 與 `fill_codex_runtime` 都不跑；而每個 `pane.agent_detected` 都會排一次 reconcile，所以 log 每兩秒一行 `reconcile failed`，直到那顆 agent 消失。
- **建議**：既有 child 以 `(parent_bot_id, runs.agent_name)` 或直接以 herdr agent 名查；名字撞到使用者 bot 時改用 `child_name_from_agent(name)`（完整 agent 名）；把每顆 agent 的收編包成獨立的 `Result` 並只 log，不用 `?` 讓整輪掛掉。
- **信心**：確定。

### 3. `DELETE /api/bots/{id}` 對 team 成員回 200 但什麼都沒刪，還清掉它的 hook 目錄
- **位置**：`daemon/src/api.rs:1330-1377`（`delete_bot`）；`daemon/src/projection.rs:162-171`（`managed_by != 'user'` 不投影）。
- **問題**：只有 `managed_by == "child"` 有特殊分支；team 成員走 `cfg.update`（config.toml 裡沒有它，retain 是 no-op）→ `reproject`（projection 跳過非 user bot，不會寫 `deleted_at`）→ `purge_bot_dir`（真的把 `bots/<id>/` 砍了）→ 回 200 `{"removed_children":[]}`。副作用是成員被 `stop_bot` 停掉、hook 材料被刪，但 bot 列仍活著。
- **觸發**：UI 或 `bin/agm` 對一個 `managed_by='team'` 的 bot id 送 DELETE → 200，側欄照舊有它，team 排程看到 `member_lost`。
- **建議**：`delete_bot` 開頭對 `managed_by == "team"` 回 409 `team_managed`（assignments 端點已有同名 reason），要真的退役走 `team::retire_workers` 那條路。
- **信心**：確定。

### 4. 停止／重啟從 default session 匯入的 bot 會關掉使用者自己的 pane，重啟還會在使用者的 default session 開 workspace（違反 SPEC §6.5.1）
- **位置**：`daemon/src/lifecycle.rs:2251-2253`（`stop_bot_locked` 無條件 `close_pane_and_tab`）；`:1924-1939`（`start_inner` 對 `session == "default"` 走 `workspace_create`）；`daemon/src/bulk_restart.rs:120-139`（`candidates` 不排除 `herdr_session='default'`）；`daemon/src/update_watch.rs:73-81`（`sweep` 也掃 default run）。
- **問題**：SPEC §6.5.1 說 default session 只觀察、pane 不由 daemon 回收；但 `stop_bot_locked` 對任何 run 都 `pane.close`，`start_inner` 對 default session 會 `workspace_create` 在使用者的 session 裡。匯入的 claude bot 是 `managed_by='user'`、kind claude，`update_watch` 會替它寫 `update_notice`，於是 `restart-idle` 會把它排進批次。
- **觸發**：使用者在自己的 herdr `default` session 開一顆 claude 在專案目錄 → daemon 匯入 → claude 自動更新 → 使用者按「一鍵套用更新」（或對那顆按重啟／停止）→ 使用者的終端 pane 被關掉，default session 多出一個 daemon 開的 workspace 與 agent（`inject_hooks=false`、`auto_approve=false`，還帶 `--resume`）。
- **建議**：`stop_bot_locked` 在 `session == "default"` 時只送 ctrl+c、不 `pane.close`；`start_bot_locked_with` 對 `bot.herdr_session == Some("default")` 回 409；`bulk_restart::plan` 多一個 `Skip::DefaultSession`。
- **信心**：確定（三段程式碼路徑都讀到，未在真機重現）。

### 5. hook 配對忽略 CAS 結果，而 progress poller 在鎖外呼叫 `try_fallback`，可能產生同一回合兩則 assistant 訊息
- **位置**：`daemon/src/hookrecv.rs:557-588`（`UPDATE … WHERE status='in_flight'` 後不看 `rows_affected`，照樣 `insert_message` assistant）；`daemon/src/lifecycle.rs:3720-3725`（poller 直接 `try_fallback`，未取 bot lock；`try_fallback` 本身以「呼叫端持鎖」為前提設計）。
- **問題**：`arm_fallback` 那條路有鎖，但 poller 的 14 秒空 composer 判定這條沒有。它與 `process_locked`（持鎖）並行時：poller 的 CAS 先贏並寫入 `terminal_fallback` 訊息，hook 的 CAS 輸了但仍插入 `source=hook` 的 assistant 訊息，且 `native_session_id/native_turn_id` 沒被蓋到那筆 turn，之後重送的同一個 hook 會走到「late hook」分支才被擋。
- **觸發**：hook 晚到約 14 秒以上（例如 hook 子命令排隊等鎖、遠端 spool drain 慢），agent 已在空 composer 停著；poller 先完成回合，hook 隨後補一則。
- **建議**：hookrecv 檢查 `rows_affected()==0` 就改走 late-hook 分支；poller 改為 `let _g = app.bot_lock(&bot_id).await.lock().await;` 再呼叫 `try_fallback`（或改成呼叫 `arm_fallback` 讓既有機制處理）。
- **信心**：確定（CAS 被忽略與鎖外呼叫是程式碼事實；實際重複訊息需要上述時序）。

### 6. 排隊中的 prompt 送出時跳過了 `needs_login` / `dialog_open` / `picker_open` 三道畫面檢查
- **位置**：`daemon/src/lifecycle.rs:220-310`（`flush_queued_locked`）對照 `:3474-3522`（`prompt_grouped` 的三道檢查）。
- **問題**：queued turn 被 claim 後直接 `agent.prompt`，沒有先讀 pane。
- **觸發**：codex 正在回合中，使用者在 web 排了下一句；期間使用者在終端打 `/model` 開了選單沒答；回合結束 → `working→idle` 排 flush → 文字被打進選單、Enter 順手換了模型——正是 2026-09-10 加 `picker_open` 檢查要擋的事。claude 的 `Switch model?` 與登入選單同理。
- **建議**：把三道檢查抽成 `pane_ready_for_prompt(app, &bot, &run) -> Result<(), (reason, hint)>`，`flush_queued_locked` 命中時 `requeue_turn` 並插同一則 system 提示。
- **信心**：確定。

### 7. 開發版預設對 LAN 開放，且 `GET /api/session` 對任何 peer 直接發 UI token（設計如此，但等同無認證 RCE）
- **位置**：`daemon/src/main.rs:318-332`（`dev_lan_default`：不在 .app bundle 就 `true`）；`daemon/src/api.rs:205-263`（`allow_lan` 讓 `peer_is_local` / `origin_is_local` 一律 `true`，`get_session` 回 token）。
- **問題**：拿到 token 就能 `POST /api/hosts/{name}/shells` + `/text` 執行任意命令、`POST /api/mem/processes/kill`、`git commit/push`、以任意 `args` 起 bot、`POST /api/hosts` 帶 `ssh_opts`（例如 `-o ProxyCommand=…`）。SPEC §7.1／README 明寫這是取捨，但「同網段任何裝置」在咖啡廳 Wi-Fi 或公司網路上不是可控的邊界。
- **觸發**：同一 LAN／Tailscale 上任何機器 `curl http://<ip>:7788/api/session` → 取得 token → 任意 API。
- **建議**：保留 0.0.0.0 綁定，但 `get_session` 只在 peer 為 loopback 時發 token（LAN 端由使用者從本機 UI 複製 token 或掃 QR 一次），其餘 `/api/*` 仍靠 `X-AM-Token`；或加一次性配對碼。
- **信心**：確定（行為）；是否要改屬產品決策。

### 8. 遠端 codex 的 `hook_token` 放在 argv 上（`ps` 對該主機所有使用者可見）
- **位置**：`daemon/src/lifecycle.rs:775-783`（`notify=[hook.sh, "codex", bot_id, hook_token]`）；對照本機路徑 `:398-404` 刻意不放 token（issue #43）。
- **問題**：這把 token 同時也是 `POST /relay/announce`（`api.rs:174-191`）與本機 `/hook/*` 的鑰匙；遠端 `hook.sh` 其實只把它原樣寫進 spool、daemon 重放也不驗（`hookrecv.rs:694-704` 沒比對 token）。
- **觸發**：多人共用的遠端主機上 `ps -ef | grep notify`。
- **建議**：`REMOTE_HOOK_SH` 改讀 `${AM_HOOK_TOKEN:-$3}`，argv 第三個參數送空字串或佔位。
- **信心**：確定。

### 9. `GET /api/bots/{id}/messages` 對不存在的 bot 回 502 而非 404
- **位置**：`daemon/src/api.rs:2323`（`db::conversation_id` 會 `INSERT INTO conversations`）；`daemon/src/db.rs:208`（`foreign_keys(true)`）。
- **觸發**：任意不存在的 id → FK 違反 → `{"error":"upstream"}` 502；API.md §1 說找不到應為 404。
- **建議**：先 `db::bot()` 判 `NotFound`。
- **信心**：確定。

### 10. 文件自相矛盾：手改 config.toml 的行為
- **位置**：`docs/SPEC.md` §18.6（「所有寫 config 的 API 會一路 409 直到 daemon 重啟」）對照 §5 與 `daemon/src/config.rs:412-436`（mtime 不符時在同一把 mutex 內重讀後套用，`issue28_tests` 驗證）。
- **建議**：§18.6 改成「會先重讀磁碟版本再套用；只有重新解析失敗才 409」。
- **信心**：確定。

## 可能的發現

### a. reconcile 用鎖前的 `agent.list` 快照覆寫 `agent_status`，可能吃掉一次 `working→idle` 邊
- **位置**：`daemon/src/reconcile.rs:299-308`；二次確認只在 `stale_possible`（pane 不同或無 run）時做（`:236-240`）。
- **情境**：reconcile 逐 bot 取鎖；前一顆 bot 的鎖被 `start_inner` 的 `agent.wait`（最長 60 秒）握著，後面每顆拿到的都是一分鐘前的快照。某顆在這期間 idle→working，reconcile 把它寫回 `idle`；真正的 `idle` 事件到時 `prev == idle` → 不 `arm_fallback`、不 `schedule_flush_queued`、不做 `turn_error` 掃描。
- **建議**：run 已存在時不從快照寫 `agent_status`（或一律 `agent.get` 一次，成本是一個 RPC）。

### b. progress poller 在 `try_fallback` 回 `Ok(false)` 後仍 `break`，安全網自己退場
- **位置**：`daemon/src/lifecycle.rs:3721-3725`；`try_fallback` 在 `pane_still_busy` / `is_tool_progress` 時回 `Ok(false)`（`:4553-4562`）。
- **情境**：畫面上殘留一行 spinner 形狀（或工具進度列），agent 其實已停在空 composer，herdr 又沒報 `working→idle`（註解裡描述的 grok 案例）→ poller 結束、回合永遠 in_flight。
- **建議**：`Ok(false)` 時 `quiet = 0; continue`，只有 `Ok(true)` 或 turn 不再 in_flight 才 `break`。

### c. default session 匯入的 agent 會因 `foreground_cwd` 暫時變動而每 8 秒被結束再收編
- **位置**：`daemon/src/default_session.rs:68-72`（`same_workdir` 精確比對 `foreground_cwd`）、`:143-150`（沒看到就 `mark_run_exited`）。
- **情境**：agent 正在跑一個在子目錄／worktree 裡的工具（前景 process 的 cwd 變了）→ 這一輪不匹配 → run 被標 exited、in-flight turn 失敗並插 system 訊息 → 下一輪又收編成新 run。
- **建議**：「消失」的判準改成 agent 名字不在 `agent.list`，cwd 只用於首次匹配。

### d. 主機斷線時 DELETE bot 會留下一個永遠 `running` 的孤兒 run
- **位置**：`daemon/src/lifecycle.rs:2219-2223`（`client_for_run` 失敗就 `?` 離開，run 未動）；`daemon/src/api.rs:1354`（`let _ = stop_bot`）。
- **情境**：遠端 host 斷線期間刪 bot → bot 軟刪、run 仍 `running`；reconcile 只走 `live_bots_on_host`，永遠不會再看它；`purge_deleted_bot_dirs` 因 active run 跳過；遠端 agent 繼續跑。
- **建議**：`delete_bot` 收到 `Upstream` 時對該 run `mark_run_exited`，或改回 502 讓使用者知道沒停成。

### e. `POST /api/bots/{id}/restore` 對 team 成員會把它寫進 config.toml 變成使用者 bot
- **位置**：`daemon/src/api.rs:1430-1463`（只有 `child` 走 DB-only）。
- **建議**：`managed_by != "user"` 一律只清 `deleted_at`。

### f. WS `seq` 可能亂序送出，重連時漏一則
- **位置**：`daemon/src/state.rs:293-303`（`fetch_add` 在拿 ring 鎖之前）。
- **情境**：兩個 task 同時 emit：A 拿到 5、B 拿到 6，B 先進 ring 與 bus。客戶端記最大 seq（`web/src/store/store.ts:310`）；若 socket 在 6 之後、5 送出之前斷線，重連 `since=6` → 5 永遠不補。
- **建議**：在 ring 鎖內配 seq。

### g. claude／grok 的 live 套用在 slash 指令被拒絕時仍把 `runtime_*` 標成新值
- **位置**：`daemon/src/lifecycle.rs:2858-2872`（`send_slash_line` 只認「沒跳確認框」就算成功）。
- **情境**：`/model` 帶一個 CLI 不認的 alias → TUI 印錯誤、模型沒換，daemon 卻寫 `runtime_model = 新值` 並回 `needs_restart: false`——正是 SPEC §4.4a 禁止的「靜靜顯示一個沒生效的值」。
- **建議**：比照 `codex_live::apply` 回讀畫面（claude 狀態列有模型名）確認後再寫。

### h. `turn_error` 對以「API error」開頭的一般回覆行會誤判
- **位置**：`daemon/src/turn_error.rs:33-36`、`:71-87`（最後一個非 chrome 行以 `api error` 開頭即命中）。
- **情境**：agent 回覆最後一行是「API error handling 已補上」→ 整個回合被釘上 `turn_error`、紅色 chip、system 訊息。
- **建議**：要求 `API Error:`（含冒號）或已知橫幅形狀（`Retrying`、`Connection lost`、`attempt n/m`）。

### i. `/hook/*` 對每個請求 `tokio::spawn` 等鎖，沒有上限
- **位置**：`daemon/src/hookrecv.rs:54-58`。
- **情境**：`start_inner` 持鎖 60 秒期間 statusLine 每次重繪都 POST 一次 → 幾十個 task 各抱一份最大 1 MiB 的 body。實務上有界，但沒有 backpressure。
- **建議**：StatusLine 類事件改成 per-bot「最新一筆」槽位（後到覆蓋前到），只對 Turn 事件排隊。

### j. `slice_after_cursor` 每次備援讀取做 O(n×400) 的 hash 與配置
- **位置**：`daemon/src/lifecycle.rs:4936-4947`。
- **情境**：200 行讀取約 1 萬字元 → 每次 `working→idle` 做約 400 萬次 char 操作與 1 萬次 String 配置；目前可接受，但它跑在每個回合結束與每次 hookless 擷取上。
- **建議**：rolling hash，或只在最後 N 個邊界找。

### k. 附件永遠不清理
- **位置**：`daemon/src/attach.rs`（`save` 寫入 `<project>/.agents-manager/attachments/` 與遠端時的本機副本 `<data_dir>/attachments/<bot>/`；沒有任何刪除路徑）。每個最多 12 MB。
- **建議**：刪 bot／專案時清；或保留 N 天。

### l. 沒有專案 workspace 可借時，host shell 關閉後留下空的 `shell` workspace
- **位置**：`daemon/src/shell.rs:96-111`、`:232-242`（只關 pane 與 tab）。
- **建議**：`close` 時若 workspace label 為 `shell` 且已無 tab 就 `workspace.close`。

## 維護性

- **dead code（clippy）**：`db::OPEN_ISSUE_STATES`（db.rs:1050）、`db::last_native_session_id`（只剩測試用）、`events::unwatch_pane`（events.rs:282）、`hookrecv::replay_all`／`reconcile::reconcile`（都掛 `#[allow(dead_code)]`）、`HookBody.received_at` / `truncated` 從未被讀（hookrecv.rs:24-26，spool 行的 `truncated` 資訊因此沒進 DB）。
- **重複實作**：`read_stdin_capped`（hook_cmd.rs:124 與 statusline_cmd.rs:186 兩份）；「ctrl+c ×2 → 輪詢 20 次」在 `stop_bot_locked`（lifecycle.rs:2230-2246）與 `restart_child_in_pane`（:2341-2361）各寫一次且行為不同（前者看 agent 或 pane 消失，後者只看 pane 空）；`remember_pane_cursor` / `_tx`；`codex_usage_notice_line` 的去 bullet 與 `strip_codex_bullet`；`insert_message` → `insert_message_grouped` → `insert_message_full` 三層轉發。
- **檔案尺寸**：`lifecycle.rs` 7,600 行同時裝了生命週期、終端擷取（`clean_screen`／`extract_reply`／回音剝除）、codex 額度公告解析與 900 行測試；擷取那一塊已有 `capture/` 模組可承接。
- **與 docs 不同步**：上面確定 #10；另外 SPEC §6.4「DELETE Bot：先 stop → TOML 移除 → DB deleted_at」沒提 team 成員；SPEC §6.5.1 與確定 #4 的行為不符。

## 最值得補的 5 個測試

1. **`restart_child_in_pane` 在 agent 不退出時的收尾**：mock pane 一直回 `agent: Some` → 斷言函式回錯後 `runs.state` 不是 `stopping`（或 reconcile 一輪後回到 `running`）。
2. **reconcile 子 agent 名字碰撞**：兩顆母 bot 各有 `<self>-ui`、以及專案裡已有使用者 bot `review` + `<parent>-review` → 斷言 `reconcile_host` 回 `Ok`、既有 child 的 `parent_bot_id` 不變、沒有多出一筆 run。
3. **DELETE team 成員 / default-session bot 的 stop**：前者斷言 409 且 bot 仍活；後者用 mock herdr 斷言 `stop_bot` 對 `herdr_session='default'` 的 run 從不送 `pane.close`，`bulk_restart::plan` 跳過它。
4. **Stop hook 與 `try_fallback` 的 CAS 競態**：先讓 fallback 完成回合，再送同一回合的 Stop hook → 斷言 assistant 訊息只有一則、`native_turn_id` 蓋在那筆 `completed_fallback` turn 上。
5. **queued prompt 遇到選單／登入畫面**：`flush_queued_locked` 在 mock 畫面為 codex picker 或 claude 登入選單時 → 斷言 turn 回到 `queued`、`agent.prompt` 沒被呼叫、對話多一則 system 提示。（加碼：reconcile 用過期快照時 `agent_status` 不得從 `working` 退回 `idle`。）

## 沒讀到的檔

本範圍 23 個檔案全部逐行讀完。範圍外但被本範圍依賴、只用 grep 抽查的：`hosts.rs`（`sh_quote`、`ssh_exec` 30 秒逾時）、`herdr.rs`（`call` 預設 15 秒逾時、`agent_wait` 逾時 +5 秒）、`tools.rs` / `pane_identity.rs`（確認沒有在持鎖路徑上再取 bot lock）、`web/src/api/transport.ts` 與 `web/src/store/store.ts`（WS seq 的處理方式）。`capture/`、`team*.rs`、`supervisor/`、`quota*.rs`、`group.rs`、`memproc.rs`、`models.rs`、`herdr_shim.rs`、`gh_auth.rs`、`github.rs`、`git_quick.rs` 未讀，其中 `capture::claude::PARSER` 的 `still_busy` / `awaits_input` 直接決定可能 #b 的實際觸發機率。
