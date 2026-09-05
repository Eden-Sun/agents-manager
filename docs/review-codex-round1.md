已實際執行 `herdr api schema --json`、`herdr --skill`，兩者皆成功；schema 為 protocol 20、schema_version 1。已讀完目前含附錄 A 的 [SPEC.md](/Users/m1pro/project/agents-manager/docs/SPEC.md)，未修改、建立或刪除任何檔案。

以下「附錄記載」是規格內的實測結果，本次未重測；Claude／Codex 的補充機制引用官方文件，**本機 CLI 版本是否支援仍須驗證，不能視為 herdr 保證**。

## 1. 名詞對應與資料模型

- **Project ↔ workspace 應定義成本系統約束。** Schema 的 `WorkspaceInfo` 沒有 `cwd`，不能據此保證「一目錄一 workspace」。以 `project_id + canonical_path` 管理目錄唯一性，另外保存 session／workspace 映射；不要靠 pane 當下的 cwd 反推專案。
- **分開 Bot 與執行實例 Run。** Bot 使用永久 `bot_id`；Run 保存 `run_id、workspace_id、tab_id、pane_id、agent_session`。Skill 明載：pane 可沒有 agent；agent 名稱在退出、釋放或替換後清除；跨 workspace 移動會改變 pane ID。因此 `name`、`pane_id` 都不適合作為聊天歷史主鍵。
- **分開連線狀態、程序生命週期與 agent 狀態。** `done → idle` 的 UI 映射合理，但 `done` 表示尚未被 Herdr UI 看過的 idle，不是任務完成證明；Web 的未讀狀態應自行保存。連線中斷也不能直接標成 `offline`。
- **Conversation 缺少原生 session 與 turn 邊界。** 補 `conversation_id、native_session_ref、turn_id、message_id、來源識別`。第一階段限定每 Bot 一個使用中的對話；新啟動與 resume 必須明確區別，避免畫面保留舊聊天卻讓 agent 使用全新上下文。

## 2. Reply extraction 的可靠性與替代來源

- **終端差分不應作為正式聊天紀錄來源。** Skill 明確指出 alternate screen 離開畫面的內容可能無法取回；schema 的 `PaneReadResult` 是含 `revision、truncated` 的快照。重繪、截斷、重複文字造成漏收／重收，是目前差分方案的設計風險；附錄中的 `⏺`、`•` 只能算觀察到的畫面格式。
- **Claude Code：優先接 hooks。** 用 `SessionStart` 的 `session_id、transcript_path` 建立映射，常見路徑為 `~/.claude/projects/<project-key>/<session-id>.jsonl`；即時回覆優先取 `Stop.last_assistant_message`。官方文件指出 Stop 時 transcript 不一定已包含最後訊息；`Notification` 可提示權限等待，但不能代表完整回覆。[Claude hooks](https://code.claude.com/docs/en/hooks)
- **Codex：hooks／notify 比解析畫面可靠。** 支援 hooks 的版本可取得 `session_id、transcript_path`，以及 `Stop.turn_id、last_assistant_message`；另一方案是 `config.toml` 的 `notify`，它以**程序參數中的 JSON**傳入 `agent-turn-complete、thread-id、turn-id、last-assistant-message`。`notify` 不能替代 blocked／審批流程。[OpenAI hooks](https://learn.chatgpt.com/docs/hooks)、[OpenAI notify](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications)
- **Transcript 用於回補，採實際回報路徑。** Codex 可把 `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` 列為候選搜尋布局，但這項路徑假設須依支援版本驗證；不要按 cwd 挑「最新檔」。官方也明言 Codex transcript 格式不是穩定介面。[OpenAI hooks](https://learn.chatgpt.com/docs/hooks#common-input-fields)
- **補持久化擷取游標。** 依 provider／session／turn／原生 message ID 去重；JSONL 只提交完整行，游標與訊息同交易保存，處理延遲落盤及重啟回補。`blocked`、中止、失敗各有獨立事件；來源不足時標示「擷取不完整」，不要存成看似完整的 assistant 回覆。

## 3. Daemon 與 herdr session 的關係

- **建議預設專用 named session。** 好處是 ownership、名稱衝突與清理範圍較清楚；代價是需管理另一個 server／session。共用使用者 session 能直接利用既有 panes，但移動、關閉與人工輸入更難協調，建議延後支援。
- **正文應回填附錄的啟動與 socket 記載。** 附錄寫的是 `herdr --session agents-manager server`，socket 為 `~/.config/herdr/sessions/agents-manager/herdr.sock`；這與 §3.1 的指令及固定 socket 不一致。應統一 endpoint 解析，連線後檢查 `ping` 的 version／protocol／capabilities。**上述啟動行為本次僅採信附錄，未另行實測。**
- **Daemon 與 server 生命週期分離。** Daemon 重啟先用 `session.snapshot` 對帳；退出 daemon 預設保留 agents。所有關閉操作限定本系統擁有的資源；把「中斷目前回合」與「停止 Bot」分開，不能把傳送 `ctrl+c` 當成已退出。

## 4. API 與 WebSocket 事件

- **修正 socket 欄位。** `agent.read.source` 用 `recent_unwrapped`；`pane.split` 明確指定 `target_pane_id`，避免落到使用者焦點。前端 REST 可以自訂名稱，但 client 必須明確轉換。
- **修正啟動狀態機。** 附錄記載 socket `agent.start` 立即回傳 `launch_pending:true`，因此 §6.1 不能直接改成 idle。保持 starting，經 `agent.wait`／狀態事件及 `agent.get` 確認就緒；blocked、timeout 後仍須檢查是否留下實例。
- **區分訂閱格式與一般事件格式。** 訂閱 type 是 `pane.agent_status_changed`，且必須帶 `pane_id`；`pane.output_matched` 還需要 source／match。一般事件另有 `pane_agent_detected` 等底線名稱，應分開解析兩種 envelope。Schema 未將 `pane.output_changed` 列為可訂閱項，不能直接假設存在完整輸出串流。
- **補送訊息交易語義。** 每 Bot 同時只允許一個進行中回合；加入 `client_request_id`、`queued/sent/failed/delivery_unknown`。HTTP 回傳 message ID 不代表 agent 完成；失去 herdr 回應時不可盲目重送。Schema 的 request `id` 沒有承諾冪等。
- **WS 補同步契約。** 加 daemon 自己的事件序號、`bot_id/run_id/conversation_id`、訊息更新事件與明確的新增／刪除 payload。斷線後重播或要求重新載入快照；schema 未承諾 herdr 事件重播。終端資料應標示為 snapshot，附 source／format／revision／truncated，不能逐份 append 到 xterm。
- **補控制端點邊界。** keys 帶預期 `run_id` 與畫面 revision，拒絕過期操作；DELETE 明訂是否停止程序、保留歷史，且不包含刪除專案目錄。REST／WS 加本機 token 與 Origin／Host 檢查，不能只依賴 localhost。[WebSocket Origin 驗證](https://cheatsheetseries.owasp.org/cheatsheets/WebSocket_Security_Cheat_Sheet.html#origin-header-validation)

## 5. Rust 技術選型與 client 型別

- **axum＋tokio＋serde 合適；建議定案 SQLx＋SQLite。** 與現有 async 呼叫鏈較一致。若選 rusqlite，也可行，但應使用專用 DB 執行緒，或把短暫阻塞工作放到 `spawn_blocking`，避免卡住事件處理。[Tokio 阻塞工作指南](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- **`toml_edit` 適合保留格式，但不處理一致性。** 若保留 UI 寫回，需另定鎖、版本衝突檢查與原子替換；TOML 保存期望設定，SQLite 保存執行狀態，避免雙重權威。它也不保證保留 dotted keys 的順序。[toml_edit](https://docs.rs/toml_edit/latest/toml_edit/)
- **Client 依附錄採「每次 RPC 新連線＋獨立長連線訂閱」。** 不要設計成一般持久 RPC multiplexing；request ID 必須是字串，並處理 timeout、EOF、錯誤 envelope 與重新訂閱。單請求連線行為是附錄記載，schema 本身未描述。
- **v0 建議手寫必要 subset，固定 schema 作契約檢查。** Request／result／兩種 event envelope 分開建型別，容忍新增欄位並處理未知事件。若改用 [typify](https://docs.rs/typify/latest/typify/)，先處理 bundle 的 `#/schemas/...` 引用與重複 definitions；不要在每次 build 時依賴機器上安裝的 herdr 動態生成。

## 6. 第一階段最應砍掉／補上的各三項

- **砍 1：** xterm.js 即時終端串流；先提供可更新的唯讀文字快照。
- **砍 2：** UI 動態配置與 TOML 保格式寫回；先以 TOML 為單一配置入口。
- **砍 3：** autostart／一鍵 restart；先完成手動 start／stop 與既有實例恢復。
- **補 1：** Claude／Codex 回覆 adapter、原生 session 綁定與擷取相容性驗收。
- **補 2：** daemon 重啟、socket 斷線、pane 移動／退出的對帳與 ownership。
- **補 3：** 每 Bot 回合序列化、冪等請求、送達未知狀態及持久化游標。

## 7. 三個待決問題的明確建議

- **問題一：採 hooks／notify 為主要來源，transcript 回補，終端僅供診斷。** Skill 建議的「請 agent 寫完整 Markdown 檔」保留為使用者主動觸發的補救操作，不自動混入正常聊天。
- **問題二：使用 `AgentSessionInfo`，但修正讀寫方向。** 從 `agent.get`／`pane.get` 的 `agent_session` 讀取 `source、agent、kind、value`；`pane.report_agent_session` 是回報，不是查詢。把附錄所述 integration 安裝與 `CLAUDE_CODE_CHILD_SESSION` 問題納入啟動驗收；不能假設 `env` 傳空字串就等於移除環境變數。
- **問題三：允許 attach 同一個專用 session。** 提供人工接管模式，接管時 Web 暫停送 prompt，並同步 TUI 產生的對話；實際 attach 指令及 detach 後存活行為列入驗收，不從 schema 推定。

## 若只能改五件事

1. 將附錄 A 回填正文，統一 socket、連線模式與非同步啟動契約。
2. 以 hooks／notify＋transcript 取代終端差分作為正式回覆來源。
3. 補永久 Bot ID、Run／Conversation／Turn 模型及可靠送訊息流程。
4. 補 snapshot 對帳、重連回補與資源 ownership，避免重啟後重複啟動或誤關。
5. 補 REST／WS 的本機存取驗證，以及 keys／stop／DELETE 的明確語義。

Codex session ID: 01a07201-461e-78a2-b18a-0eb95cd8648b
Resume in Codex: codex resume 01a07201-461e-78a2-b18a-0eb95cd8648b
