# 交接：v4.0 後端（接手者：Grok）

工作樹狀態：前一位工程師的半成品已以 `wip:` commit 保存。契約在 `docs/API.md` §12（v4.0），是唯一權威；前端依它實作。Rust：`export PATH=$HOME/.cargo/bin:$PATH`。

## 要完成的功能（全部在 daemon/，不要動 web/）
1. `GET /api/models?kind=&host=&refresh=`：codex 用 `codex app-server`（stdio JSON-RPC：initialize → initialized 通知 → `model/list`）；grok 解析 `grok models` 文字；claude 靜態 opus/sonnet/haiku。快取 10 分鐘；遠端 host 走 `HostConn::ssh_exec_path`。（`daemon/src/models.rs` 已有草稿）
2. bot 欄位 `fast: bool`、`effort` 依 kind 驗證（grok low/medium/high/xhigh；codex none/minimal/low/medium/high/xhigh/max/ultra；claude 清空）；啟動注入 codex `-c service_tier="priority"`（fast）、`-c model_reasoning_effort="<effort>"`；grok `--reasoning-effort`。grok 模型清單的 efforts 來自 `~/.grok/models_cache.json`（per-model）。
3. `hosts[].attach_command`：本機 `herdr --session <session>`；遠端 `herdr --remote <user@host> --session <session>`，非 22 埠 `herdr --remote ssh://<user@host>:<port> --session <session>`。
4. 額度 `GET /api/quota` + WS `quota_updated`：codex 每 5 分鐘 `account/rateLimits/read`（primary=5h、secondary=7d）；claude 由 daemon 注入 statusLine（`agents-managerd statusline --bot --token --port`，讀 stdin 的 `rate_limits` POST 到 `/hook/claude` 當 `StatusLine` 事件，然後轉呼叫使用者原本的 statusLine 指令並原樣輸出）；grok null。（`quota.rs`、`statusline_cmd.rs` 已有草稿）
5. `hosts[].tools`（claude/codex/grok 的 installed/path/version/logged_in）與 `POST /api/hosts/:name/tools/install {kind, via_bot_id}`（把官方安裝＋login 指令當 prompt 送給該主機上一個 running bot）。（`tools.rs` 草稿）
6. bot `persona`：claude `--append-system-prompt`、grok `--rules`、codex `-c developer_instructions=…`（實測有效性）。
7. `projects[].github` 偵測（origin 解析）與 `GET /api/projects/:id/issues`、`/issues/:number`（用該主機的 `gh issue list --json …`）。（`github.rs` 草稿）

## 驗收（不要重啟 7788 的 daemon）
用獨立實例：`AM_DATA_DIR=/tmp/am-v40 ./target/release/agents-managerd serve --config /tmp/am-v40/config.toml`（listen 127.0.0.1:7800、herdr_session am-v40、project 指向 /Users/m1pro/project/agents-manager 與 /Users/m1pro/powertech、bots v-claude/v-codex/v-grok）。逐項 curl 驗證並記到 `docs/PROGRESS.md`「v4.0」。用完 stop bots、`herdr --session am-v40 server stop`、`herdr session delete am-v40`。`cargo test` 通過。commit 訊息最後一行 `Co-Authored-By: grok <noreply@x.ai>`。不要動主 repo 的 branch。
