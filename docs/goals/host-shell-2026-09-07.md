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
- [x] 設計定案（寫在下面）
- [x] daemon 端點（新檔 `daemon/src/shell.rs`；`api.rs` 只加 router + handler，`state.rs` 只加一個欄位）
- [x] HostShellPanel（新檔 `web/src/components/HostShellPanel.tsx`）
- [x] 本機 / m4p 實測（用**另一個** daemon 實例，見下面「實測」）

## 實測（2026-09-07）

不重啟使用者的 daemon，改用一個隔離的第二實例：`AM_DATA_DIR=<scratch>` +
`--config <scratch>/config.toml`，`listen 127.0.0.1:7799`、本機 herdr session 用全新的
`am-shelltest`、遠端主機另取名 `m4ptest`（同一台 m4p、同一個遠端 session，但 ssh ctl /
socket 路徑不同，`hook_port` 改 7799，所以不碰使用者那條反向轉發）。UI 用
`VITE_DAEMON=http://127.0.0.1:7799 npx vite --port 5312`。實測前後都確認過使用者那個
daemon 的 `/api/state` 一切正常（三個 project 的 `workspace_id` 沒動）。

過的項目：

- `local`：開 shell → `echo hi` → `hi`；`pwd && git rev-parse --abbrev-ref HEAD` 正確；
  `sleep 300` 後送 `ctrl+c` 出現 `^C`；`clear` 真的清空。
- `local` 兩條 workspace 路徑都走過：沒有可借的 workspace → `workspace.create` 並用 root
  pane（`w1:p1`）；有可借的 → `tab.create` 開新分頁（`w1:t2` / `w1:t3`），cwd 分別是專案根
  與明確指定的 `daemon/src`。分頁都拿到整個 workspace 的寬度（185 欄）。
- `m4ptest`（真的遠端）：`hostname` → `m4p.local`、`sw_vers` → `26.6.2`、`gh auth status`
  印出遠端那顆 `gh` 的帳號；`ctrl+c` 一樣有效。
- 白名單：對**沒開過的** pane（`w8:p3`，bot pane 的形狀）打 `keys` / `text` / `terminal`
  都是 `404 {"error":"not_found","what":"shell"}`。
- 上限：第 9 個 → `409 too_many_shells`；`DELETE` 兩次都 200（冪等）。
- 錯誤碼：壞 `source` → 400、不存在的 host → 404、空 `keys` → 400。
- UI：截圖在 `docs/screenshots/host-shell/`（430–436），深色 / 淺色 / 430px 手機寬度都拍過，
  CDP 主控台無錯誤；Tab 走得到面板上每一顆按鈕與輸入框。

**一個實測發現（已寫進 API.md 與 UI-DECISIONS.md）**：herdr 的 `recent` /
`recent_unwrapped` 只給「已經捲出畫面」的內容，還沒捲過的 pane 兩者都回 `text: ""` +
`truncated: true`（`visible` 有內容）。面板因此在空的時候明講「還沒有捲出畫面的內容」，
而不是顯示一片空白。

---

## 設計定案（2026-09-07，實作前寫下）

大方向照「建議設計」走，下面是實作時真正定下來、以及和建議不同的地方。

### daemon

**新檔 `daemon/src/shell.rs`**，只在 `api.rs` 加 router 與 handler、在 `state.rs` 加一個欄位。
`herdr.rs` / `lifecycle.rs` 一行都不改：要用的 `tab_create` / `pane_read` / `pane_send_text` /
`pane_send_keys` / `pane_close` / `pane_size` 都已經在 `HerdrClient` 上，收尾用的
`lifecycle::close_pane_and_tab` 本來就是 `pub(crate)`。

- **註冊表就是白名單。** `App.host_shells: Mutex<Vec<HostShell>>`（`{host, session,
  workspace_id, tab_id, pane_id, cwd, created_at}`）。除了 `POST …/shells` 以外，每一支端點
  第一件事都是在這張表裡找 `(host, pane_id)`，找不到就 404 `{"error":"not_found",
  "what":"shell"}`。這樣「不能對任意 pane_id 送鍵」不是靠檢查 pane 長什麼樣，而是靠
  「不是我開的就不認」——daemon 重啟後表是空的，所有舊 pane 一律不認，符合建議裡的
  「直接清空」。
- **workspace：借用，不新開。** 依序試 ① 該主機任一 live project 的 `workspace_id`（用
  `workspace_get` 確認還在）② 都沒有才 `workspace_create(cwd, "shell", {})`，並且**直接用它的
  root pane 當這個 shell**，不再多開一層 tab（root pane 本身已經是獨占寬度的一個 tab，
  和 `lifecycle::acquire_run_pane` 的 `fresh_root` 同一個理由）。借到現成 workspace 時才
  `tab_create`。
  - 刻意**不**把新建的 workspace 寫回 `projects.workspace_id`：那一格是 bot 的地盤，shell
    只是路過，寫回去會讓下一次 bot 啟動把自己的 pane 開進一個標籤叫 `shell` 的 workspace。
- **cwd**：body 給了就用；沒給就取該主機任一 live project 的 `path`；都沒有就
  `HostConn::home()`。回應一律回實際用的 cwd，UI 直接顯示。
- **session 只用 manager 的那一個**（`App::session_for_host`）。本機刻意不支援 `default`：
  那是使用者自己的 herdr session，daemon 不往裡面塞東西（`ensure_session` 已經有同樣的
  紅線）。主機沒連線 → 502，不要開了個開不起來的 pane 進表裡。
- **每台主機上限 8 個 shell**（超過回 409 `{"error":"conflict","reason":"too_many_shells"}`）。
  純 shell 沒有 agent 那種「一個 bot 一個」的天然上限，按錯幾次就能開出一整排看不見的
  pane；8 個夠用又還看得完。
- **`GET …/shells` 會順手掃墓。** 每一列都 `pane_get`，不在了就從表裡移掉再回。使用者在
  herdr 裡把 pane 關掉是很正常的事，UI 不該一直顯示一個死掉的 shell。
- **`DELETE` 是冪等的**：`pane_close` 之後 `close_pane_and_tab` 收空 tab，然後從表裡移掉；
  找不到那一列也回 200（已經沒了就是成功，同 `tab_close` 的 `Ok(false)`）。
- **`text` 端點的 Enter 是分兩步送的**：`pane_send_text(text)` 再 `pane_send_keys(["enter"])`。
  把 `\n` 塞進 `send_text` 在 herdr 是「貼上換行」而不是「按下 Enter」（`fold_newlines` 的
  註解記的就是這件事），所以 `enter` 必須是獨立的一次按鍵。`text` 允許空字串——「只按
  Enter」是終端裡真的會用到的動作。
- **不發 WS 事件。** shell 沒有 run、沒有 turn，也沒有任何別的視圖需要跟著動；狀態就是那張
  快照，由開著面板的人自己輪詢。少一種事件型別，`ring` 也不會被打字聲塞滿。

### web

**新檔 `web/src/components/HostShellPanel.tsx`**；既有檔案只碰 `App.tsx`（多一個路由分支）、
`store.ts`（一個欄位＋動作，並在四個 `select*` 裡清掉它）、`HostsPanel.tsx`（兩顆按鈕）、
`api/index.ts`、`api/types.ts`、`api/normalize.ts`、`api/mock.ts`、`styles.css`。

- **輪詢寫在面板裡，不改 `useTerminalSnapshot`。** 那支 hook 的參數是 `botId`，要它同時吃
  bot 與 host 得改簽名，而 `ChatPanel` / `BlockedModal` 都在用——為了一個新面板去動兩個現有
  呼叫端不划算。節奏、來源、清空時機都照抄它（1 秒、換目標就在 render 當下清畫面）。
- **兩種讀法，預設「畫面」。** `visible` 是「終端現在長什麼樣」，這是 shell 的常態；另一顆
  切到 `recent_unwrapped` ＋ 行數選單，用來看已經捲上去的輸出（`ls` 一個大目錄、
  `gh auth status` 之後再滾幾行就會用到）。
- **底部一行輸入 + 一排按鍵。** Enter 送出並清空、↑/↓ 走歷史（每台主機各記一份在
  localStorage，20 筆）、`ctrl+c` / `Esc` / `Tab` 按鈕直接打 `…/keys`，另有「清畫面」＝送
  `clear` 這一行（不是前端清 state：使用者要的是終端真的乾淨，不是畫面假裝乾淨）。
  送出後立刻重讀一次，不等下一個 tick。
- **不做整頁鍵盤接管。** `BlockedModal` 會把每一顆鍵都轉給 pane，那是因為那裡在回答 TUI 的
  問題；shell 的主要動線是「打一行、按 Enter」，把 ↑↓ 讓給指令歷史比讓給 pane 有用得多，
  Tab 也得留給瀏覽器走焦點。要按整片鍵盤的情境，按鍵列上的那幾顆＋`text` 端點就夠。
- **`shellView: {host, paneId} | null`**，和 `selectedBotId` / `selectedProjectId` /
  `selectedTeamId` / `teamLaunch` 互斥，在 `App.tsx` 的分支裡排**最前面**（它是使用者剛剛按
  出來的暫時性視圖，比背景那個選取更該被看到）。不寫進 localStorage 的選取記憶：pane id
  活不過 daemon 重啟，記住它只會在下次開啟時指向一個不存在的 shell。
- **缺端點要靜默退回**（FRONTEND.md §8）：405 / 501 / 沒有機器碼的 404 → `hostShellSupported
  = false`，兩顆「開 shell」從此消失，不跳錯誤。
- 深色／淺色只用既有 token；`≤1024px` 時按鍵列自己折行，輸入框與終端各自捲。

### 文件
`docs/API.md` 補一節「主機 shell」，`docs/UI-DECISIONS.md` 補這次的取捨（為什麼是主面板而
不是 modal、為什麼不做整頁鍵盤接管、為什麼「清畫面」是送指令）。
