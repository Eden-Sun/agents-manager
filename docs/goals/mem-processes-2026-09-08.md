# RAM 那格可展開：列出真正吃記憶體的 process，並能 kill 掉不是 AG Man 的（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon、只改任務需要的檔）。
一個功能一個 commit（daemon 一個、web 一個可以），做完在此打勾。

## 背景
- 左上角 `RAM 4.5G` 是 `daemon/src/memstat.rs` 每 15 秒跑 `ps -Awwo pid,ppid,rss,args`，找出所有 `herdr` 根程序，把整棵子樹的 RSS 加總（SPEC §15、`docs/API.md` 的 `GET /api/mem`）。
- 現況本機底下有 15 個 `claude` 各 220–390 MB，其中一半是使用者自己在 herdr 開的 pane 或舊的 `--resume`，**不是 AG Man 管的 bot**，但使用者從 UI 看不出來哪些可以砍。
- 已驗證：macOS `ps -Ewwo pid=,args= -p <pid>` 會把該程序的環境變數印在 args 後面（同一個 user 的程序都讀得到）。AG Man 起的 bot 都帶 `AM_BOT_ID=<bot id>`（`lifecycle.rs` 啟動時注入）；只有 `HERDR_PANE_ID` 沒有 `AM_BOT_ID` 的，就是使用者自己開的 pane。Linux 改讀 `/proc/<pid>/environ`（NUL 分隔）。

## 1. daemon：`GET /api/mem/processes?host=<name>`
- [x] 新檔 `daemon/src/memproc.rs`（不要把邏輯塞進 `memstat.rs`；`memstat::sum_herdr` 的樹走訪可以抽成共用 helper 但 hunk 要小）。
- [x] 回傳該主機 herdr 樹裡**每一個 process**（不含 herdr 本身也可以，但要標出來）：
  ```json
  {"host":"local","sampled_at":"…","processes":[
    {"pid":59407,"ppid":37845,"rss_bytes":412000000,"exe":"claude","argv":"claude --dangerously-skip-permissions",
     "pane_id":"w168:p1","bot_id":null,"bot_name":null,"project_id":null,"owner":"pane|bot|herdr",
     "subtree_bytes":420000000,"children":3}
  ]}
  ```
  - `owner`：`bot`（環境有 `AM_BOT_ID` 且 daemon 認得這個 bot）、`pane`（只有 `HERDR_PANE_ID`，使用者自己開的）、`herdr`（herdr 本身）、`unknown`（都沒有，例如 daemon 起的 probe）。
  - `subtree_bytes`：這個 process 加上它底下所有子孫的 RSS 總和；UI 用這個排序、顯示「砍掉這個能省多少」。列出時只列**每棵子樹的最上層非 herdr 程序**（通常是 pane 的 shell 或直接的 CLI）＋它底下 RSS 最大的那個 CLI；不要把幾十個 node worker 都攤平列出來。簡單做法：只列 `exe` 在 {`claude`,`codex`,`grok`,`node`,`bash`,`zsh`,`sh`,`fish`} 且 `subtree_bytes ≥ 8 MiB` 的；其餘併入父程序的 `subtree_bytes`。
  - 環境變數讀法：macOS 一次 `ps -Ewwo pid=,args=`（所有程序）再自己切；Linux `/proc/<pid>/environ`。遠端主機走 `conn.ssh_exec_path()` 跑同一支 script（script 裡用 `uname` 分支）。讀不到 env 的程序 `owner: "unknown"`。
  - `bot_id` → 用 `app` 現有的 bot 查表補 `bot_name` / `project_id`；查不到的（bot 已刪）仍回 `bot_id`，`owner: "bot"`。
- [x] `POST /api/mem/processes/kill` body `{"host":"local","pid":59407,"signal":"TERM"|"KILL"}`（預設 TERM）。**只允許 kill herdr 樹裡的程序**（先重新取樣確認 pid 在樹裡，且 exe 不是 `herdr`），不在樹裡回 400。`owner == "bot"` 的也擋（回 409，訊息「這是 AG Man 的 bot，請用停止 bot」）——砍 bot 走現有的 `POST /bots/{id}/stop`。kill 完直接觸發一次 `memstat` 取樣並推 `mem_updated`。
- [x] `docs/API.md` 補這兩支；`docs/SPEC.md` §15 加 15.x「展開看程序 / 砍程序」，把 owner 判定規則寫進去。
- [x] 單元測試：owner 判定（有 `AM_BOT_ID` / 只有 `HERDR_PANE_ID` / 都沒有）、subtree 加總、kill 拒絕不在樹裡與 bot 的 pid。

## 2. web：點 RAM 展開清單
- [x] `api/types.ts` `MemProcess`、`api/normalize.ts` `toMemProcesses`、`api/index.ts` `fetchMemProcesses(host)` / `killMemProcess(host,pid,signal)`、`api/mock.ts` 對應（mock 給 4–5 筆：2 個 bot、2 個 pane、1 個 unknown）。
- [x] 新檔 `web/src/components/MemPopover.tsx`：`MemBadge` 改成可點（`button`，保留現有 tooltip），點開一個 popover（比照專案裡既有的浮窗，例如「這回合的提問」浮窗的做法；不要新做一套 overlay 系統）。內容：
  - 標題列：`本機 RAM 4.5G · herdr 0.2G · 15 個 process`，右邊「重新整理」。
  - 表格，依 `subtree_bytes` 降冪：`大小 | 程式（argv 縮到一行，hover 看全文）| 誰的（bot 名 / 「自己開的 pane w168:p1」/ herdr / ?）| 動作`。
  - 動作：`owner=bot` → 「停止 bot」（呼叫既有 `stopBot`）；`owner=pane|unknown` → 「結束」（TERM），按一次變成「強制」（KILL）確認。有一行淡字提醒「結束的是那個 pane 裡的程序，pane 本身還在」。
  - 開著時每 15 秒（跟 `mem_updated` 同步）重抓一次；關閉就不抓。
  - 遠端主機（標題列的 `@host` 那顆）一樣可以點，用 `host` 參數。
- [x] `styles.css` 加 `.mem-popover` 一小段；`MemBadge.tsx` 的改動只有把 `span` 換成可點與掛 popover。
- [x] `docs/UI-DECISIONS.md` 補一節「RAM 展開清單」：為什麼只列子樹頂層、為什麼 bot 走「停止 bot」不走 kill、TERM→KILL 兩段式。
- [x] `docs/FRONTEND.md` 若有元件清單，補 `MemPopover`。

## 驗證
- `cargo build --release -p agents-managerd`、`cargo test -p agents-managerd`。
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`。
- curl：`GET /api/mem/processes?host=local` 看得到 owner 分類正確（拿目前這台的 15 個 claude 對照 `ps -Ewwo pid=,args=`）；`POST …/kill` 對一個不在樹裡的 pid（例如 1）回 400，對一個 `owner=bot` 的回 409。**不要真的砍使用者正在用的 pane**。
- UI 截圖：`OUT=/tmp/shots node scripts/ui-goal-shots.mjs` 或 ego-browser，展開後的清單一張，放 `docs/screenshots/mem-processes/`。
- 完成後**需要重啟 daemon 才生效**，回報時寫明，不要自己重啟。

## 回報
三到五行：commit hash、驗證數字、需重啟 daemon、沒做到的與原因。
