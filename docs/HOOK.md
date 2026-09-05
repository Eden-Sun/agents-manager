# Hook 子命令與 M4 時序測試

對應 SPEC v3 的 §4.1、§4.4、§6.7、附錄 B、附錄 D（M4）。
負責檔案：`daemon/src/hook_cmd.rs`、`scripts/hook-timing-test.sh`、`scripts/hook-smoke.sh`。

---

## 1. `agents-managerd hook claude|codex`

Agent CLI 在每次事件（Claude 的 `SessionStart` / `Stop`、Codex 的 notify）會 fork 出這個
子程序。它位於 agent 的關鍵路徑上（Claude 會**阻塞**等 Stop hook 回來），所以整支程式只做
一件事：把 payload 丟給 daemon，丟不到就落地成 spool。

### 1.1 契約

| 條件 | 實作方式 |
|---|---|
| wall-clock ≤ 3 秒 | 內部總預算 2.5 秒；stdin ≤ 800 ms、HTTP connect ≤ 300 ms、HTTP total ≤ min(2 s, 剩餘預算) |
| 永遠 exit 0 | `run()` 不呼叫 `process::exit`；`main.rs` 在 `run()` 回來後 `exit(0)` |
| 永不 panic | 全部包在 `catch_unwind` 內，並換上只印一行的 panic hook |
| **永遠空 stdout** | 全檔沒有任何 `print!` / `println!`；Claude 會把 Stop hook 的 stdout JSON 當作決策物件 |
| stderr | 失敗時一行，例如 `agents-managerd hook: http 500; spooled` |

### 1.2 payload 取得

- **Claude**：讀 stdin，上限 1 MiB。實作多讀 1 byte 判斷是否超限，超限就在 char boundary
  截斷並在 body 標 `truncated: true`。stdin 讀取跑在 helper thread + `recv_timeout(800ms)`，
  避免呼叫端不關 pipe 時卡死。
- **Codex**：`main.rs` 取 argv 最後一個元素當 `payload_arg`，同樣套 1 MiB 上限。
- **解析容錯**：`serde_json` 解析成功**且是 object** → 原樣當 `payload`；其他情況
  （非法 JSON、純量、陣列、被截斷的片段）→ `{"raw": "<字串>"}`。

### 1.3 POST 契約

```
POST http://127.0.0.1:<port>/hook/<provider>
X-AM-Bot-Token: <per-bot hook token>
Content-Type: application/json

{"bot_id":"…","provider":"claude","payload":{…原始 JSON 物件…},
 "received_at":"2026-09-05T15:22:49.894Z","truncated":false}
```

- 寫死 IPv4 loopback `127.0.0.1`，不吃 `HTTP_PROXY` / `ALL_PROXY`（reqwest `.no_proxy()`）。
- 這個 crate 的 reqwest 沒有開 `blocking` feature，所以用 `tokio` current-thread runtime
  包一層 `block_on`。
- `--port` 來自命令列；只有在 `--port` 為 0（未給）時才退回環境變數 `AM_PORT`，再退回 7788。

### 1.4 失敗處理

任何失敗（連線被拒、逾時、非 2xx）→ 把**同一份 body** 以 `O_APPEND` 追加一行 JSON 到

```
~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl
```

目錄不存在會建。spool 寫失敗 → 追加到同目錄的 `hook.log`；再失敗就靜默。
daemon 端的重放在 `daemon/src/hookrecv.rs::replay_spool`（§4.4.6）。

### 1.5 `AM_DATA_DIR`

`hook_cmd` 額外支援 `AM_DATA_DIR` 環境變數覆寫資料目錄根，只為了讓測試腳本能用
拋棄式目錄跑真 CLI 而不污染 `~/.config/agents-manager`。**daemon 端目前不讀這個變數**
（`main.rs::data_dir()` 寫死 `~/.config/agents-manager`），所以正式路徑完全不受影響。
若後端之後想讓 daemon 也吃同一個變數以便做端到端隔離測試，可自行加上；目前不需要。

### 1.6 對 Cargo.toml 的需求

**沒有新的相依需求。** 現有的 `reqwest`（`json` + `rustls-tls`）、`tokio`（full）、
`serde_json`、`chrono`、`dirs` 已足夠。

---

## 2. `scripts/hook-timing-test.sh`

用 `curl` 直接偽造 hook，驗 §6.7 的配對規則與 §4.4 的時序契約。

```bash
scripts/hook-timing-test.sh                       # 全自動探測
scripts/hook-timing-test.sh --bot <BOT_ID> --port 7788 \
    --bot-token <T> --ui-token <T> --binary target/debug/agents-managerd
```

前置條件：daemon 在 `127.0.0.1:<port>`、且**至少一個 bot 的 Run 是 `running`**。
未指定的參數會自動取得：

| 參數 | 來源 |
|---|---|
| `--port` | `~/.config/agents-manager/config.toml` 的 `[server] listen`，預設 7788 |
| `--ui-token` | `~/.config/agents-manager/ui-token` |
| `--bot` | `GET /api/state` 中第一個 `run.state == "running"` 的 bot |
| `--bot-token` | `sqlite3 ~/.config/agents-manager/agents-manager.sqlite3` 的 `bots.hook_token` |
| `--binary` | `target/debug/agents-managerd` → `target/release/…` → `PATH` |

情境：

| 代號 | 內容 | 斷言 |
|---|---|---|
| A | 背景送 `POST /api/bots/:id/prompt`，50 ms 後**立刻**打 `/hook/<kind>` 模擬 Stop | 該 Turn `completed`（不是 `completed_fallback`）、只有一則 assistant 訊息 |
| B | 同一個 native turn id 再打一次 | assistant 訊息仍是 1 則、turns / messages 總數不變 |
| C | Claude 打 `SessionStart`（Codex 打非 `agent-turn-complete` 事件） | turns / messages 總數皆不變 |
| D | 用真 binary 對**沒人在聽的 port** 執行 `hook` 子命令 | wall-clock ≤ 3 s、exit 0、stdout 0 bytes、spool 恰好 1 行且欄位齊全 |
| E | 沒有 in-flight Turn 時打 Stop | 新 Turn `origin=external`、`status=completed`、1 則 assistant 訊息 |

設計上的兩個取捨：

- **情境 D 用關閉的 port 取代停掉 daemon。** 對子程序而言 connection refused 與 daemon
  停機完全相同，而且不必中斷其他情境。spool 寫在一個**拋棄式的假 `bot_id`** 目錄下
  （daemon 的 `replay_all` 只掃 live bots，不會誤讀），跑完自動刪除；`--keep` 可保留。
- **bot 若 `inject_hooks = true`，真 agent 會在情境 A 之後補送自己的 Stop hook。**
  那時我們的假 hook 已經把 Turn 收成 `completed`，真 hook 找不到 in-flight Turn，於是
  依 §6.7.5 建立一筆 `origin=external` 的 Turn。這是規格內的正確行為，因此所有斷言都
  **只針對我們自己的 turn_id**，不比對全域訊息總數（B、C 除外，那兩個情境前會先等到沒有
  in-flight Turn）。

### 2.1 實測結果（2026-09-05）

環境：daemon `127.0.0.1:7788`、bot `am-claude`（kind=claude、`inject_hooks=true`、
Run `running`）、binary `target/debug/agents-managerd`。

```
A. hook lands before the prompt RPC returns
  PASS A hook accepted (200)
  PASS A Turn completed
  PASS A exactly one assistant message
B. the same native turn id twice
  PASS B duplicate hook accepted (200)
  PASS B still exactly one assistant message
  PASS B no Turn / Message added by the duplicate
C. SessionStart creates nothing
  PASS C hook accepted (200)
  PASS C no Turn and no Message created (turns=2, messages=4)
D. daemon unreachable
  ·    elapsed 0.087s, exit 0, stdout 0B
  PASS D wall-clock 0.087s <= 3 s
  PASS D exit 0
  PASS D stdout empty
  PASS D one well-formed spool line
E. Stop with no in-flight Turn -> origin=external
  PASS E hook accepted (200)
  PASS E origin=external
  PASS E status=completed
  PASS E one assistant message

summary: 16 passed, 0 failed, 0 skipped
```

跑完後的對話狀態（`GET /api/bots/:id/messages`）：

```
TURN … external completed ok 5d061504-…      ← 真 claude 在情境 A 之後補送的 Stop
TURN … external completed ok timing-e-…      ← 情境 E
TURN … web      completed ok timing-a-…      ← 情境 A（被我們的假 hook 收掉）
TURN … web      completed ok 4c99e86d-…      ← 後端 agent 先前的真 PONG 測試
MSG  assistant hook 'PONG-A'
MSG  assistant hook 'PONG-E'
```

---

## 3. `scripts/hook-smoke.sh`

用**真** Claude Code 與 Codex CLI 各跑一次，確認注入的 hook / notify 真的會叫到子命令。

```bash
scripts/hook-smoke.sh                 # spool 模式（預設，不需要 daemon）
scripts/hook-smoke.sh --claude-only
scripts/hook-smoke.sh --live --bot <BOT_ID> --bot-token <T> --port 7788
```

- **spool 模式（預設）**：`AM_DATA_DIR` 指到 `mktemp -d`、`--port` 指到沒人聽的 port，
  所以 payload 一定落到 spool，驗證檔案內容即可，完全不碰真的設定目錄與 daemon。
- **live 模式**：用真 bot id / token 打真 daemon，改以
  `GET /api/bots/:id/messages` 的 assistant 訊息數是否增加來判定。

兩邊都用 `env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE` 清掉會讓 Claude 不寫 transcript 的
繼承變數（附錄 A）。Claude 用 `claude -p "Reply with exactly PONG" --settings <abs>`，
Codex 用 `codex exec -c 'notify=[…]' --skip-git-repo-check "Reply with exactly PONG"`。

CLI 自己失敗（未登入、額度用盡、斷網）且完全沒有 hook 活動時，判為 **SKIP** 而不是 FAIL，
以免環境問題蓋掉真正的 hook 錯誤。

### 3.1 實測結果（2026-09-05）

```
claude
  ·    claude exit 0, stdout: PONG
  PASS claude spool grew (0 -> 2)          ← SessionStart + Stop 各一行
  PASS claude payload shape
  ·    last line: {"bot_id":"smoke-…","payload":{"hook_event_name":"Stop",
       "last_assistant_message":"PONG","prompt_id":"28aa1fe7-…","session_id":"3a4d3c6e-…",
       "cwd":"…"},"provider":"claude","received_at":"…","truncated":false}
codex
  SKIP codex — the codex CLI exited 1 without firing a hook:
       ERROR: You've hit your usage limit. …
```

Claude 端完整驗證通過（含 §4.1 實測的 `prompt_id` / `last_assistant_message` /
`session_id` 欄位）。Codex 端因為當下**帳號額度用盡**無法跑真 CLI；改用等價的手動驗證：

```bash
target/debug/agents-managerd hook codex --bot B4 --token T --port <closed> \
  '{"type":"agent-turn-complete","thread-id":"t1","turn-id":"u1",
    "last-assistant-message":"hi","input-messages":["ask"]}'
# → spool: {"provider":"codex","payload":{"type":"agent-turn-complete",…},"truncated":false}
```

argv 最後一個 JSON 參數的解析、provider 路由、spool 落地都正確。**額度恢復後請重跑
`scripts/hook-smoke.sh --codex-only` 補齊這一格。**

---

## 4. 其他手動驗證紀錄（2026-09-05）

以 `python3` 假 HTTP server 當接收端，用真 binary 驗證：

| 檢查 | 結果 |
|---|---|
| POST 路徑 | `/hook/claude`、`/hook/codex` ✅ |
| header | `x-am-bot-token: <token>`（HTTP header 大小寫不敏感，axum `headers.get("X-AM-Bot-Token")` 取得到）、`content-type: application/json` ✅ |
| body 欄位 | `bot_id` / `provider` / `payload` / `received_at`(RFC3339 UTC，毫秒 + `Z`) / `truncated` ✅ |
| `HTTP_PROXY=http://127.0.0.1:9` 干擾 | 忽略，仍直連 127.0.0.1 ✅ |
| HTTP 500 | 落 spool，stderr `http 500; spooled` ✅ |
| 連線被拒 | 落 spool，0.035–0.087 s ✅ |
| 非法 JSON stdin | `payload = {"raw":"this is not json"}` ✅ |
| 1.5 MiB stdin | `truncated=true`，`payload.raw` 長度剛好 1048576 ✅ |
| 完全沒有 stdin（`</dev/null`） | 0.032 s 結束、exit 0 ✅ |
| `cargo test`（`hook_cmd` 單元測試 3 項） | payload 解析、raw fallback、UTF-8 邊界截斷 ✅ |
