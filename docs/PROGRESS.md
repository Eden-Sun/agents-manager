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

