# Goal：從 AG Man 起的 bot，開出來的子 agent 一律可追蹤（2026-09-07）

## 背景
現況只靠人設（`lifecycle::spawn_rule`）請 agent 把子 pane 命名成 `<父 agent 名>-<字尾>`，reconcile 再用前綴認領（5a4fb03、75a2b67）。這是請求不是機制：agent 忘了、用別的名字、或用 codex / grok 就漏掉，變成無法追蹤的 sub task。

## 要做的三件事（各自獨立 commit）

### 1. 用 pane 血緣認領，不靠名字（daemon/src/reconcile.rs）
- 一個 bot 一個 tab（one-bot-one-tab）。對帳時，herdr 裡沒被任何 bot 認領的 agent，只要它的 `tab_id` 等於某個 bot 的**活動 run 的 `tab_id`**（`runs.tab_id`），就當成那個 bot 的子 agent，走現有的 child 認領路徑（`managed_by='child'`、`parent_bot_id`、adopted run、重用同名 live child）。
- 名字前綴仍然支援（跨 tab、team workspace 的情況），但血緣優先；兩者都命中以血緣為準。
- 子 agent 的 `name`：有前綴就取字尾，否則用 herdr 的 agent name（去掉不合法字元、超過 32 字截斷）。
- 加測試（`reconcile.rs` 既有的 mock herdr 有 `agent.list` / `tab`，比照 `an_adopted_agent_records_the_tab_it_is_in`）。

### 2. PATH shim：把規則變成機制（daemon/src/lifecycle.rs + 新檔 daemon/src/herdr_shim.rs）
- daemon 起 pane 時，在 bot 的目錄（本機 `app.bot_dir(bot_id)/bin`，遠端走跟 hook script 一樣的上傳路徑 `REMOTE_HOOK_SH` 那套）放一支可執行的 `herdr` 包裝腳本，並在 pane env 的 `PATH` 前面加上這個目錄（`pane_env` 已經在組 env；`PATH` 要保留原本的，寫成 `<dir>:$PATH` 由 shell 展開，或在 script 內處理）。
- 腳本行為（sh，不依賴 bash）：
  - `herdr agent start <name> …`：若 `<name>` 不是以 `$AM_AGENT_NAME-` 開頭，自動改成 `$AM_AGENT_NAME-<name>`，並在 stderr 印一行「已改名為 …」。
  - `herdr pane split` / `pane new` / `tab create`：原樣轉發，但子 pane 需要繼承帳號與 hook：對 claude 在轉發前 `export CLAUDE_CONFIG_DIR`（父的值已在 env）、`AM_BOT_ID` / `AM_HOOK_TOKEN` / `AM_PORT` / `AM_RUN_ID` 都保留（子 pane 從父 shell 繼承 env，本來就會帶著；確認 herdr 的 `--env` 或 `env` 參數不會清掉）。
  - 其他子指令：`exec` 真的 herdr（用 `AM_REAL_HERDR` 或 `command -v -p herdr` 找，避免找到自己）。
- `pane_env` 多放 `AM_AGENT_NAME=<agent name>`（daemon 已經在 start 時算好 `agent`）。
- 加單元測試：腳本本身用 `sh` 跑一次（用一個假的 herdr 記錄 argv），確認改名與轉發。

### 3. 確保 claude 有 herdr skill（daemon/src/lifecycle.rs）
- claude 的 skill 目錄是 `$CLAUDE_CONFIG_DIR/skills/<name>/SKILL.md`（預設 `~/.claude/skills`）。daemon 在啟動 claude bot 前，把 `herdr --skill` 的輸出寫到該身份 config dir 的 `skills/herdr/SKILL.md`（冪等：內容相同就不寫；遠端主機用 ssh 執行 `herdr --skill > …`）。
- 在 SKILL.md 最前面（frontmatter 之後）補一段 AG Man 的規則：子 agent 命名、`pane split --pane $AM_PANE_ID`、不要 `git stash`、子 agent 會被掛在自己底下。`pane_env` 補 `AM_PANE_ID`（run 的 pane_id，start 時已知）。
- `herdr --skill` 原文的 description 寫「只有使用者明確提到 Herdr 才用」，AG Man 的版本要改成「當你需要開子任務 / 平行工作時就用這個，並且照下面的命名規則」。
- codex / grok：把同一段規則放進它們的 persona（`persona_args` 已有 `spawn_rule`，把它擴成同一份文字來源）。

## 硬規則
- 不要 `git stash` / `--autostash`；工作樹有其他 agent（perf、unread）未提交的改動，他們在改 `store.ts`、`ChatPanel.tsx`、`Sidebar.tsx`，你不要碰 web/。
- `cargo build --release -p agents-managerd` 與 `cargo test -p agents-managerd` 要過。**不要自己重啟 daemon**，回報時說明要重啟。
- 只 `git add` 自己的 hunk；commit 訊息 `feat(daemon): …`；push 被拒就 `git pull --rebase --no-autostash`。
- docs/API.md 的「子 agent」段落與 docs/SPEC.md 補上血緣認領與 shim。
- 驗證：daemon 在 127.0.0.1:7788（token `~/.config/agents-manager/ui-token`，header `X-AM-Token`）。重啟由派工者做；你先用測試與 `sh` 跑 shim 驗。

## 進度
- [ ] 1 血緣認領
- [ ] 2 PATH shim
- [ ] 3 herdr skill 注入
