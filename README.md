# agents-manager

本機執行的多 agent 管理器。Web UI 把每個跑在 [herdr](https://herdr.dev) pane 裡的 coding agent CLI（**claude** / **codex** / **grok**）呈現成一個「bot」，以目錄為單位分組，用聊天視窗送訊息、看回覆、處理 blocked 提示。

終端裡同時開好幾隻 agent 時，輸出會互相擠、狀態看不清、也沒有跨 bot 的時間軸。這個專案不管 agent 程序本身——那是 herdr 的工作——它負責把它們編成 Project / Bot / Run / Turn，把 hook 回來的回覆收進對話，並在瀏覽器裡操作。

## 架構

```mermaid
flowchart LR
    web["React 前端 (Vite)"]
    subgraph daemon["Rust daemon (axum)"]
        direction TB
        d1["herdr client"]
        d2["registry + state"]
        d3["conversation store"]
        d4["hook receiver"]
    end
    store[("SQLite + config.toml<br/>~/.config/agents-manager/")]
    subgraph herdr["herdr headless server (session=agents-mgr)"]
        direction TB
        panes["panes: claude / codex / grok"]
    end

    web <-- "REST + WebSocket" --> daemon
    daemon <-- "Unix socket (JSON lines)" --> herdr
    panes -- "hook / notify HTTP" --> d4
    daemon --- store
```

三者分工：

| 層 | 做什麼 | 不做什麼 |
|---|---|---|
| **web**（`web/`） | 側欄、對話、群組、設定、額度條、Team 面板 | 不直接碰 agent 程序 |
| **daemon**（`daemon/`，二進位 `agents-managerd`） | 對帳 herdr、注入 hook、配對 Turn、投影 TOML→SQLite、開 HTTP / WS | 不 `spawn` claude / codex / grok |
| **herdr** | 真正的 pane / agent 生命週期、送 prompt、讀畫面 | 不理解 Bot / Turn / 群組 |

設定檔 `~/.config/agents-manager/config.toml` 是 Project / Bot **期望設定**的權威；SQLite 保存 Run / Turn / Message 等執行期狀態。daemon 啟動與每次 TOML 寫回後做投影。

## 畫面

數字越大的檔名通常越新。下面幾張是目前 UI，不是早期 demo。

**主畫面對話** — 側欄依專案列出 bot 與燈號；右側是選定 bot 的氣泡、即時輸出、排隊中的下一則與額度條。回覆來源標在氣泡上（`hook` 或 `terminal_fallback`）。側欄左上角是 `AG Man ｜ pane N ｜ RAM …`：現在開著幾個 herdr pane、所有 herdr 進程樹吃掉多少記憶體。

![主畫面對話](docs/screenshots/360-readme-chat-dark.png)

側欄可以搜尋 bot、在列上直接改暱稱，每列的操作鈕（啟動 / 停止 / 設定 / 刪除）排成 2×2；燈號是環狀，看得出「連線中」與「在跑」的差別。

回合狀態多了一段 **已完成（未讀）**：bot 回完但你還沒看，側欄列與折疊起來的專案標題都會掛上未讀徽章，切到那個對話（或分頁回到前景）才清掉；未讀狀態重新整理不會丟。

![未讀徽章](docs/screenshots/unread/440-unread-badge.png)

**回合進行中可以中止** — 回合在跑時輸入框不鎖（可以先打、送出排隊），上面那條給兩個出口：「中斷回覆」請 agent 停手（送 `esc`）；**強制中止**不等 agent，直接把回合收掉、解開輸入框——`esc` 送不進去（pane 沒了、herdr 斷線、agent 不理）時就靠它，不必停掉整個 bot。

![強制中止](docs/screenshots/358-abort-button.png)

**群組聊天** — 一個 Project 就是一個群組。`@<bot>` / `@all` 扇出給成員，每人各自一個 Turn；時間軸把同一次發言折成一顆氣泡。沒寫 mention 不會送出。

![群組聊天](docs/screenshots/361-readme-group-dark.png)

**Bot 設定** — 暱稱可隨時改（不必重啟）。模型 / 強度依 kind：claude 有 `--effort`（low…max，2.1+），模型與強度都能靠 TUI 的 `/model` / `/effort` 當場套用；grok 的 reasoning effort 是 per-model（4.6 才有 `xhigh`，4.5 沒有），同樣當場套用；codex 一律重啟。身份（`cc0`～`cc6`）只對 claude。

![Bot 設定](docs/screenshots/353-claude-effort.png)

**額度** — 標題列常駐 claude / codex / grok 的 5h / 7d（grok 只有週視窗）。claude 可依身份拆條（cc0 / cc1 / …）。剩餘低於門檻時顯示數字；更低時側欄 bot 列會警告。門檻由 daemon 計算，前端只讀 `low` / `critical`。

額度是**按主機**分的（[SPEC §14](docs/SPEC.md)）：這條列一次只看一台——預設本機，點進 ssh 主機上的 bot 或專案就換成那台，並掛上主機名牌。遠端的數字同樣是 daemon 去那台讀回來的（codex 走 ssh RPC，claude / grok 在那台開一個用完即丟的 pane 問 `/usage`）。

![額度條](docs/screenshots/231-quota-order-labeled.png)
![遠端主機的額度](docs/screenshots/341-quota-host-remote.png)

**身份 cc0～cc6** — 多帳號不必再寫設定：daemon 會讀每台主機登入 shell 裡的 `alias ccN='CLAUDE_CONFIG_DIR=… claude …'`，把 `cc0`～`cc6` 當成可指派的身份（[SPEC §16](docs/SPEC.md)）。同一個 `cc1` 在本機和遠端可以是不同帳號——它跟著那台機器的 alias 走。手寫的 `[[identities]]` 仍然有效，同名時以它為準。

![身份](docs/screenshots/350-identities-shell-local.png)

**Team 面板** — 從 Issues 列對某個 GitHub issue 按「組 team」：逐角色選 kind / 模型 / 強度 / **身分**（PM 用 cc2、執行者用預設帳號這種分法很常見），daemon 在資料目錄下建 git worktree 與獨立 herdr workspace，使用者自己的 checkout 不動。側欄出現 team 節點，主區是成員燈號、轉送時間軸、task 清單與暫停 / 插話 / 中止。成員的身分兩邊都標得出來。

![Team 面板](docs/screenshots/357-team-member-identity.png)

實作見 [`docs/SPEC-team.md`](docs/SPEC-team.md)。**Scheduler 是第一刀原型**：會跑、會在預算 / 協定 / 額度觸頂時暫停、daemon 重啟會把它拉回來，但還不穩定，請當實驗功能。team 可以整個中止；成員自己開出去的子 agent 也會被認領回 team 底下。

**子 agent 追蹤** — bot 在 pane 裡用 herdr 再開子 agent（平行子任務、reviewer 之類）時，AG Man 會把它掛在父 bot 底下，而不是變成一個沒人管的 pane。這靠三層機制，不靠 agent 自覺：

- **pane 血緣**：一個 bot 一個 tab，對帳時凡是 split 在某個 bot 活動 run 那個 tab 裡的 agent，一律當它的子代；名字前綴只是跨 tab / team workspace 的備援。孫代照樣掛在子代下面。
- **herdr PATH shim**：daemon 起的每個 pane，PATH 最前面放一支包裝過的 `herdr`。`agent start <name>` 自動補上 `<父 agent 名>-` 前綴；`pane split` / `tab create` 自動把父的帳號（`CLAUDE_CONFIG_DIR` / `CODEX_HOME`）與 hook 環境用 `--env` 帶下去——herdr 的 pane 是 server 生的，不會繼承呼叫端 shell，沒有這段子 pane 會用預設帳號起來、也收不到 hook。
- **herdr skill**：啟動 claude bot 前，daemon 把 `herdr --skill` 寫進該身份的 `skills/herdr/SKILL.md`（內容相同就不動），前面插一段 AG Man 規則：先重用閒置的 child、命名、`pane split --pane "$HERDR_PANE_ID"`、不要 `git stash`。codex / grok 在 persona 裡拿到同一份文字。

**主機 shell** — 主機列（含遠端）可以直接開一個 shell：裝工具、看 log、清 worktree，不必另外開終端 ssh。畫面是終端快照加一行指令輸入，附 Ctrl+C / Esc 鈕，↑↓ 翻歷史；只能操作 daemon 自己開的 pane。開著的 shell 列在主機列上，可以點回去或結束。

![遠端主機 shell](docs/screenshots/host-shell/433-remote-shell-light.png)

**遠端 `gh` 登入** — issue / team 功能靠該主機上的 `gh`。遠端沒登入時，主機列給一顆按鈕，daemon 依序試：切到已有效的帳號 → 丟掉失效的 active 帳號再切 → 把本機 `gh auth token` 經 ssh stdin 轉發過去（不進 argv、不落 log）→ 最後才走 GitHub 裝置碼，UI 顯示 `user_code` 與連結。

**圖片暫存托盤** — 畫面右緣一條常駐托盤，圖片可以先 drop 進去再切到別的 bot，拖或點進那個對話的附件托盤一起送出。跨 bot / project / team 都在，只存記憶體，重新整理就清空。

![圖片托盤](docs/screenshots/231-drop-tray-dark.png)

## 安裝與啟動

需要：Rust（`cargo`）、[Bun](https://bun.sh)、已安裝的 [herdr](https://herdr.dev)（本專案實測 0.8.2，socket protocol 20），以及至少一種 agent CLI（`claude` / `codex` / `grok`）。

```bash
git clone git@github.com:Eden-Sun/agents-manager.git
cd agents-manager

# 一次跑起 daemon（0.0.0.0:7788）+ Vite dev server（0.0.0.0:5173）
cargo dev
# ...
cargo down    # 兩個都停掉
```

`cargo dev` / `cargo down` 是 `xtask/` 的別名（見 `.cargo/config.toml`），行為是 `cargo build --bin agents-managerd` + `bun install` 之後，把 daemon 與 `vite --host` 當成一般子行程啟動，PID 記在 `target/dev.pids`，log 在 `target/dev-logs/`。

**開發版的 daemon 一律 bind 每張網卡**（不只 loopback），peer 位址與 Origin 檢查也整個放行（不只 RFC1918——Tailscale 之類 overlay network 用的是 100.64.0.0/10，硬列白名單追不完），這樣同一區網或 Tailscale 上的手機/其他機器可以直接連 `:7788` 或 `:5173`，不必再過 SSH tunnel。**只有打包成 macOS app**（`scripts/package-dmg.sh`，執行檔在 `…app/Contents/MacOS/agents-managerd`）才只 bind `127.0.0.1`、只認本機。

以前反過來：預設關閉，只有 `cargo dev` 帶的 `AM_DEV_LAN=1` 才打開。但這顆 binary 一天會被人和其他 agent 用 `cargo build --release && ./target/release/agents-managerd serve` 重啟幾十次，每次忘記帶環境變數，手機和其他機器就悄悄連不上 `:7788`。忘得掉的環境變數不是安全邊界，「在不在 .app bundle 裡」是啟動器忘不掉的，而且非開發者只會跑打包版。要覆寫就用 `AM_DEV_LAN`：`=0` 讓開發版只收本機，`=1` 讓打包版對外開。

也可以手動分開跑，這樣 7788 自己就送 UI，不必另開 Vite：

```bash
# 1. 先建前端：release daemon 預設開 `embed-ui`，編譯當下把 web/dist 內嵌進二進位，所以順序不能反
cd web
bun install
bun run build        # → web/dist
cd ..

# 2. daemon（首次啟動會寫 ~/.config/agents-manager/）
cargo build --release -p agents-managerd
./target/release/agents-managerd serve    # http://127.0.0.1:7788
```

- 跳過第 1 步也編得過，但那顆 daemon 沒有 UI：開 7788 只會拿到一行「web UI not embedded」的 404。之後補 build 前端，還要讓 daemon **重新編譯**才會嵌進去（cargo 看不到 `web/dist` 變了，見 `daemon/src/assets.rs` 開頭的說明）。
- workspace 裡的 `desktop/`（Tauri 殼）需要 `desktop/binaries/agents-managerd-<triple>` 這個 sidecar，只有 `scripts/package-dmg.sh` 會放進去，所以它不在預設建置範圍：不帶 `-p` 的 `cargo build` / `cargo test` 只建 `daemon` 與 `xtask`。要打包成 app 請看 [`docs/PACKAGING.md`](docs/PACKAGING.md)。

前端要邊改邊看時，daemon 照上面跑，另一個終端開 Vite dev server（把 /api、/hook、/ws 代理到 7788）：

```bash
cd web
bun run dev          # http://localhost:5173
```

開發期也可以 `cargo run -p agents-managerd -- serve`（debug build）。`vite.config.ts` 必須 `changeOrigin: true`：daemon 檢查 `Host` 必須是 `127.0.0.1:<port>` / `localhost:<port>`，不改寫 Host 會拿到 403。

沒有 herdr、只想看 UI：

```bash
cd web
bun install
VITE_MOCK=1 bun run dev
```

設定與資料在 `~/.config/agents-manager/`（可用 `AM_DATA_DIR` 覆寫）。`config.toml`、SQLite、`ui-token` 都在那裡，**不要**提交進 git。

## 支援的 agent kind 與 hook

回覆的主要來源是各 CLI 的 hook / notify，終端畫面只是備援。hook 身分是 **per-bot**（`bot_id` + `hook_token`），不改使用者的全域設定。

| kind | 注入方式 | 回覆事件 |
|---|---|---|
| **claude** | 每次啟動 `--settings <daemon 產生的 json>`，內含 `SessionStart` / `Stop` 指到 `agents-managerd hook claude` | stdin JSON；`last_assistant_message` |
| **codex** | 每次啟動 `-c notify=[agents-managerd, hook, codex, …]`（此實例會蓋掉使用者原本的 notify） | argv 最後一個參數是 JSON；`last-assistant-message` |
| **grok** | **沒有**每次啟動的 hook 旗標。daemon 寫入 `<GROK_HOME>/hooks/agents-manager.json` + 固定分派腳本，靠 pane env `AM_BOT_ID` / `AM_HOOK_TOKEN` 找到 bot | stdin JSON，與 claude 同一條子命令 |

三種 hook 子命令的契約相同：wall-clock ≤ 3 秒、永遠 exit 0、永遠空 stdout；失敗就 spool 到該 bot 目錄，daemon 重啟後重放。細節見 [`docs/HOOK.md`](docs/HOOK.md) 與規格書 §4、§12。

`inject_hooks = false` 時不注入（grok 則不給 token），回覆改走終端備援，氣泡會標「可能不完整」。pane 太窄時 TUI 會把字排成一欄，備援會拒絕猜、改提示把 pane 拉寬（或移到自己的分頁）。

## 文件

| 文件 | 內容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 規格書（目前標 v3.6，內文含後續修訂）：目標、資料模型、架構、hook、對帳、遠端主機、grok、群組聊天 |
| [`docs/SPEC-team.md`](docs/SPEC-team.md) | 「以 issue 為單位叫出一整個 team」的設計提案（PM / 執行者 / reviewer）。**提案 + 早期實作**，尚未併入 SPEC.md |
| [`docs/API.md`](docs/API.md) | HTTP / WebSocket 契約（前端以此為準） |
| [`docs/FRONTEND.md`](docs/FRONTEND.md) | 前端結構、mock、與 API 的對齊、已知 UI 決策 |
| [`docs/PROGRESS.md`](docs/PROGRESS.md) | 實作進度、驗收紀錄、**已知問題** |
| [`docs/HOOK.md`](docs/HOOK.md) | hook 子命令契約與時序測試 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | 打包成 macOS `.dmg`（Apple Silicon、ad-hoc 簽章）|
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 已定案的 UI 取捨 |
| [`docs/goals/`](docs/goals/) | 每項功能的目標、設計與實測紀錄（子 agent 追蹤、主機 shell、遠端 gh 登入、圖片托盤、UI 打磨） |
| [`docs/screenshots/`](docs/screenshots/) | 歷次 UI 截圖 |

後端 / 前端交接筆記在 `docs/HANDOFF-BACKEND.md`、`docs/HANDOFF-FRONTEND.md`。

## 現況

- 單 bot 對話、群組 `@mention`、遠端 host（SSH + 反向 hook）、額度條、身份、圖片附件與暫存托盤、主機 shell、遠端 gh 登入、子 agent 血緣認領，是正在用的路徑。
- 判斷邏輯（專案刪除守門、team 面板狀態、bot 燈號）抽成純函式，用 `node --test` 跑；daemon 端 `cargo test -p agents-managerd`，shim 腳本另有 `sh` 測試。
- **Issue Team 的 scheduler 仍早期**：能組隊、轉送、暫停，但不是穩定產品。協定解析失敗、預算觸頂都會 `paused`，不會自己無限轉。
- 已知問題（Codex hook 未在真帳號驗完、群組時間軸 ULID 同毫秒排序等）寫在 [`docs/PROGRESS.md`](docs/PROGRESS.md) 的「已知問題」。
