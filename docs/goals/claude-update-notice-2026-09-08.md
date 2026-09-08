# Claude 的「Update installed · Restart to update」要浮到 header 上，點一下就重啟套用（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的 hunk、不要重啟 daemon、只改任務需要的檔）。
daemon 一個 commit、web 一個 commit，做完在此打勾。

## 背景
- Claude Code 自動更新後會在 TUI 最底下那行（跟使用者的 statusLine 同一行、靠右）印：
  ```
  ✔ Update installed · Restart to update
  ```
  2026-09-08 本機 7 個 claude pane 都掛著這行（`herdr pane read <pane>` 看得到；有 statusLine 時同一行左邊是
  `hunta | amber | OP5 10% | …`，靠右才是這句）。使用者要一路點進終端才看得到，等於沒通知。
- 既有可借用的東西：
  - `daemon/src/tui_prompts.rs`：`flatten()` 把畫面壓成一行小寫；`spawn_survey_watcher` 每 10 秒對 `idle`/`blocked` 的 run 做 `pane.read visible 80`。`is_login_menu` / `is_feedback_survey` 是同一種「認畫面」的判斷，照那個寫法。
  - `runs.agent_title` / `runs.status_line`：run 上已經有「從畫面讀來的字」欄位，`events.rs` 更新後 emit `bot_status`。照這個模式。
  - `POST /api/bots/{id}/restart` 已存在（`api.rs:81`，web `store.restartBot`）。重啟就是套用更新，不用新 API。

## 1. daemon：認出通知、掛在 run 上
- [x] `db.rs`：`runs` 加欄位 `update_notice TEXT NULL`（照既有 migration 寫法，`ALTER TABLE … ADD COLUMN` 加 `IF NOT EXISTS` 或 try-ignore），`Run` struct 加 `pub update_notice: Option<String>`，`lifecycle.rs` 建 run 的地方補 `update_notice: None`。
- [x] `tui_prompts.rs`：`pub fn update_notice(screen: &str) -> Option<String>`——`flatten` 後含 `update installed` 且含 `restart to update`（兩個都要中，避免 agent 正在讀這份原始碼時誤判）就回 `Some("Update installed · Restart to update")`（固定字串，不要回整行，那行還有 statusLine 的字）。之後 claude 若改文案（例如 `new version available`），加在同一個函式裡。單元測試：正常、折行、statusLine 同一行、只有一半字樣→None。
- [x] 新檔 `daemon/src/update_watch.rs`：`spawn_update_watcher(app)`，每 30 秒掃 `db::all_active_runs` 裡 `kind == "claude"` 且 `state == "running"` 的 run（不限 idle，working 也掃——通知是回合結束時印的，但要在使用者下一句話送出前看到），`pane.read visible 80` → `update_notice()`；跟 `run.update_notice` 不同時 `UPDATE runs SET update_notice=?` 並 emit `bot_status`（照 `events.rs` 更新 `agent_title` 的那段）。畫面上沒有了就清回 NULL（重啟後新 run 本來就是 NULL）。讀不到 pane 就跳過，不清。`main.rs` 接上。
- [x] `docs/API.md` §run 欄位表加 `update_notice`；`docs/SPEC.md` 加一小節（放 §15 附近或 TUI 對話框那節旁）：認的是哪句、多久掃一次、為什麼掛在 run 而不是 bot（更新是這個 process 的事，重啟就沒了）。

## 2. web：header 上一顆點得下去的徽章
- [x] `api/types.ts` `Run.update_notice: string | null`、`api/normalize.ts` 讀 `update_notice`。
- [x] 新檔 `web/src/components/UpdateBadge.tsx`：`<UpdateBadge botId />`。run 的 `update_notice` 非 null 才畫，放在 `ChatPanel.tsx` 的 `.main-title-row` 裡 `HostBadge` 後面（一行 hunk）。長相：小 chip，`⬆ 有更新 · 重啟套用`，tooltip 寫原句 `Update installed · Restart to update` 與「重啟這個 bot 會用新版 claude 接著跑（session 會 --resume）」。
  - 點下去：`working` / `blocked` 時先問一句（用專案裡既有的確認方式，例如 `HeadMoreMenu` 的 danger item 或 `ConfirmDialog`，不要 `window.confirm`）「它正在忙，現在重啟會打斷這一回合」；`idle` 直接 `restartBot(botId)`。重啟中 chip 變灰、文字「重啟中…」。
  - 遠端主機的 bot 一樣有（run 是哪台的就哪台）。
- [x] 側欄 `BotRow`（`Sidebar.tsx`）在名字後面加一個 8px 的小 `⬆`（class `bot-update-dot`，tooltip 同上）——只是提示，點了還是走 header 那顆。hunk 要小。
- [x] `styles.css` 加 `.update-badge` / `.bot-update-dot` 一小段，顏色用既有的 accent-soft，不要新顏色。
- [x] `api/mock.ts`：讓其中一個跑著的 bot 的 run 帶 `update_notice`，截圖用。
- [x] `docs/UI-DECISIONS.md` 補「Claude 更新通知」：為什麼是 header 而不是 toast（它會一直在、不是事件）；為什麼點了就是重啟（Claude 自己說的就是 restart）；為什麼忙碌時要問。`docs/FRONTEND.md` 元件清單補 `UpdateBadge`。

## 驗證
- `cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`（現在 HEAD 是綠的，335 個）。
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`（既有 36 個 warning 不算）。
- 真機：**不要重啟 daemon**，但本機 7 個 claude pane 現在就掛著那句，可以拿 `herdr pane read` 的輸出跑單元測試；UI 用 mock（`VITE_MOCK=1 npx vite --port 5199`）截 header 徽章與側欄小點各一張，放 `docs/screenshots/update-notice/`。
- 做完後**需要重啟 daemon 才生效**，回報時寫明。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon、沒做到的與原因。
