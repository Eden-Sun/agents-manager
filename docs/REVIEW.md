# Codebase Review（Fable，2026-09-06，HEAD af1e12d）

範圍：`daemon/src/{lifecycle,hookrecv,hosts,events,reconcile,state,api,db,config,hook_cmd,main}.rs`、`web/src/{store/store,api/transport}.ts`。逐檔閱讀，非抽樣。每項標注嚴重度與建議修法；「狀態」欄由修復者更新。

## A. 正確性（建議立即修）

| # | 嚴重度 | 位置 | 問題 | 建議 | 狀態 |
|---|---|---|---|---|---|
| A1 | 高 | `reconcile.rs:37` | `client.agent_list().await.unwrap_or_default()`：herdr RPC 暫時失敗時 `by_name` 為空，`(Some(run), None)` 分支會把該 host **所有** active Run 標成 `exited`（鎖內的 `agent_get` 再查一次只在 RPC 恢復時有救）。下一次對帳雖會重新收養，但中間 in-flight Turn 已被 `fail_in_flight`。 | `agent_list` 失敗直接 `return Err`，讓呼叫端退避重試，不要以空清單對帳。 | 待修 |
| A2 | 中 | `lifecycle.rs:980 extract_reply` | 取整個緩衝區**最後一個** `⏺` 行。若本回合沒有產生標記（只有工具輸出），會把上一回合的回覆誤配給本回合；`last_read_tail_hash` 只在 fallback 時前進，hook 完成的回合不會推進游標，所以這條路徑實際會踩到。 | 先找最後一行 prompt 回音（`❯ `/`› `），只在其後搜尋標記；找不到再走 `clean_screen`。 | 待修 |
| A3 | 中 | `hookrecv.rs:35` | `receive` 只查 `db::bot` 不看 `deleted_at`：已刪除 bot 的殘存 agent 仍能打 hook，`process_locked` 會為它建立 external Turn 與訊息。 | `deleted_at.is_some()` → 401/410，並在 `process_locked` 開頭同樣防守（spool 重放路徑）。 | 待修 |
| A4 | 中 | `lifecycle.rs:147 REMOTE_HOOK_SH` | Claude stdin 為空或被 `head -c` 截斷時，`printf '…"payload":%s…'` 產生非法 JSON → daemon 400 → 寫入 spool → 每次對帳重放都 `unparseable`，永遠清不掉。`truncated` 也永遠是 false。 | `PAYLOAD=${PAYLOAD:-null}`；若 `printf '%s' "$PAYLOAD" \| head -c1` 不是 `{` 就包成 `{"raw":"…"}`（用 `sed` 逃逸引號）或直接 `null`；重放端遇到不可解析行改為丟棄並 log 一次。 | 待修 |
| A5 | 低 | `api.rs:80 origin_is_local` | `starts_with("http://localhost")` 會放行 `http://localhost.attacker.com`；`Origin: null` 也放行。因為沒有 CORS 標頭，瀏覽器讀不到回應，實際風險低，但 `/ws` 沒有 CORS 保護（只靠 token）。 | 解析 Origin 的 host:port，與 `listen` 精確比對；移除 `null`（file:// 不是支援情境）。 | 待修 |
| A6 | 低 | `events.rs:75` | 本機 global 訂閱失敗分支把 `connected=false` 但**沒有** `emit_daemon_status`，UI 燈號不會變灰，直到下一次成功才更新。 | Err 分支也呼叫 `emit_daemon_status`。 | 待修 |

## B. 穩健性 / 設計（排入下一輪）

| # | 位置 | 問題 | 建議 |
|---|---|---|---|
| B1 | `hosts.rs:330 spawn_supervisor` + `events.rs:63` | host 連上後 supervisor 對帳一次，`spawn_global_for_host` 的 `global_loop` 建立訂閱後**再**對帳一次；重連時對帳兩次。冪等但多餘，遠端每次對帳含 ssh 抽 spool（最多 30 s 鎖住該 bot）。 | 對帳只在 global 訂閱建立後做一次；supervisor 只負責 master 與訂閱。 |
| B2 | `hookrecv.rs:262 replay_spool_remote` | 在 per-bot 鎖內執行 `ssh_exec`（上限 30 s），期間該 bot 的 prompt / hook 全部排隊。 | 先在鎖外抽取 spool 內容，再進鎖逐行處理。 |
| B3 | `hookrecv.rs:282` | spool 中 `bot_id` 不符的行被跳過，但檔案已 rename+刪除 → 該行永久遺失。 | 寫回到對應 bot 的 spool，或至少落到 `hook.log`。 |
| B4 | `lifecycle.rs:672 prompt` | 在 per-bot 鎖內等 `agent.prompt` 最多 10 s；herdr 卡住時該 bot 的 hook 也被擋 10 s。 | 可接受；若要改，`delivery=pending` 先釋放鎖，RPC 完成後再鎖回寫 delivery。 |
| B5 | `state.rs:213 ensure_session` | `Command::new("herdr")` 依賴 PATH；由 launchd / GUI 啟動 daemon 時 PATH 可能沒有 `/opt/homebrew/bin`。 | 找不到時依序嘗試 `/opt/homebrew/bin/herdr`、`~/.local/bin/herdr`，或加 `server.herdr_bin` 設定。 |
| B6 | `lifecycle.rs:235` / `hook_cmd_parts` | codex 以 `-c notify=[…token…]` 注入，hook token 出現在 `ps` 的 argv（本機與遠端皆然）。 | 改為只傳 bot_id，token 由 hook 腳本從 `bots/<id>/token` 檔讀取（600 權限）。 |
| B7 | `hosts.rs:493 reconnect` | HTTP handler 內最長等 35 s 才回應。 | 回 202 並以 `host_changed` 事件通知；前端已能處理事件。 |
| B8 | `api.rs:714 get_messages` | `turns` 只回最近 `limit+1` 筆且與 messages 分頁無關聯；`before` 分頁時 turns 不會跟著往前。 | 依回傳訊息的 `turn_id` 集合查 turns。 |
| B9 | `db.rs` | `messages` 沒有 `(conversation_id, id)` 索引，`get_messages` 以 `id` 排序分頁。 | 加 `CREATE INDEX messages_conv_id ON messages(conversation_id, id)`。 |
| B10 | `web/src/store/store.ts:228 sendPrompt` | 伺服器回 `delivery: failed`（agent_blocked）時，前端仍先塞入一筆 `status: in_flight` 的假 Turn，直到 `turn_updated` 到達前 composer 短暫鎖住。 | `delivery === 'failed'` 時不要塞假 Turn，改 `loadMessages`。 |

## C. 已確認沒問題（避免重複審）

- Turn 狀態機：`in_flight` 唯一索引 + per-bot 鎖，hook / fallback / stall watchdog 三者互斥，CAS 更新皆檢查 `rows_affected`。
- hook 早於 `agent.prompt` 回應：Turn 先 commit 再 RPC，配對正確。
- 晚到 hook 不覆蓋 fallback：`native_turn_id IS NULL AND completed_at > now-120s` 標記後丟棄。
- `stop_bot`：先 fail in-flight → ctrl+c ×2 → 等 10 s → 一律關 pane → `stopped`；`pane_closed` 事件與之競爭時最終狀態一致。
- `ConfigStore::update`：mutex 序列化 + mtime 檢查 + 原子 rename。
- `hook_cmd`：panic hook、stdin 800 ms 上限、無 proxy、失敗落 spool、永不寫 stdout。實測 87 ms。
- hosts：`ExitOnForwardFailure`、`StreamLocalBindUnlink`、短路徑檢查、master 死亡偵測、退避重連、generation 防止舊 supervisor 復活。
- DB 遷移：additive `ALTER` 與 `projects` 重建都可重入。
- 前端：WS `seq` 追蹤與 `resync`、host 斷線燈號、composer 鎖定原因完整。

## D. 尚未審視

`projection.rs`、`assets.rs`、`web/src/api/{normalize,mock}.ts`、各 React 元件（僅看過行為截圖）。
