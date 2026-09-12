# agents-manager web/src code review（origin/main 3c6bf8a，2026-09-12）

範圍：`web/src` 全部（store／api／hooks／lib／components／styles.css／mock）。主線由我逐檔讀 store、api、hooks、lib、App；components 分三組（chat／blocked／附件、側欄／team、設定／額度／主機／CSS／mock 契約）由子審查者逐檔讀全文，結論經我抽查關鍵行後合併。輔助工具：`bunx tsc -p tsconfig.app.json --noEmit`（0 錯）、`bunx oxlint src`（34 warning，皆既有類型）、`node --test --experimental-strip-types` 逐檔跑 19 個測試檔。

## 總評

整體架構清楚：所有 JSON 形狀集中在 `api/normalize.ts`、store 單一來源、WS 事件有 seq／resync／single-flight 保護，這一層的設計是紮實的。真正的問題集中在三處：（1）**「排隊／中止並取代」的送出路徑在 409／502 時會把使用者的文字連附件一起丟掉**（store 的 `flushQueued` 與 ChatPanel 兩顆按鈕都是先清佇列再送）；（2）**設定面板與 ModelPicker 會用開啟當下的舊值覆寫剛在別處套用的 model／effort**，且 ModelPicker 的自動修正會讓表單無故 dirty；（3）**幾個「同一動作兩個入口」的地方行為不一致**（側欄清理 Team 不經確認、面板「加碼預算」不 resume、`openSettings` 不清 team／shell 視圖）。此外 web 的單元測試從未進 `scripts/check.sh` 與 CI，目前 19 個測試檔裡 2 個跑不起來、1 個（`updateBatch`）已因 09-12 的行為變更而失敗卻沒人發現。安全面沒有發現 token 外洩、XSS（react-markdown v10 預設安全）或 shell 注入。

## 確定（依嚴重度）

### 1. 資料遺失／錯誤寫入／不可逆動作沒有防呆

- **[確定] components/TeamNodes.tsx:208-222** — 側欄 team 節點的「清理」（✕）直接 `controlTeam(id,'cleanup')`，沒有確認框；主面板同一動作（TeamPanel.tsx:1209）要先過 `ConfirmDialog`。觸發：終態 team 的側欄節點 hover 浮出 ✕，誤點一下 → 成員 bot 軟刪、worktree 移除（不可逆）。與 UI-DECISIONS P0「降低刪除誤操作」及檔內註解「清理與刪除都是不可逆的」不一致。修法：側欄那顆改開 cleanup `ConfirmDialog`（抽成共用元件，同 `TeamDeleteDialog`）。
- **[確定] store/store.ts:2571-2581** — `flushQueued` 先把排隊訊息從 `queuedSends` 移除再 `sendPrompt`；`sendPrompt` 回 false（409 `picker_open`／`dialog_open`／`needs_login`、502、網路錯）時只 toast，排隊的文字與附件 id 永久遺失。觸發：回合結束時 codex 的 `/model` 選單還開著 → daemon 回 409 `picker_open` → 排隊的那則消失。FRONTEND.md 說「避免吃 409 把訊息弄丟」，但只擋了 `composerState` 看得見的兩種情況。修法：`sendPrompt` 回 false 時把 `pending` 放回 `queuedSends[botId]`（或寫回 `drafts[bot:<id>]`）。
- **[確定] components/ChatPanel.tsx:784-797、803-813** — 「中止並取代」「併行送入」在有排隊訊息時先 `cancelQueuedSend`，失敗（`sendPrompt` 409／`typeAlongside` false）後沒有把文字放回輸入框或佇列。觸發：回合進行中已排隊一則 → 按「中止並取代」→ `abortBot` 成功但 `sendPrompt` 回 409 → toast 報錯、`queuedSends` 已清、draft 是空的 → 訊息（含附件 id）消失。修法：失敗分支 `setText(body)`（或重新 `queueSend`），且 `abortBot` 未真的中止時不要送。
- **[確定] components/BotSettingsPanel.tsx:312-324、352-367** — 表單 state 只在 `botId` 變時重置，面板開著時不跟著 store 的 bot 更新；`base` 讀最新 `bot`、表單仍是開啟當下的舊值。觸發：設定面板開著（桌機非模態）→ 在標題列 `ModelQuickPicker` 把 effort 改成 `high`（或另一分頁／TeamRoleEditor 改了同一顆）→ `refreshState` 後 `bot.effort='high'`，表單 `effort` 仍 `null` → 顯示「已變更：effort」，按「儲存」把 `effort: null` 送回去蓋掉剛套用的值；按關閉則問「放棄未儲存的變更？」。修法：每個欄位記「使用者有沒有動過」，沒動過的欄位在 `bot` 對應值改變時同步。
- **[確定] components/ModelPicker.tsx:120-132（`ApiModelFields`）** — 兩個 effect 在 API 清單載入後主動 `onFast(false)`／`onEffort(null)` 改父層表單，使用者沒動任何東西表單就 dirty。觸發：codex bot 存著 `effort:'max'`、清單不含 max → 開設定面板 → `effort` 被改成 null → 「已變更：effort」→ 關閉被問「放棄未儲存的變更？」，儲存則悄悄送 `effort: null`。同一元件也用在 `TeamLaunchPanel`／`TeamRoleEditor`。修法：effect 只標「這個值不受支援」並顯示警告，或把自動修正併進 `saved` 基準不算 dirty。
- **[確定] components/ChatPanel.tsx:1449、1477-1483 ＋ components/BlockedDraft.tsx:78-104** — 多分頁問卷時 `BlockedPanel`（`paused` 只停輪詢，`BlockedDraft` 照掛，BlockedPanel.tsx:78）與 1 秒後自動彈出的 `BlockedModal` **同時掛載**，兩份 `preload` 同時對同一個 pane 送 ←／→／tab 導覽鍵互相插隊。觸發：bot 進 blocked 且畫面是 ≥2 分頁的 AskUserQuestion → 面板 t0 開始 preload → 1 秒後全畫面再跑一份 → `moveTab` 驗畫面對不上就整份退回即時模式，或把終端留在別的分頁；每次自動彈窗都重演。修法：`paused` 時不渲染 `BlockedDraft`，或把預載提到 botId 層級的單例（module-level `Map<botId, Promise<Draft>>`）讓兩個視圖共用。
- **[確定] components/BlockedModal.tsx:48-74（配 useDialogFocus:46）** — 鍵盤直通在選單模式仍預設開啟且開關被 `showRaw` 藏起來；window capture 的 `onKey` 只放過 input／textarea／select，焦點在 ✕ 或 `.bc-item` 按 Enter／Space 會被攔下送 `enter`／`space` 進 pane，而不是啟動有焦點的按鈕。觸發：全畫面自動彈出、焦點在 ✕，按 Enter → TUI 游標所在那一項被選走。鍵盤使用者在選單模式無法使用可點清單。修法：target 是 `button`／`a`（或 `menu !== null`）時 return 交給按鈕；或選單模式下 `passthrough` 預設關。
- **[確定] components/TeamPanel.tsx:1122-1139** — 「加碼預算」`title` 寫「各加一倍，然後繼續」，`onClick` 只 `patchTeam` 不 `resume`；側欄同一動作（TeamNodes.tsx:124-137）patch 後會 `controlTeam('resume')`。觸發：`budget_time` 暫停 → 面板按「加碼預算」→ 預算翻倍但 team 仍 paused。修法：`.then((ok) => ok && controlTeam(teamId,'resume'))`。

### 2. 狀態不一致／導覽錯誤

- **[確定] store/store.ts:974-983 ＋ components/ChatPanel.tsx:1500** — `openSettings` 只清 `selectedProjectId`，不清 `selectedTeamId`／`teamLaunch`／`shellView`，而 `BotSettingsPanel` 只在 `ChatPanel` 內渲染。觸發：主面板是 Team／組隊／主機 shell 視圖時按側欄 bot 列齒輪（BotRowMenu.tsx:36）→ 主面板不變、設定面板不出現、網址仍 `/teams/:id`，只有 `selectedBotId` 暗中換掉。深連結 `/bots/:id/settings` 在 localStorage 還原的 `teamId` 非 null 時同樣走 `openSettings`（routeSync.ts:124-126）→ 畫面留在 team、網址被 `syncNow` 改回 `/teams/:id`。修法：`openSettings` 補 `selectedTeamId: null, teamLaunch: null, shellView: null`（與 `selectBot` 一致）。
- **[確定] store/store.ts:754-770 ＋ store/routeSync.ts:116-131、196-216** — `refreshState` 是 `singleFlight(run, onError)`，錯誤被吞、永不 throw，`bootstrap` 在第一次 `GET /api/state` 失敗（401／502／daemon 重啟中）時照樣 `ready: true`；routeSync 一見 `ready` 就套用路徑，清單是空的 → `backHome('這個 Bot 已經不在了')` 把深連結 `replaceState` 成 `/`。觸發：daemon 重啟中打開 `/bots/<id>` → toast「這個 Bot 已經不在了」、網址改成首頁、畫面是「還沒有專案」onboarding；幾秒後同步成功也回不到那顆 bot。修法：`bootstrap` 對第一次 refresh 失敗設 `bootError`（boot 卡重試），或 routeSync 只在 `stateStale === false` 且至少套用過一份 state 後才判定「不在了」。
- **[確定] store/store.ts:987-1022、2732-2743** — 樂觀順序 `botOrder`／`projectOrder` 從不清除（寫入點只有 moveBot／moveProject／cloneBot；`refreshState` 只重對應佔位 id），而 `botsOfProject`／`orderedProjects` 永遠先用本地順序。觸發：桌機拖過一次專案 A → 手機再拖（daemon 推 `project_changed`、`GET /api/state` 回新順序）→ 桌機仍顯示自己那份直到重整；`POST /api/order` 失敗時 toast 說「重新整理會回到原本的順序」，不重整就一直錯。修法：`refreshState` 成功套用後清掉 `botOrder[pid]`／`projectOrder`（只保留 `saveOrder` 尚未回應的那一次）。
- **[確定] components/Sidebar.tsx:880（`lampState`）** — `useMemo(() => ({ ...useStore.getState(), runs }), [runs])` 在 render 期整包快照 store，只在 `runs` 變時更新；`kidsLampOf`／`kidsWaitOf` 用它算 `botLamp`，而 `botLamp` 讀 `hosts[].connected`／`connected`／`defaultConnected`／`projects`。觸發：父列收合、子 agent working；herdr 或遠端 host 斷線（`daemon_status`／`host_changed` 只改 `connected`／`hosts`）→ 子列自己的燈變灰，父列收合處仍藍點、「在等子 agent」，直到下一個 `bot_status`。修法：把 `hosts`／`connected`／`defaultConnected`／`projects` 一併訂閱進依賴，或改成回字串的 `useShallow` selector。
- **[確定] store/store.ts:2284-2292** — `bot_status` working→idle 的未讀補記：去重 key 取 `turns[botId]` 中字典序最大的 turn id，而那筆幾乎一定已被 `message_added`／`turn_updated` 記過（`takeTurnCompletion` 回 false）；map 全空才用 `run:<id>`，且同一 run 只記一次。觸發：bot 曾從網頁送過一次訊息 → 之後使用者直接在終端裡跟 agent 講話 → working→idle → 未讀不亮（程式註解宣稱的目的正是這個情境）。修法：這條路改用獨立 key（`run:<id>:<idle 邊緣計數>`），或只在 latest turn 仍 in_flight 時共用 turn id。
- **[確定] store/store.ts:2824-2830 ＋ 1211-1232** — `unknownDeliveryTurn` 只看 `delivery === 'unknown' && status !== 'failed'`，不要求 `status === 'in_flight'`（API.md §5：只有 in-flight 的 unknown 才擋下一則），而 `sendPrompt` 對既有 turn 無條件用 `delivery: res.delivery` 覆寫；`pruneTurns`（lists.ts:88）也永久保留這種筆。程式上的不一致確定；實際觸發需要「可能」第 1 條的時序（RPC 逾時回 `unknown`，Stop hook 已先把 turn 推成 completed/ok，hookrecv.rs:559）→ 本地變成 completed+unknown → composer 鎖「送達狀態未知」，「放棄該回合」打 abandon 拿 409 → 只能重整。修法：`unknownDeliveryTurn` 加 `t.status === 'in_flight'`；`sendPrompt` 若既有 turn 已非 in_flight 就不覆寫 delivery。
- **[確定] components/TeamLaunchPanel.tsx:258-263** — 預檢 `used >= budget.quota_stop_pct` 在 `quota_stop_pct = 100` 仍擋；daemon（team.rs:178-182、SPEC-team §4.5）明訂 100 = 關掉額度檢查。觸發：某 kind 5h 已用 100%（`toQuotaWindow` 夾到 100）、停手線 100 → 「建立並啟動」disabled。修法：`quota_stop_pct >= 100` 時跳過。
- **[確定] components/TeamPanel.tsx:948-966（`BudgetMeter`）** — 量表用 `relays / max_relays`，但無限併行時 daemon 上限是 `max_relays × 已開工 issue 數`（SPEC-team §4.5）。觸發：`count = 0`、3 個 issue 同時跑、relays 60/40 → 量表 100% `crit`，實際還有 60 次。修法：`workingIssuesOf(team) > 1` 時分母乘上 `workingIssues`。
- **[確定] components/TeamLaunchPanel.tsx:264-274** — `fetchIssues` 失敗一律 `setCandidates([])`，畫面寫「沒有其他開啟中的 issue」。觸發：`gh` 未登入回 502 → 使用者以為 repo 沒有 open issue。修法：另存 `err` 顯示「讀取失敗：…」（TeamPanel.tsx:304-310 的 `OpenIssuePicker` 已這樣做）。
- **[確定] components/IdentitiesPanel.tsx:238-243 ↔ components/BotSettingsPanel.tsx:146-147、367** — 新增身份表單允許 kind `codex`／`grok`（API 也允許），但 `IdentityOptions` 對 `kind !== 'claude'` 直接回 `null`，PATCH 也只在 claude 才送 `identity`；`QuotaStrip.collectEntries` 同樣只拆 claude 身份。觸發：建立 kind=codex 的身份 → 新增 Bot／設定／組隊／角色編輯都看不到它，永遠指派不了；IdentitiesPanel.tsx:206 預填的 `CLAUDE_CONFIG_DIR=$HOME/.claude-` 切到 codex／grok 仍會送出。修法：表單只開放 claude，或 `IdentityOptions` 依 kind 過濾並對三種 kind 渲染、PATCH 比對不限 claude。
- **[確定] components/GroupChatPanel.tsx:199-204** — `mentionAtCaret` 的 query 只吃 `[A-Za-z0-9_-]*`，但 v3.8 起暱稱允許 CJK，`parseMentions` 也吃 CJK。觸發：bot 叫「小幫手」，輸入 `@小` → 自動完成不彈（`@` 剛打完那一刻有，接著打中文就消失）。修法：query 改用與 `MENTION_RE` 同一個字元集。
- **[確定] api/mock.ts:1525（`emitBotStatus`）** — 每顆 bot 的 `bot_status` 一律送 `connected: this.connected`（本機 herdr）且不帶 `host`；API.md §8 說 `connected` 是該 bot 所屬 host 的狀態，store 的 `botStatusConn.ts` 對遠端 bot 會寫進 `hosts[<name>].connected`。觸發：`__amMock.hostDown('m4p')` → 對該主機每顆 bot `emitBotStatus` → `connected: true` → store 把 m4p 標回已連線、error 清 null，斷線示範被自己蓋掉。修法：帶 `host: <project.host>`，`connected` 取該主機的值。

### 3. 資源／效能

- **[確定] components/Attachments.tsx:358-370** — `urlCache` 的 object URL 永遠不 revoke、Map 無上限；每個看過的附件把整個 Blob（最大 12 MB）釘在記憶體到頁面關閉。觸發：一天內在對話／群組時間軸滾過幾十張截圖 → 記憶體線性成長，換 bot 也不回收。修法：改 LRU（例如上限 64 筆），淘汰時 `URL.revokeObjectURL`；或 `MessageAttachments` unmount 時引用計數歸零就 revoke。
- **[確定] components/IssuesBar.tsx:49-50、79-102** — `github` selector 回的是 `projects[]` 物件欄位，`normalize.toProject` 每次 `GET /api/state` 重建新物件 → 引用每次都變 → 兩個 effect 重跑：重抓 submodules 與 `fetchIssues(limit 100)`（只為算 open 數）。觸發：任何 `bot_changed`／`project_changed`／`resync`（別人啟停 bot）→ 每次打 `/submodules` 與 `/issues?limit=100`，daemon 2 分鐘快取一冷就是一次 `gh`。修法：deps 改用 `github?.url` 字串。

### 4. 可及性與 UI-DECISIONS 不符

- **[確定] components/ChatPanel.tsx:1395-1425** — 對話／終端 `role="tablist"` 沒接 `onTabListKeyDown`（`tabKeys.ts` 整個 repo 只有 IssuesBar 用）、沒有 `aria-controls`、內容沒有 `role="tabpanel"`；與 UI-DECISIONS〈無障礙語意（2026-09-11，#11）〉寫的「分頁加 ←/→/Home/End … 包一層 `.tab-panel` 掛 `role="tabpanel"`」不符。觸發：焦點在「對話」分頁按 → 沒反應。修法：`onKeyDown={onTabListKeyDown}`、tab 加 `id`／`aria-controls`，內容外包 `role="tabpanel"`；或改文件。
- **[確定] components/UnreadChip.tsx:291-300、lib/kidsScroll.ts:32（由 Sidebar.tsx:1316 掛上）** — 在 React `onWheel` 內 `e.preventDefault()`；React 17+ 把 wheel 註冊成 passive listener，preventDefault 無效並噴 "Unable to preventDefault inside passive event listener"。觸發：≤720px 滾輪滾在未讀列上 → 列橫捲**同時**整頁也垂直捲，「到底把滾動還給整頁」的分流失效；子 agent 列 shift+滾輪同理。修法：`useEffect` 內原生 `addEventListener('wheel', h, { passive: false })`。
- **[確定] components/GroupChatPanel.tsx:111-121、components/TeamPanel.tsx:172-186、227** — 成員圖示是 `<button role="listitem">`，role 覆蓋 button 語意，AT 唸成清單項目；TeamPanel 多 issue 分段時外層 `<span role="listitem">` 再包 listitem 按鈕（listitem 巢 listitem）。修法：`role="listitem"` 放外層包裝、button 維持原生角色；分段層改 `role="group"` + `aria-label`。
- **[確定] components/HeadMoreMenu.tsx:49-66** — `role="menu"`／`menuitem` 但沒有鍵盤行為：開啟時焦點不進選單、↑↓/Home/End 不動、Tab 直接走出去；`hooks/useMenuKeys.ts` 已存在（Tools／ModelPicker 在用）卻沒接。修法：`useMenuKeys(open, pop, btnRef, close)` 掛 `.head-menu-pop`，項目 `tabIndex={-1}`。
- **[確定] components/Sidebar.tsx:1101-1107** — 搜尋框 `onKeyDown` 對所有按鍵 `stopPropagation()`（原生也停），`App.tsx` 掛 window 的 `useBotSwitchKeys` 收不到。觸發：焦點在搜尋框按 ⌥↑/⌥↓ 不換 bot，與 FRONTEND.md「⌥↑/⌥↓ 任何地方（輸入框裡也算）」不符。修法：只在 Escape 時 stopPropagation。
- **[確定] components/TeamNodes.tsx:181** — team 節點按鈕用 `aria-pressed={selected}`；UI-DECISIONS #11 已把「開著的是這個」統一為 `aria-current`（bot 列、專案標題鍵都換了）。修法：`aria-current={selected ? 'true' : undefined}`。

### 5. 維護性／文件／測試

- **[確定] api/types.ts:563-573** — `WsEventType` 無任何引用（死碼），且缺 `mem_updated`／`team_changed`／`team_task_updated`／`team_event`／`identities_changed`／`bots_restart_progress`／`bots_restart_done`，與 `handleFrame` 實際處理的事件不同步。修法：刪掉，或讓 `handleFrame` 的 switch 用它做窮舉檢查。
- **[確定] docs 與程式不同步**：
  - FRONTEND.md〈路由〉與 UI-DECISIONS〈`?token=` 不進分享出去的連結〉都說 `?token=` 只在第一次載入用、由 `transport.ts` 快取——`transport.ts` 完全沒讀 URL 的 `token`（`session()` 只打 `GET /api/session`），這段行為不存在。
  - API.md §2／§6 沒有 `bots[].queued_turn` 與 turn status `queued`（daemon api.rs:355、web normalize.ts:132、store.ts:2861 都在處理它）。
  - FRONTEND.md §3 說 lamp 規則兩邊相同，但 `normalize.lampOf`（normalize.ts:652-663）對 running+unknown 在起跑 90 秒內回 `starting`，與 API.md 的 lamp 表不同（刻意的，但沒寫進文件）。
  - UI-DECISIONS（2026-09-11 額度列）寫「手機那份身分名本來就讓位」，但 styles.css ≤640 區塊約 10905 行 `.main-head .quota-hp .quota-identity { display: inline }` 蓋過更早的 `display: none`，手機現在會畫身分名。
- **[確定] 測試基礎：`scripts/check.sh` 與 `.github/workflows/ci.yml` 完全不跑 web 的 `node --test`**。實跑 19 個測試檔：16 個全綠，**`components/teamProgress.test.ts`**（teamProgress.ts:3-4 import `'../api/types'` 無副檔名 → `ERR_MODULE_NOT_FOUND`）與 **`components/termLinks.test.ts`**（import `./TermLinks` 是 `.tsx`）跑不起來；**`lib/updateBatch.test.ts` 第 5 案例失敗**——`updateBatch.ts` 在 09-12（587b07f）改成子 agent 也進批次，測試（09-09）仍斷言 `child` 被排除，紅了三天沒人看到。修法：`check_web` 加 `for f in src/**/*.test.ts; do node --test --experimental-strip-types "$f"; done`（或 `bun test`）；兩個 import 補 `.ts`／把 `termPieces` 抽成 `.ts`；更新 updateBatch 測試。

## 可能

### store／api／hooks／lib

- **[可能] store/store.ts:1194-1232（sendPrompt）vs handleFrame:2351-2370** — 「確定」unknownDeliveryTurn 那條的觸發時序：prompt RPC 逾時（10s）期間 agent 已答完且 Stop hook 先到。程式不一致確定，現場能否碰到取決於 herdr 逾時原因。
- **[可能] hooks/usePaneKeys.ts:61-94** — `pending`／`sending` 兩個 ref 跨 `botId` 共用：換 bot 時 cleanup 只清 `pending`，正在跑的 `while` 迴圈仍用舊閉包的 `botId`；此時按下的鍵會被舊迴圈以舊 botId／舊 `expect_run_id` 送到上一顆 bot 的 pane。觸發：全畫面 blocked 視窗鍵盤直通中，遠端主機一次 `POST /keys` 還在路上時 ⌥↓ 換 bot 並繼續打字。修法：`pending` 存 `{botId, key}`、迴圈每輪讀最新 botId；或換 bot 時中止舊迴圈。
- **[可能] api/normalize.ts:426** — `toTurn` 對認不得的 `status` 退回 `'in_flight'`：daemon 未來新增終態（API.md 第 55 行的 assignment 已有 `cancelled`）時前端會當進行中 → composer 進 queued 模式、`liveReplyOf` 也認它。修法：fallback 改 `'failed'`。
- **[可能] store/store.ts:2184-2203、1027-1053** — `resync` 對每個 `loadedBots` 依序 `await loadMessages`（每個 200 則），而 `loadedBots`／`messages` 永不縮減（每顆看過的 bot 保留 ≤500 則）。觸發：一天看過 30 顆 bot、daemon 重啟一次 → 30 個序列 GET；記憶體隨看過的 bot 數線性成長（`MESSAGE_CAP` 只管單一 bot）。修法：resync 只重載目前看得到的 bot／群組，其餘 `loadedBots[id] = false`。
- **[可能] store/store.ts:657-663（keptAfterPage）** — 頁是空的時候 `cutoff = startedAt`（瀏覽器時鐘）跟 `created_at`（daemon 時鐘）比：手機時鐘快幾秒時，請求飛行期間經 WS 到的訊息被判成「比頁舊」而丟掉。觸發：新對話（空頁）＋手機時鐘偏快 → 第一則回覆不見，要重整。修法：頁空時不用時間門檻（保留所有不在頁裡的）。
- **[可能] store/store.ts:803-806** — 沒有選取時 `refreshState` 自動選 `bots[0]`（routeSync.ts:95-96 註解承認）：`removeBot` 特地清成 null 的選取，會在下一次任何 `bot_changed`／`project_changed` 又選回第一顆，網址跟著跳 `/bots/:id`。修法：只在 `selectedBotId` 指向已消失的 bot 時才退回。
- **[可能] api/transport.ts:138-146** — `onmessage` 的 try/catch 把 `handlers.onFrame` 的例外一起吞掉（註解「ignore malformed frame」），`handleFrame` 任何 case 丟例外都無聲消失，而 `lastSeq` 已在開頭推進（store.ts:2179-2181），重連時不會補。修法：只包 `JSON.parse`，`onFrame` 例外至少 `console.error`；或 `lastSeq` 在 case 處理完才推進。
- **[可能] store/store.ts:2402-2405** — `turn_progress` 節流的 trailing `apply` 在 250ms 後執行，回合若已結束（`message_added` 清掉 `liveReply`）會把舊 partial 寫回；`liveReplyOf` 擋住顯示，但 state 殘留到下一回合。修法：trailing apply 前檢查 `inFlightTurn(...)?.id === turnId`。
- **[可能] api/supervisor.ts:90、171** — `(o.assignments as unknown[]) ?? []` 對非陣列值直接 `.map` 會 throw，`fetchSupervisor` 只攔 404。修法：`Array.isArray(...) ? … : []`。
- **[可能] store/store.ts:1464-1470** — `removeBot` 用 `s0.bots` 原始順序找「下一顆」，不是側欄顯示順序；拖過順序後刪除，選取跳到看起來不相鄰的那顆。修法：用 `botsOfProject(s0, bot.project_id)`。
- **[可能] store/store.ts:1355-1417** — `cloneBot` 的 finally 把 `busy[key]` 設 `false` 而非刪除（其他路徑都 delete），`busy` map 每 clone 一次多一個永久 key。

### chat／blocked／附件／終端

- **[可能] components/ChatPanel.tsx:910-915** — `textarea disabled={sending}` 讓送出期間 textarea 失焦，送完不自動拿回（`useComposerFocus` 只在 draftKey 變或 `forceFocus` 上升沿 focus）。觸發：桌機 Enter 送出 → `POST /prompt` 期間（可達數秒）→ 回來後要再點一次輸入框。修法：改 `readOnly`／只擋 submit，或送完在桌機且焦點仍在 body 時 `focus()`。
- **[可能] components/BlockedDraft.tsx:126-136** — `commit` 回 `ok` 後 `phase` 停在 `'sending'` 不回 `'ready'`；若送完 TUI 仍停在同一份問卷（`ident` 不變不重載），整塊按鈕永遠「送出中…」。修法：`ok` 也 `setPhase('ready')`。
- **[可能] components/BlockedDraft.tsx:62-69、78-104** — `ident` 換了只換 `round`，上一輪 `preload` 沒取消（`alive` 只擋 setState），舊 job 仍對 pane 送導覽鍵。修法：`preload` 接 `AbortSignal`，`moveTab` 每步檢查。
- **[可能] lib/choiceDraft.ts:77-87、170-175** — `moveTab` 用 `question` 是否改變判斷換頁、`locate` 用 `question + choices.length` 反查分頁：兩題問句相同（或都 `null`）且選項數相同時會誤判沒換頁（預載放棄）或定位到錯的 tab（commit 走錯頁）。修法：比對 `tabs` 的 done 標記或加入選項標題做 fingerprint。
- **[可能] components/BlockedChoices.tsx:96-116** — 無 deps 的 `useLayoutEffect` 每次 render 對所有 `.bc-row` 讀 `scrollWidth/clientWidth`；`BlockedPanel` 每秒因新 `snap` 重渲染 → 每秒一次強制 layout。修法：deps 加 `[menu, open]` 或改 `ResizeObserver`。
- **[可能] components/UpdateBadge.tsx:42-44** — selector 每次 store 更新跑 `updateBatchCounts(all bots)`；側欄每顆 bot 一個 `variant="dot"` 實例 → 每個 `bot_status`／`turn_progress` frame 是 O(bots²)。修法：在 Sidebar 一次算完傳 prop，或做成 store 派生。
- **[可能] components/ChatPanel.tsx:253、270-275** — `LiveBubble` 整個 `article` 掛 `aria-live="polite"`，meta 那行「看目前內容（N 字）」與 `activity` 幾乎每幀都變 → 螢幕閱讀器每 250ms 重唸。修法：`aria-live` 只掛 `alert` 那行。
- **[可能] components/TurnErrorBadge.tsx:91-101** — 「重送上一則」只送 `lastUserText`，不帶原訊息 `attachments`，也不排除 `relay_from` 的代轉訊息。修法：找到那則 `Message` 連 `attachments.map(a => a.id)` 一起送。
- **[可能] components/TurnErrorBadge.tsx:103-135、MemPopover.tsx:156、QuotaStrip.tsx:1057** — 三個 `role="dialog"` 浮層沒接 `useDialogFocus`：開啟後焦點留在觸發鍵、Tab 走到底下頁面、Esc 後焦點不歸位。修法：與 `Lightbox` 一樣接 `useDialogFocus`，或改 `role="region"`。
- **[可能] components/GroupChatPanel.tsx:512** — `useAttachments(members[0]?.id ?? null, …)`：`members[0]` 可能是 `cloneBot` 放進清單的 `pending:` 佔位 bot → 上傳打到不存在的 bot 回 404。修法：挑第一個 `!b.pending` 的成員。
- **[可能] components/ChatPanel.tsx:175、257** — react-markdown 產生的 `<a>` 沒有 `target="_blank" rel="noopener"`，點回覆裡的外部連結會在同一分頁離開 app（非安全問題，v10 `urlTransform` 已擋 `javascript:`）。修法：`components={{ a: … }}` 補 target／rel。
- **[可能] components/HostShellPanel.tsx:186-188** — 掛載就 `inputRef.focus()`：手機開 shell 或切 bot 時 embedded 分頁重掛就彈鍵盤，與 UI-DECISIONS「切 bot 不彈鍵盤」的精神不一致。修法：`PHONE_QUERY` 時不自動 focus。
- **[可能] components/Attachments.tsx:145-152** — 上傳進行中按 × 移除，daemon 仍把檔案寫到 `<project>/.agents-manager/attachments/`，前端不告知也不清；長期累積孤兒檔。修法：記錄 in-flight promise，完成後若已移除就呼叫（尚未有的）刪除端點，或至少文件化。

### 側欄／team

- **[可能] components/Sidebar.tsx:562-567（`NewBotForm`）** — `pid` 改變的 effect 只重設 `kind`／`name`，不重設 `model`／`effort`／`fast`／`identity`（`pickKind` 才清）。觸發：先替 codex 選 `gpt-6-astra`，再點另一個 project（該主機沒裝 codex → kind 變 claude）→ 可能送出 `kind: claude, model: 'gpt-6-astra'`（取決於 `ApiModelFields` 在 kind 變時有無回呼 `onModel(null)`）。修法：effect 內 kind 真的變了就走 `pickKind(nextKind)`。
- **[可能] components/UnreadChip.tsx:75** — 用 `p.label === 'AGM'` 認 AGM 專案；專案名可就地改（`ProjectNameField`），改掉後 AGM 每輪 loop 把未讀列洗滿；使用者自取名 AGM 的專案則被整個排除。修法：改用 `SupervisorInfo.project_id` 或 bot 的 `managed_by`。
- **[可能] components/UnreadChip.tsx:111-113** — `paused` 的 team 不進未讀列，但 `paused(ask_user)`／`gate:*`／`member_blocked` 正是最需要使用者的狀態。修法：paused 且 `teamPauseAction(reason) === null` 的 team 以 `needsReply` 等級進列。
- **[可能] components/TeamLaunchPanel.tsx:237-246** — `fetchIssue` 回 `null`（`toIssueDetail` 解不出 `number`）時 `setIssue(null)`、`issueError` 仍 null → 標題永遠「讀取中…」。修法：`d === null` 時 setIssueError。
- **[可能] components/Sidebar.tsx:234-286、1181-1199** — 拖曳來源（`.bot-row`／`.project-head` `draggable`）內含改名 `<input>`；Firefox 在 draggable 祖先內拖選文字會啟動拖曳而不是選字（synthetic `onMouseDown` stopPropagation 擋不住原生 dragstart）。修法：編輯中把該列 `draggable` 設 false。
- **[可能] components/UpdateAllBanner.tsx:54** — `failed.map((f) => <li key={f.name}>)`：bot 名只在專案內唯一，兩專案各有 `am-claude` 同時失敗會撞 key。修法：把 API 的 `failed[].bot_id` 帶進 store 當 key。
- **[可能] components/TeamPanel.tsx:823-831** — `question` 從 `teamEvents` 尾端往回找**任何**帶 `payload.question` 的事件；第二次 `ask_user` 時若 `team_changed` 先於 `team_event` 到，composer 先顯示上一次的舊問題。修法：只認晚於本次 paused phase 事件的 question。
- **[可能] components/TeamNodes.tsx:34-47（`MemberRow`）** — team 成員列 `tabIndex=0` 只處理 Enter/Space，沒有 ↑/↓；一般 bot 列有（Sidebar.tsx:254-265），而 `orderedBotIds` 的 ⌥↑/↓ 會走進 team 成員。修法：接同一個 `onStep`（`adjacentBotId`）。
- **[可能] components/BotSwitcher.tsx:87-131** — `role="listbox"` 彈窗：`<button role="option">` 覆蓋 button 語意、開啟時焦點不進去、無 ↑↓／`aria-activedescendant`；手機主要用觸控所以影響小。修法：改 `role="menu"` + `useMenuKeys`。
- **[可能] components/Sidebar.tsx:1354** — 底部「新增 Bot」用 `selectedProjectId ?? projects[0]?.id`，看著某顆 bot 時預選的是第一個專案而不是該 bot 的專案。修法：退回 `selectedBot.project_id`。

### 設定／額度／主機／CSS

- **[可能] components/QuotaLoginShell.tsx:39-45** — `openHostShell(host)` 接回該主機最新的既有 shell，接著直接把 `<cli> login` + Enter 打進去，不看那個 pane 正在做什麼（跑指令、vim、上一次登入提示）；確認框寫「會開一個 shell」與實際不符。修法：登入一律開新 shell，或送字前先讀快照確認在提示符。
- **[可能] components/BotSettingsPanel.tsx:277-300 ↔ ModelPicker.tsx:402-505** — 文件層級的 Escape／pointerdown 監聽沒判斷自己是不是最上層；`ModelQuickPicker` 是 portal 到 body，不在 `.confirm-backdrop/.modal-backdrop` 內。觸發：設定面板 dirty → 點標題列 model chip → 跳「放棄未儲存的變更？」；在選單按 Esc 兩層一起關。修法：排除 `.model-quick-pop`，或改用 `useDialogFocus` 的 stack 判斷。
- **[可能] components/GhAuth.tsx:54** — `window.open` 在 `await` 之後、不在使用者手勢裡，Safari／Chrome 預設擋彈窗。修法：先同步開空視窗再導向，或只給連結。
- **[可能] components/GhAuth.tsx:81-85、153-157** — 2 秒輪詢只在 `pending` 變 null 時停；若 daemon 對過期裝置碼仍保留 `pending` 並附 `error`，輪詢永遠不停（每 2 秒 ssh + gh）。修法：`error` 非空或倒數到 0 也停。
- **[可能] components/GitBar.tsx:42-54** — 每 15 秒 `GET /projects/:id/git`（遠端是一次 ssh），分頁在背景也照跑。修法：`visibilityState !== 'visible'` 時暫停。
- **[可能] components/UpdateQuotaChip.tsx:44-65** — 五個 selector 各自每次 store 更新跑一遍 `updateBatchCounts`（掃全部 bots + `inFlightTurn`），`turn_progress` 每 bot 最多 4 幀/秒，額度列掛在每個畫面。修法：一個 `useShallow` selector 回三個值。
- **[可能] components/UpdateQuotaChip.tsx:123、131** — `readyNames.split('、')` 當名單與 React key；bot 名允許 `、`，含它的名字會被切開、key 重複。修法：selector 直接回陣列。
- **[可能] components/QuotaStrip.tsx:962-965** — 手機用 `scrollIntoView` 捲焦點格，會連帶捲祖先；UI-DECISIONS（未讀列）明講手機上它會把 `.app` 推歪。修法：只改 `.quota-open` 的 `scrollLeft`。
- **[可能] components/HostsPanel.tsx:82、88-102** — `useStore((s) => s.busy)` 讓每列主機訂閱整張 busy 表，effect deps 又含 `current`／`opening`，任何 busy 變動或 shellView 切換都重打 `GET /hosts/:name/shells`（遠端 ssh）。修法：selector 只取兩個 key；清單由 `tick` 與 `shellView.paneId` 驅動。
- **[可能] components/blockedChoices.css:185（`.bc-mark` 10.5px，在 ≤640 block 內）、unreadChip.css:224（10px）、quotaLimitHit.css:22（10px）、styles.css `.identity-diverged-mark` 10px** — 低於 UI-DECISIONS「≤640 其他文字下限 12px（只有 `.term` 例外）」。修法：≤640 補一條拉到 12px，或在文件列為例外。
- **[可能] styles.css:685、1992、4716、7506（`.bot-search-input`、`.field input`、`.dirpicker-filter`、`.issues-tools input` 的 `:focus`）** — `outline: none` 後只靠 1px 邊框變色當焦點指示，深色主題幾乎看不出。修法：`:focus-visible` 外框或 `box-shadow` 光暈（同 `.composer textarea:focus`）。
- **[可能] components/BotSettingsPanel.tsx:304-309** — 桌機開面板時無條件 `focus()` 名稱欄，使用者正在輸入框打字時按齒輪，游標被拉走，與「Composer focus 邊界」同類。修法：沿用 `useDialogFocus` 的 opener 判斷。
- **[可能] components/ModelPicker.tsx:19-31（`staticModels`）** — 退回靜態清單時把 `MODEL_OPTIONS[kind][0]` 標成 default，claude 第一顆是 `haiku`；`model: null` 的 bot 在舊 daemon／API 失敗時被畫成「選了 haiku」。修法：靜態清單不標 default。
- **[可能] components/DirPicker.tsx:144-147** — 篩選框空白時 `Backspace` 等於上一層，刪完篩選字再多按一下就離開目前目錄；文件有記載但與輸入框慣例衝突。修法：只接 `ArrowLeft`。

## 最值得補的 5 個測試

1. **送出路徑失敗不丟字**（store `flushQueued`、ChatPanel「中止並取代」「併行送入」）：把 send 邏輯抽成可注入 `sendPrompt` 的純函式，斷言 `sendPrompt` 回 false 時 `queuedSends[botId]` 或 `drafts[bot:<id>]` 仍含原文與附件 id——對應「確定」§1 的兩條。
2. **`handleFrame` 與 `sendPrompt` 的時序**：`turn_updated(completed, ok)` 先到、HTTP 回 `delivery=unknown` 後到 → `unknownDeliveryTurn` 必須為 null、`composerState` 不鎖；`resync` 後 `appliedStateSeq` 歸零且比較舊 `daemon_seq` 的快照仍被套用；`refreshState` 成功後 `botOrder`／`projectOrder` 要以 daemon 順序為準。
3. **`BotSettingsPanel` 的 patch 計算純函式化**（`computeBotPatch(base, form, touched, kind)`）：相等時空物件；外部更新後未 touched 欄位不算 dirty；`fast` 只在 codex、`identity` 三種 kind 都進；persona 空白 → `null`。
4. **未讀補記的三條路**：`message_added`＋`turn_updated` 同一回合只 +1；`bot_status` working→idle 對「已有 web 回合但這次是終端直接輸入」的 bot 也要 +1；`viewingBot && windowActive` 時不 +1 且標記推到最後一則。
5. **routeSync 開頁**：第一次 `GET /api/state` 失敗時深連結不被改寫成 `/`；`/bots/:id/settings` 在 localStorage 有 `teamId` 時仍開設定面板；`/teams/:id` 找不到時才 `backHome`。

（子審查者另外提了值得排入的：`UnreadChip` 排序純函式、`TeamLaunchPanel` 額度預檢的 `stopPct=100`、Sidebar 拖曳落點、`choiceDraft` 同名題目、mock 的 `bot_status` 契約。）

## 沒讀到／只讀部分的檔

- `api/mock.ts`（3732 行）：只讀 emit／prompt／finishTurn／restart-idle／quota／resync／setHostConnected 段落，做契約比對；內部品質未審。
- `styles.css`（11840 行）：只讀 ≤640 手機區塊、`prefers-reduced-motion`、`outline: none`／`:focus`、`.quota-pop`、`.bot-name-input` 段落（約 2000 行）；`components/*.css` 只 grep `640`／`reduced-motion`／`font-size`／44px。
- `lib/tuiChoices.test.ts`、`lib/choiceDraft.test.ts`：只看測試名稱與假 TUI 送鍵段，未逐行核對每個斷言。
- `docs/SPEC.md`、`docs/SPEC-team.md`：只讀標題與被引用的段落（§2.2、§4.5、§6.9、§13、§14、team §4.5／§7／§10／§11），沒有整份讀完。
- `web/src/api/types.test.ts`：只跑了（1 pass），沒讀內容。
