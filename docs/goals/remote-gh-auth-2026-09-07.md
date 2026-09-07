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
（你來寫）

## 進度
- [ ] 設計定案
- [ ] daemon 端點
- [ ] UI 入口
- [ ] m4p 實測通過
