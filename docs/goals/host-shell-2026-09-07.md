# Goal：對主機（含遠端）開一個 shell 視窗、在 UI 下指令（2026-09-07）

## 需求
使用者想在 agents-manager 裡直接對某台主機（本機或 `m4p` 這種遠端）開一個 shell，下指令、看輸出，不用另外開 terminal ssh。
用途：裝工具、看 log、跑 `gh auth status`、清 worktree 之類的雜事。

## 建議設計（可調整，理由寫進 docs/UI-DECISIONS.md）
- daemon：
  - `POST /api/hosts/{name}/shells` `{cwd?}` → 在該主機 herdr 的 manager session 開一個 tab（`herdr.rs` 的 `tab_create`；workspace 用該主機任一 live project 的 `workspace_id`，沒有就 `workspace_create` 一個標籤 `shell` 的），純 shell、不起 agent。回 `{pane_id, tab_id, workspace_id, cwd}`。
  - `GET /api/hosts/{name}/shells` 列出還活著的（存在 `App` 記憶體即可，daemon 重啟就用 herdr snapshot 對回來或直接清空）。
  - `GET /api/hosts/{name}/shells/{pane_id}/terminal?source=&lines=` → `pane_read`，回的形狀比照 `GET /bots/{id}/terminal`（含 columns/rows）。
  - `POST /api/hosts/{name}/shells/{pane_id}/text` `{text, enter: true}` → `pane_send_text` +（enter 時）`pane_send_keys(["Enter"])`；`POST …/keys` `{keys:[…]}` 給 ctrl+c / esc / 方向鍵。
  - `DELETE /api/hosts/{name}/shells/{pane_id}` → `pane_close`（tab 會自動收）。
  - 只允許操作 daemon 自己開的 pane（記在 App 裡的清單），不能對任意 pane_id 送鍵。
  - 本機主機（`local`）同樣可用。
- web：
  - 主機設定（`HostsPanel.tsx` 每一列）加「開 shell」；側欄專案標題的主機徽章旁也可放入口（可選）。
  - 新的主面板 `HostShellPanel`：標題列（主機名、cwd、關閉 / 結束 shell）、終端快照區（沿用 `TerminalTab.tsx` 的呈現與 `useTerminalSnapshot` 的輪詢方式，但打新的 host 端點）、底部一行指令輸入（Enter 送出、↑↓ 歷史、Ctrl+C 按鈕、Esc 按鈕、清畫面）。
  - store 加 `shellView: {host, pane_id} | null`，`App.tsx` 的主面板路由比照 `teamLaunch` 那樣多一個分支。
  - 深色 / 淺色、手機寬度都要能用；鍵盤可達。
- 文件：`docs/API.md` 補端點；`docs/UI-DECISIONS.md` 補取捨。

## 硬規則
- **不要 git stash / --autostash**：工作樹裡未提交的改動是其他 pane 的。HEAD 已等於 origin/main，不用 pull。
- 其他 agent 同時在改：`reconcile.rs`、`lifecycle.rs`（子 agent 對話）、`ChatPanel.tsx`、`Attachments.tsx`（圖片托盤）、`hosts.rs` / `GhAuth.tsx`（gh 登入）。你新增檔案優先，必須改既有檔案時只改 `api.rs` 的 router 與 handler、`state.rs` 的欄位、`HostsPanel.tsx`、`App.tsx`、`store.ts`、`types.ts`、`normalize.ts`、`api/index.ts`、`styles.css`，且 hunk 要小。
- daemon：`cargo build --release -p agents-managerd` 與 `cargo test -p agents-managerd` 要過。**不要自己重啟 daemon**，回報時說明要重啟才生效。
- web：`cd web && npx tsc --noEmit && npx oxlint src && npm run build` 過。
- 只 `git add` 自己改的檔案，commit 訊息 `feat(hosts): …`，`git push origin main`；被拒就 `git pull --rebase --no-autostash`。
- 驗證：daemon 在 `127.0.0.1:7788`，token 在 `~/.config/agents-manager/ui-token`（header `X-AM-Token`）。你可以用 API 對 `local` 開 shell 送 `echo hi` 驗證；遠端 `m4p` 也試一次。UI 由派工者用 ego 試用，你把截圖存到 `docs/screenshots/host-shell/`。

## 進度
- [ ] 設計定案（寫在這裡）
- [ ] daemon 端點
- [ ] HostShellPanel
- [ ] 本機 / m4p 實測
