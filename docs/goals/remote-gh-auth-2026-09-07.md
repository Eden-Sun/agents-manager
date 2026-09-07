# Goal：讓遠端主機也能做 `gh auth`（2026-09-07）

## 背景
agents-manager 的 issue 與 team 功能靠該主機上的 `gh` CLI。本機 OK，但遠端主機（例：`m4p`）的 `gh` 沒登入，
`GET /api/projects/{id}/issues` 對 m4p 的專案回 `HTTP 401: Requires authentication`。
現在 UI 只會顯示「gh 未登入（gh auth login）」，使用者得自己 ssh 過去登入。

## 目標
在 agents-manager 裡提供一條「對遠端主機做 gh 登入」的路，使用者不用離開 UI。

## 先想再做（把設計寫在這份檔案的「設計」段落，再實作）
可考慮的方向，選一個或組合，說明理由：
1. **裝置碼流程（device flow）**：daemon 在遠端跑 `gh auth login --hostname github.com --git-protocol ssh --web`
   （或 `gh auth login --with-token` 的替代），把 `gh` 印出的一次性代碼與網址抓回來顯示在 UI，使用者在本機瀏覽器完成，
   daemon 輪詢遠端 `gh auth status` 直到成功。
2. **token 轉發**：本機已登入時 `gh auth token` 取得 token，經既有的 ssh 連線以 `gh auth login --with-token` 餵給遠端。
   要評估安全性（token 不能落地在 log、不能出現在 ps 命令列，用 stdin 餵）。
3. 既有的「透過現有 agent 安裝 / 登入」路徑（`POST /api/hosts/{name}/tools/install`，`daemon/src/tools.rs`、`api.rs:1082`）
   能不能直接沿用給 `gh`。

## 硬規則
- 先 `git pull --rebase`。另一個 agent 同時在改 `web/src/components/ChatPanel.tsx`、`Attachments.tsx`、`AttachButton.tsx` 與 store 的附件相關程式，**不要碰那些**；你的前端改動放在 `HostsPanel.tsx` / `IssuesBar.tsx`（錯誤提示裡加「登入」按鈕）或新元件。
- daemon 改動照專案慣例：`daemon/src/hosts.rs` 的 ssh 執行方式（`sh_quote`、stdin 餵 script）、`docs/API.md` 補端點、`cargo test -p agents-managerd` 全綠。
- token / 密碼一律不寫 log、不進命令列參數。
- 不要重啟 daemon（其他 agent 在用）；改完 `cargo build --release -p agents-managerd` 通過即可，在回報裡說明要重啟才生效。
- 前端：`cd web && npx tsc --noEmit && npx oxlint src` 通過。
- 只 `git add` 你自己改的檔案，commit 訊息 `feat(hosts): …`，`git push origin main`；push 被拒就 `git pull --rebase` 再推。
- 實測對象：主機 `m4p`（`ssh m4p@100.112.229.82`，daemon 已有它的連線；`gh` 在 `/opt/homebrew/bin`）。
  可以用 `GET /api/projects/01M1S6QY63XTASKZ8H8VXWHJ1T/issues?repo=facetogo` 驗證登入後能列 issue。
  daemon 在 `127.0.0.1:7788`，token 在 `~/.config/agents-manager/ui-token`，header `X-AM-Token`。

## 設計

### 結論：三條路組合，預設 `auto`；不沿用 `tools/install`

實測 m4p（`gh 2.98.0`）並不是「完全沒登入」，而是：

- 作用中帳號 `eddysun-alt` 的 token 失效（`gh api user` → HTTP 401）
- 另一個帳號 `Eden-Sun` 在 `~/.config/gh/hosts.yml` 裡是有效的，但 `active: false`

所以 `GET /api/projects/…/issues` 才會被翻成「gh 未登入」。只做 device flow 或只轉發 token 都解得了，但這台其實先 `gh auth switch` 就夠。

三個選項的取捨：

| 方向 | 採用？ | 理由 |
|---|---|---|
| 1. 裝置碼（device flow） | 要，當後備 | 本機 gh 也沒登入時，使用者仍不用 ssh。daemon 自己跟 GitHub 要裝置碼（不在遠端掛互動式 `gh auth login`，避免 30 秒 ssh timeout 與 PTY）。拿到 token 後與 copy 走同一條 stdin 餵入。 |
| 2. token 轉發 | 要，當本機已登入且遠端沒有可用帳號 | 一鍵完成。token 只經 ssh stdin，不進 argv、不進 log、不落地。遠端用 `gh auth login --with-token --insecure-storage`，因為之後 daemon 也是非互動 ssh 跑 `gh`，keychain 讀不到。 |
| 3. `POST /api/hosts/{name}/tools/install` | **不用** | 那條路只認 `claude` / `codex` / `grok`，而且要該主機上有一個正在跑的 bot，再靠 agent 互動印 URL。`gh` 不是 agent kind；m4p 上也不該為了登入 gh 先開一個 bot。 |

`auto` 的順序（每一步成功就停）：

1. **已經能用**（`gh auth status --json hosts` 裡 github.com 的 *active* 帳號 `state=success`）→ 直接回狀態。
2. **切換**：有 `state=success` 但不是 active 的帳號 → `gh auth switch --hostname github.com --user <login>`。成功就停。m4p 上這步會失敗（作用中 token 在 keyring `default`、有效帳號在 `hosts.yml`，`gh auth switch` 拒絕切）。
3. **丟掉失效的 active**：若 active 帳號 `state=error` 且另有可用帳號 → `gh auth logout --user <失效的>`（只丟壞掉的，不丟有效帳號），再 switch。這是讓 m4p 的登入**持久**的關鍵：不登出 keyring 裡的 `eddysun-alt`，下次非互動 ssh 又會把它當 active。
4. **轉發**：目標是遠端、且本機 active 帳號可用 → 本機 `gh auth token`（只留在記憶體）→ 遠端 `--with-token`。完後若還不是 active，再 switch / 丟掉失效 active。
5. **裝置碼**：其餘情況。daemon 用 GitHub CLI 那組公開 OAuth app（`cli/cli` 原始碼註明 client secret 可進版控）跟 `https://github.com/login/device/code` 要碼，UI 顯示 `user_code` + 連結；背景輪詢 `access_token`，成功後同樣 `--with-token` 餵給目標主機。

本機 (`local`) 的 `auto` 不做 copy（自己轉發給自己沒意義），直接 device。

### 端點

都掛在既有 `/api/hosts/{name}/…`（`name=local` 合法），404 / 502 慣例與 `tools/refresh` 相同。

- `GET /api/hosts/{name}/gh` → 該主機 gh 是否安裝、作用中帳號能不能用、所有帳號、若有進行中的裝置碼則帶 `pending`（**不含** `device_code` / token）。
- `POST /api/hosts/{name}/gh/login` body `{ "mode"?: "auto"|"copy"|"device"|"switch", "user"? }`，預設 `auto`。回同一形狀，外加實際走的 `mode`。device 當下回 `pending`，之後靠 GET 輪詢。
- `POST /api/hosts/{name}/gh/cancel` → 放棄進行中的裝置碼。

狀態判準以 `gh auth status --hostname github.com --json hosts` 為準（腳本本身永遠 exit 0，避免「token 失效」被 ssh_exec 當成失敗）：**只有 active + success 才算 `logged_in`**。有帳號但 401 仍是未登入。

遠端執行沿用 `hosts.rs`：一般指令 `ssh_exec_path`；餵 token 新增 `ssh_exec_path_stdin`（script 在 argv、token 在 stdin，與 `ssh_put` 同一套 quoting）。token / 密碼不寫 `tracing`、不進錯誤字串（`gho_` / `ghp_` / `github_pat_` 一律打碼）。

裝置碼 session 只活在 daemon 記憶體（`App.gh_device`），重啟即丟。輪詢 task 可 abort。

### UI

不碰 ChatPanel / Attachments / AttachButton / store 附件。新元件 `GhAuth.tsx`：

- **HostsPanel**（本機列 + 每一台遠端）：顯示 `gh · <account>` 或「未登入」+「登入」鈕。device 進行中顯示一次性代碼（可複製）與 GitHub 連結，2 秒輪詢 GET。
- **IssuesBar** 502 且訊息像未登入 / 401 / `auth login`：錯誤列加「在 \<host\> 登入 gh」，成功後重抓 issue。文案不再叫使用者自己 ssh。

`mode=auto` 對 m4p 預期是一鍵 switch，使用者不必去瀏覽器。

### 實測

不能重啟正在跑的 daemon，新端點要重啟才掛得上。實測改跑與 daemon 相同的遠端指令（先 `gh auth switch -u Eden-Sun`），再打現有

`GET /api/projects/01M1S6QY63XTASKZ8H8VXWHJ1T/issues?repo=facetogo`

確認 200。`cargo test` / `cargo build --release` / 前端 `tsc` + `oxlint` 必須全綠。

## 進度
- [x] 設計定案
- [x] daemon 端點
- [x] UI 入口
- [x] m4p 實測通過（copy + 登出失效的 `eddysun-alt` 後，新 ssh session 仍是 Eden-Sun；`GET /api/projects/01M1S6QY63XTASKZ8H8VXWHJ1T/issues?repo=facetogo` → 200。新 HTTP 端點要重啟 daemon 才掛得上。）
