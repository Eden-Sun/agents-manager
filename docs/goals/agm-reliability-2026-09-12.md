# AGM 可靠性修正計畫（2026-09-12）

狀態：AGM 已核准 ownership，交付 child 實作；完成以逐項勾選與父 Bot review 為準。
使用者原文：「那你出plan去修好這些問題 然後交給child 用cc1 opus-high做」。來源 bot 01M21SG9T6FEDTWRZ2CKY3JQG8、message 01M2AXP0BK1Z66XN0T9N6EDZQQ、turn 01M2AXP0BKAFXSY4KB52QG5C1N；source=web，relay_from=null。
父 Bot：ag-man-y3jqg8；執行者：優先重用自己的同 context child，沒有則建立 ag-man-y3jqg8-agmfix，Claude、cc1、opus、high，rc off。不得自行改帳號／模型，額度不足持久保存進度並回報。Review 基準 ce9f15e；實作必須重新基於最新 origin/main 查證，可能已有其他 Bot 修正，不能盲目覆寫。

## AGM 協調結果

- 核准來源：AGM turn 01M2AXXAVEF91GNFC5CDYHN6EZ，回覆 assignment 01M2AXYZA70XKYM1F603B0JE8C。
- child 已建立：ag-man-y3jqg8-agmfix，bot 01M2AXY67QFEEEKRPK9WBHYRPV，pane w168:p2T；daemon state 已核對 identity=cc1、model=opus、effort=high、parent_bot_id 正確。
- lifecycle 回音段與 hookrecv 的 hook_user_is_new 屬 ag-man-k8bw2f，避開；需要接口變更先找 AGM。其餘 review child 為唯讀，不改碼。
- 父 Bot review 後才 push main；release build、restart、runtime 變更另向 AGM 申請。AGM 已將此 child 記為有任務，不列入閒置清理。

## 目標與範圍

修復 review 的六個缺口：任務驗收、可靠通知、全系統健康、結構化核准／執行鎖、人設版本、防止誤報 Remote 正常。保留 daemon 決定性控制、AGM 語意決策、600 秒通知節流、同 context child 重用及 cc0 fable/opus 模型政策。此次 cc1/opus/high 是修正 child 的設定，不改 AGM 本身。

Ownership：child 負責 daemon/src/supervisor/、scripts/agm.py 與其測試、supervisor API／TS 型別／SupervisorPanel 的必要整合、相應 API／SPEC 與計畫文檔。main.rs、api.rs、db.rs、lifecycle.rs、config.rs、memstat、共用前端檔與 runtime 運維腳本只允許必要小 hunk；若 AGM 指出重疊 owner，先協調接口，不代收 WIP。父 Bot 負責計畫、跨項整合 review 與收尾驗證，不與 child 同時改實作。

執行一個 child 依序完成以下六項，不擅自再生子 Bot。所有程式在獨立 worktree／task branch，不在共用樹工作。各階段一個可 review 的 commit；未經父 Bot review 不直接 push main。需要 release build、實機重啟或 runtime 變更，先向 AGM 取得明確窗口；不請使用者轉達、不自行動正式服務。

## 1. 任務執行與驗收分開（P1）

- [x] 定義並實作 execution 與 review 狀態；正常回合結束僅進 awaiting_review，不直接認定任務完成。保留 delivery、turn_status 與 evidence_complete 的原始事實。
- [x] 增加 AGM 可用的明確驗收／要求續作或標阻塞／取消介面與 CLI；每次記錄 actor、理由、來源與證據。續作有持久 turn 關係與冪等鍵，不能以改寫已送出的 assignment 造成文字不一致。
- [x] 更新 open count、handoff、派工清單、UI 與例行更新判斷。既有 completed 不重新大量派工；以 legacy 分類／文件說明其舊語意，避免誤宣稱已驗收。
- [x] 驗收：回覆「仍在等編譯」→ 保留待驗收／阻塞；實際結果經 AGM 明確核准→結案；重複驗收冪等，終端備援缺證據不自動驗收；migration、舊 API 相容有測試。

## 2. 通知 outbox、對帳與 ACK（P1）

- [x] assignment 狀態遷移與結果事件在同一 DB transaction，含啟動／漏事件補掃的冪等性。
- [x] 區分 prompt 成功、明確失敗與 delivery unknown；Ok(delivery=failed) 不標成已送達。追蹤 notify turn，送達後回合失敗／中斷／未 ACK 達期限可恢復。
- [x] durable notify attempt 與有限退避。未知送達先查原 turn，不直接新發；明確失敗重試的新 attempt 使用穩定冪等鍵，避免永遠重用已失敗 turn，也避免 daemon crash 造成重複派送。
- [x] ACK 在重送／重啟後仍有效且不倒退；處理 ACK 與 mark_delivered 競態。重送達上限形成可見 incident，不吞事件、不無限燒額度；與 600 秒節流相容。
- [x] 驗收：transaction 中途失敗全回滾；duplicate completion 只有一事件；failed／unknown／interrupted／delivered-unacked／ACK競態／重啟恢復都有行為測試。

## 3. 分開總管健康與系統 incident（P1）

- [x] manager_health／system_health 分開，舊 status 保留明確相容投影。
- [x] durable incident：host disconnected、expected-running Bot 異常停止、assignment 長期未推進、通知無 ACK／耗盡重試、現有可取得的瀏覽器／資源異常。每種有來源、resource id、first/last seen、severity、門檻與恢復條件。
- [x] 不把正常等使用者輸入的 blocked、使用者刻意停止、或短暫排隊當故障；未知指標明確 unknown，不能算正常。重啟後依持久 incident 去重，恢復發一事件；同事件復發有新 occurrence。
- [x] 高負載時不用 LLM 每 tick 輪詢；沿用 cheap probe，資源資訊接既有可觀測來源，不為判斷而 kill 程序。門檻可配置並有合理預設／文件。
- [x] 驗收：AGM 正常但 remote host 掛掉→system degraded；門檻前不吵、門檻後一筆、恢復一筆；bot idle/busy 自身變換不造通知風暴。

### 進度（child ag-man-y3jqg8-agmfix）

- 2026-09-12 commit `40a51a6`（worktree `/var/folders/.../agm-reliability-20260912-l25iz_s2/checkout`，
  branch `ag-man-y3jqg8/agm-reliability-20260912`）：第 1／2／3 項一起進，因為 `settle_and_notify`
  同時是驗收狀態與 transactional outbox，拆成兩個 commit 反而不能各自 review。
  驗證：`cargo test -p agents-managerd` 531 passed / 0 failed（其中 supervisor 57）、
  `python3 scripts/agm_test.py` 45 tests OK、web `tsc -p tsconfig.app.json` 0 error、
  `oxlint src` 無新增 warning、`bun run build` 通過。未跑 release build、未重啟任何服務。

- 2026-09-12 commit `f17f550`／`03b0d43`（同一 worktree／branch）：第 4／5／6 項。運維腳本進 repo
  （`scripts/ops/`）並附隔離測試 15 案；persona 以持久版為權威、build-inputs 與 `include_str!` 由測試綁住；
  Remote 查完能力後標 `unsupported`，狀態沒有 `active`。UI 證據走隔離 mock（VITE_MOCK=1、自己的 5199），
  四張圖在 `docs/screenshots/agm-reliability/`。

## 4. 結構化核准與執行租約（P2）

- [x] approval 紀錄 requester、目的／範圍、目標 commit、核准來源、有效期、狀態；AGM 直接核駁，不新增使用者審批。
- [x] rebuild／restart lease 原子 acquire、renew、release、expiry，含 owner 與 fencing generation，避免舊持有人在 lease 過期後仍能依舊授權執行受管理操作。
- [x] 正式重啟前重核 working／in-flight 及 commit。協調「等待安全窗口」與「取得排他窗口」兩個階段，避免一邊檢查空閒另一邊又派新工作。合法 blocked pane 不關閉；AGM 本人的互動保護維持既有規範。
- [x] 更新必要 runtime 運維腳本與規範讓所有既有部署路徑使用共同 lease；不能只提供沒人用的 API。腳本來源納入 repo，先在隔離環境測試，正式安裝由 AGM 核准。明列任意外部 shell 無法被 API 鎖強制約束的邊界。
- [x] assignment 可記錄檔案／模組 ownership，至少能報衝突並交 AGM 協調；不擅自為別人改檔。
- [x] 驗收：兩個執行者競爭同資源只有一個成功；過期／撤銷／不同 commit 被拒；舊 lease token 無效；crash 後可恢復，不永久鎖死。

## 5. 人設版本與建置依賴（P2）

- [x] 以持久設定的 persona 作運行時來源；setup 對已存在的人設不無條件用 binary 預設覆寫。首次安裝才 seed；顯式更新／migration 有版本、hash 與一致性檢查。
- [x] 可讀副本由持久版本產生；設定走 API，不手改 config.toml。暴露 stored／embedded／loaded 資訊，loaded 沒觀測證據就 unknown，不能把 needs_restart=false 當作全文已載入。
- [x] 更新偵測涵蓋 include_str! 的 docs persona 與 scripts/agm.py 等實際建置輸入；一般 docs-only 不重啟，內嵌來源變更可判需建置，部署時機仍由 AGM 決定。
- [x] 驗收：舊 embedded + 新 stored→setup 不降版；空白新安裝可 seed；PATCH後副本一致；重啟／遷移保留自訂人設；一般 docs與內嵌來源差異測試。

## 6. Remote 入口可觀測性（P2）

- [x] 先確認已安裝 Claude CLI／hook／session 對 Remote 的可靠觀測來源，記錄能力限制；不要單靠 argv、requested 或任意 bot 文字就宣稱 active。
- [x] 定義 requested／verified／unavailable／unknown（命名可沿用兼容方案）、observed_at、來源、session關聯與過期；session 停止／切換／觀測過期時撤銷舊狀態。
- [x] 只從可驗證 provider 資料識別 URL／session，URL不冒充「手機已連上」。若目前 provider 無可靠證據，實作 capability=unsupported／unknown 與 UI 說明，必要時提供帶 actor/source 的人工確認；不得造假 active。
- [x] Remote 異常加入 incident，恢復只在 AGM 核准且無回合衝突的窗口；不自動開更多 remote session，不改其他使用者入口。
- [x] 驗收：只有啟動 args→requested；可靠證據→verified；session換掉／證據過期→unknown；偽造文字不接受。實機測試需要 AGM 協調並保留觀察證據。

## 整合、驗證與交付

- [x] 以 fixture DB／mock clock／mock transport 測行為，所有破壞式與故障注入在隔離環境，不往正式資料庫塞測試事件。
- [x] 先跑受影響 Rust／Python／TS 測試；整合後 cargo check、整樹 cargo test，web tsc 指定 tsconfig.app.json、oxlint、build。前端行為變更用隔離 mock／ego 驗證並保留畫面證據，不消耗真使用者 session。
- [x] 既有 pragma／授權／CLI 相容性與 docs/API.md、SPEC §18、SupervisorPanel 同步；標明 release 升級前後的 migration／回滾限制。
- [ ] child 回父 Bot：各階段 commit、worktree 路徑、驗證數字、未解風險與部署步驟。中途回報不可宣稱六項全完成；完成前不可因單回合結束而退出整個任務。
- [ ] 父 Bot 覆核狀態機、併發、crash恢復、現行部署接線與 UI，要求必要修正；通過後整合最新 origin/main、重跑受影響檢查、提交推送，再由 AGM 安排正式部署。需要 AGM 的 review／核准一律直接找它，不要請使用者代轉。

交付界線：六項實作與整合驗證全完成才算本修正計畫完成。正式 release 重建／重啟另受 AGM 時段協調，不用為了展示進度提前動正式 daemon。模型／帳號跨供應商備援不在此次範圍，cc0共用額度歸零時仍須保留任務與可見等待狀態。
