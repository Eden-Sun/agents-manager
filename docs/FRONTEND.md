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
bun test                        # 單元測試（見下：走 bunfig.toml 的 preload）
```

- `bun test`（`scripts/check.sh web`／CI）經 `web/bunfig.toml` 的 preload 把 `node:test` 對到 `bun:test`：bun 1.3.14 內建的 node:test 墊片在一條 async 測試失敗後會讓之後每個檔都報 `test() inside another test()`（#426）。測試用到新的 node:test API 時要補進 `web/test/node-test-shim.ts`。
- 測試不要用固定毫秒等非同步結果（負載高就假紅，#426）：等條件成立——假 fetch 由測試手動放行、逾時由注入的 signal 手動 abort，只用 `setTimeout(r, 0)` 清一輪 macrotask。

- `vite.config.ts` 把 `/api`、`/hook` 轉 http、`/ws` 轉 ws，**`changeOrigin: true` 必要**（daemon 檢查 `Host`）；`server.host: true` 讓手機／LAN 連得到。proxy 把 Host／Origin 改成 daemon 自己的位址，所以不管用 IP、`.ts.net` 還是 `localhost` 連 5173，daemon 的 Host／Origin 檢查（含 `allow_lan` 的名稱白名單，API.md §0）都看到 loopback、不會 403；也因此 5173 自己才是擋 DNS rebinding 的那道：`server.allowedHosts`（`src/lib/devHosts.ts`，只放 IP／localhost／`agm`／`.ts.net`）擋一般 HTTP，proxy 的 WebSocket upgrade 不經過它，`/ws` 的 `bypass` 用同一份名單補上。`.env*`、`/@fs/` 越界、`../` 路徑 vite 預設就擋（fs.strict／fs.deny），ui-token 在 web 根目錄之外取不到。
- 有 AGM 的機器上 5173 由看門狗維護、只跟 `origin/main`（SPEC §18.1）；驗自己未提交的改動用自己的 port。
- UI 改動要看真畫面（ego-browser 或 `scripts/` 裡的截圖腳本），手機至少看 390px。

## 結構

```
web/src/
  api/        index.ts（對外的 API 門面）· types.ts（共用型別）· normalize.ts（共用的寬鬆解碼）· transport.ts（fetch + WS 重連）
              mock.ts（VITE_MOCK 假 daemon）· mentions.ts（@mention 規則，與 daemon group.rs 同一套）
              自成一組的 API 各自帶型別與解碼：preview.ts（`toPreview*`）· supervisor.ts · judge.ts · rebuildRequests.ts · deploy.ts · changelog.ts
  store/      store.ts（單一 Zustand store：server state 鏡像 + UI state + WS 事件）· routeSync.ts（網址 ↔ store）
              unread.ts · shelf*.ts · quotaHide.ts · queuedSend.ts 等（有 .test.ts 的是純函式）
  lib/        純函式：routes · tuiChoices（終端快照 → 選單）· choiceDraft／draftPreload（多分頁問卷草稿）· missionView（任務卡推導）
              agmQuote（轉述來源）· shortModel · updateBatch（與 daemon bulk_restart::plan 同規則）· runtimeDrift …
  hooks/      useTerminalSnapshot · usePaneKeys（鍵名對照＋依序送鍵佇列）· useChoiceMenu · useDialogFocus · useComposerFocus · useMediaQuery …
  components/ 依畫面分：Sidebar／ChatPanel／GroupChatPanel／BotSettingsPanel／QuotaStrip／UnreadChip／Blocked*／Missions*／HostShellPanel／MemPopover …
              元件專屬 CSS 放同目錄（*.css），其餘在 styles.css
```

原則：**共用**的型別放 `types.ts`、共用的解碼放 `normalize.ts`——這一組後端改欄位名時只改 `normalize.ts`。
某支 API 自成一組時（`preview.ts`／`supervisor.ts`／`judge.ts`／`rebuildRequests.ts`）型別與解碼跟著那個模組走，
store 與元件就直接 import 它（例：`store.ts` 的 `toPreviewEvent`、`fetchSupervisor`），改那幾支的欄位名要改的是那個檔。

## 讀程式看不出來的約定

- **螢幕保持亮著**：Screen Wake Lock 只在安全來源（HTTPS／localhost）存在；手機走 `https://<機器>.<tailnet>.ts.net:8443`（tailscale serve）才有。開關存 `am-keep-awake`，預設關，切回前景要重拿鎖。key 與「這個瀏覽器／這個網址能不能用」（`wakeSupport()`：`ok`／`insecure`／`unsupported`）在 `lib/wakeLock.ts`，拿鎖與回前景重拿在 `hooks/useWakeLock.ts`，開關 UI 在 `components/KeepAwakeToggle.tsx`。
- **token**：啟動時打一次 `GET /api/session`，只存記憶體；不讀網址上的 `?token=`，`routeSync` 第一次 `replaceState` 時把它從網址拿掉。
  **GET 撞 401 會自己重拿一次 token 再送一次**（single-flight，2026-09-20 使用者：blocked 面板卡在「讀取終端失敗：missing or bad X-AM-Token」，
  只能重整）；重拿後仍 401 就照實丟。寫入請求（POST/PATCH…）不重送——401 時 daemon 沒跑到處理函式，但重送仍可能變成送兩次。
  WS 連線前 token 是空的就先補一次再連。
- **燈號前端自己算**（`normalize.lampOf`）：`bot_status` 事件只帶 run 不帶 lamp。與 SPEC §2.2 一處刻意不同——run 起跑 90 秒內
  `agent_status=unknown` 畫成 `starting`（claude 要約 20 秒才回第一個狀態）。
- **送出**：前端產生 `client_request_id` 當冪等鍵；使用者氣泡不做本地暫存，一律等 `message_added`（以 message id 去重）。
  `delivery="failed"` 不塞假 turn，文字留在框裡；`pending/ok/unknown` 才先補一筆 turn 讓輸入框立即鎖住。
  輸入框鎖定原因的順序在 `composerState()`；Enter 送出、Shift+Enter 換行、組字中的 Enter 不送。
- **預覽分頁**（issue #253）：`api/preview.ts` 三個端點與型別、`store.previews`（`preview_changed` 寫入）、`components/PreviewPanel.tsx`；取捨見 UI-DECISIONS〈預覽分頁〉。
- **往前翻頁的游標丟了**（#766）：`loadEarlierMessages`／`loadEarlierGroupMessages` 收到 404 `before_message_gone`／`before_message_not_in_conversation`（最舊那則已被刪、或不是這條時間軸的）時，不跳「載入更早的訊息失敗」，而是重載第一頁（`loadMessages`／`loadGroupMessages`，比最新頁新的訊息照留）再翻一次；**只自救一次**（`store/pageCursor.ts` 每條時間軸一把旗標），重載後游標仍 404 才照舊報錯。mock 的 `before` 同樣回這兩種 404。
- **圖片附件上傳前先縮**（`lib/imageCompress.ts`，2026-09-23 使用者）：JPEG／PNG／WebP 長邊縮到 1568px（Claude 視覺的有效解析度；
  token 按像素算，省 token 的是縮尺寸）。JPEG 出 JPEG q0.85；PNG 維持 PNG 只縮尺寸（多半是截圖，壓成 JPEG 字會糊，透明度也留不住）；
  WebP 不透明出 JPEG、有透明出 PNG。EXIF 方向用 `createImageBitmap(imageOrientation:'from-image')` 烤進像素。壓完沒變小、解不開就傳原檔；
  GIF、SVG、HEIC 與非圖片不動。50 MB 上限對壓完的檔案判。附件卡片的大小是實際上傳的，壓過的附「原 N MB」。
  實測手機照片 4032×3024／2.6 MB → 1568×1176／288 KB（桌機 Chrome 約 0.1 秒）。
- **長對話的渲染成本**（2026-10-02 量測，`components/longConvoPerf.test.tsx`、`store/frameFlood.test.ts`）：一顆 bot 的清單最多 500 則（`MESSAGE_CAP`），
  每次 store 更新（含每一幀 WS）都會通知清單上所有掛著的 selector，所以「每顆泡泡一個 selector」是會乘以則數的成本。守則：
  - **泡泡上的 selector 一律 O(1)**。以前「倒回」的 `after` 計數器每顆使用者泡泡各掃兩遍整段清單，120 則就是 10 次更新 72,000 次陣列讀取（500 則約每次 19 萬次）；
    現在只在確認框打開的當下才數（`RewindControl` 的 `afterOf`），不可倒回的泡泡只掛一個布林 selector。新加的泡泡功能不要在 selector 裡 `find`／`filter` 整段訊息。
  - **點擊才用的東西（`notify` 這類）用 `useStore.getState()`**，不為它掛 selector。
  - **Markdown 解析快取**（`lib/markdownCache.tsx`）：換 bot 再換回來，清單整個重掛，每則 assistant 訊息的 parse＋轉 React 樹約 3–5 ms；
    快取 `react-markdown` 的輸出樹（鍵＝原文＋bot id＋元件／URL transform 身分），上限 300 則／100 萬字，單則超過 5 萬字不快取，LRU；手動展開的超長／太深訊息不快取，渲染失敗的樹會淘汰。`Bubble` 的 `memo` 只擋同一次掛載內的重 render。
  - **`turn_progress`（每個 run 每秒約 4 幀）不為 seq 單獨 `set`**：`lastSeq` 沒有人訂閱，只記在模組變數、併進下一次耐久幀／快照；liveReply 本來就 250 ms 節流。
  - 沒做、已知：清單沒有虛擬化（上限 500 則 DOM）；逛過的 bot 的訊息陣列（各最多 500 則）留在 store 裡，只有 bot 被刪才釋放（`prune.ts`）——
    要釋放就得連未讀計數（`recountBot` 讀 `messages`）一起改，這一輪沒碰。
- **來源標籤**：`hook` 不標；`terminal_fallback` 標「可能不完整」；系統訊息另有來源標。
- **WS**：指數退避重連（250ms 起跳、上限 3 秒 + jitter；`transport.ts`），存活偵測（#760）：daemon 每 20 秒送 `{type:"ping"}`，前景 60 秒沒收到任何幀（或切回前景時靜默 >35 秒）就主動關掉重連（`api/socketLiveness.ts`；不能只看 `readyState`）；重連帶 `?since=<lastDurableSeq>`——**最後一則耐久事件的 seq，不是最高的那個 seq**：store 裡 `lastSeq` 跟著每一幀走，`lastDurableSeq` 只跟著會進 daemon 重播環的幀走（`seqAfterFrame`，即時幀如 `bots_restart_progress` 不推進；送成 `lastSeq` 會指到環裡沒有的號碼，重連落回保守分支多一次 resync）。daemon 那邊 `state::is_ephemeral` 加了新的即時幀，這裡的名單要一起改；`resync` 或 `project_changed`／`bot_changed` → 重新 `GET /api/state`。　**只在這個分頁見過 `ping` 之後才用靜默判半開**（舊 daemon 不送心跳，安靜時整條線沒有幀；不加這個條件的話舊 daemon 搭新前端閒置每分鐘重連、整份重抓 state 與對話）；旗標跨重連保留。
  **重連審查（2026-10-02，`api/wsReconnect.test.ts`、`components/reconnectRefetch.test.tsx`）**：
  ① ws 握手被拒（daemon 換 token）瀏覽器只看得到「沒開成」，所以連續 2 次沒開成就重拿 `/api/session`（至少隔 10 秒、single-flight），不再拿舊 token 重試到天荒地老；
  ② 退避只在連線**活過 10 秒**後才歸零（開成功又馬上被關的連線不再讓退避卡在 250 ms、每次都整份重抓）；
  ③ 延遲改成一半固定一半隨機（equal jitter，封頂仍約 3 秒），一群分頁同時重連不再全落在同一個 150 ms 內；
  ④ 重連與 `resync` 後也重抓 pane 清單（原本只靠 `panes_changed` 與 30 秒輪詢）。
  已確認完整的：重連會重抓 state（bot／run／turn／queued_turn／未讀與已讀標記／主機）、額度、草稿、身分停用、已載入與有開啟回合的對話；daemon 重啟後 seq 變小由 `resync` 與 `refreshState` 的 `lastSeq` 降階處理。
  已知沒做：切回前景／`online` 的立即重連沒有抖動（多分頁同時醒來會同刻打回去）。
- **blocked**：`BlockedModal`（全畫面，blocked 1 秒後自動彈出，只彈正在看的 bot，關過就不再彈直到下一次 blocked）與
  `BlockedPanel`（對話上方，全畫面開著時暫停輪詢）共用 `useTerminalSnapshot` 與 `usePaneKeys`。
  全畫面的鍵盤直通把 `KeyboardEvent` 翻成 herdr 鍵名（⌘ 系列留給瀏覽器，Home/End/PgUp/PgDn herdr 不收）；直通時 Esc 也送給 agent。
  送鍵走佇列合批（`agent.send_keys` 吃陣列），不然快打會亂序。
  送鍵／送字都帶 `expect_run_id`（bot 重啟過就不要把鍵打進新的 agent）；快取的 run id 過期時 daemon 回 409
  `run mismatch` **並附上現在的 run id**，`store.sendWithFreshRun` 就拿它**自動重試一次**再順手 `refreshState`——
  以前只跳「送出按鍵失敗：run mismatch」要使用者自己再按一次（2026-09-19 w168:p7J）。其他 409（框裡有字、
  回合在飛）不重試，原樣回報。
- **全域鍵盤**：⌥↑／⌥↓ 換 bot（bot 列內與對話框開著時不接）；**Control+1…9 跳到側欄第 n 個專案的群組對話、把側欄捲到那一列並 focus 輸入框**（2026-09-16 使用者；「第 n 個」＝側欄**畫出來**的第 n 個，搜尋時沒命中的專案整塊不畫，不算在內）——認 `event.code` 的 `Digit1…9`，所以中文輸入法照樣有效；用 Control 而非 ⌘（⌘1…9 是瀏覽器換分頁）；正在組字、對話框開著、事件已被處理就不接；第 n 個專案不存在就什麼都不做。
- **群組任務入口**：`/api/missions` 不存在時整個入口靜默不出現，不重試不報錯。
- **Mock**：`api/mock.ts` 的回應形狀刻意與 `daemon/src/api.rs` 一致。訊息含 `blocked`／`rm -rf` → 進 blocked；`tinyask`（或 `__amMock.tinyAsk()`）→ 題目被裁掉的 AskUserQuestion（畫面只剩選項、題目只在 `pending-question`）；`twoask`（或 `__amMock.twoAsk()`）→ 兩題（第二題可複選）、分頁列被裁掉，能一題一題答完送出（`api/mockTwoAsk.ts`，`scripts/verify-blocked-mobile.mjs` 用）；`__amMock.bulletAsk()` → 問句跟選項之間夾著 `·` 條列說明的 claude 提示（2.1.280 fullscreen renderer 邀請）；`fallback` → terminal_fallback 回覆；`authfail` → 回合收在「失敗收尾（帳號或授權）」（對話長出「立即登入」，搭 `__amMock.loggedOut(bot, 'cc1')` 把 bot 綁到沒登入的身份）；
  `slow` → 延遲 8 秒；`outbox` → 回合裡放一個 `download-test-N.txt` 進「bot 給你的檔案」（回合結束就該自己出現）。console 有 `__amMock.dropSocket() / resync() / block(name) / disconnect() / reconnect()`，另有 `failNext(method, 路徑正規式, status, body)`——下一個符合的請求回那個錯（手動看錯誤路徑、截圖用，用過一次就拿掉）。更新框（`api/mockReleaseTriage.ts`，issue #561）：`__amMock.updateNotice(bot, 通知字)` 給 bot 掛更新通知（沒在跑就先啟動），`__amMock.triageOff('codex')` 清掉那個 kind 的分診帳本（演「尚未分析」）。herdr 一鍵更新（`api/mockHerdrUpdate.ts`）：`__amMock.herdrUpdate()` 推一筆 `kind:"herdr"` 的上游快照（local／m4p 都落後）並把本機 bot 開起來，header 出現 herdr 徽章；`{failOne: true}`／`{reason: 'busy_timeout'}`／`{reason: 'restart_failed'}`／`{hold: 'waiting_idle'}` 演沒接回／整次沒做／換回舊版／停在某一步；形狀照 daemon `herdr_upgrade.rs`，分診帳本跟 daemon 一樣對 herdr 回空的 rows。`__amMock.remoteDemo()` 加一台遠端主機 `m4p`＋專案＋在跑的 bot、五格額度都有（cc0／cc1／cc2／codex／grok），回 bot id——重現遠端標題列最擠的情況（名牌＋遠端記憶體）。`__amMock.patchBot(bot, body)` 直接 PATCH 設定；`__amMock.runtime(bot, {runtime_model, runtime_fast, agent_status})` 改 active run 讀回的值（演 codex 0.157 狀態列的顯示名 `GPT-6-Luna`、TUI 裡手切的 fast、在忙；#712）。
  被 trace 的 pane（§6.5e）跟 daemon 一樣分兩份白名單：看過畫面不會變成「自己開的 shell」，有 port 的每次打字都 403；第一個專案底下有 dev server（唯讀）、跑 vim 的 shell（沒 port，打得進去）、手開的 shell，另有對不到專案的 scratch 與「多出來的」各一顆。
- 深淺色：側欄左上角一顆主題鈕三段輪流切換（跟隨系統 → 淺 → 深，存這台瀏覽器；`components/ThemeToggle.tsx`、`lib/theme.ts`）；`prefers-reduced-motion` 關掉所有動畫。
