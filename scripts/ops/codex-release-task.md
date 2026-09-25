AGM 交辦：Codex CLI 出新版了，請解析這一版有什麼**這個專案用得上**的東西，然後把結論送到使用者看得到的地方。

**通知怎麼送**：如果你是巡檢 AGM（使用者入口那顆），你這則回覆就是通知。如果你是協調者或其他 child（使用者看不到你的對話），解析完要用
`bin/agm assign --notice --bot <巡檢 bot id> --request-id agm-codex-release-<新版號>-notice --text '…'` 把結論交給巡檢，由它出現在使用者入口。

這個 id 是**這一條管線（CLI 介面差異）專用**的。同一個版本的 changelog 逐條分診是另一條管線，它的公告用 `agm-release-triage-codex-<新版號>-notice`——兩邊不能共用一個 id，否則後送的那一則會被 daemon 以「client_request_id already used with different text」拒絕（issue #519）。

本次版本（由 daemon 填在訊息末尾）：舊版與新版的版本號，以及這次版差的 changelog 原文。

**先看分診帳本，不要重做**：`curl -s -H "X-AM-Token: …" 'http://127.0.0.1:7788/api/release-triage?kind=codex'`（或 `bin/agm release-triage show`）。
那條管線已經對每一條 changelog 下過 verdict 的版本，你只補它看不到的東西（CLI 介面、設定鍵、實際行為），逐條結論引用帳本就好，不要再逐條判一次。

怎麼解析（唯讀，不要 build、不要重啟、不要改設定、**不要升級正在用的 codex**）：

1. 拿到兩顆 binary：舊版在 `~/.codex/packages/standalone/releases/<舊版號>-<平台>/bin/codex`（`ls` 那個目錄）；新版通常**還沒安裝**，
   裝到拋棄式目錄，例如 `d=$(mktemp -d) && npm install --prefix "$d" @openai/codex@<新版號> && NEW="$d/node_modules/.bin/codex"`，用完刪掉 `$d`。
   不要跑 `codex update`、codex 自己的更新選單或安裝腳本（那會換掉所有 codex bot 正在用的那一顆）。
2. CLI 介面差異：`diff <($OLD --help 2>&1) <($NEW --help 2>&1)`，`exec`／`resume`／`fork` 各自再 `--help` 一次；`features list` 有的話也比一次。
3. 設定鍵：兩顆 binary 各自抓 `config.toml` 的鍵名與 `CODEX_*` 環境變數名，取差集（`strings` ＋ grep 即可）。
4. 找到疑似有用的功能就**實測**再下結論（新旗標用 `codex exec` 在拋棄式目錄跑一次、新輸出格式就看實際 JSON），不要只讀字串猜。
   測試一律用拋棄式目錄與拋棄式 `CODEX_HOME`，不要動正式 daemon、不要在同機起第二顆 daemon。

判斷「用得上」的標準——這個專案靠這些跟 codex 打交道，任何能讓它們更穩的都算：

- 啟動參數：`lifecycle/start.rs` 固定帶的旗標（`--no-alt-screen`、`--no-daemon` 等）有沒有改名、預設有沒有翻面。
- 送達與畫面判讀：`lifecycle/delivery.rs`、`lifecycle/screen.rs`、`codex_live.rs`（狀態列、composer、回覆擷取、更新提示 `codex_update.rs`）。
- 額度：`quota.rs` 讀 codex 狀態列與撞限橫幅。
- hook 與 session：`hookrecv.rs`、`codex resume`／`codex fork`、`CODEX_HOME` 隔離身分。

回報（就是給使用者的通知，三到五行）：新版版本號、值得用的項目（每項一句：是什麼、對應我們哪個痛點、要改哪個檔）、明確沒用的略過不列、以及你建議的下一步（要不要派人改、要不要升級）。沒有值得用的就一句「本次無」。**不要**在這則交辦裡直接改程式、升級或部署；要改另外走既有的派工與核准流程。
