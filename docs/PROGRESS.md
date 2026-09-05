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

