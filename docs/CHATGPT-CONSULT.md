# GPT 外腦（ChatGPT Consult）

**統一用語：「GPT 外腦」**。使用者說「問 GPT 外腦」「請 GPT 外腦 review」，就是使用這份流程，透過 ego lite 的 ChatGPT 網頁取得第二意見；不是建立新的 bot 或改用 API 模型。技術上的 task space 名稱仍是「ChatGPT 決策顧問」，腳本與登錄檔名稱不變。

使用者在 ego lite 登入了 ChatGPT，用來補強各方（claude／codex／grok bot、AGM）的決策：遇到要取捨、要第二意見的問題，
就去問它。**每個專案一個固定的 ChatGPT 對話、一個分頁，不重複開**——同一個專案的問題永遠問在同一個對話裡，脈絡才接得起來。

## 怎麼問

先讀本機程式與測試，整理真正需要第二意見的問題，再呼叫腳本。**所有 bot（含 child）都明確帶 `-p <AG Man 的 project label>`**；不要用 worktree／bot 名稱，否則同專案會分成不同對話。從其他 repo 呼叫時，使用 agents-manager checkout 裡腳本的絕對路徑。

```sh
scripts/chatgpt-consult.sh -p "Agents Manager" "問題"     # 指定專案名（用 AG Man 的 project label）
scripts/chatgpt-consult.sh -p "Agents Manager" -f q.md     # 問題很長就寫成檔案
```

輸出第一行是 `[chatgpt-consult] project=… conversation=https://chatgpt.com/c/… tab=pN space=N`，接著是 ChatGPT 的回答全文。
回答要等 ChatGPT 講完才回來，預設最多等 10 分鐘（`CONSULT_TIMEOUT_MS=毫秒` 可改）。

**問什麼**：要做取捨的設計決定、兩個做法選一個、review 意見要不要採納、根因推論要第二意見。
**別問什麼**：查得到的事實（讀程式、跑測試比較準）、例行進度。問題裡**不要貼 token、密碼、ui-token、客戶資料**——那是外部服務。

問題寫清楚背景：專案、現況、選項、各自的代價、你傾向哪個。ChatGPT 看不到 repo，只看得到你貼給它的。
它的回答是參考意見：照做前自己核對，最後的決定仍照原本的流程（使用者、AGM 的裁示）。回報時區分外腦的建議與本機已驗證的事實，並保留輸出的對話連結方便追查。

遇到忙碌或逾時，先檢查原對話是否仍在生成；不要連續重送或用 `--new` 繞過等待。這是需要第二意見時使用的工具，不是每個請求的必經關卡，也不用先向 AGM 申請一般諮詢。

父 bot 派工或重用 child 時，把「GPT 外腦」、本文件位置與相同的 project label 帶進交接；child 同樣遵守本文件。教學通知不用回覆「收到」，避免額外喚醒 AGM。

## 怎麼做到「每個專案一個對話、不重複開」

- 全部在 ego lite 的同一個 task space：**「ChatGPT 決策顧問」**（agent 擁有；跟使用者自己開的 ChatGPT 分頁同一個登入，但不碰使用者的分頁）。
- 專案 → 對話網址記在 `~/.config/agents-manager/chatgpt-consult.json`：
  `{"agents-manager": {"url": "https://chatgpt.com/c/…", "label": "p2", "updated_at": "…"}}`。
- 每次問：先在 space 裡找開著這個對話的分頁 → 有就直接用；沒有（分頁被關了、ego 重開了）就照登錄檔的網址**回到同一個對話**；
  登錄檔沒有這個專案才開新對話，第一句會說明這是該專案的固定諮詢對話，送出後把新網址寫回登錄檔。
- 同一個專案一次只問一題（`~/.config/agents-manager/chatgpt-consult.<專案>.lock`）：兩個 agent 同時問，第二個會排隊等，不會打進同一個對話互相搶答。
- 對話真的壞了或脈絡太亂才重開：`scripts/chatgpt-consult.sh -p <專案> --new "問題"`（會丟掉記住的網址，舊對話留在 ChatGPT 歷史裡）。

## 不要關

- **任何 bot、AGM、browser-gc 都不准關「ChatGPT 決策顧問」這個 task space 或它的分頁**，不論 ownership 是 agent、閒置多久。
  browser-gc 的規則見 SPEC §18.4。
- 腳本本身也不呼叫 `task.finish()`：space 與分頁留著給下一次。
- 萬一被關了（或 ego lite 卡死被重開）也不用手動復原：下一次問的時候會照登錄檔重開同一個對話。
- 登錄檔 `chatgpt-consult.json` 不要刪；刪了等於每個專案都重開新對話、前面的脈絡接不上。

## 直接用 ego-browser 的注意事項

腳本的做法（`scripts/chatgpt-consult.mjs`）：`taskSpace("ChatGPT 決策顧問")` 依名字重用同一個 space；
輸入框 `#prompt-textarea`、送出鈕 `[data-testid="send-button"]`、回答是最後一個 `[data-message-author-role="assistant"]`，
`[data-testid="stop-button"]` 消失才算講完。ChatGPT 改版讓這些選擇器失效時，改腳本、不要各自手刻一套。
ego 的 `nodejs` 不繼承呼叫端的環境變數，所以 `.sh` 把參數做成一行 `globalThis.CONSULT_ARGS = {...}` 接在腳本前面。
