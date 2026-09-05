# 前端（`web/`）說明

Vite + React 19 + TypeScript + Zustand + 自寫 CSS，對應 SPEC §3.2 與附錄 D 的 **M7**。
API 契約以 [`docs/API.md`](./API.md) 為準（型別已對齊，見 §3）。

---

## 1. 執行方式

```bash
cd web
npm install

# A. 真後端（需先啟動 daemon）
cargo run -- serve        # 另一個終端，daemon 監聽 127.0.0.1:7788
npm run dev               # http://localhost:5173

# B. Mock 模式（不需要 daemon、不需要 herdr）
VITE_MOCK=1 npm run dev

# 建置（M8 由 rust-embed 內嵌 web/dist）
npm run build             # tsc -b && vite build → web/dist
npm run lint              # oxlint
```

`vite.config.ts` 的 proxy 把 `/api`、`/hook` 轉 http、`/ws` 轉 ws 到 `127.0.0.1:7788`。
**`changeOrigin: true` 是必要的**：daemon 會檢查 `Host` 必須是 `127.0.0.1:<port>` /
`localhost:<port>`（API.md §0），不改寫 Host 會拿到 403。
要指向別的 port 用環境變數：`VITE_DAEMON=http://127.0.0.1:9999 npm run dev`。

---

## 2. 檔案結構

```
web/src/
  api/
    types.ts       # SPEC §7 / API.md 的 TypeScript 型別（單一集中處）
    normalize.ts   # 寬鬆解碼：吸收 0/1 vs bool、args vs args_json、巢狀 vs 扁平等差異
    transport.ts   # Transport 介面 + 真後端實作（fetch + WebSocket 自動重連）
    mock.ts        # 記憶體假後端（VITE_MOCK=1）
    index.ts       # 依 VITE_MOCK 選 transport，對外只暴露具名 API 函式
  store/store.ts   # 單一 Zustand store：server state 鏡像 + UI state + WS 事件處理
  components/
    Sidebar.tsx    # Project 分組、狀態燈、start/stop、新增 Project / Bot 表單
    ChatPanel.tsx  # 標題列、對話/終端分頁、氣泡列表、輸入框
    BlockedPanel.tsx # blocked 時的終端快照 + 按鍵面板
    TerminalTab.tsx  # recent_unwrapped 唯讀快照 + 刷新
    StatusLamp.tsx   # §2.2 合成燈號
  styles.css       # 全部 CSS（淺色在 :root，深色在 prefers-color-scheme）
  App.tsx / main.tsx
```

設計原則：**所有跟後端 JSON 形狀有關的知識都集中在 `api/`**。元件與 store 只看
`types.ts` 的型別，之後 API 有變動只要改 `normalize.ts`。

---

## 3. 與 `docs/API.md` 的對齊狀況

型別是先依 SPEC §7 寫好，`docs/API.md` 出現後逐項核對過，目前**完全一致**：

| 項目 | 對齊方式 |
|---|---|
| `GET /api/session` → `{token, port}` | `transport.ts` 取 `token`，之後 REST 帶 `X-AM-Token`、WS 帶 `?token=` |
| `GET /api/state` 巢狀 `projects[].bots[].run` | `normalize.toState()` 攤平成 `projects / bots / runs` |
| `autostart` / `inject_hooks` bool、`adopted` 0/1 | `normalize.bool()` 同時吃 bool 與整數 |
| 錯誤 body `{error, reason\|message\|what}` | `ApiError` 依 `reason → message → what → error` 取人類可讀訊息 |
| `POST /prompt` → `{turn_id, message_id, delivery}` | `delivery=unknown` 會跳警示並提供「放棄該回合」 |
| `GET /messages` → `{messages(正序), turns, has_more}` | `turns` 用來判斷 in-flight / delivery 警示 |
| `GET /terminal` → `{text, revision, truncated, agent_status}` | blocked 面板每秒輪詢 `visible`、終端分頁手動刷新 `recent_unwrapped` |
| WS `{seq, type, data}`、`resync` | 記錄最高 `seq`，重連帶 `?since=`；`resync` → 重新 `GET /state` + 目前 bot 的 messages |
| keys 鍵名 `enter/esc/y/n/up/down/ctrl+c` | 按鍵面板使用同一組字串，並帶 `expect_run_id` |

**一處刻意的差異**：API.md §6 建議 `source = "hook"` 不加標籤。本 UI 依 SPEC §3.2
（「assistant 氣泡顯示來源標籤（hook / terminal-fallback）」）兩種都顯示標籤，
`terminal_fallback` 另外加「可能不完整」黃字。

**`lamp` 欄位**：`GET /api/state` 會回後端算好的 `lamp`，但前端**自己重算**
（`normalize.lampOf(run, connected)`），因為 `bot_status` WS 事件只帶 `run` 不帶 `lamp`，
若採用後端值會在事件更新後失準。兩邊的規則相同（SPEC §2.2）。

---

## 4. UI 行為重點

### 狀態燈（SPEC §2.2）

| lamp | 呈現 |
|---|---|
| `disconnected` | 灰點 |
| `offline` | 空心灰圈 |
| `starting` | 黃點閃爍 |
| `stopping` | 黃點 |
| `idle` | 綠點 |
| `working` | 藍點 + 外圈脈動動畫 |
| `blocked` | 紅點 |
| `unknown` | 灰黃點 |

`prefers-reduced-motion: reduce` 時所有動畫關閉。

### 輸入框鎖定（SPEC §6.3）

`composerState()` 依序判斷並顯示原因：

1. 未選 Bot
2. daemon 與 herdr 連線中斷
3. 無 active Run（「請先按啟動」）
4. Run 不在 `running`
5. `agent_status = blocked`（引導到上方按鍵面板）
6. 有 `delivery = unknown` 的 Turn → 附「放棄該回合」按鈕（`POST /turns/:id/abandon`）
7. 有 `in_flight` Turn → 附「中斷（esc）」按鈕

送出時前端產生 `client_request_id`（`crypto.randomUUID()`，非安全來源時退回自製 v4），
作為冪等鍵。使用者氣泡**不做本地暫存**，一律等 `message_added` 推回來（依 `message.id` 去重），
符合 API.md §9 的建議。後端回 409 時，`reason` 直接顯示在右下角通知。

輸入框：Enter 送出、Shift+Enter 換行，並排除輸入法組字中的 Enter（`isComposing`）。

### blocked 面板

`agent_status = blocked` 時，在聊天視窗上方展開：等寬字體的 `visible` 快照（每 1 秒輪詢
`GET /api/bots/:id/terminal?source=visible&lines=40`），下方是
`Enter / Esc / y / n / ↑ / ↓ / ctrl+c` 按鍵按鈕，呼叫 `POST /api/bots/:id/keys` 並帶
`expect_run_id`。

### 終端分頁

右上「對話 / 終端」切換。終端分頁是 `recent_unwrapped` 的唯讀快照，可選 50/100/200/500 行，
手動「刷新」。不做 xterm.js（SPEC §9 非目標）。

### WebSocket

`transport.ts` 內建指數退避重連（上限 10 秒 + jitter）。重連時帶 `?since=<最高 seq>`；
收到 `resync` 就重新 `GET /api/state` 並重載目前 bot 的訊息（有 in-flight 保護避免重入）。
`project_changed` / `bot_changed` 也會觸發重新 `GET /api/state`。

### 主題與版面

深淺色跟隨系統（`prefers-color-scheme`），不提供手動切換。
版面在 1280 與 900 寬都測過；≤1080 會收起 sidebar 的連線文字與標題列的 run/pane 細節，
≤780 sidebar 變成可開合的抽屜（左上 ☰）。

---

## 5. Mock 模式（`VITE_MOCK=1`）

`api/mock.ts` 是一個記憶體假 daemon，實作 API.md 的全部端點與 WS 事件（回應形狀刻意
和 `daemon/src/api.rs` 一致），時間軸壓縮：start 約 1.4 秒就緒、回覆約 1.9 秒。
左上角會顯示 `MOCK` 徽章。預設種了一個 project（`agents-manager`）與兩個 bot。

### 用訊息內容觸發不同情境

| 訊息包含 | 效果 |
|---|---|
| `blocked` 或 `rm -rf` | agent 進入 `blocked`，終端快照顯示確認框；按 `y` / `Enter` 繼續，`n` / `Esc` / `ctrl+c` 取消 |
| `fallback` | 回覆以 `source = terminal_fallback`、`incomplete = 1` 進來（標「可能不完整」） |
| `slow` | 回覆延遲約 8 秒，方便觀察輸入框鎖定與 typing 指示 |
| 其他 | 約 1.9 秒後以 `source = hook` 回覆 |

### 瀏覽器 console 的除錯開關

```js
__amMock.dropSocket()   // 斷線 1.5 秒後重連並送 resync
__amMock.resync()       // 直接送一則 resync
__amMock.block('demo-claude')  // 不送訊息直接讓某個 bot 進 blocked
__amMock.disconnect() / __amMock.reconnect()  // 模擬 daemon 與 herdr 斷線（燈號轉灰）
```

---

## 6. 驗收紀錄（2026-09-05）

截圖在 [`docs/screenshots/`](./screenshots/)，用 headless Chrome（CDP）自動走完流程產生。

### Mock 模式完整流程

| 檔案 | 內容 |
|---|---|
| `01-overview.png` | 初始畫面，兩個 offline bot |
| `02-new-project-form.png` / `03-new-bot-form.png` | 新增 Project（`/Users/me/project/demo-app`）與 Bot（`demo-claude`, claude, `--model opus`） |
| `04-starting.png` | start 後 `starting`（黃閃燈） |
| `05-working.png` | 送出訊息，`working`（藍動畫）+ 輸入框鎖定 + typing 指示 |
| `06-reply-hook.png` | 收到 `source = hook` 的回覆 |
| `07-reply-fallback.png` | `source = terminal_fallback` 回覆，標「可能不完整」 |
| `08-blocked.png` | blocked 面板：終端快照 + 按鍵列，輸入框顯示鎖定原因 |
| `09-after-keys.png` | 按 `y` 之後恢復並收到回覆 |
| `10-terminal-tab.png` | 終端分頁（`recent_unwrapped` + 刷新） |
| `10b-socket-reconnecting.png` / `10c-after-resync.png` | 斷線 → 重連 → `resync` 後訊息數不變（7 → 7） |
| `11-dark.png` | 深色主題 |
| `12-narrow-900.png` | 900 寬版面 |
| `13-stopped.png` | stop 之後回到 offline |

### 真後端（`agents-managerd serve` @ 127.0.0.1:7788）

| 檔案 | 內容 |
|---|---|
| `20-real-daemon-overview.png` | 真 state：`agents-manager` project 下的 `am-claude` / `am-codex`，皆為綠燈（running + idle），WS `open` / herdr `connected` |
| `21-real-daemon-terminal.png` | 真 pane 的 `recent_unwrapped`（可看到 Claude Code v2.1.261 的畫面） |
| `22-real-daemon-chat.png` | 切換 bot 後重新載入該 bot 的對話 |

真後端這一輪**刻意只做讀取**（state / messages / terminal / WS / 切換 bot），沒有
start / stop / prompt，因為當時後端 agent 正在同一個 daemon 上跑 M4 / M5 的驗證，
不想干擾他的 in-flight Turn 與 fixtures。寫入路徑（新增、start/stop、prompt、keys、
abandon）已在 mock 模式完整走過，且請求形狀與 `daemon/src/api.rs` 逐一核對過。

驗證結果：`GET /api/session` → `GET /api/state` → `/ws` 全部正常，載入 20 則訊息，
`terminal_fallback` 訊息正確標示，主控台無任何錯誤或例外。

---

## 7. 已知問題與待辦

1. **真後端的寫入路徑尚未在瀏覽器實跑**（原因見 §6）。等後端 M4/M5 驗證告一段落，
   應再用真 daemon 走一次「新增 → start → prompt → blocked → keys → stop」。
2. **訊息分頁未實作**。目前 `GET /messages?limit=200` 一次抓完，`has_more` 與 `before`
   參數已在型別與 `normalize` 內備妥，但 UI 還沒有「載入更早訊息」按鈕。
   對話很長時會全部塞在 DOM 裡。
3. **未讀計數固定 0**（SPEC §9 第一階段非目標），`unread` 欄位讀進來但不顯示。
4. **`turn_updated` 的 in-flight 判斷依賴事件抵達**。若 WS 在 prompt 與 hook 之間斷線且
   `resync` 沒補上，輸入框可能短暫誤判為可用；再送會被後端 409 擋下並顯示原因，
   不會造成錯配，但體驗上會多一次失敗。
5. **`PATCH /api/bots/:id` 尚未接 UI**（改 args / autostart / 改名）。目前只能刪掉重建。
   後端端點已存在。
6. **`inject_hooks` 沒有出現在新增 Bot 表單**（後端支援，預設 true）。要測終端備援
   目前得手改 `config.toml`。
7. **終端快照是純文字**，ANSI 已由後端去除，但 box-drawing 字元在含中文的行會對不齊
   （等寬字體對 CJK 的寬度處理）。不影響操作。
8. **`terminal_fallback` 的長訊息**會整段終端內容進氣泡；超過 ~900 字或 18 行的氣泡預設
   收合並提供「展開全文」。
9. **oxlint 兩個 warning** 未修：`StatusLamp.tsx` 同時 export 常數（fast-refresh 提示）、
   `TerminalTab.tsx` 的 set-state-in-effect（非同步刷新，誤報）。不影響 build。

---

## 8. 給後端 / M8 的注意事項

- `npm run build` 產物在 `web/dist`，入口 `dist/index.html`，資產路徑是絕對的 `/assets/…`，
  rust-embed 掛在 root fallback 即可（`daemon/src/assets.rs` 已經是這個形狀）。
- 前端只在啟動時打一次 `GET /api/session`；token 存在記憶體，不寫 localStorage。
- 前端**不會**呼叫 `/hook/*`，proxy 設定只是為了本機除錯方便。
- 如果 `GET /api/state` 之後要加欄位，前端會安全忽略未知欄位；但如果**改欄位名**，
  請同步更新 `web/src/api/normalize.ts`（那是唯一需要改的地方）。


## 目錄選擇器（2026-09-05 新增）

新增 Project 表單的「瀏覽…」按鈕會開啟 `DirPicker`（`web/src/components/DirPicker.tsx`），透過 `GET /api/fs/dirs` 逐層瀏覽：上一層、家目錄、麵包屑、手動輸入路徑、單擊進入子目錄、雙擊直接選取、「選擇此目錄」帶回表單並自動填 label。mock 模式有一棵假目錄樹。截圖 `docs/screenshots/40-42`。
