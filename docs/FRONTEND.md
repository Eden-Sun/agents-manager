# 前端（`web/`）

Vite + React + TypeScript + Zustand + 自寫 CSS。API 契約以 [`API.md`](./API.md) 為準，UI 取捨見 [`UI-DECISIONS.md`](./UI-DECISIONS.md)。
前端只做 daemon 狀態的投影；程式碼是實作細節的權威，這份只記「讀程式看不出來」的約定。

## 執行與驗證

```bash
cd web
bun install
bunx vite                       # 5173，proxy 到真 daemon（127.0.0.1:7788）
VITE_MOCK=1 bunx vite           # 記憶體假 daemon，不需要 daemon 與 herdr
VITE_DAEMON=http://127.0.0.1:9999 bunx vite   # 指到別的 daemon

bunx tsc --noEmit -p tsconfig.app.json   # 真的檢查型別（只寫 --noEmit 什麼都不檢查）
bunx oxlint src
bun run build                   # tsc -b && vite build → web/dist，release daemon 用 rust-embed 內嵌
node --test --experimental-strip-types src/**/*.test.ts   # 純函式單元測試
```

- `vite.config.ts` 把 `/api`、`/hook` 轉 http、`/ws` 轉 ws，**`changeOrigin: true` 必要**（daemon 檢查 `Host`）；`server.host: true` 讓手機／LAN 連得到。
- 有 AGM 的機器上 5173 由看門狗維護、只跟 `origin/main`（SPEC §18.1）；驗自己未提交的改動用自己的 port。
- UI 改動要看真畫面（ego-browser 或 `scripts/` 裡的截圖腳本），手機至少看 390px。

## 結構

```
web/src/
  api/        types.ts（型別）· normalize.ts（寬鬆解碼，JSON 形狀的知識只在這裡）· transport.ts（fetch + WS 重連）
              mock.ts（VITE_MOCK 假 daemon）· mentions.ts（@mention 規則，與 daemon group.rs 同一套）· supervisor.ts · changelog.ts
  store/      store.ts（單一 Zustand store：server state 鏡像 + UI state + WS 事件）· routeSync.ts（網址 ↔ store）
              unread.ts · shelf*.ts · quotaHide.ts · queuedSend.ts 等（有 .test.ts 的是純函式）
  lib/        純函式：routes · tuiChoices（終端快照 → 選單）· choiceDraft／draftPreload（多分頁問卷草稿）· missionView（任務卡推導）
              agmQuote（轉述來源）· shortModel · updateBatch（與 daemon bulk_restart::plan 同規則）· runtimeDrift …
  hooks/      useTerminalSnapshot · usePaneKeys（鍵名對照＋依序送鍵佇列）· useChoiceMenu · useDialogFocus · useComposerFocus · useMediaQuery …
  components/ 依畫面分：Sidebar／ChatPanel／GroupChatPanel／BotSettingsPanel／QuotaStrip／UnreadChip／Blocked*／Missions*／HostShellPanel／MemPopover …
              元件專屬 CSS 放同目錄（*.css），其餘在 styles.css
```

原則：元件與 store 只看 `types.ts`；後端改欄位名時只改 `normalize.ts`。

## 讀程式看不出來的約定

- **token**：啟動時打一次 `GET /api/session`，只存記憶體；不讀網址上的 `?token=`，`routeSync` 第一次 `replaceState` 時把它從網址拿掉。
- **燈號前端自己算**（`normalize.lampOf`）：`bot_status` 事件只帶 run 不帶 lamp。與 SPEC §2.2 一處刻意不同——run 起跑 90 秒內
  `agent_status=unknown` 畫成 `starting`（claude 要約 20 秒才回第一個狀態）。
- **送出**：前端產生 `client_request_id` 當冪等鍵；使用者氣泡不做本地暫存，一律等 `message_added`（以 message id 去重）。
  `delivery="failed"` 不塞假 turn，文字留在框裡；`pending/ok/unknown` 才先補一筆 turn 讓輸入框立即鎖住。
  輸入框鎖定原因的順序在 `composerState()`；Enter 送出、Shift+Enter 換行、組字中的 Enter 不送。
- **來源標籤**：`hook` 不標；`terminal_fallback` 標「可能不完整」；系統訊息另有來源標。
- **WS**：指數退避重連（上限 10 秒 + jitter），重連帶 `?since=<最高 seq>`；`resync` 或 `project_changed`／`bot_changed` → 重新 `GET /api/state`。
- **blocked**：`BlockedModal`（全畫面，blocked 1 秒後自動彈出，只彈正在看的 bot，關過就不再彈直到下一次 blocked）與
  `BlockedPanel`（對話上方，全畫面開著時暫停輪詢）共用 `useTerminalSnapshot` 與 `usePaneKeys`。
  全畫面的鍵盤直通把 `KeyboardEvent` 翻成 herdr 鍵名（⌘ 系列留給瀏覽器，Home/End/PgUp/PgDn herdr 不收）；直通時 Esc 也送給 agent。
  送鍵走佇列合批（`agent.send_keys` 吃陣列），不然快打會亂序。
- **群組任務入口**：`/api/missions` 不存在時整個入口靜默不出現，不重試不報錯。
- **Mock**：`api/mock.ts` 的回應形狀刻意與 `daemon/src/api.rs` 一致。訊息含 `blocked`／`rm -rf` → 進 blocked；`fallback` → terminal_fallback 回覆；
  `slow` → 延遲 8 秒。console 有 `__amMock.dropSocket() / resync() / block(name) / disconnect() / reconnect()`。
- 深淺色跟隨系統，不提供手動切換；`prefers-reduced-motion` 關掉所有動畫。
