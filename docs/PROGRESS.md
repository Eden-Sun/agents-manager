# 實作進度（agents-manager，daemon 後端）

負責範圍：daemon（`daemon/**`，不含 `daemon/src/hook_cmd.rs`）。前端 `web/**` 由另一 agent 負責。

---

## M1 — herdr client + session 管理

日期：2026-09-05

驗收指令與結果：

1. `env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE ./target/debug/agents-managerd serve`
   - log：`herdr socket not reachable; spawning server session="agents-manager"`
   - log：`herdr ping ok version=0.8.2 protocol=20` ✅
   - `ls ~/.config/herdr/sessions/` → `agents-manager` ✅（未動使用者 default session）
2. 手動在該 session 建 workspace 與 agent：
   - `herdr --session agents-manager workspace create --cwd /tmp --label m1test --no-focus` → `w1` / `w1:p1`
   - `herdr --session agents-manager agent start m1codex --kind codex --pane w1:p1` → `agent_status: idle`
3. daemon 以 `--dev-watch-all-panes` 重啟後 `herdr --session agents-manager agent prompt m1codex "Reply with exactly PONG"`：
   ```
   INFO watching pane agent status pane_id=w1:p1
   DEBUG pane.agent_detected data={"agent":"codex","pane_id":"w1:p1","type":"pane_agent_detected",...}
   INFO pane.agent_status_changed pane_id="w1:p1" status=working
   INFO pane.agent_status_changed pane_id="w1:p1" status=idle
   ```
   ✅ 事件名稱點號/底線兩種寫法都比對得到（`pane_agent_detected` 底線、`pane.agent_status_changed` 點號）。

已知問題：無。

---

## M2 — config 載入 / 補 id / 寫回、SQLite migrations、TOML→SQLite 投影、`GET /api/state`

日期：2026-09-05

驗收指令與結果：

1. 手寫 `~/.config/agents-manager/config.toml`（1 project + 2 bots，**沒有 id**）→ 啟動 daemon 後
   檔案被補寫成含 `id = "01M1S2SQPS9TA1DYRKNYCF2SJK"` 等 ULID ✅（同時補寫 `inject_hooks`）。
2. `curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:7788/api/state` → `401` ✅（需 token）
3. `curl -s http://127.0.0.1:7788/api/session` → `{"token":"…","port":7788}` ✅
4. `curl -H "X-AM-Token: …" .../api/state` → 回傳 1 project / 2 bots，`run: null`、`lamp: "offline"`、
   `connected: true`、`daemon_seq: 6` ✅
5. `docs/API.md` 已產出（`GET /api/state` 結構、REST 契約、WS 事件 JSON 範例），供前端 agent 使用。

已知問題：無。

---

## M3a — 單 Bot start / stop

日期：2026-09-05

1. 並行兩次 `POST /api/bots/{codex}/start`：一次 `200 {"run_id":"01M1S2WBD5…"}`、一次
   `409 {"error":"conflict","reason":"active run already exists","run_id":"01M1S2WBD5…"}` ✅
   （active Run 部分唯一索引 + per-bot 鎖生效）
2. `herdr --session agents-manager agent list` → `am-codex codex w2:p1 idle` ✅
3. 再 `POST /api/bots/{claude}/start`（4.06s）→ 走 `pane.split` 路徑，`am-claude claude w2:p2 blocked`
   （claude 的 trust 提示），`GET /api/state` 顯示 `lamp: "blocked"`、`run.state: "running"` ✅
   符合 §6.2.7「blocked → Run running，UI 顯示終端」。
4. `POST /api/bots/{claude}/keys {"keys":["down","enter"]}` → 6 秒後 `agent_status: idle` ✅
5. `POST /api/bots/{codex}/stop` → `200 {}`；pane `w2:p1` 上的 agent 消失、Run `stopped`；
   再次 stop → `204` ✅

## M3b — 對帳

日期：2026-09-05

1. kill daemon（不停 herdr，am-claude 仍活著）→ 重啟：
   ```
   INFO reconcile: kept active run bot=am-claude run=01M1S2WPGW0RJFQK53DXFYD18M pane=w2:p2
   INFO reconcile: closing orphan pane pane_id=w2:p1
   ```
   同一個 `run_id`、同一個 `pane_id`、`agent list` 仍只有一個 `am-claude` ✅
2. orphan pane 回收：已 `stopped` 的 Run 留下的無 agent pane `w2:p1` 被 `pane.close` ✅

## M4（daemon 端）— `/hook/*` receiver、Turn 配對、spool 重放

日期：2026-09-05

hook 子命令本身由 hook agent 負責（16/16 PASS，見 `docs/HOOK.md`）。以下是 daemon 端驗收：

1. **真 Claude**：`POST /api/bots/{claude}/prompt {"text":"Reply with exactly PONG"}` → `delivery: ok`；
   約 6 秒後 `GET messages` 出現 `[assistant/hook] 'PONG'`，Turn `completed`，
   `native_turn_id = 4c99e86d-…`（Claude `prompt_id`）✅
2. **SessionStart 不建 Turn**：假 hook `{"hook_event_name":"SessionStart","session_id":"sess-M4A",…}`
   → turns 數量 5→5 不變，`runs.native_session_id/transcript_path` 被回填 ✅
3. **token 驗證**：錯誤的 `X-AM-Bot-Token` → `401` ✅
4. **重複 prompt_id**：同一則 Stop 送兩次 → `turns where native_turn_id='pid-M4B'` 只有 1 筆、
   `DUPTEST` 訊息只有 1 則，log `duplicate hook ignored` ✅
5. **`stop_hook_active = true` 忽略**：log `hook ignored reason="stop_hook_active"`，訊息數 0 ✅
6. **hook 早於 prompt RPC 回應（決定性測試）**：`kill -STOP <herdr pid>` 後送 prompt，
   使 `agent.prompt` 卡到 10 秒逾時：
   ```
   herdr STOPped at 23:32:18.259
   hook posted 200 at 23:32:19.305     <- hook 早了 9 秒
   prompt returned at 23:32:28.318     -> delivery: "unknown"
   turn: ('web','completed','unknown','m4-race2','pid-M4C2')
   msg:  ('assistant','hook','RACE-OK-2')
   ```
   ✅ 先 INSERT Turn 再送 RPC 的設計讓早到的 hook 仍正確配對。
7. **`delivery=unknown` 禁止再送 prompt**：
   `409 {"reason":"a previous turn has unknown delivery; abandon it first","turn_id":…}` →
   `POST /api/turns/{id}/abandon` → `200` → 再送 prompt `200 delivery: ok`，回覆 `OK2`（source=hook）✅
8. **spool 重放**：daemon 停機時手動寫一行到
   `~/.config/agents-manager/bots/<bot>/hook-spool.jsonl` → 重啟 daemon：
   `INFO hook spool replayed bot_id="01M1S2…" replayed=1`，spool 檔已刪除，
   產生 `origin=external` 的 Turn 與 `SPOOL-REPLAYED` assistant 訊息 ✅

已知問題：
- **真 Codex 的 `Reply PONG` 無法驗證**：codex 帳號用量額度已用盡（終端顯示
  "You've hit your usage limit"），notify hook 因此不會觸發。codex 的 notify 參數注入
  （`-c notify=[…]`）與 payload 解析已由 hook agent 以手動 argv JSON 驗過。額度恢復後可補跑。
  副作用：這次 codex prompt 反而完整驗證了 §4.3 終端備援（見 M5）。

---

## M5 — blocked + keys + 終端備援

日期：2026-09-05

**blocked / terminal / keys**（用新專案 `/tmp/am-blocked-test` 觸發 claude 的 trust 提示；
codex 因額度用盡無法用「需確認的指令」觸發，改用等價的 blocked 情境）：

1. `POST /api/projects {"path":"/tmp/am-blocked-test"}` → 200；重複 → `409`；
   `POST /api/projects/{id}/bots {"name":"Bad Name"}` → `400` ✅
2. `POST /api/bots/{bt-claude}/start` → `lamp: "blocked"` ✅
3. `GET /api/bots/{id}/terminal?source=visible` → `agent_status: blocked`，內容為
   `Quick safety check: Is this a project you created or one you trust? … ❯ No, exit / Yes, I trust this folder` ✅
4. blocked 期間 `POST prompt` → `409 {"reason":"agent is blocked; answer the prompt first"}` ✅
5. `POST /api/bots/{id}/keys {"keys":["down","enter"]}` → 6 秒後 `lamp: "idle"` ✅
6. `POST keys {"expect_run_id":"NOPE"}` → `409` ✅
7. `DELETE /api/projects/{id}`（bot 仍在跑）→ `409`；`DELETE /api/bots/{id}` → 200（先 stop）；
   `DELETE /api/projects/{id}` → 200；DB 中 `bt-claude.deleted_at` 非 NULL（歷史保留）✅

**終端備援**（把 am-claude 的 `inject_hooks` 改成 false，等於停用 hook 注入）：

1. `PATCH /api/bots/{id} {"inject_hooks":false}` → 200 → stop / start
2. `POST prompt {"text":"Reply with exactly SECOND-FALLBACK"}` → `delivery: ok`
3. 約 5 秒後（`working→idle` 起算）log `terminal fallback engaged turn=…`，
   Turn 變 `completed_fallback`，訊息
   `('assistant','terminal_fallback', incomplete=1, 'SECOND-FALLBACK')` ✅
4. **晚到的 hook 不覆蓋**：事後補送 `last_assistant_message="LATE-HOOK-SHOULD-BE-DROPPED"` 的 Stop
   → `LATE msg count: 0`，log `late hook dropped; turn already completed via terminal fallback`，
   該 Turn 只被寫入 native ids 作為去重標記 ✅
5. 另有一次非預期但真實的備援驗證：codex 因額度用盡沒有 notify，5 秒後同樣走到 `terminal_fallback`。

備註：第一版抽取會把狀態列（`✻ Crunched for 9s`）與分隔線一起收進來，已修正為遇到
box-drawing / 水平線即停、略過 spinner 行、去尾端空行；第二次驗證得到乾淨的 `SECOND-FALLBACK`。

## M6 — WebSocket

日期：2026-09-05（`scripts` 外的臨時 node 腳本，見 PROGRESS 內容）

1. 單客戶端 `ws://127.0.0.1:7788/ws?token=…`：觸發 hook 後依序收到
   `seq=9 bot_status`、`seq=10 message_added`、`seq=11 turn_updated` ✅
2. 斷線後帶 `?since=8` 重連 → 補齊 seq 9/10/11 三則 ✅
3. 帶 `?since=999999`（seq 倒退，模擬 daemon 重啟）→ 立即收到 `{"type":"resync","seq":11}` ✅
4. `?token=nope` → 連線被拒（401）✅

---

## M8 — 前端內嵌 + release 一鍵啟動

日期：2026-09-05

1. `daemon/Cargo.toml` 新增 feature `embed-ui`（`default = ["embed-ui"]`），
   `daemon/src/assets.rs` 以 `rust_embed::Embed` 內嵌 `../web/dist`，未命中的路徑 fallback 到 `index.html`。
2. `cargo build --release` 通過（55s）。
3. `env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE ./target/release/agents-managerd serve`：
   - `GET /` → `200 text/html`，內容為前端的 `index.html`（`<title>Agents Manager</title>`）✅
   - `GET /assets/index-BCsfVINw.js` → `200 text/javascript 224377 bytes` ✅
   - `GET /some/deep/route` → `200 text/html`（SPA fallback）✅
   - `GET /api/session` → `{"port":7788,"token":"…"}`（API 未被 fallback 蓋掉）✅

## 最終 demo 驗收

日期：2026-09-05，release binary，PID 見最終回報。

1. 兩個 bot 皆可 stop / start：`am-codex` → `w5:p1 idle`、`am-claude` → `w5:p2 idle` ✅
2. `POST /api/bots/{am-claude}/prompt {"text":"Reply with exactly PONG"}` → `delivery: ok`
   → Turn `('web','completed','ok','3c35d278-fb7b-4691-a923-677092334937')`
   → 訊息 `('assistant','hook',0,'PONG')` ✅ **回覆來源為 hook**
3. `http://127.0.0.1:7788` 可開（內嵌前端）✅

---

## 偏離規格（與理由）

1. **新增 bot 設定欄位 `inject_hooks`（TOML + `bots.inject_hooks` 欄）**。SPEC 沒有這個欄位，
   但附錄 D 的 M5 驗收要求「停用 hook 注入後 prompt → 5 秒後出現 terminal_fallback」，
   需要一個可切換的開關。預設 `true`，行為與 SPEC 相同。
2. **`stop` 一律關閉該 Run 的 pane**。SPEC §6.4 只在「agent 沒消失」時 `pane.close`，
   但實測 agent 退出後會留下一個裸 shell pane，與附錄 D「stop 後 pane 消失」的驗收不符
   （要等下一次對帳的 orphan 回收才會清掉）。改為停止流程結束時一律關 pane。
3. **終端備援用 `pane.read {pane_id}` 而非 `agent.read {target}`**。備援發生時 agent 可能已經
   不在（herdr 的 agent name 會被清掉），用 `pane_id` 比較穩；讀到的內容與 revision 相同。
4. **晚到 hook 的處理**：SPEC §4.3 說「之後晚到的 hook 不覆蓋（去重後丟棄並 log）」，
   §6.7.5 卻說「沒有 in-flight Turn → 建 external Turn」，兩者衝突。採 §4.3 優先：
   若該 Run 有一筆 **120 秒內**完成、且還沒有 native ids 的 `completed_fallback` Turn，
   就把 native ids 寫到那筆 Turn（作為去重標記）並丟棄訊息；否則仍走 §6.7.5 建 external Turn。
   時間窗是必要的，否則任何後續的 external hook 都會被永久吞掉（M6 測試時踩到）。
5. **`session.snapshot` 需解包**。附錄 A 說回傳 `{version, protocol, workspaces[], …}`，
   實測外層還包了一層：`{"type":"session_snapshot","snapshot":{…}}`。herdr client 已處理。
   （這個 bug 一開始讓 orphan pane 回收整段變成 no-op。）
6. **`--dev-watch-all-panes` 旗標**：只為 M1 驗收（daemon 還沒有任何 Run 時要能看到
   `pane.agent_status_changed`）而加；正式路徑仍是 SPEC §3.1 的「每個 active Run 一條」。
7. **`{"type":"resync"}` 多帶一個 `seq`**：方便前端 log，欄位相容。
8. **`GET /api/bots/:id/messages` 多回 `turns` 與 `has_more`**：前端需要知道「這回合還在跑」
   與 delivery 警示，否則得再打一支 API。
9. **`delivery=unknown` 的封鎖判定**寫成「同一 conversation 內存在 delivery=unknown 且
   status 不是 failed 的 Turn」。SPEC 只說「該 Bot 禁止再送 prompt」，此為具體化。
10. **`agent.prompt` 不帶 `wait`**，以 10 秒 RPC 逾時界定 delivery。SPEC §6.3.4 的語義即此。
11. **API 未匹配路徑會回 `index.html`**（SPA fallback），不是 404。第一階段可接受。

## 已知問題

1. **真 Codex 的 hook 未驗**：codex 帳號用量額度用盡（"You've hit your usage limit"），
   notify 不會觸發。`-c notify=[…]` 的注入與 payload 解析已由 hook agent 用手動 argv JSON 驗過；
   額度恢復後跑 `scripts/hook-smoke.sh --codex-only` 補齊。
2. **daemon 重啟後 WS `seq` 歸零**：客戶端帶舊的 `since` 會拿到 `resync`。這是 SPEC §7.3 的設計。
3. **config.toml 寫回不保留註解**（第一階段以 serde 全量序列化，`toml_edit` 為第二階段）。
4. **`transcript` 回補未實作**（第二階段），`runs.transcript_path` 已由 SessionStart hook 回填。
5. 測試期間 DB 內留有多筆測試用的 external / 假 hook Turn（`sess-M4*`、`ws-*` 等），
   不影響功能；要乾淨的話刪掉 `~/.config/agents-manager/agents-manager.sqlite3*` 重來即可。
