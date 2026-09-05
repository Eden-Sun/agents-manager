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

## §11 遠端主機（Remote hosts，v3.1）

日期：2026-09-06

實作檔案：`daemon/src/hosts.rs`（新）、`config.rs`、`db.rs`、`projection.rs`、`state.rs`、
`events.rs`、`reconcile.rs`、`lifecycle.rs`、`hookrecv.rs`、`api.rs`、`main.rs`；
`scripts/dev-sshd.sh`（新）、`scripts/remote-loop-test.sh`（新）；`docs/API.md`。

驗收環境：
- `loop`：`scripts/dev-sshd.sh` 起的使用者權限 sshd（127.0.0.1:2222），herdr session `am-loop`，
  `hook_port = 17788`（不能用 7788，反向轉發的另一端就是本機、會撞到 daemon 自己）。
- `m4p`：`m4p@100.112.229.82`（真遠端，macOS，herdr 0.8.2），herdr session `agents-manager`，
  `hook_port = 7788`。

### R1 — HostManager ✅

```
$ curl -sX POST -H "X-AM-Token: $TOK" -H 'Content-Type: application/json'     -d '{"name":"loop","ssh":"m1pro@127.0.0.1","ssh_port":2222,"herdr_session":"am-loop",
         "remote_path":"/opt/homebrew/bin:$HOME/.local/bin","hook_port":17788,
         "ssh_opts":["-i",".../clientkey","-o","UserKnownHostsFile=...","-o","StrictHostKeyChecking=yes"]}'     http://127.0.0.1:7788/api/hosts
{"connected":true,"error":null,"name":"loop"}
```

daemon log（`m4p` 同時連上）：

```
INFO host configured host=loop ssh=m1pro@127.0.0.1
INFO host configured host=m4p  ssh=m4p@100.112.229.82
INFO remote session ensured host=loop session=am-loop socket=/Users/m1pro/.config/herdr/sessions/am-loop/herdr.sock
INFO remote session ensured host=m4p  session=agents-manager socket=/Users/m4p/.config/herdr/sessions/agents-manager/herdr.sock
INFO ssh master up; herdr ping ok host=loop socket=/tmp/agents-manager-501/loop.sock hook_port=17788
INFO ssh master up; herdr ping ok host=m4p  socket=/tmp/agents-manager-501/m4p.sock  hook_port=7788
INFO global herdr event subscription established host=loop
INFO global herdr event subscription established host=m4p
```

`ps` 確認兩條 master（短路徑 socket + 反向轉發）：

```
ssh -N -M -S /tmp/agents-manager-501/loop.ctl -p 2222 -i .../clientkey     -L /tmp/agents-manager-501/loop.sock:/Users/m1pro/.config/herdr/sessions/am-loop/herdr.sock     -R 17788:127.0.0.1:7788 m1pro@127.0.0.1
ssh -N -M -S /tmp/agents-manager-501/m4p.ctl     -L /tmp/agents-manager-501/m4p.sock:/Users/m4p/.config/herdr/sessions/agents-manager/herdr.sock     -R 7788:127.0.0.1:7788 m4p@100.112.229.82
```

殺掉 master → 自動重連（healthy ping 每 10 秒；本次 6 秒偵測到、8 秒內恢復，遠低於 30 秒上限）：

```
$ kill -9 $(pgrep -f "ssh -N -M -S /tmp/agents-manager-501/m4p.ctl")   # 00:29:30
00:29:32 True  None
00:29:36 False 'ssh master exited'
00:29:38 True  None            <- 重連完成，並對該 host 重跑對帳 + 重建訂閱
```

### R2 — 遠端 Project / Bot ✅（`m4p` 真遠端 + `loop`）

```
$ curl -s "…/api/fs/dirs?host=m4p&path=~/am-remote-test"
{"entries":[],"home":"/Users/m4p","parent":"/Users/m4p","path":"/Users/m4p/am-remote-test"}
$ curl -sX POST … -d '{"path":"~/am-remote-test","label":"m4p-test","host":"m4p"}' …/api/projects
{"project_id":"01M1S6VGWNNQ77WDGBSXNH4YE7"}                 # path 由遠端 `cd && pwd -P` 正規化
$ curl -sX POST … -d '{"name":"m4p-claude","kind":"claude","auto_approve":false}' …/projects/<pid>/bots
$ curl -sX POST …/api/bots/<bid>/start        # 4.6 s
{"run_id":"01M1S6VGZG1SB4WTZ63SAHZJ5R"}
lamp = blocked                                # claude 的 trust 提示
$ curl -sX POST … -d '{"keys":["down","enter"]}' …/api/bots/<bid>/keys
lamp = idle                                   # 3 秒內
```

遠端 hook 材料確實由 ssh 佈署（`m4p` 上）：

```
$ ssh m4p@… ls -la ~/.config/agents-manager/bots/01M1S6VGYNSRTR2688B3KCFJTB/
-rw-r--r--  claude-settings.json      # --settings 指向這個檔
-rwxr-xr-x  hook.sh                   # 附錄 E 的 POSIX sh 腳本
```

codex 亦驗過（`m4p-codex`，trust 提示按 `enter` → idle；`-c notify=[<hook.sh>,"codex",…]`）。

### R3 — 遠端 hook ✅

**`loop`（完整往返）**

```
$ curl -sX POST … -d '{"text":"Reply with exactly PONG","client_request_id":"r3-loop-1"}' …/prompt
{"turn_id":"…","delivery":"ok"}
$ curl -s …/messages?limit=10
user      web   'Reply with exactly PONG'
assistant hook  'PONG'                       <- 來源 hook，經 -R 17788 反向通道
turns: [('completed','ok','479ae148-712a-432a-beef-af0abfcab0d7')]
```

**spool（daemon 停機 → 重啟補入）**

```
$ pkill -f "agents-managerd serve"
$ herdr --session am-loop agent prompt loop-claude "Reply with exactly SPOOLTEST"
$ wc -l ~/.config/agents-manager/bots/<bid>/hook-spool.jsonl      # 1（curl 打不到 daemon）
$ <重啟 daemon>
INFO hook received bot=loop-claude provider=claude kind=TurnComplete { … assistant: Some("SPOOLTEST") }
INFO remote hook spool replayed bot_id=… host="loop" replayed=1
messages: assistant/hook 'SPOOLTEST'（external Turn），spool 檔已被 ssh 端 mv+rm 清掉
```

**`m4p`（真遠端，完整往返）**：先是 `SessionStart` 回填——

```
INFO hook received bot=m4p-claude provider=claude kind=Identity {
  session_id: Some("838bdf5e-…"), transcript_path: Some("/Users/m4p/.claude/projects/…jsonl") }
run.native_session_id = 838bdf5e-5932-4ecf-b302-d3638a86ca16
```

00:36 與 00:46 兩次 prompt 撞到該機 claude 的用量上限
（terminal `Usage limit reached · continuing automatically at 1am`），
Turn 走了終端備援（`completed_fallback` / `source = terminal_fallback`，行為正確）。
額度於 01:00 恢復後重跑，`Stop` hook 也通了：

```
$ date +%T ; curl -sX POST … -d '{"text":"Reply with exactly PONG","client_request_id":"r3-m4p-3"}' …/prompt
01:01:17  {"turn_id":"01M1S88W5DC34W405Y2MWYVCFZ","delivery":"ok"}

INFO hook received bot=m4p-claude provider=claude kind=TurnComplete {
  session_id: Some("838bdf5e-…"), turn_id: Some("d0d87669-…"), assistant: Some("PONG") }

17:01:17 user      web   'Reply with exactly PONG'
17:01:19 assistant hook  'PONG'          <- ✅ 真遠端的 Stop hook 經 -R 7788 反向通道回來
17:01:21 assistant hook  'PONG'          <- claude 在 1am 自動續跑先前那回合產生的另一個
                                            prompt_id，依 §6.7.5 建成 external Turn（正確）
turns: [('external','completed','ok','3a970edc-…'), ('web','completed','ok','d0d87669-…'),
        ('web','completed_fallback','ok',None), ('web','completed_fallback','ok',None)]
```

### R4 — 本機不受影響 ✅

```
$ curl -sX POST … -d '{"text":"Reply with exactly R4OK","client_request_id":"r4-local-1"}' …/bots/<am-claude>/prompt
assistant/hook 'R4OK'                                  # 本機 hook 仍走 agents-managerd hook 子命令
$ curl -sX POST …/bots/<am-codex>/start ; …/stop
idle → offline                                          # 本機 start/stop 正常
```

`GET /api/state` 中本機 project 的 `host` 為 `"local"`、`lamp` 與 M1–M8 相同；
既有三個本機 bot 在整個 §11 開發期間持續運行未被打斷（對帳保住 run）。

### R5 — 開發測試自動化 ✅

`scripts/dev-sshd.sh start|stop|status|ssh-opts|key`
（key / config 在 `~/.config/agents-manager/dev-sshd/`，不需 sudo、不改系統設定）。

`scripts/remote-loop-test.sh` 一鍵跑完 R1–R3 並自我清理，可重複執行：

```
$ scripts/remote-loop-test.sh
=== 1. dev sshd on 127.0.0.1:2222            ok
=== 2. POST /api/hosts (loop -> am-loop)     ok: host loop connected
=== 2b. remote directory listing (§11.5)     ok
=== 3. remove leftovers from a previous run
=== 4. POST /api/projects (host=loop, path=/tmp/am-loop-test.5f1FvN)   ok
=== 5. POST /api/projects/…/bots (loop-claude, claude)                 ok
=== 6. POST /api/bots/…/start                ok: started, lamp=blocked
=== 7. remote hook material installed over ssh                          ok
=== 8. answer the trust prompt (down, enter) ok: trust prompt answered, lamp=idle
=== 9. POST /api/bots/…/prompt               reply source=hook body=PONG
=== 10. POST /api/bots/…/stop                ok: stopped
=== 11. cleanup                              ok: host, project, bot and dev sshd removed
R1 (host up) / R2 (remote project+bot, trust prompt) / R3 (hook reply): PASS
```

預設每次用 `mktemp -d /tmp/am-loop-test.XXXXXX` 新目錄，所以 trust 提示（blocked 分支）
一定會被走到；`AM_PROJECT_DIR=` 可固定目錄、`AM_KIND=codex` 換 agent、`AM_KEEP=1` 保留環境。

### §11 實作過程中發現並修掉的 bug

1. **`reconcile` 的 snapshot 早於 per-bot 鎖**（原本就有，遠端把時間窗放大到必現）：
   `session.snapshot` / `agent.list` 在迴圈外先取，之後才逐 bot 上鎖；若某 bot 在這中間
   剛啟動成功，reconcile 會拿舊快照判定「agent gone」把 Run 標成 exited。
   修法：`(Some(run), None)` 這一支在鎖內對該 bot 再打一次 `agent.get` 才下結論。
2. **同一 host 會有兩條 global 訂閱**：`spawn_global_for_host` 原本 fire-and-forget，
   兩次快速呼叫會交錯。改為 `async fn`，在同一把鎖內 abort 舊的、插入新的。
3. **`projection.rs` 沒有把 `projects[].host` 投影進 SQLite**，遠端 project 全部被當成本機。
4. **遠端登入 shell 是 zsh**：把 sh 片段當 argv 傳過去會被 zsh 解析（`printf` 的 `%`、
   `${d%/}` 都會炸）。改成把腳本從 stdin 灌進 `ssh <target> /bin/sh -s`。
5. **macOS 的 `nohup` 在 stderr 不是 console 時拒絕 detach**
   （`nohup: can't detach from console: Inappropriate ioctl for device`），遠端 herdr server
   起不來。改用 `( trap '' HUP; herdr … & )`。
6. **`projects.path` 的 UNIQUE**：改成 `UNIQUE(host, path) WHERE deleted_at IS NULL`
   （否則同一路徑不能同時存在於兩台機器，而且刪掉的 project 也無法重建）。

---

## 身份 identities（per-bot env / 多帳號）

日期：2026-09-06

需求：使用者的 `cc0` / `cc1` 是 zsh alias，差別只在 `CLAUDE_CONFIG_DIR`。實作成具名的
`[[identities]]`（env + args），bot 以 `identity = "cc1"` 綁定，另可有自己的 `env`。

實作：`config.rs`（`IdentityCfg`、`BotCfg.identity` / `BotCfg.env`、`expand_home`）、
`db.rs`（`bots.identity` / `bots.env_json`，additive migration）、`projection.rs`、
`lifecycle.rs`（`pane_env` 合併、`identity_args`）、`api.rs`（`/api/identities`、
`GET /api/state` 的 `identities[]` 與 bot 的 `identity` / `env`）、`docs/API.md`。

合併順序：pane env = daemon 注入 ∪ identity.env ∪ bot.env（後者覆蓋）；
args = daemon 注入 ++ identity.args ++ bot.args。env 值裡的 `$HOME` / `${HOME}` /
開頭的 `~` 以**該 host 的 home** 展開（本機用 `dirs::home_dir`，遠端用 `HostConn::home()`，
由 `ssh 'printf %s "$HOME"'` 取得並快取）。

驗收：

```
$ curl -sX POST … -d '{"name":"cc0","kind":"claude","env":{},"args":[]}' …/api/identities
{"name":"cc0"}
$ curl -sX POST … -d '{"name":"cc1","kind":"claude","env":{"CLAUDE_CONFIG_DIR":"$HOME/.claude-ccompany"},"args":[]}' …/api/identities
{"name":"cc1"}
$ curl -sX POST … -d '{"name":"am-cc1","kind":"claude","identity":"cc1","env":{"AM_IDENTITY_PROBE":"yes"}}' …/projects/<pid>/bots
$ curl -sX POST …/api/bots/<bid>/start          # lamp=blocked（新 config dir 尚未信任該目錄）
$ for p in $(pgrep -f "claude --dangerously-skip-permissions"); do ps -E -o command= -p $p | tr ' ' '\n' \
    | grep -E '^(CLAUDE_CONFIG_DIR|AM_BOT_ID|AM_IDENTITY_PROBE)='; done
AM_BOT_ID=01M1S7P95A658P4VN2S0Z2AHAX
AM_IDENTITY_PROBE=yes
CLAUDE_CONFIG_DIR=/Users/m1pro/.claude-ccompany        <- ✅ $HOME 已展開
$ curl -sX POST … -d '{"keys":["down","enter"]}' …/keys   → lamp=idle
```

錯誤情境：

```
identity kind 不符      400 {"error":"bad_request","message":"identity `cc1` is for claude but this bot is codex"}
identity 不存在         404 {"error":"not_found","what":"identity"}
identity 重名           409 {"error":"conflict","reason":"identity name already in use","name":"cc1"}
DELETE 仍被 bot 使用    409 {"error":"conflict","reason":"identity still used by bots","bot_id":"…"}
PATCH {"identity":null} 200 → 解除綁定（bot.identity = null）
```

**遠端 host 的 home 展開**（前端 agent 在 `m4p` 上建了 identity=cc1 的 bot，順帶驗到）：

```
$ ssh m4p@… 'ps -E -o command= -p <claude pid>' | tr " " "\n" | grep CLAUDE_CONFIG_DIR
CLAUDE_CONFIG_DIR=/Users/m4p/.claude-ccompany           <- ✅ 展開成「該 host」的 home
```

使用者的 `~/.config/agents-manager/config.toml` 已補上 `cc0` 與 `cc1` 兩個 identity。
驗收用的 `am-cc1` probe bot 與 `m4p-test` 專案（m4p-claude / m4p-codex）測完已刪除，
`m4p:~/am-remote-test` 也已移除。

### identities 偏離規格 / 設計選擇

23. **identities 只存 TOML，不進 SQLite**（依需求）；bot 端存 `bots.identity` 與
    `bots.env_json` 兩欄，`GET /api/state` 的 `identities[]` 直接由 TOML 讀出。
24. **`PATCH /bots/:id` 的 `identity` 用 double-option**：欄位不存在 = 不動、
    `null` 或 `""` = 解除綁定、字串 = 綁定。`env` 傳整個物件即為取代（不做 merge）。
25. **`$HOME` 展開只認完整識別字**：`$HOMEBREW_PREFIX` 不會被誤展開；`~` 只在字串開頭展開。
26. **啟動時若取不到遠端 home**（ssh 暫時失敗），env 值中的 `$HOME` 會原樣保留並記一行 warn，
    而不是讓整個 start 失敗。

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

### §11 偏離規格（與理由）

12. **`hosts[].ssh_opts`（字串陣列，原樣附加到每個 ssh 指令）**。SPEC §11.2 只有 `ssh`
    與 `ssh_port`，但 R5 的 dev sshd 需要 `-i <key>` 與 `-o UserKnownHostsFile=…`
    才能在 `BatchMode=yes` 下免密登入。預設 `[]`，不影響既有設定。
13. **`ssh_port` 只在不等於 22 時才加 `-p`**。若一律加 `-p 22`，會蓋掉 ssh_config
    別名自訂的 Port（SPEC §11.2 說「可含 ssh_config 別名」）。
14. **遠端 sh 片段一律以 `ssh <target> /bin/sh -s` + stdin 執行**，不用 SPEC §11.3.1 寫的
    `ssh <host> '<指令>'`。遠端登入 shell 可能是 zsh/fish，argv 形式會被它再解析一次。
15. **遠端 herdr server 用 `( trap '' HUP; … & )` 而非 `nohup`**（附錄 E 的 nohup 在
    macOS 非 console stderr 下會失敗）。
16. **`fallback_timers` 仍以 `run_id` 為鍵**，不是 SPEC §11.3.6 說的 `(host, pane_id)`。
    `run_id` 是 ULID、全域唯一，本來就沒有跨 host 碰撞問題；`pane_watchers` 依規格改成
    `(host, pane_id)`（pane id 只在單一 host 內唯一）。
17. **`projects.path` 的唯一性改為 `UNIQUE(host, path) WHERE deleted_at IS NULL`**。
    SPEC §2 的「路徑正規化後唯一」在多主機下必須帶上 host；加上 `deleted_at IS NULL`
    是為了讓刪除過的 project 目錄可以重新註冊（沿用 `bots_name_live` 的既有寫法）。
18. **遠端 project 的 `path` 不做本機 canonicalize**，改由 `ssh 'cd <p> && pwd -P'` 取得；
    `projection.rs` 也只對 `host = "local"` 的 project 做 `canonical_path`。
19. **`GET /api/fs/dirs?host=` 在 host `connected:false` 時仍可能回 200**：目錄瀏覽只用
    ssh、不經過 herdr，所以只要 ssh 通就能列。ssh 失敗才回 502。已寫進 `docs/API.md`。
20. **`daemon_status` 保留 `connected` 作為 `herdr_connected` 的同義欄位**（相容既有前端）；
    `bot_status` 多帶 `host`，其 `connected` 是**該 bot 所屬 host** 的狀態（驅動 `lamp`）。
21. **`POST /api/hosts` 是 upsert**（同名視為更新，先斷開舊連線再以新設定連），
    SPEC 只寫「新增」。UI 的「編輯主機」用同一支 API 即可。
22. **daemon 收到 SIGTERM / SIGINT 才會 `ssh -O exit`**。被 `kill -9` 時 master 會殘留，
    但下次連線的 `start_master` 會先 `ssh -O exit` + 刪 socket 清乾淨。


## 已知問題

1. **真 Codex 的 hook 未驗**：codex 帳號用量額度用盡（"You've hit your usage limit"），
   notify 不會觸發。`-c notify=[…]` 的注入與 payload 解析已由 hook agent 用手動 argv JSON 驗過；
   額度恢復後跑 `scripts/hook-smoke.sh --codex-only` 補齊。
2. **daemon 重啟後 WS `seq` 歸零**：客戶端帶舊的 `since` 會拿到 `resync`。這是 SPEC §7.3 的設計。
3. **config.toml 寫回不保留註解**（第一階段以 serde 全量序列化，`toml_edit` 為第二階段）。
4. **`transcript` 回補未實作**（第二階段），`runs.transcript_path` 已由 SessionStart hook 回填。
5. 測試期間 DB 內留有多筆測試用的 external / 假 hook Turn（`sess-M4*`、`ws-*` 等），
   不影響功能；要乾淨的話刪掉 `~/.config/agents-manager/agents-manager.sqlite3*` 重來即可。

6. **`m4p` 上的 codex `notify` hook 未驗到**：該機 codex 帳號在用量上限
   （`You have 1 usage limit reset available`），prompt 只走到終端備援。
   claude 端的 `SessionStart` 與 `Stop` 皆已在 m4p 實測通過，
   codex 的 `-c notify=[<遠端 hook.sh>,…]` 注入格式與本機相同、僅未跑到真實 turn。
7. **`kill -9` daemon 會殘留 ssh master**：SIGTERM / SIGINT 有 graceful shutdown（`ssh -O exit`），
   `kill -9` 沒有。殘留的 master 會在下次 `start_master` 被 `ssh -O exit` 清掉，不影響功能。
8. **remote_path 之外的遠端環境不做偵測**：遠端沒有 herdr（或不在 `remote_path` 上）時，
   host 會停在 `disconnected` 並在 `error` 顯示 `could not determine remote herdr socket path`。
9. **遠端 `~/.config/agents-manager/bots/<id>/` 不會被清掉**：DELETE Bot 只刪本機設定，
   遠端的 `hook.sh` / `claude-settings.json` / `hook.log` 留著（下次同 bot id 會覆寫）。


## v3.2 — prompt-stall watchdog 與備援擷取清理（2026-09-06）

問題（使用者截圖）：遠端 `cc1` 身份未登入，claude 對 prompt 完全不反應 → Turn 永遠 `in_flight`、輸入框鎖死；另一個回合的備援擷取把整個畫面（banner、⚠ 警告、狀態列）塞進氣泡。

修正：
- `lifecycle::arm_stall` / `cancel_stall` / `fail_stalled_turn`：delivery ok 後 12 秒內未見 `working`/`blocked` → CAS `in_flight → failed`，system 訊息含原因（畫面偵測 `Not logged in` / `usage limit`）與快照。`events.rs` 收到 working/blocked 即取消。
- `lifecycle::clean_screen` + `is_noise`：無 `⏺`/`•` 標記時只保留最後 prompt 回音之後的內容並去雜訊，保留 `⎿` 行。

驗收：
- 遠端 `test`（cc1，未登入）送 `echo 2` → 12 秒後 `TURN failed ok`，system 訊息「agent 尚未登入（畫面顯示 Not logged in · Please run /login）…」；daemon log `prompt stalled; turn failed` ✅
- 本機 `am-claude` 送 `Reply with exactly WATCHDOG-OK` → `completed`、`assistant/hook WATCHDOG-OK`，stall 計數未增加（working 事件已取消 watchdog）✅
- `cargo test -p agents-managerd extract_tests`：以截圖畫面為 fixture，`clean_screen` 只留下 `Not logged in · Please run /login`；`extract_reply` 仍優先取 `⏺` 行 ✅

## v3.3 — Bot 可編輯 / 可刪除 / 可指定模型（2026-09-06）

需求：UI 要能改 bot（含新的 `model` 欄位）、重啟套用、刪除 bot；watchdog 訊息不要斷言「尚未登入」。

實作：`config.rs`（`BotCfg.model`）、`db.rs`（`bots.model TEXT` + additive migration）、
`projection.rs`（model 同步）、`lifecycle.rs`（`model_args()`、`restart_bot()`、`purge_bot_dir()`、
`stall_hint_lines()` / `stall_reason()`）、`api.rs`（`PATCH` 擴充 + `needs_restart`、
`POST /bots/:id/restart`、`DELETE /bots/:id` 清理 bot 目錄、`GET /state` 的 `model`）、`docs/API.md` §10。

argv 注入順序：daemon 旗標（auto_approve、hooks）→ model（claude `--model`／codex `-m`）
→ identity.args → bot.args。

驗收（daemon 版本 = 本次 release build）：

```
$ curl -sX PATCH … -d '{"model":"gpt-5.5"}' …/api/bots/<am-codex>      # 無 Run
{"needs_restart":false}
$ curl -sX POST …/api/bots/<am-codex>/start ; ps -o command= -p <pid>
codex --yolo -c notify=[…] -m gpt-5.5                                   ✅ 順序正確

$ curl -sX PATCH … -d '{"model":"opus","args":["--append-system-prompt","AM-V33-TEST"]}' …/<am-claude>  # 有 Run
{"needs_restart":true}
$ curl -sX PATCH … -d '{"autostart":false}' …/<am-claude>
{"needs_restart":false}                                                 # 只改 autostart 不需重啟
$ curl -sX PATCH … -d '{"name":"am-claude2"}' …/<am-claude>
409 {"error":"conflict","reason":"cannot rename a bot with an active run","run_id":"01M1S5P4…"}
$ curl -sX POST …/api/bots/<am-claude>/restart → {"run_id":"01M1S9XP…"}
claude --dangerously-skip-permissions --settings …/claude-settings.json --model opus --append-system-prompt AM-V33-TEST  ✅
herdr agent list：只有一個 am-claude（舊 agent 無殘留）、舊 pane 已關（w8 剩 p4/p5）✅
```

刪除（本機 `amtmp`，claude，model=haiku）：

```
start → argv 帶 --model haiku ✅；有 Run 時 rename → 409；stop 後 rename → 200 {"needs_restart":false} ✅
prompt "Reply with exactly TMP-OK" → assistant/hook TMP-OK
DELETE /api/bots/<id> → 200 {}
  herdr agent list 無 amtmp、pane w8:p7 消失 ✅
  config.toml 無該 bot ✅
  ~/.config/agents-manager/bots/<id>/ 已刪除 ✅
  GET /bots/<id>/messages 仍回 2 則；DB bots.deleted_at 非 NULL、messages 仍在 ✅
```

遠端（m4p / project `pt` 的 `m4ptmp`）：

```
start → 遠端 argv `claude … --settings /Users/m4p/.config/agents-manager/bots/<id>/claude-settings.json --model haiku` ✅
prompt → assistant/hook RTMP-OK（遠端 hook 正常）
DELETE → 遠端 agent list 只剩 pt-opu、pane wD:p2 消失、
        /Users/m4p/.config/agents-manager/bots/<id>/ 已由 ssh rm -rf 刪除 ✅，訊息仍在 ✅
```

watchdog 新訊息（`test` @ m4p，identity cc1）：

```
--- system system
agent 在 12 秒內沒有對訊息作出反應。終端畫面：
⎿  Not logged in · Please run /login
· Run in another terminal: security unlock-keychain
Not logged in · Run /login
若該身份使用 macOS Keychain 儲存憑證，透過 ssh 啟動的 herdr 可能讀不到（畫面提示 `security unlock-keychain`）。
```

驗收用的 `amtmp` / `m4ptmp` 已刪除；`am-claude` / `am-codex` 的 model 與 args 已還原成原本的空值並重啟，
`test` 已停回原本的 offline 狀態。

### v3.3 偏離規格 / 設計選擇

27. **`needs_restart` 的判定**：有 active Run 且本次 PATCH 動到會影響啟動 argv / env 的欄位
    （`model` / `args` / `identity` / `env` / `auto_approve` / `inject_hooks`）才是 `true`。
    只改 `autostart` 回 `false`（它本來就只在下次 daemon 啟動時才讀）。已寫進 `docs/API.md` §10.2。
28. **PATCH 改名多了一個重名檢查**：SPEC 只寫「有 active Run 拒絕」，但改成別的 bot 已用的名字
    會讓 herdr agent name 撞名，因此比照 POST 回 409 `bot name already in use`。
29. **`DELETE /bots/:id` 對已刪除 / 不存在的 bot 回 404**（原本一律回 200）。
30. **`model` 不做白名單驗證**：值直接傳給 CLI（只把空白字串正規化成 `null`），
    因為模型名稱由各家 CLI 自行演進。
31. **watchdog 訊息改為中性敘述 + 原樣引用畫面行**（使用者指出 ssh 主機其實已登入）。
    比對關鍵字 `Not logged in` / `/login` / `unlock-keychain` / `usage limit` / `limit`（不分大小寫），
    無匹配行時只說「請查看終端分頁」。

### Codebase review A 節修復（2026-09-06，commit `69165d3`）

`docs/REVIEW.md` A1–A6 全數修復並驗證：A1 對帳不再以空 `agent.list` 誤殺 active Run；
A2 `extract_reply` 只看最後一次 prompt 回音之後的畫面（新增單元測試，`cargo test` 6 passed）；
A3 已刪除 bot 的 hook 回 `410`（實測：刪掉的 `amtmp` 打 hook → 410，訊息數未增加；
存活 bot 仍 200）；A4 遠端 hook 腳本對空 / 非 JSON payload 產生合法 JSON（`sh` 實測三種輸入
皆可 `json.loads`），4xx 不再進 spool，遠端 `m4ptmp2` 實測 prompt → `source=hook`、spool 0 行；
A5 Origin 精確比對 host（`localhost.attacker.com`、`null`、`https://evil.com` → 403；
`localhost:5173`、`127.0.0.1:7788`、`[::1]:5173` → 200）；A6 本機訂閱失敗即推 `daemon_status`。

**A5 偏離建議**：不鎖定 port（Vite dev server 的 Origin 會被 proxy 原樣轉送，鎖 port 會直接
擋掉整個開發環境）；理由已寫在 `docs/REVIEW.md` A 節下方的註。


## v3.4 — 遠端 herdr 改由 launchd GUI 網域啟動（2026-09-06）

問題：cc1 身份（`CLAUDE_CONFIG_DIR=~/.claude-ccompany`）的遠端 bot 顯示「Not logged in · security unlock-keychain」，但主機確實已登入。根因：herdr 由非互動 ssh 拉起，該工作階段的登入 Keychain 是鎖住的；預設 `~/.claude` 靠 `.credentials.json` 檔案所以沒事。

修正：`hosts.rs ensure_remote_session` 在 macOS 且 ssh 使用者擁有 `/dev/console` 時，改以 LaunchAgent（`gui/<uid>`、KeepAlive）啟動 herdr server；否則退回 nohup。

驗收：
- ssh 工作階段 `security find-generic-password` → 錯誤 36；launchd GUI 網域 → OK ✅
- daemon 重啟後 log `remote session ensured … mode=launchd`；m4p `launchctl print gui/501/dev.agents-manager.herdr-agents-manager` state=running，herdr ppid=1 ✅
- `cctest`（m4p，cc1）start → idle → prompt「Reply with exactly LAUNCHD-OK」→ 4 秒 `assistant/hook: LAUNCHD-OK`；程序 env `CLAUDE_CONFIG_DIR=/Users/m4p/.claude-ccompany` ✅
- 為何不用 `herdr --remote`：只是 TUI 串流（`--remote can only be used with the default launch command`），無 API 轉發，遠端 server 同樣經 ssh 啟動。


## v3.6 — grok 支援（第三種 kind，2026-09-06）

規格：`docs/SPEC.md` §12 + 附錄 F；API：`docs/API.md`「bot.kind = grok」；前端：`docs/FRONTEND.md`「grok kind」。

### 研究結論（grok 1.0.13）

- 旗標：`--always-approve`（= `--permission-mode bypassPermissions`）、`-m <model>`；`grok models` → `grok-4.6`（預設）、`grok-4.5`。
- **沒有每次啟動的 hook 注入旗標**（`--settings` / `--hooks` / `--plugin-dir` 對 TUI 都是 `unexpected argument`）。hook 來源只有 `<GROK_HOME>/hooks/*.json`（全域、永遠信任）、專案 `.grok/hooks/`（需 trust）、config.toml `[[hooks.<Event>]]`、plugin。
- hook payload 由 stdin 給、camelCase：`stop` 帶 `sessionId` / `promptId` / `transcriptPath` / `lastAssistantMessage` / `reason` (`end_turn` | `shutdown`) / `stopHookActive`；`session_start` 在 TUI 下**延遲到第一次 prompt 才觸發**；父程序 env 會傳到 hook（`AM_BOT_ID` 實測可見）。
- 終端：prompt 回音 `❯ `，回覆**無標記**（縮排純文字 + 右側 `h:mm AM` 時戳 + `█` 捲軸），另有 `◆ …` 事件行、`Worked for … stop [hooks: N]`、遙測 opt-in banner（依寬度換行）。
- 身份隔離：`GROK_HOME`（預設 `~/.grok`，含 auth / sessions / hooks / config）。
- herdr：內建 grok manifest，`agent.start --kind grok` 3–4 秒 idle，本機無 trust 提示。

### 設計

全域 hooks 檔 `<GROK_HOME>/hooks/agents-manager.json`（SessionStart + Stop，timeout 5）→ 固定分派腳本 `~/.config/agents-manager/grok-hook.sh` → 讀 pane env `AM_BOT_ID` / `AM_HOOK_TOKEN` / `AM_PORT` → `agents-managerd hook grok --bot … --token … --port …`（遠端：`bots/$AM_BOT_ID/hook.sh grok …`）。無 `AM_BOT_ID` 時立即 `exit 0`，使用者自己的 grok 不受影響。pane env 新增 `AM_HOOK_TOKEN`；`inject_hooks = false` 不給 token → hook no-op → 走終端備援。`bots.kind` CHECK 以 `bots_new` 重建加入 `grok`（重建後索引為 `bots_name_project_live`，配合 v3.5 專案內唯一）。

### 驗收指令與結果（本機，daemon = 本 worktree release build）

```
POST /projects/01M1S2SQ…/bots {"name":"am-grok","kind":"grok"}   → {"bot_id":"01M1SCMS…"}
POST /bots/<id>/start                                             → 4s；lamp idle；argv `grok --always-approve`
  ~/.grok/hooks/agents-manager.json、~/.config/agents-manager/grok-hook.sh 已寫入（log: grok hook installed）
POST /bots/<id>/prompt "Reply with exactly GROK-OK"               → 7s completed；assistant source=hook「GROK-OK」
  log: hook received kind=Identity{session_id} → TurnComplete{promptId, transcriptPath, "GROK-OK"}
PATCH {"model":"grok-4.5","inject_hooks":false} + restart         → argv `grok --always-approve -m grok-4.5`，pane env 無 AM_HOOK_TOKEN
prompt "Reply with exactly GROK-FALLBACK"                         → 10s completed_fallback；terminal_fallback「GROK-FALLBACK」
  （第一版把換行後的遙測 banner 帶進來，已改為整塊跳過並加單元測試）
PATCH {"model":null,"inject_hooks":true} + restart + prompt       → hook「GROK-OK-2」
merge main（v3.5 agent_name）後 restart am-grok                     → herdr agent list `agents-manager-am-grok`，
  GET /state agent_name=agents-manager-am-grok，prompt → hook「GROK-OK-3」
daemon 重啟 ×3：reconcile: kept active run bot=am-grok（同 pane）
cargo test → 15 passed；npm run build ✅；scripts/demo-grok.mjs → docs/screenshots/130-*.png、131-*.png
```

herdr 測試 session `am-grok` 已 `server stop` + `session delete`；探測用的 `~/.grok/hooks/am-probe.json` 已移除。

### 已知問題 / 注意

1. **舊 daemon 二進位遇到 `kind = "grok"` 會 fatal**（`project config into sqlite: invalid bot kind grok`）。驗收期間另一個 agent 曾以 main 的舊 build 重啟 7788，daemon 立刻退出；請在合併前不要用未含此分支的 build 重啟 daemon，或先把 `am-grok` 從 config 移除。
2. 全域 `~/.grok/hooks/agents-manager.json` 在刪除最後一個 grok bot 後不會被清掉（無害 no-op），第二階段可加清理。
3. `session_start` 延遲觸發，所以 grok bot 剛啟動時 `runs.native_session_id` 為空，直到第一次 prompt。
4. grok 的 Stop hook 在回合結束時是 gate（預設 600 秒 timeout），子命令 ≤ 3 秒且 stdout 為空，不會卡住回合；但 hook 失敗對 grok 是 fail-open，不會有錯誤提示，只能從 daemon log / `hook.log` 看。
