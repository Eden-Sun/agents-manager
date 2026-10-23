AGM 交辦：Antigravity CLI（agy） 出新版了，請解析這一版有什麼**這個專案用得上**的東西，然後把結論送到使用者看得到的地方。

**通知怎麼送**：如果你是巡檢 AGM（使用者入口那顆），你這則回覆就是通知。如果你是協調者或其他 child（使用者看不到你的對話），解析完要用
`bin/agm assign --notice --bot <巡檢 bot id> --request-id agm-agy-release-<新版號>-notice --text '…'` 把結論交給巡檢，由它出現在使用者入口。

這個 id 是**這一條管線（CLI 介面差異）專用**的。同一個版本的 changelog 逐條分診是另一條管線，它的公告用 `agm-release-triage-agy-<新版號>-notice`——兩邊不能共用一個 id，否則後送的那一則會被 daemon 以「client_request_id already used with different text」拒絕（issue #519）。

本次版本（由 daemon 填在訊息末尾）：舊版與新版的版本號，以及這次版差的 changelog 原文。

**先看分診帳本，不要重做**：`bin/agm release-triage show --kind agy`（或 `curl -s -H "X-AM-Token: …" 'http://127.0.0.1:7788/api/release-triage?kind=agy'`）。
那條管線已經對每一條 changelog 下過 verdict 的版本，你只補它看不到的東西（CLI 介面、設定鍵、實際行為），逐條結論引用帳本就好，不要再逐條判一次。

怎麼解析（唯讀，不要 build、不要重啟、不要改設定、**不要升級正在用的 agy**）：

0. 先讀官方說明，不要只靠 daemon 附在末尾的摘錄：官方 repo 的 CHANGELOG.md（`gh api repos/google-antigravity/antigravity-cli/contents/CHANGELOG.md --jq .content | base64 -d`）與
   `gh release view <版本> -R google-antigravity/antigravity-cli`（舊版之後到新版的每一版）。
1. 拿到新版 binary：官方 manifest `https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/<platform>.json`（`{version,url,sha512}`）只當資料讀；
   要實測才把 tarball 下載到拋棄式目錄、驗 sha512、解出 `antigravity` 跑 `--help`（`AGY_CLI_DISABLE_AUTO_UPDATE=true`），用完刪掉。**不要跑官方 `install.sh`、`agy install`、`agy update`**（會改 shell profile、換掉所有 agy bot 正在用的那顆）。
2. CLI 介面差異：`diff <($OLD --help 2>&1) <($NEW --help 2>&1)`，`plugin`／`mcp` 各自再 `--help` 一次；`agy changelog` 只有已裝版本以前的內容，看不到新版。
3. 設定鍵：`~/.gemini/antigravity-cli/settings.json`、`~/.gemini/config/hooks.json` 的鍵名與 `AGY_*` 環境變數名，新舊 binary 各抓一次取差集（`strings` ＋ grep 即可）。
4. 找到疑似有用的功能就**實測**再下結論，一律用拋棄式目錄與拋棄式 `HOME`，不要動正式 daemon、不要送 prompt。

判斷「用得上」的標準——這個專案靠這些跟 agy 打交道，任何能讓它們更穩的都算：

- 啟動參數與設定注入：`lifecycle/start.rs`、`lifecycle/setup.rs` 固定帶的旗標、設定檔與環境變數有沒有改名、預設有沒有翻面。
- 送達與畫面判讀：`lifecycle/delivery.rs`、`lifecycle/screen.rs`、`tui_prompts.rs`（狀態列、輸入框、確認框、更新提示）。
- 額度：`quota_agy.rs` 讀 agy 的額度（登入後 statusLine JSON 的 `quota`）與撞限橫幅
- hook 與 session：`hookrecv.rs`、`hook_cmd.rs`、resume／fork、`~/.gemini/config/hooks.json` 的事件與 payload 欄位。

回報（就是給使用者的通知，三到五行）：**先一句結論：適不適合現在升級**，再列該提防的（會弄壞畫面判讀、按鍵、啟動參數、hook、額度讀取的改動，每項一句＋對應檔案）、新版版本號、值得用的項目（每項一句：是什麼、對應我們哪個痛點、要改哪個檔）、明確沒用的略過不列、以及你建議的下一步（要不要派人改、要不要升級）。沒有值得用的就一句「本次無」。**不要**在這則交辦裡直接改程式、升級或部署；要改另外走既有的派工與核准流程。
