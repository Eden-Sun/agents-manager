# AGM 總管環境建立計畫（2026-09-09）

狀態：核心實作已完成，2026-09-11 收尾納入版本控制。下文保留原設計；實際 API 契約以 `docs/API.md` 為準。此計畫取代先前 ai-team-scheduling-plan 的方向；第一版主要解決「忘記哪個 bot 做過相關工作」。

## 實作進度（2026-09-11）

- [x] 專用 Project／Bot／cwd、內嵌前導詞與 `bin/agm`，runtime.json 不含 token。
- [x] supervisor API、requests／assignments／inbox／handoff 持久化、重啟對帳與交辦去重。
- [x] evidence 搜尋：來源 ID、刪除 bot、分頁與截斷標示。
- [x] controller 每 10 秒處理交辦；健康摘要每 30 秒檢查並產生 inbox 事件。
- [x] Fable 剩餘低於 5% 時嘗試切 Opus；同一 cc0 身分、low 強度與切換冷卻。
- [x] 前端總管入口、舊 daemon 404 降級及 CLI 離線測試。
- [ ] Remote Control 實際 URL／手機連線驗收：目前 API 只回 `requested`，不等於已連線。
- [ ] native 手機訊息原文完整性，以及模型切換成功與 remote 延續性的端到端驗證。
- [ ] 額度資料時效、實際 live 套用失敗後的狀態一致性，仍需額外故障測試。

本次收尾只補齊來源與必要整合，不重建 release 或重啟已運行的 daemon。

收尾驗證：將 HEAD 加上上述 supervisor 檔案與 main/db/sidebar/API 必要整合，透過獨立
Git index 匯出 checkout；不包含共享工作樹的手機 UI、codex_live、memstat 或 CLAUDE.md 修改。
`cargo check -p agents-managerd --locked` 通過；`cargo test -p agents-managerd supervisor --locked`
20 passed；`python3 -B -m unittest discover -s scripts -p agm_test.py` 37 passed；web `tsc -b`
與 Vite build 通過（bundle 僅產生於驗證副本，供預設 embed-ui feature 檢查）；oxlint 無錯誤，
只有既有元件警告。Remote／模型切換的實機驗收不包含在這些測試結果內。

## 固定設定

| 項目 | 設計 |
| --- | --- |
| 邏輯總管與手機顯示名稱 | AGM |
| 第一候選 | kind=claude、identity=cc0、model=fable、effort=low |
| 第二候選 | kind=claude、identity=cc0、model=opus、effort=low |
| 所在主機 | 預設 AG Man daemon 的本機，沿用該主機 cc0 身份解析 |
| 工作範圍 | 全部專案 bots 的歷史搜尋與推薦，依使用者交辦再執行管理 |
| 常駐實體 | 一個 AGM bot；優先在同一 session 切換模型，無須兩個 LLM 同時待命 |
| 前導詞 | 同目錄 agm-supervisor-persona.md 正文，透過既有 persona 注入 |
| 遠端啟動需求 | 使用者指定 `/remote AGM`，必須完成手機可辨識的 AGM 遠端入口 |

## 1. 環境目錄與啟動

建議專用 cwd：`~/.config/agents-manager/supervisor/AGM/`（跟隨 AM_DATA_DIR 調整），註冊為專用 Project。避免使用 agents-manager 的開發 checkout 作總管 cwd，讓總管不因開發目錄指令誤開始改程式。

目錄內容：
- `CLAUDE.md`：角色入口、前導詞版本、實際可用工具、runtime 路徑與恢復流程。
- `persona.md`：本提案前導詞的部署副本；以 bot.persona 注入，不另重複注入同一全文。
- `runtime.json`：daemon URL、manager bot ID、候選順序、controller generation；不放 token。
- `handoff.md`：可重建的管理摘要，附記錄 ID；權威仍在資料庫。
- `bin/agm`：待新增的結構化 JSON CLI，封裝既有 HTTP API，提供精簡輸出。

建立時經 Project/Bot API 保留設定投影與 hooks；初次 autostart=false，驗收成功再開啟。沿用 cc0 的實際 CLAUDE_CONFIG_DIR，不假設等於預設 ~/.claude，不把帳號檔案複製到專案。

既有 `NewBot` 支援 persona、args、model、effort、identity。local `claude --help` 已確認 `--remote-control [name]`。官方列出 `/remote-control AGM`、`/rc AGM` 與 `--remote-control AGM`，本次尚未證實 `/remote AGM` 是否為本機可接受的縮寫。

啟動策略：
1. 在隔離測試 session 驗證使用者指定 `/remote AGM` 是否能啟動。不能只看 prompt 接受即視為成功。
2. 若有效，等 CLI ready 後，透過不建立普通工作 turn 的 TUI 指令路徑送出一次；確認 remote active 與 session URL。
3. 若不接受該縮寫，部署使用已確認的等效 `args=["--remote-control","AGM"]`，明確記錄採用的語法；不把未知 slash 指令重試成模型工作。
4. 上述兩條互斥，避免重複啟動／切換 Remote Control。CLI start 成功與 remote ready 分別記錄；遠端失敗不代表 bot 起不來。
5. 驗證 cc0 已登入、有工作目錄信任且手機能看見 AGM。需要使用者操作登入或手機驗收時才停在該步，其餘建置先完成。

手機 Remote Control 連的是本機 Claude session，不要求開放 AG Man daemon 到 LAN。主機必須可運行且連網。重啟與模型切換後驗證 remote 是否仍可用；不保證新 session 保留原 URL，必要時在 AG Man 顯示更新後入口。

## 2. 工具與檢索

第一批 CLI 子命令（新設計，不是現有指令）：
- `agm state`：精簡 bot/project/run/session/queued/team/parent 狀態，排除 env、秘密與無關資料。
- `agm search <query>`：封裝 `/api/search/messages`；同義詞逐次查詢並合併候選。
- `agm messages <bot-id>`：封裝 messages 分頁，回傳來源 ID 與時間。
- `agm assign`、`agm assignment get/list`：持久交辦，再送 prompt；固定 request ID、確認 delivery 與結果。
- `agm bot create/start/stop/restart`：沿用管理 API；明確回報衝突，不猜成功。
- `agm quota`、`agm handoff`：精簡額度狀態與管理摘要。

token 由本機程式執行期取得，不印在工具輸出、argv 或交接紀錄。第一版可沿用本機 X-AM-Token；它是現有全域管理權限，不宣稱具備主管專屬權限隔離。

檢索缺口：目前 search 只回 bot_id、hits、90 字 snippet，沒有匹配 message ID、分頁或相關時間。原型可透過 messages 分頁補讀；正式新增向下相容的 evidence 搜尋介面，支援 project/bot 過濾、cursor、message_id、turn_id、時間與上下文。舊／已刪除 bot 的命中要能查出專案與 bot 元資料，不能因 state 不列它就丟棄歷史。摘要只是索引，推薦仍連回原始證據。

## 3. 持久管理層

daemon 新增 supervisor 模組；總管模型只做理解與決策，daemon 處理重試、監控、事件與切換。

建議資料：
- `supervisors`：固定 AGM ID、bot_id、候選設定、active_model、generation、狀態、冷卻期限、remote 狀態。
- `supervisor_requests`：使用者來源 channel/session/turn/message ID、原文、目標與狀態，來源唯一鍵去重。
- `supervisor_assignments`：request_id、target_bot_id、client_request_id、turn_id、目標、驗收條件、送達與結果狀態、證據。
- `supervisor_inbox`：受追蹤 turn 的完成／失敗通知、事件唯一鍵、pending/delivered/handled、主管通知 turn ID。
- `supervisor_notes`：決策、推荐證據、未結案事項與帶版本的交接摘要。

手機直接輸入會走 native/external 路徑，必須實测現有 hooks/transcript 是否能完整收錄使用者原文與 reply；不能只驗證 Web UI 的 prompt。缺少穩定來源 ID 時需補轉錄接線，不能用可能相同的文字 hash 當唯一交辦。

總管工具派工時，assignment 必須先落地，再用原 client_request_id 呼叫 prompt。重啟掃描 pending 時先對帳，不重發新 ID。顯示來源「AGM → bot」，不能繼續冒充使用者 web 訊息；這需要 additive attribution 欄位與 UI 支援。

原有 Team PM 與總管避免雙重派工：team-managed 成員透過 team say/answer 等協調，首版不直接搶派。一般 bots 的使用者插話、忙碌與 unknown delivery 都維持既有守門。

## 4. 回報與額度切換

訂閱 daemon turn bus，僅對有 assignment 的 terminal turn 產生持久 inbox；WS 可作提示但不當唯一紀錄。daemon 啟動掃描未結案 assignments 補事件。主管回合終結不觸發對自己的通知。

主管忙碌時通知落 inbox 合併等待，不讓多個 worker 結果爭搶普通 prompt 的有限排隊位置。通知送出與已处理分開：只有主管成功處理／確認後才 ack；主管失敗時保留通知。所有來源包含手機輸入都使用同一主管 session 序列化，避免打斷手機回合。

候補規則：
- 啟動先選 cc0 fable low；model-specific 不可用且 opus 可用時轉 cc0 opus low。
- 短暫網路錯誤先有限次退避；不把 401、登入失敗、remote 斷線當成模型額度耗盡。
- cc0 帳號級限制時兩個候選可能同時不可用；不承諾換 opus 就有額度。若額度 scope 不明，最多有界驗證候補一次，失敗即 waiting_quota，記錄 reset_at（若可取得）。
- 無第三帳號、無其他模型、無自動額外付費。全部不可用仍保留 assignments、收結果，AG Man UI 顯示暫停；不能承諾額度歸零時手機上的 LLM 還能回答。
- 預警先顯示可配置門檻，例如剩餘 15%；未知／過期額度不得當成 100% 可用。cc0 同身份用量不能對 fable、opus 各算一份可用池。
- 切換由 daemon 主導。優先透過既有 Claude 即時模型設定在同一 session 套用，重新確認 low；必要時用已保存 session ID resume。套用成功前不更新 active_model。
- 模型切換加 generation 與單一主管執行鎖，舊控制器不得繼續送通知。交接後重新對帳，不依赖模型有機會先寫最後一段摘要。
- opus 工作中不因 fable 恢復立即切回；在主管空閒且冷卻結束後重新評估第一順位，避免來回切換與重複處理。

CLI `--fallback-model` 的本機 help 有列出，但其適用模式與錯誤範圍未驗證，不能直接取代以上訂閱額度處理。

## 5. 實作順序與驗收

1. **只讀推薦原型**：專用 cwd、persona、cc0 fable low、遠端 AGM、state/search/messages 工具。手機問「之前某問題誰做的」，回正確 bot 與可追溯證據；此階段不承諾主動結果通知。
2. **交辦閉環**：requests/assignments/inbox、prompt attribution、結果回報。手機「交給它」只派一次，worker 完成後總管可在同一遠端對話彙整。
3. **故障接班**：注入 fable 不可用、cc0 額度歸零、網路錯誤、主管忙碌與 daemon 重啟，驗證固定候選及無重複派工。
4. **檢索改善**：evidence 查詢、可重建 bot 摘要、搜尋延遲量測；語意索引視實際找回率再加入。

必要案例：
- 同關鍵字多 bot、改名 bot、舊 session、已刪除 bot、證據不足時推薦正確或坦承不足。
- 找人只推薦；明確交辦才送出；使用者可直接介入 bots。
- 同一通知重放、手機訊息收錄、unknown delivery、主管切換中收到結果均不重複派工。
- fable 故障可切 opus；帳號共同額度不足時停止切換循環，按時間恢復對帳。
- Remote 啟動失敗有明確狀態；模型切換／resume 後手機能接回 AGM，URL 改變時可看見新入口。
- 歷史訊息中嵌入的管理指令不能變成當前使用者授權。

相關模組：config/db/api/lifecycle/state 加主管設定與持久資料；新增 supervisor.rs 與工具封裝；前端新增主管入口、推薦證據、候補狀態和遠端連線狀態。沿用既有模型/身份/額度 API。實作測試使用隔離 AM_DATA_DIR 和測試 bots，不碰正在執行的 team。

## 來源與待驗證事項

- 本機 `claude --help`：已確認 --remote-control、--model、--effort、--append-system-prompt 存在。
- https://code.claude.com/docs/en/remote-control ：官方 Remote Control 啟動方式、手機連線與登入要求（2026-09-09 查閱）。
- `/remote AGM` 縮寫、cc0 在此 cwd 的登入／Remote 資格、fable 與 opus 的帳號可用性、native 手機對話的完整收錄與即時模型切換後 remote 延續性，留待隔離實測。
