# AGM reliability 父 review（2026-09-13）

基底 origin/main c271971，覆核 child 最終 98721ff 及父補修。#30、#31、#32 均納入本輪。

## 已處理

- prompt_relayed 三處：交辦帶 AGM bot ID，啟動／通知帶 daemon。來源讀失敗延後，不冒充使用者。
- 續派在單一 transaction 中更新原单、插入 queued 續作與稽核；提交後派送。不同續派 request ID／文字／目標不可冒充冪等成功。交易層也比對已有續作內容。
- watchdog 第五次啟動成功但立即死亡：持久一次性回報。取消 unknown／delivered 僅取消追蹤，保留送達事實。
- 通知耗盡不再經壞掉的通道通知自己；探針失敗不結束已有故障。明確 stopped 的 autostart bot 不當作意外停止。
- 核准決定、取得租約與續租共用 supervisor lock；到期／撤銷／核准紀錄遺失不得延長授權。
- ops 保留同 commit 核准 ID，跨整點接續審批；資料讀失敗停止派工，拒絕不重複申請，過期後可重申請。
- persona 首次遷移保留既有自訂正文；副本同步失敗回報已保存版本，重送同文可修復。Remote 外部宣稱只收 manual 並要求 evidence／actor／有效 run。
- 前端集合防禦；故障 API 錯誤與 404 不再顯示健康。手機 AGM 面板與錯誤畫面已用 ego 驗證，證據在 docs/screenshots/agm-reliability/。

## 驗證

- cargo test -p agents-managerd：612 passed，0 failed；cargo check 通過。
- web：tsc --noEmit 通過、bun test 168 passed、oxlint exit 0（既有 warnings）。
- scripts/agm_test.py：63 passed；scripts/ops/daemon-update-kick_test.sh：29 passed。
- Rust 測試以移除 AM_KIND／AM_MODEL／AM_EFFORT 的環境跑，避免 herdr_shim 測試繼承父 bot 的模型。
- 隔離 worktree 的 sqlite 與 node_modules 快取有缺檔，重建該測試依賴後驗證；沒有清理共用 WIP。
- 本輪未執行 release build、重啟或安裝 runtime。

## AGM 部署交接與限制

1. 從已推送 main 建置新 daemon。啟用後確認 supervisor API、CLI approval／lease／review 可用，再安裝 scripts/ops/daemon-update-kick.sh 到 AGM 的 bin/（0755）。
2. launchd 的 AGM_BUILD_BOT 必須是 AGM 建置 child 的實際 bot ID；預設未設會跳過。daemon-update-task.md 維持 AGM 維護；restart 另外核准與取得 lease。
3. 保存 daemon-update.approval.json、daemon-update.last、daemon-update.built。daemon-update.lock 殘留時先確認沒有活躍執行者，再由 AGM 處理，不能直接當作空閒。
4. **restart lease 只暫停 supervisor assignment**，普通 prompt／team／scheduler 尚未共用閘門；正式替換前 AGM 仍須重驗窗口。ops README 的這段限制保留。
5. 瀏覽器／資源異常尚未整合進本模組 durable incident，仍依既有獨立監控處理。原 goal 相關項目退回未勾，不能稱全系統監控完整。
6. Inbox 屬可重送、需 ACK 的通知；事件 ID 用於去重，不承諾任何 crash 時點都恰好只看一次通知。Remote 手機連線仍需要實際人工確認。
