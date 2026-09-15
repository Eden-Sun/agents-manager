# OB（網頁 GPT 外腦）

「問 OB」「請 OB review」指透過 ego lite 的 ChatGPT 網頁取得第二意見，原稱 GPT 外腦。操作員固定 **Sonnet、low**，只負責把問題交給網頁並取得回答；原任務 bot 決定如何採納，仍須自己查程式與驗證。設計取捨、review 分歧、根因第二意見適合 OB，查得到的事實與例行進度不必問，也不必先向 AGM 申請一般諮詢。

## 專案隔離

- 索引只用 **AG Man project ID**，不用 project label／目錄名／worktree 名。同 ID 改名或換 bot 仍沿用對話；不同 ID 即使同名也不能共用 URL。
- 每專案一個 ChatGPT 對話、一個分頁。首則新對話訊息寫 `OB｜專案名稱｜project ID`；ChatGPT 自動產生的側欄標題不是索引依據。
- 共用一個 worker（不是每個 project 一顆常駐模型）。每筆呼叫 `claude --print --model sonnet --effort low`，不 resume、不保存模型 session；在新的空目錄中只帶本筆問題，不載入 caller 的 hooks、MCP、CLAUDE.md 或其他專案 context。
- Sonnet 只有綁定本筆 project/request 的 `consult` MCP tool，不能改收件專案、正文或自行回覆。tool 收據的網頁原文才是 `answer`，不採用 Sonnet 的轉述當原文。
- 請求、結果與 project→URL 對應在 `~/.config/agents-manager/ob/ob.sqlite3`。SQLite 交易、唯一鍵與 OS worker/browser lock 防止重複派送、跨專案 JSON 覆蓋與同時操作網頁。

## bot 怎麼問

```sh
python3 scripts/ob.py ask --project-id 01M1Y7BNVP843V9MFEDJ2KW9NQ \
  --request-id design-review-001 --wait 600 "背景、選項、代價與需要第二意見的問題"
python3 scripts/ob.py ask --project-id <project-id> --request-id <stable-id> -f question.md
python3 scripts/ob.py status <回傳的id>
python3 scripts/ob.py status --project-id <project-id>
```

不給 `--project-id` 時，必須有 `AM_BOT_ID`，CLI 從本機 daemon 找到該 bot 的 project；找不到就拒絕，不猜 cwd。明給的 ID 也會核對 project 存在。本機 daemon 固定 `127.0.0.1:7788`，遠端專案需在對應主機提交。`--request-id` 必填：同 project＋request ID＋相同正文／來源只回原單；換內容回 `request_mismatch`。

`ask` 先持久化，再啟動共用 worker；不帶 `--wait` 立即回收據，等待逾時也不取消、不重送。`--no-start` 只排隊。`OB_DATA_DIR`／全域 `--data-dir` 可指定隔離資料目錄。其他 repo 可用已安裝 CLI 的絕對路徑；父 bot 派工／重用 child 時帶入 CLI 路徑、文件與同一 project ID。

舊入口 `scripts/chatgpt-consult.sh` 現在轉交 `ob.py ask`，`-p` **改為 ID**、必須附 `--request-id`，不再直接操作瀏覽器。移除自動猜 label 與 `--new`，避免不知情地建立第二串對話。

## 狀態與額度

- `pending`：已持久化待送（未 configure 也能排隊，回 `operator_configured:false`）。
- `running`：已由 worker 領取。
- `done`：瀏覽器回答與 URL 已保存，可由任何原呼叫者查原單。
- `waiting_quota`：Sonnet 回額度錯誤，保留原單，全佇列退避 30 分鐘後才再嘗試；沒有 fallback 帳號或模型。空佇列只做本機輪詢，不呼叫模型。已確認恢復可 `retry <id>` 提早解除退避。
- `failed`：沒有送出收據，檢查登入／CLI／瀏覽器後可 `retry <id>`。不能把 Sonnet 自己輸出的文字當答案。
- `unknown`：可能送出過或 worker 中斷。該 project 的後續單暫停，其他 project 可繼續。用 `collect <id>` **只讀取原回答、不再送出**，依 request marker 配對，不拿最後一則不相關回答冒充。尚無 URL 或對話狀態無法確認時，先人工／bot 查看 journal 指向的原分頁；確定根本沒送出才 `resolve <id> --confirmed-not-sent`，並保留原 journal 作為對帳紀錄。不要用新 request ID 繞過。

worker 意外退出留下的 `running`，在重放 `ask`、`ask --wait`、`status`、`collect` 或執行 `recover` 時，以 `worker.lock` 確認沒有存活 worker 後轉為 `unknown`；保留原單與瀏覽器 journal，絕不重新送出。存活 worker 持鎖時不改狀態。`recover` 回 `{worker_running,recovered[]}`，無 id 的 `status` 也列出本次恢復的 id。`collect` 等待原 request marker 對應的完整回答，不以頁面目前渲染的歷史訊息數判斷完成。

問題先整理專案、現況、選項與取捨；ChatGPT 看不到 repo，只看得到提供的內容。不要送 token、密碼、ui-token 或客戶資料。回報區分「OB 建議」與「本機已驗證」，附對話 URL 方便查證。OB 不替代使用者授權或 AGM 的 ownership／運維裁示。

## 安裝與帳號

只需 Python 3、Node、已登入的 Claude Code 與 ego-browser，不需要 Rust rebuild 或 daemon 重啟。worker 不是 AG Man pane，不開 remote control；由 `ask` 啟動的一個本機服務程序承接佇列，模型只在有請求時啟動。

將 `ob.py`、`ob_store.py`、`ob_operator.py`、`agm.py`、`chatgpt-consult.mjs`、`chatgpt-consult.sh` 放在同一個穩定目錄（例如 `~/.config/agents-manager/ob/bin/`），不要指向之後會刪掉的 worktree。先 configure，再交給 bots 使用：

```sh
python3 scripts/ob.py configure --claude-config-dir ~/.claude
# 或明確選已登入的另一個帳號目錄；不在任務中自動換帳號
python3 scripts/ob.py work --once  # 手動處理一筆；work 不帶 --once 持續處理
```

configure 只存帳號設定目錄與 CLI 路徑，不保存 token。`~/.claude` 使用預設 keychain 身分；其他目錄明帶 `CLAUDE_CONFIG_DIR`。USER/LOGNAME 保留給 keychain，caller 的 AM_*、session、模型與 API key 不繼承。worker 運行中禁止 configure；先 `stop`，等 `status` 的 `worker_running:false`，再改設定。更新安裝檔前也先等佇列閒置並停止 worker，避免執行半套版本；不須重啟 daemon。

既有 `chatgpt-consult.json` **不刪、不依相似名稱自動合併**。由維護者明確指定原 entry 與 project ID，保留已有脈絡：

```sh
python3 scripts/ob.py link --project-id <project-id> --legacy-key <舊登錄key>
# 或明確指定原對話
python3 scripts/ob.py link --project-id <project-id> --url https://chatgpt.com/c/<conversation-id>
```

一個 URL 不能同時綁兩個 project，也不覆寫已綁定 project 的不同 URL。先 link，再讓該專案送第一題。

## 瀏覽器保留與驗證

沿用唯一 task space **「ChatGPT 決策顧問」**，不因簡稱 OB 改名。任何 bot、AGM、browser-gc 都不能關它或它的分頁、刪除 OB 資料庫／journal／舊登錄檔（SPEC §18.4）。已知對話的分頁消失會回原 URL；新專案不用他人的空白 ChatGPT 草稿頁。使用者接手、登入問題或網頁改版時停止，不另開 space 規避。

`scripts/check.sh ob` 跑隔離的 Python 佇列／operator 測試與 Node 瀏覽器契約測試，不消耗模型／網頁額度。真實驗證另以明確指定專案的短問題測試，確認回傳 `done` 與相同對話 URL。
