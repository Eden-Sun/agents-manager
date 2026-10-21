# hook 子命令與測試腳本

契約在 SPEC §4.4（遠端 §11.4）與 §6.7；這份記實作上的取捨與怎麼驗。檔案：`crates/am-base/src/hook_cmd.rs`、`scripts/hook-timing-test.sh`、`scripts/hook-smoke.sh`。

## `agents-managerd hook claude|codex|grok`

agent CLI 每個事件 fork 一次，位在 agent 的關鍵路徑上（Claude 會**阻塞**等 Stop hook），所以只做一件事：把 payload 丟給 daemon，丟不到就落地成 spool。

| 條件 | 實作 |
|---|---|
| wall-clock ≤ 3 秒 | 總預算 2.5 秒；stdin ≤ 800 ms（helper thread + `recv_timeout`，防呼叫端不關 pipe）、connect ≤ 300 ms、HTTP total ≤ min(2 s, 剩餘) |
| 永遠 exit 0、永不 panic | `run()` 不呼叫 `process::exit`，全包在 `catch_unwind` 內（panic hook 只印一行），`main.rs` 最後 `exit(0)` |
| **永遠空 stdout** | 全檔沒有 `print!`；Claude／grok 會把 Stop hook 的 stdout JSON 當決策 |
| stderr | 失敗時一行，例如 `agents-managerd hook: http 500; spooled` |

- payload：claude／grok 讀 stdin，codex 取 argv 最後一個；上限 1 MiB（多讀 1 byte 判斷，在 char boundary 截斷並標 `truncated`）。解析成 JSON object 就原樣用，否則包成 `{"raw":"<字串>"}`。
- POST `http://127.0.0.1:<port>/hook/<provider>`，header `X-AM-Bot-Token`，body `{bot_id, provider, payload, received_at, truncated}`。寫死 IPv4 loopback、reqwest `.no_proxy()`；
  crate 的 reqwest 沒開 `blocking`，用 tokio current-thread runtime `block_on`。`--port` 為 0 時退回 `AM_PORT`，再退回 7788。
- 任何失敗（拒絕、逾時、非 2xx）→ 同一份 body `O_APPEND` 到 `<data dir>/bots/<bot_id>/hook-spool.jsonl`；寫不進去追加 `hook.log`；再失敗靜默。重放在 `hookrecv.rs::replay_spool`。
- `AM_DATA_DIR` 覆寫資料目錄根（daemon 與子命令都讀），測試用拋棄式目錄。hook 子命令另收 `--data-dir`，daemon 寫進 `hook.sh`／dispatcher 的 argv，**優先於** env：舊 pane 的 env 換不掉，但這些檔案每次啟動都重寫，spool 才一定落在啟動它的那顆 daemon 的目錄（SPEC §3.1）。

## `scripts/hook-timing-test.sh`

用 `curl` 偽造 hook 驗 §6.7 的配對與 §4.4 的時序。前置：daemon 在跑且至少一顆 bot 的 Run 是 `running`。未給的參數自動取（port 讀 config、ui-token 讀檔、bot 取第一個 running、
bot token 從 sqlite 讀、binary 依序找 debug／release／PATH）。

```bash
scripts/hook-timing-test.sh
scripts/hook-timing-test.sh --bot <BOT_ID> --port 7788 --bot-token <T> --ui-token <T> --binary target/debug/agents-managerd
```

| 情境 | 內容 | 斷言 |
|---|---|---|
| A | 背景送 prompt，50 ms 後打 Stop hook | 該 Turn `completed`（非 fallback）、恰一則 assistant |
| B | 同一個 native turn id 再打一次 | 訊息與 Turn 數不變 |
| C | SessionStart（codex 打非 turn-complete 事件） | 不建 Turn／Message |
| D | 真 binary 對沒人聽的 port 跑子命令 | ≤ 3 s、exit 0、stdout 0 bytes、spool 恰一行且欄位齊 |
| E | 沒有 in-flight Turn 時打 Stop | 新 Turn `origin=external`、`completed`、一則 assistant |

- D 用關閉的 port 取代停 daemon（對子程序等價），spool 寫在拋棄式假 `bot_id` 目錄，跑完刪除（`--keep` 保留）。
- `inject_hooks = true` 的 bot 在 A 之後真 agent 會補送自己的 Stop，照 §6.7 變成一筆 external Turn，所以斷言只針對自己的 turn_id。

## `scripts/hook-smoke.sh`

用真 Claude Code 與 Codex CLI 各跑一次，確認注入的 hook／notify 真的叫到子命令。

```bash
scripts/hook-smoke.sh                 # spool 模式：AM_DATA_DIR=mktemp、port 指到沒人聽的，驗 spool 內容
scripts/hook-smoke.sh --claude-only | --codex-only
scripts/hook-smoke.sh --live --bot <BOT_ID> --bot-token <T> --port 7788   # 打真 daemon，看 assistant 訊息數
```

兩邊都 `env -u CLAUDE_CODE_CHILD_SESSION -u CLAUDECODE`。CLI 自己失敗（未登入、額度用盡）且沒有任何 hook 活動時判 **SKIP** 不是 FAIL。

## `scripts/claude-hook-debug.py`（離線 hook 耗時統計）

Claude Code 2.1.296+ 的 `--debug` log 會在每個 command hook 結束時留一行 `Hooks: <event>[:<matcher>] [<command>] … finished with status N (Xms)`（官方 <https://code.claude.com/docs/en/hooks#debug-hooks>）。這支腳本統計那些行：`claude --debug-file <檔>` 之後 `python3 scripts/claude-hook-debug.py <檔>`，或 `… | python3 scripts/claude-hook-debug.py -`。

- **輸入**：只讀使用者明確給的檔案或 stdin；不自己找 `~/.claude/debug`、不讀 session、不替 bot 開 `--debug`。總量 ≤64 MiB（每讀一個 chunk 就查，沒有換行的串流也在上限停下，標 `truncated`）、單行 ≤8 KiB（超過整行 skip）、數字最多 9 位（更長的整行 skip，不會崩潰）、每群組最多留 10000 筆耗時樣本。
- **輸出**：JSON，只有 event（白名單）× outcome（`finished`／`completed`／`timed_out`／`cancelled`／`backgrounded`／`failed_to_start`）的 count、`nonzero_status`、`with_duration`（有耗時的總筆數）、p50／p95／max（最近鄰）。`max_ms` 與 `with_duration` 是全體；超過樣本上限時百分位只來自前 N 筆，`percentile_sample` 會標出 N。**不輸出**原始行、command、plugin、matcher、參數——那些是任意文字，不保證不含敏感內容。看不懂的行計入 `skipped_lines`；沒有耗時的行（`timed out`、`cancelled`、`completed with status`…）不算 0、不進百分位。
- **限制**：量的是 Claude 眼中 hook 本身的耗時，**不能區分 MCP 或模型慢**；跟 `hook_events.received_at`／`processed_at` 的差值（送端戳到 daemon 處理完的延遲，含排隊與重試）是兩回事，不要互相替代。
- 測試：`scripts/claude-hook-debug_test.sh`（包 `claude-hook-debug_test.py`，自造文字，不碰真實 log）。
